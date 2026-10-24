mod agent;
mod airplay;
mod asr;
mod capture;
mod config;
mod device;
mod mcp;
mod mcp_legacy_sse;
mod music;
mod server;
mod shell;
mod speech;
mod tools;
mod tts;
mod vad;
mod weather;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::agent::AgentRuntime;
use crate::airplay::AirPlayService;
use crate::asr::AsrClient;
use crate::capture::{
    record_utterance, record_utterance_server_vad_streaming, record_utterance_streaming,
};
use crate::config::{classify_config_change, AppConfig, ConfigChanges, ConfigHandle, DeviceConfig};
use crate::device::Device;
use crate::music::MusicService;
use crate::shell::TIMEOUT_ERROR_PREFIX;
use crate::speech::{SocketOwnership, SpeechHandle, SpeechService, SpeechServiceEvent, SpeechTurn};
use crate::tts::{TtsHealth, TtsRouter};

/// TTS 健康度是**进程级**的：局部变量会随每次 `run_session` 归零，
/// 「坏了三天」就永远攒不到阈值。
static TTS_HEALTH: LazyLock<TtsHealth> = LazyLock::new(TtsHealth::default);

const ASR_SERVICE_ERROR_PROMPT: &str = "抱歉，语音识别服务遇到问题，请稍后重试";
const LLM_SERVICE_ERROR_PROMPT: &str = "抱歉，大模型服务遇到问题，请稍后重试";

#[derive(Debug, Parser)]
#[command(name = "xiaoai-agent")]
#[command(about = "Standalone XiaoAI on-device agent using the native mipns speech frontend")]
struct Cli {
    #[arg(short, long, default_value = "/data/open-xiaoai/agent.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let config = ConfigHandle::new(Arc::new(
        AppConfig::load(&cli.config)
            .with_context(|| format!("failed to load config {}", cli.config.display()))?,
    ));
    // 启动期快照：socket / device / asr / airplay 四段按「改完要重启」处理，
    // 所以这里取一次就够；llm / agent / music / capture 走句柄，随时可热加载。
    let initial = config.get().await;
    let mut speech = SpeechService::bind(&initial.runtime.speech_socket).await?;
    let speech_handle = speech.handle();
    // 让位判断用：我绑的是哪个 socket。让位脚本会把原生 socket mv 回同一路径，
    // inode 随之改变——用它判断「到底有没有交出去」，比看退出码可靠。
    let ownership = speech.ownership();

    // 本地控制面（issue #9）：默认关闭，关闭时不 bind 任何端口。
    // 放在 speech bind 之后：/health 的判据是「speech socket 还是不是我们的」，
    // 手里必须先有 ownership 才有意义。
    let control_state = Arc::new(server::ControlState {
        token: initial.server.token.clone(),
        socket: ownership.clone(),
        liveness: speech.liveness(),
        started_at: std::time::Instant::now(),
        config_path: Some(cli.config.clone()),
    });
    let _control_task = server::spawn(&initial.server, control_state).await?;

    let device = Device::new(initial.device.clone());
    let asr = AsrClient::new(initial.asr.clone())?;
    let music = Arc::new(MusicService::new(config.clone(), device.clone())?);
    let airplay = AirPlayService::start(initial.airplay.clone()).await?;
    let agent = Arc::new(AgentRuntime::new(config.clone(), device.clone(), music.clone()).await?);
    let failures = Arc::new(AtomicU32::new(0));
    let (reload_tx, mut reload_rx) = mpsc::channel::<()>(1);
    spawn_sighup_listener(reload_tx);
    // 重载去抖门：同时只跑一个，期间的请求合并成下一轮。
    let reload_gate = Arc::new(ReloadGate::default());
    let launcher = launcher_path();
    if launcher.is_none() {
        warn!("找不到 start-agent.sh：故障时无法主动让位，只能退出等 watchdog 重启");
    }

    info!(socket = %initial.runtime.speech_socket, "xiaoai-agent ready");
    device
        .blink_ready(initial.device.led_listening, Duration::from_millis(250))
        .await;

    let mut active_session: Option<ActiveSession> = None;
    let mut session_check = interval(Duration::from_millis(250));
    session_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            event = speech.recv() => {
                let Some(event) = event else {
                    // 服务任务自己结束了：和 Fatal 同一类故障，先让位再退出。
                    let outcome = yield_to_native(
                        &device,
                        launcher.as_deref(),
                        &ownership,
                        "speech.usock service stopped",
                    )
                    .await;
                    std::process::exit(if outcome.succeeded() { 0 } else { 1 });
                };
                match event {
                    SpeechServiceEvent::Registered { vendor, codec } => {
                        info!(vendor, codec = codec.as_deref().unwrap_or("pcm16"), "speech frontend registered");
                    }
                    SpeechServiceEvent::Fatal(message) => {
                        // 协议级故障不再直接 bail 等 watchdog 在几十秒后重启。
                        // 先把手上这一轮收干净，再把麦克风还给原厂小爱。
                        error!(reason = %message, "speech frontend reported a fatal error");
                        if let Some(active) = active_session.take() {
                            let interrupted = active.turn_id.load(Ordering::Relaxed);
                            if !active.task.is_finished() {
                                active.task.abort();
                            }
                            let _ = active.task.await;
                            let state = build_turn_state(
                                &config, &initial.device, &device, &asr, &agent, &music,
                                &airplay, &failures,
                            );
                            // 服务已经报故障，收尾命令可能没人应答；给 2s 上限，
                            // 别让「让位」本身卡在一个等不到回复的 oneshot 上。
                            let _ = timeout(
                                Duration::from_secs(2),
                                interrupt_turn(
                                    &state,
                                    &speech_handle,
                                    interrupted,
                                    InterruptCause::Failure("speech service fatal"),
                                ),
                            )
                            .await;
                        }
                        let outcome = yield_to_native(
                            &device,
                            launcher.as_deref(),
                            &ownership,
                            "speech.usock fatal",
                        )
                        .await;
                        std::process::exit(if outcome.succeeded() { 0 } else { 1 });
                    }
                    SpeechServiceEvent::Turn(turn) if turn.activation().is_wakeup() => {
                        info!(
                            turn_id = turn.id(),
                            interaction_mode = turn.interaction_mode(),
                            "WAKE native mipns"
                        );
                        let state = build_turn_state(
                            &config, &initial.device, &device, &asr, &agent, &music, &airplay,
                            &failures,
                        );
                        if let Some(active) = active_session.take() {
                            let interrupted = active.turn_id.load(Ordering::Relaxed);
                            if !active.task.is_finished() {
                                active.task.abort();
                            }
                            let _ = active.task.await;
                            // 被 abort 的任务走不到自己的收尾，旧对话只能在这里
                            // finish 掉、会话只能在这里重置。
                            interrupt_turn(
                                &state,
                                &speech_handle,
                                interrupted,
                                InterruptCause::Wake,
                            )
                            .await;
                        } else {
                            agent.reset_session("wake keyword").await;
                        }

                        let music_interrupted = music.interrupt_for_wake().await;
                        let airplay_interrupted = airplay.interrupt_for_wake().await;
                        if !music_interrupted && !airplay_interrupted {
                            device.abort_current_output().await;
                        }
                        // LED 清理留在这一层：任务已被 abort，run_turn 的尾巴跑不到。
                        cleanup_turn_leds(&device, &initial.device).await;

                        let (followup_tx, followup_rx) = mpsc::channel(2);
                        let handle = speech_handle.clone();
                        let turn_id = Arc::new(AtomicU64::new(turn.id()));
                        let task_turn_id = turn_id.clone();
                        let task = tokio::spawn(async move {
                            if let Err(err) =
                                run_turn(state, handle, turn, followup_rx, task_turn_id).await
                            {
                                error!("turn failed: {err:?}");
                            }
                        });
                        active_session = Some(ActiveSession {
                            task,
                            followup_tx,
                            turn_id,
                        });
                    }
                    SpeechServiceEvent::Turn(turn) => {
                        info!(
                            turn_id = turn.id(),
                            activation = ?turn.activation(),
                            interaction_mode = turn.interaction_mode(),
                            "FOLLOW_UP native mipns"
                        );
                        let receiver_ready = active_session
                            .as_ref()
                            .is_some_and(|active| !active.task.is_finished());
                        if receiver_ready {
                            let send_result = active_session
                                .as_ref()
                                .expect("active session was checked")
                                .followup_tx
                                .send(turn)
                                .await;
                            if let Err(err) = send_result {
                                reject_orphaned_turn(err.0, &speech_handle).await;
                            }
                        } else {
                            reject_orphaned_turn(turn, &speech_handle).await;
                        }
                    }
                }
            }
            Some(()) = reload_rx.recv() => {
                // 重载不进主循环：MCP 重连单 server 最坏 20 秒（mcp.rs 的
                // 2×timeout_s 上限），这段时间必须继续 poll 语音事件——
                // 语音是这台设备唯一的命脉，配置重载是可以等的东西。
                spawn_config_reload(&config, cli.config.clone(), &agent, &reload_gate);
            }
            _ = session_check.tick() => {
                let session_finished = active_session
                    .as_ref()
                    .is_some_and(|active| active.task.is_finished());
                if session_finished {
                    if let Some(active) = active_session.take() {
                        if let Err(err) = active.task.await {
                            warn!("turn task ended unexpectedly: {err:?}");
                        }
                    }
                    let streak = failures.load(Ordering::Relaxed);
                    if streak >= YIELD_AFTER_CONSECUTIVE_FAILURES {
                        error!(
                            consecutive_failures = streak,
                            "连续多轮会话失败，故障让位给原厂小爱"
                        );
                        if let Some(code) = yield_after_consecutive_failures(
                            &device,
                            launcher.as_deref(),
                            &ownership,
                            &failures,
                        )
                        .await
                        {
                            std::process::exit(code);
                        }
                    }
                    device
                        .blink_ready(initial.device.led_listening, Duration::from_millis(250))
                        .await;
                }
            }
        }
    }
}

struct ActiveSession {
    task: JoinHandle<()>,
    followup_tx: mpsc::Sender<SpeechTurn>,
    /// 会话当前正在处理的 turn。唤醒打断时靠它把「被打断的是哪一轮」告诉
    /// `interrupt_turn`——任务被 abort 之后，任务内部的状态已经取不回来了。
    turn_id: Arc<AtomicU64>,
}

/// 中断原因。它决定两件事：session 重置的日志理由，以及收尾失败该按 debug
/// 还是 warn 记。
///
/// 关键区分不是「谁触发的」，而是**这条收尾是不是预期内的正常结束**：
/// 唤醒打断（旧轮被新流取代，`finish_dialog` 会 stale）、会话空闲超时、
/// 前端正常关流都属于「预期」，只有 `Failure` 才是真故障。把它们混在
/// `Failure` 里会让正常收尾在日志里以 warn 出现，现场排查时全是噪音。
#[derive(Debug, Clone, Copy)]
enum InterruptCause {
    /// 新的唤醒词打断了上一轮。
    Wake,
    /// 会话空闲超时（采集等不到人说话、等不到 follow-up 流）。
    IdleTimeout,
    /// 语音前端正常关闭了流（follow-up 发送端没了），不是故障。
    FrontendStopped,
    /// 采集中途失败（mipns 取消流、ASR 出错、ASR/TTS 抛出错误等）。
    Failure(&'static str),
}

impl InterruptCause {
    fn reason(self) -> &'static str {
        match self {
            Self::Wake => "wake keyword",
            Self::IdleTimeout => "session idle timeout",
            Self::FrontendStopped => "speech frontend stopped",
            Self::Failure(reason) => reason,
        }
    }

    /// 这条收尾是否属于预期内的正常结束。`false` 只有 `Failure`。
    fn expected(self) -> bool {
        !matches!(self, Self::Failure(_))
    }
}

/// 三条中断路径唯一的收尾出口：唤醒打断、会话空闲超时、采集中途失败。
///
/// 全部 best-effort，每一步都不向外传播错误——清理本身绝不该把一轮会话
/// 变成失败。两个具体理由：
///
/// 1. 唤醒打断时 mipns 已经准备了新 stream，`speech.rs` 的 `last_turn_id`
///    检查会把对旧 turn 的 `finish_dialog` 判成 stale。那是预期路径。
/// 2. 写成显式 `async fn` 而不是 Drop guard：Drop 不能 await，只能在 Drop 里
///    塞一个 `tokio::spawn`，于是断言前得先「睡一下」，测试立刻变成靠时间猜。
///
/// 这里**不发** `stop_capture`：停采集是 `capture.rs` 的职责，而且它有不对称
/// 契约——超时必须先补发 stop_capture（否则 mipns 的 stream 一直挂着、下次
/// 唤醒绑不上新流），`End`/`Cancelled` 则不许补发（流是对方结束的）。在这一层
/// 统一补一条会把那个契约踩掉。
async fn interrupt_turn(
    state: &TurnState,
    speech: &SpeechHandle,
    turn_id: u64,
    cause: InterruptCause,
) {
    if let Err(err) = speech.finish_dialog(turn_id).await {
        // 预期内的收尾（唤醒打断的 stale、空闲超时后对面已经关了）不打 warn：
        // 否则每轮正常结束都往设备日志里写一条「失败」，真故障就被淹了。
        if cause.expected() {
            debug!(
                turn_id,
                cause = cause.reason(),
                error = %err,
                "收尾的 finish_dialog 没有送达（属于预期路径）"
            );
        } else {
            warn!(
                turn_id,
                cause = cause.reason(),
                error = %err,
                "中断收尾时 finish_dialog 失败"
            );
        }
    }
    state.agent.reset_session(cause.reason()).await;
}

#[derive(Clone)]
struct TurnState {
    /// 热加载句柄：每轮取一次快照，所以改完 yaml 下一轮就生效。
    config: ConfigHandle,
    /// LED id 来自启动期快照（device 段按「改完要重启」处理，不能一半新一半旧）。
    ///
    /// 注意这是**第二份真值**：它与 `config.get().device` 恒等，靠
    /// `keep_pending_sections` 把 device 段回填成旧值来维持。改造 device 段的
    /// 热加载时，这两处必须一起改，否则会出现「LED 用新 id、TTS 用旧模板」。
    led: DeviceConfig,
    device: Device,
    asr: AsrClient,
    agent: Arc<AgentRuntime>,
    music: Arc<MusicService>,
    airplay: AirPlayService,
    /// 连续失败轮数。到阈值就说明「能唤醒但答不上」已经持续了整段会话，
    /// 与其继续硬撑，不如把麦克风还给原厂小爱。
    failures: Arc<AtomicU32>,
}

/// 连续失败到这个数就让位。取 3：一次失败可能是网络抖，连续三次已经完全
/// 不像「偶发」了，而用户在这期间只会看到灯亮灯灭、听不到一句话。
const YIELD_AFTER_CONSECUTIVE_FAILURES: u32 = 3;

fn build_turn_state(
    config: &ConfigHandle,
    led: &DeviceConfig,
    device: &Device,
    asr: &AsrClient,
    agent: &Arc<AgentRuntime>,
    music: &Arc<MusicService>,
    airplay: &AirPlayService,
    failures: &Arc<AtomicU32>,
) -> TurnState {
    TurnState {
        config: config.clone(),
        led: led.clone(),
        device: device.clone(),
        asr: asr.clone(),
        agent: agent.clone(),
        music: music.clone(),
        airplay: airplay.clone(),
        failures: failures.clone(),
    }
}

/// 把本次不生效的配置段（需重启 / 被拒绝）回填成旧值。
///
/// 这样 `ConfigHandle` 里始终是「当前真正在跑的配置」，热加载后读句柄不会读到
/// 一份「device 是新值、实际 Device 还是旧模板」的假象。
fn keep_pending_sections(
    current: &AppConfig,
    next: &Arc<AppConfig>,
    changes: &ConfigChanges,
) -> AppConfig {
    let mut effective = (**next).clone();
    for section in changes.rejected.iter().chain(changes.needs_restart.iter()) {
        match *section {
            "runtime.speech_socket" => {
                effective.runtime.speech_socket = current.runtime.speech_socket.clone();
            }
            "device" => effective.device = current.device.clone(),
            "asr" => effective.asr = current.asr.clone(),
            "airplay" => effective.airplay = current.airplay.clone(),
            "server" => effective.server = current.server.clone(),
            // 这条分支以前只 warn、不回填，等于「没保留旧值」：新段的字段会原样
            // 进句柄，而实际跑着的还是旧配置。server.token 一旦这样漏掉，日志说
            // 「已热加载」但监听器用的还是旧 token —— 安全相关的显示与实际不符。
            other => warn!(
                section = other,
                "未知的待生效配置段：没有回填逻辑，句柄里会是新值（与实际运行值不一致）"
            ),
        }
    }
    effective
}

/// 监听 SIGHUP，只负责「通知主循环该重载了」。
///
/// 真正的重载在主循环里做：那样配置切换不会和一轮对话抢同一个 await 点，
/// 也让 `apply_config_reload` 能脱离信号被单测直接调用。
fn spawn_sighup_listener(tx: mpsc::Sender<()>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::hangup()) {
                Ok(mut stream) => {
                    while stream.recv().await.is_some() {
                        if tx.send(()).await.is_err() {
                            break;
                        }
                    }
                }
                Err(err) => warn!(error = %err, "无法安装 SIGHUP 处理器，配置热加载不可用"),
            }
        }
        #[cfg(not(unix))]
        {
            drop(tx);
            warn!("SIGHUP 配置热加载只在 unix 上可用");
        }
    });
}

/// 重载去抖门：同一时刻只跑一个重载任务，期间的请求**合并**成「跑完再跑一轮」。
///
/// 为什么是合并而不是丢弃：重载要跑几十秒（MCP 重连），用户完全可能在这期间
/// 发现自己写错了再改一次。丢弃的话最终生效的是**第一次那份**，用户只看到一条
/// 很容易被当成噪音的 warn——那正好是「改了、发了信号、没生效」这个本 PR 要消灭的
/// 失败模式。合并（每轮重读文件）等于「用最后的文件状态跑一次」。
#[derive(Default)]
struct ReloadGate {
    running: AtomicBool,
    pending: AtomicBool,
}

impl ReloadGate {
    /// 抢占本轮执行权。`false` 表示已有任务在跑，本次请求已并入下一轮。
    fn begin(&self) -> bool {
        if self.running.swap(true, Ordering::AcqRel) {
            self.pending.store(true, Ordering::Release);
            return false;
        }
        true
    }

    /// 一轮结束：还有合并进来的请求就返回 `true`（需要再跑一轮）。
    fn finish_round(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }

    fn release(&self) {
        self.running.store(false, Ordering::Release);
    }
}

/// 用 RAII 复位 `running`，而不是手动 `store(false)`。
///
/// 手动复位的版本一旦 `apply_config_reload` panic（rig 代码、分配失败、task 被
/// abort），`running` 就永久停在 `true`，此后每次 SIGHUP 都在 `begin()` 处被拒，
/// 热加载永久失效——而用户下次用到这个功能可能是几周后。
struct ReloadGuard(Arc<ReloadGate>);

impl Drop for ReloadGuard {
    fn drop(&mut self) {
        self.0.release();
    }
}

/// 连续失败达到阈值后的处置：让位成功就退出，**没成功就继续跑**。
///
/// 这是「让位失败后的存活策略」里选的那一档（C-lite）：
///
/// - `Fatal` / 事件通道关闭 = 语音服务已经死了，留着也没用 → 让位后一律退出；
/// - 连续会话失败 = 服务**还活着**（只是答不上来），此时退出会留下
///   「哑 + watchdog 已被 stop_watchdog 停掉」的状态，比继续撑着更糟。
///   所以让位没成功就把失败计数清零、继续服务，等下一次攒够再试。
///
/// 不做「立刻重试让位」：重试会再调一次 `start-agent.sh stop`，而它用 `mkdir`
/// 锁和 watchdog 互斥；第一次因为「杀不掉 agent」失败的话第二次大概率同因失败，
/// 结果是反复 stop/start 原生服务，比不动更糟。
///
/// 返回 `Some(退出码)` = 调用方应当退出；`None` = 继续跑。
async fn yield_after_consecutive_failures(
    device: &Device,
    launcher: Option<&Path>,
    ownership: &SocketOwnership,
    failures: &Arc<AtomicU32>,
) -> Option<i32> {
    let outcome = yield_to_native(device, launcher, ownership, "consecutive failures").await;
    if outcome.succeeded() {
        return Some(0);
    }
    // 「让位失败」不是一种状态。超时意味着脚本可能正停在还原中途（保护已卸、
    // 原生 socket 还没 mv 回），而 watchdog 早在 restore 第 2 步就被停了。
    // 这种情况下继续运行 = LED 照常闪、进程照常在、但麦克风不工作、没人兜底，
    // 比退出更难被发现。所以超时一律判失败退出，只有「脚本根本没动前端」
    // 那一类（锁冲突、launcher 不存在等）才继续运行。
    if outcome.timed_out {
        error!(
            "让位命令超时：脚本可能停在还原中途，判失败并退出（继续运行会表现为「一切正常但麦克风不工作」）"
        );
        return Some(1);
    }
    // 只有「脚本根本没动前端」（锁被占用 / launcher 不可执行）才允许继续运行。
    // 其余（4 = pns/watchdog 已停但 mico 没停、5 = 保护没卸或原生没恢复）都已经
    // 动过前端：此时继续跑表现为「LED 正常闪、进程正常在、麦克风不工作、
    // watchdog 已停」，比 exit(1) 更难被发现，所以一律退出。
    if outcome.exit_code == Some(LAUNCHER_EXIT_UNTOUCHED) {
        warn!("让位命令未执行（launcher 锁被占用等），前端未受影响，继续运行；连续失败计数清零");
        failures.store(0, Ordering::Relaxed);
        return None;
    }
    if outcome.exit_code == Some(LAUNCHER_EXIT_HALF_DISMANTLED) {
        error!(
            "让位命令报告原生服务未起来（exit=5）：inode 已换回原生，但 mico/pns 不可用，判失败退出"
        );
    } else {
        error!(
            exit = ?outcome.exit_code,
            "让位失败且前端可能已被拆开：判失败退出，避免「看起来一切正常但麦克风不工作」"
        );
    }
    Some(1)
}

/// 把一次重载丢到后台跑。
///
/// 为什么不在主循环里 `await`：`agent.reload()` 里含 MCP 重连，单 server 上限
/// 2×`timeout_s`（默认 20 秒），多 server 累加。那段时间主循环不被 poll，
/// `speech.recv()` 收不到唤醒、`session_check` 推迟失败检测、`Fatal` 触发的
/// 让位也会被推迟——等于让一条配置重载堵住故障恢复路径。
fn spawn_config_reload(
    config: &ConfigHandle,
    path: PathBuf,
    agent: &Arc<AgentRuntime>,
    gate: &Arc<ReloadGate>,
) {
    if !gate.begin() {
        info!("配置重载进行中，本次 SIGHUP 合并到下一轮");
        return;
    }
    let config = config.clone();
    let agent = agent.clone();
    let gate = gate.clone();
    tokio::spawn(async move {
        // 无论正常结束还是 panic 展开，都会把 running 复位。
        let _guard = ReloadGuard(gate.clone());
        loop {
            apply_config_reload(&config, &path, &agent).await;
            if !gate.finish_round() {
                break;
            }
            info!("上一轮重载期间又来了一次 SIGHUP，按最新文件内容再跑一轮");
        }
    });
}

/// 执行一次配置重载：读文件 → 分类 → 换句柄 → 重建工具与 MCP。
///
/// 任何一步失败都保留旧配置：一份写坏的 yaml 不能让音箱因为一次 SIGHUP 变哑。
async fn apply_config_reload(handle: &ConfigHandle, path: &Path, agent: &AgentRuntime) {
    let current = handle.get().await;
    let next = match AppConfig::load(path) {
        Ok(next) => Arc::new(next),
        Err(err) => {
            warn!(
                error = %err,
                path = %path.display(),
                "配置重载失败，继续使用旧配置"
            );
            return;
        }
    };

    let changes = classify_config_change(&current, &next);
    if !changes.rejected.is_empty() {
        warn!(
            rejected = ?changes.rejected,
            "以下配置段运行中无法切换，本次忽略（其余段照常生效）"
        );
    }
    if !changes.needs_restart.is_empty() {
        warn!(
            needs_restart = ?changes.needs_restart,
            "以下配置段要重启 start-agent.sh 才生效，本次沿用旧值"
        );
    }

    if !changes.should_apply() {
        // 告警已经打过。两种「不用重载」给不同文案，但只需要这一处判断：
        // is_noop 是 should_apply == false 的子集，并排写两个会让人以为有别的区别。
        if changes.is_noop() {
            info!("配置文件无变化，跳过重载");
        } else {
            info!("本次没有可热加载的段，句柄与工具服务器保持不变");
        }
        return;
    }

    // 关键不变量：句柄里的配置必须等于「运行中真正生效的配置」。
    // 所以待重启段与拒绝段要回填旧值，否则读句柄的人会以为 device/asr 已经换了。
    // 放在 should_apply 判断之后：省掉「确定不重载」时白 clone 一整份配置。
    let effective = keep_pending_sections(&current, &next, &changes);
    let previous = handle.replace(Arc::new(effective)).await;
    // 工具结构体各自持有配置快照（见 agent.rs 的 build_tool_server），
    // 所以换配置之后必须让 agent 重建工具服务器与 MCP 连接。
    agent.reload().await;
    info!(
        applied = ?changes.applied,
        "配置已热加载（下一轮生效）"
    );
    drop(previous);
}

/// `start-agent.sh` 的位置：优先 `XIAOAI_AGENT_HOME`，否则跟二进制同目录
/// （launcher 启动的就是 `$AGENT_HOME/xiaoai-agent`）。
fn launcher_path() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("XIAOAI_AGENT_HOME") {
        let candidate = PathBuf::from(home).join("start-agent.sh");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.parent()?.join("start-agent.sh");
    candidate.is_file().then_some(candidate)
}

/// 故障让位：把麦克风还给原厂小爱。
///
/// 只做一件事——调 launcher 的 `stop`。它内部按顺序 stop pns、停 watchdog、停
/// mico、卸掉 speech.usock 保护、还原原生文件，再把原生 mico/pns 起回来
/// （`start-agent.sh` 的 `restore_native_frontend`）。agent **不**自己拼这套动作：
/// 顺序错一步音箱就哑，而且那份逻辑已经在脚本里被真机验证过。
///
/// 两个必须知道的副作用：
/// - launcher 会在中途 `kill` 掉本进程，所以这里打的日志是「意图」，结果得看
///   launcher 日志与 `start-agent.sh status`；正常情况下后面的 `exit` 根本跑不到。
/// - `stop` 会顺手停掉 watchdog。让位之后音箱停在原厂状态，直到有人再跑
///   `start-agent.sh start`——这就是这个 issue 想要的结果。
///
/// 还有一条别动的细节：本进程被 SIGTERM 打死时不会跑析构，`SpeechService` 的
/// Drop（它 `remove_file(socket_path)`）因此不会执行。这正是我们要的——脚本
/// 已经把原生的 `speech.usock` 移回原位，跑析构会把刚还原好的原生 socket 删掉。
/// 一次让位尝试的结果。
///
/// 两个事实分开记：`issued` 是「命令跑成功了吗」，`taken_over` 是「麦克风真的
/// 交出去了吗」。只看前者会误判——launcher 可能因为「杀不掉 agent」在收尾阶段
/// 返回非 0，而原生前端早已恢复；调过来也一样，脚本成功但 socket 还在我们手里
/// 就绝不能当成功。
/// `start-agent.sh stop` 的「前端一点没动」退出码（锁被占用等）。
///
/// 契约在 `start-agent.sh` 顶部：0 成功 / 3 未受影响 / 4 半哑 / 5 拆到一半。
/// 只有 3 允许继续运行——其余都意味着前端已经被动过，继续跑会表现为
/// 「LED 正常闪、麦克风不工作」。
const LAUNCHER_EXIT_UNTOUCHED: i32 = 3;
/// `restore_native_files` 之后还有两步（`start_native_mico` / `pns start`）。
/// 它们失败时 **inode 已经换回原生的了**，但 mico/pns 没跑起来——socket 在位、
/// 麦克风不工作。这是唯一一个「inode 事实说成功、脚本自述说失败」的格子，
/// 必须以脚本为准，所以它的判定要排在 `succeeded()` 之前。
const LAUNCHER_EXIT_HALF_DISMANTLED: i32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YieldOutcome {
    issued: bool,
    taken_over: bool,
    /// 脚本真实退出码（没跑起来 / 超时则为 None）。
    exit_code: Option<i32>,
    /// 命令超时（脚本可能仍停在还原中途）。超时是「脚本正在跑」的最强信号，
    /// 此时任何 inode 快照都不可靠，不能当成「什么都没发生」。
    timed_out: bool,
}

impl YieldOutcome {
    /// 让位是否达成：命令成功，或者事实上已经被接管走。
    ///
    /// 但 `exit 5`（脚本自述「原生服务没起来」）**压过 inode 事实**：那两格
    /// （`start_native_mico` / `pns start`）发生在 `rm` + `mv` **之后**，inode 已经
    /// 换回原生的了，但 mico/pns 没跑——socket 在位、麦克风不工作。脚本比 inode
    /// 知道得多，所以这一档以脚本为准。
    ///
    /// 这条规则必须放在这里而不是某个调用点：`succeeded()` 有三个调用点
    /// （`Fatal` ×2、连续失败 ×1），规则写在外面就要求三处各自解释一遍，
    /// 而漏掉的那处恰好是更常走的 `Fatal` 路径。
    fn succeeded(&self) -> bool {
        if self.exit_code == Some(LAUNCHER_EXIT_HALF_DISMANTLED) {
            return false;
        }
        self.issued || self.taken_over
    }
}

async fn yield_to_native(
    device: &Device,
    launcher: Option<&Path>,
    ownership: &SocketOwnership,
    reason: &str,
) -> YieldOutcome {
    let Some(launcher) = launcher else {
        error!(
            reason,
            "找不到 start-agent.sh，无法让位；只能退出交给 watchdog 处理"
        );
        return YieldOutcome {
            issued: false,
            taken_over: ownership.taken_over(),
            timed_out: false,
            exit_code: None,
        };
    };
    let script = format!(
        "{} stop",
        shell_words::quote(&launcher.display().to_string())
    );
    warn!(
        reason,
        launcher = %launcher.display(),
        "故障让位：把麦克风还给原厂小爱"
    );
    // 走 detached 通道：还原序列必须能跑完。用普通通道的话，180s 超时会连带
    // 杀掉正在 restore 的 /bin/sh，停在「保护已卸、原生 socket 还没 mv 回」的中间态。
    let mut timed_out = false;
    let mut exit_code = None;
    let issued = match device.run_detached(&script, Duration::from_secs(180)).await {
        Ok(result) if result.exit_code == 0 => {
            info!("launcher stop 已执行，原厂前端应当已恢复");
            exit_code = Some(0);
            true
        }
        Ok(result) => {
            error!(
                exit = result.exit_code,
                stderr = result.stderr.trim(),
                "让位命令执行失败"
            );
            exit_code = Some(result.exit_code);
            false
        }
        Err(err) => {
            timed_out = err.to_string().contains(TIMEOUT_ERROR_PREFIX);
            error!(error = %err, timed_out, "让位命令没能执行");
            false
        }
    };

    let taken_over = ownership.taken_over();
    // 注意：超时（`run_detached` 返回 Err）时脚本其实还在跑，这里是紧接着做的
    // 一次瞬间快照，可能正好落在 restore_native_files 的 `rm -f` 与 `mv` 之间——
    // 那种情况下 `taken_over()` 为 false（严格化后不再把「路径不存在」当接管），
    // 于是判失败、退出码 1。方向上是刻意的：误报失败只是多查一次，误报成功会把
    // 唯一的信号消灭掉。真机上若见到「明明恢复了却退出 1」，先看这里。
    // 两种矛盾态都要留痕：它们正是「命令说的」和「事实」对不上的地方，
    // 而 `succeeded()` 会把它们合成一个结论——不记录就再也看不见了。
    if taken_over && !issued {
        warn!(
            socket = %ownership.path().display(),
            "让位命令报失败，但 speech.usock 已被原生接管，按成功处理"
        );
    }
    if issued && !taken_over {
        // 典型来源：restore_native_files 里 [ -e "$NATIVE_SPEECH_SOCKET" ] 为假时，
        // 脚本既不 rm 也不 mv，一路返回 0——麦克风其实没交出去。
        // 另外也可能是「不知道当初绑的是哪个 inode」，那种情况下结论同样存疑。
        let detail = if ownership.still_owned() {
            "让位命令报告成功，但 speech.usock 还是本进程绑的 inode：麦克风可能没交出去"
        } else {
            "让位命令报告成功，但无法确认 speech.usock 归属：让位结果存疑"
        };
        warn!(socket = %ownership.path().display(), "{detail}");
    }
    YieldOutcome {
        issued,
        taken_over,
        timed_out,
        exit_code,
    }
}

async fn reject_orphaned_turn(mut turn: SpeechTurn, speech: &SpeechHandle) {
    warn!(turn_id = turn.id(), activation = ?turn.activation(), "rejecting speech turn without an active session");
    let turn_id = turn.id();
    let _ = turn.stop_capture().await;
    let _ = speech.finish_dialog(turn_id).await;
}

async fn run_turn(
    state: TurnState,
    speech: SpeechHandle,
    first_turn: SpeechTurn,
    followup_rx: mpsc::Receiver<SpeechTurn>,
    active_turn_id: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let result = run_session(
        state.clone(),
        speech.clone(),
        first_turn,
        followup_rx,
        active_turn_id.clone(),
    )
    .await;
    if result.is_err() {
        // 采集/ASR/LLM/TTS 任何一环中途失败都从这里收尾：run_session 里的
        // 采集错误不再各自重复 finish_dialog，只在这一处发一次。
        interrupt_turn(
            &state,
            &speech,
            active_turn_id.load(Ordering::Relaxed),
            InterruptCause::Failure("turn failed"),
        )
        .await;
        state.failures.fetch_add(1, Ordering::Relaxed);
    } else {
        // 一轮正常结束就清零：偶发失败不该累积成让位。
        state.failures.store(0, Ordering::Relaxed);
    }
    cleanup_turn_leds(&state.device, &state.led).await;
    state.music.restore_after_interruption().await;
    state.airplay.restore_after_interruption().await;
    result
}

async fn run_session(
    state: TurnState,
    speech: SpeechHandle,
    mut turn: SpeechTurn,
    mut followup_rx: mpsc::Receiver<SpeechTurn>,
    active_turn_id: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let led = &state.led;

    loop {
        // 每轮重新取一次配置快照：SIGHUP 之后从下一轮开始用新值，而本轮
        // 从头到尾是同一份配置。
        let config = state.config.get().await;
        let idle_timeout = Duration::from_secs_f64(config.runtime.session_idle_timeout_s.max(1.0));
        let turn_id = turn.id();
        active_turn_id.store(turn_id, Ordering::Relaxed);
        state.device.show_led(led.led_listening).await;
        let device_for_speech = state.device.clone();
        let maybe_stream = match state
            .asr
            .start_streaming_transcription(config.capture.sample_rate)
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                return Err(err.context("ASR failed after retries"));
            }
        };
        let text = if let Some(mut stream) = maybe_stream {
            if stream.server_vad_enabled() {
                let device_for_server_start = device_for_speech.clone();
                let device_for_server_stop = state.device.clone();
                let led_user_speaking = led.led_user_speaking;
                let led_thinking = led.led_thinking;
                match record_utterance_server_vad_streaming(
                    config.capture.clone(),
                    idle_timeout,
                    &mut turn,
                    &mut stream,
                    move || {
                        let device = device_for_server_start.clone();
                        async move {
                            device.show_led(led_user_speaking).await;
                        }
                    },
                    move || {
                        let device = device_for_server_stop.clone();
                        async move {
                            device.show_led(led_thinking).await;
                        }
                    },
                )
                .await
                {
                    Ok(text) => text,
                    Err(err) if is_capture_timeout(&err) => {
                        stream.close().await;
                        info!(turn_id, "session idle timeout");
                        interrupt_turn(&state, &speech, turn_id, InterruptCause::IdleTimeout).await;
                        return Ok(());
                    }
                    Err(err) => {
                        stream.close().await;
                        speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                        return Err(err.context("ASR failed after retries"));
                    }
                }
            } else {
                let appender = stream.appender();
                let appender_for_chunk = appender.clone();
                let appender_for_reject = appender.clone();
                let led_user_speaking = led.led_user_speaking;
                let _pcm = match record_utterance_streaming(
                    config.capture.clone(),
                    idle_timeout,
                    &mut turn,
                    move || {
                        let device = device_for_speech.clone();
                        async move {
                            device.show_led(led_user_speaking).await;
                        }
                    },
                    move |bytes| {
                        let appender = appender_for_chunk.clone();
                        async move { appender.append_pcm(bytes).await }
                    },
                    move || {
                        let appender = appender_for_reject.clone();
                        async move { appender.clear().await }
                    },
                )
                .await
                {
                    Ok(pcm) => pcm,
                    Err(err) if is_capture_timeout(&err) => {
                        stream.close().await;
                        info!(turn_id, "session idle timeout");
                        interrupt_turn(&state, &speech, turn_id, InterruptCause::IdleTimeout).await;
                        return Ok(());
                    }
                    Err(err) => {
                        stream.close().await;
                        return Err(err);
                    }
                };

                state.device.show_led(led.led_thinking).await;
                match stream.commit_and_transcribe().await {
                    Ok(text) => text,
                    Err(err) => {
                        speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                        return Err(err.context("ASR failed after retries"));
                    }
                }
            }
        } else {
            let pcm = match record_utterance(config.capture.clone(), idle_timeout, &mut turn, {
                let led_user_speaking = led.led_user_speaking;
                move || {
                    let device = device_for_speech.clone();
                    async move {
                        device.show_led(led_user_speaking).await;
                    }
                }
            })
            .await
            {
                Ok(pcm) => pcm,
                Err(err) if is_capture_timeout(&err) => {
                    info!(turn_id, "session idle timeout");
                    interrupt_turn(&state, &speech, turn_id, InterruptCause::IdleTimeout).await;
                    return Ok(());
                }
                Err(err) => return Err(err),
            };

            state.device.show_led(led.led_thinking).await;
            match state
                .asr
                .transcribe_pcm(&pcm, config.capture.sample_rate)
                .await
            {
                Ok(text) => text,
                Err(err) => {
                    speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                    return Err(err.context("ASR failed after retries"));
                }
            }
        };

        let command = text.trim();
        if command.is_empty() {
            info!(turn_id, "empty ASR result; ending session");
            speech.finish_dialog(turn_id).await?;
            state.agent.reset_session("empty ASR result").await;
            return Ok(());
        }
        info!("USER_ASR text={command}");

        let reply = match state.agent.run_turn(command).await {
            Ok(reply) => reply,
            Err(err) => {
                speak_service_error(&state.device, led, LLM_SERVICE_ERROR_PROMPT).await;
                return Err(err.context("LLM failed after retries"));
            }
        };
        state.device.shut_led(led.led_thinking).await;
        if !reply.text.trim().is_empty() {
            state.device.show_led(led.led_speaking).await;
            // TTS 失败**不计入**「连续失败 → 让位」：那条机制的用途是解决
            // 「能唤醒但答不上」的空转（大脑坏了）；TTS 挂了是嗓子坏了，模型其实
            // 已经答出来。而且配置了 fallback_to_native: false 的人预期是「宁可
            // 不说话」，若因此让位，音箱会整体下线等人工 start-agent.sh start，
            // 落差太大。所以这里只 warn + 继续这一轮剩下的流程。
            match TtsRouter::new(state.device.clone())
                .speak(&config.tts, &reply.text)
                .await
            {
                Ok(()) => TTS_HEALTH.on_success(),
                Err(err) => {
                    TTS_HEALTH.on_failure(&err);
                }
            }
        }
        if reply.should_end {
            info!("agent ended conversation: {}", reply.end_reason);
            speech.finish_dialog(turn_id).await?;
            state.agent.reset_session("agent ended conversation").await;
            return Ok(());
        }

        speech.continue_dialog(turn_id).await?;
        request_followup_capture(&state.device).await?;
        state.device.shut_led(led.led_speaking).await;
        state.device.show_led(led.led_listening).await;
        turn = match timeout(idle_timeout, followup_rx.recv()).await {
            Ok(Some(turn)) => turn,
            Ok(None) => {
                // follow-up 发送端没了：会话是正常收尾，不是失败。用 `?` 会让
                // finish_dialog 的错误把整轮判成 Err，从而给「连续 3 轮失败让位」
                // 白记一次，并且跳过 reset_session。
                interrupt_turn(&state, &speech, turn_id, InterruptCause::FrontendStopped).await;
                return Ok(());
            }
            Err(_) => {
                info!(turn_id, "timed out waiting for follow-up stream");
                interrupt_turn(&state, &speech, turn_id, InterruptCause::IdleTimeout).await;
                return Ok(());
            }
        };
    }
}

async fn request_followup_capture(device: &Device) -> anyhow::Result<()> {
    // Let mipns process DialogFinish and return to idle before asking its
    // public mic-open path to start a real multi-round VPM stream.
    sleep(Duration::from_millis(100)).await;
    let script = r#"ubus -t 1 -S call pnshelper event_notify '{"src":1,"event":0}'"#;
    let result = device
        .run_raw(script, Duration::from_secs(10))
        .await
        .map_err(|err| anyhow::anyhow!("failed to request follow-up capture: {err}"))?;
    if result.exit_code != 0 {
        anyhow::bail!(
            "pnshelper follow-up request failed exit={} stderr={}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    let response: serde_json::Value = serde_json::from_str(&result.stdout)
        .context("pnshelper returned an invalid follow-up response")?;
    if response.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        anyhow::bail!(
            "pnshelper rejected follow-up capture: {}",
            result.stdout.trim()
        );
    }
    info!("requested follow-up capture through pnshelper");
    Ok(())
}

/// 服务错误提示**故意**不走 `TtsRouter`：要念的是「服务出问题了」，此时外部
/// TTS 大概率同样不可用，只有设备自带的 `tts_play.sh` 还靠得住。
async fn speak_service_error(device: &Device, led: &DeviceConfig, text: &str) {
    device.shut_led(led.led_thinking).await;
    device.show_led(led.led_speaking).await;
    if let Err(err) = device.speak(text).await {
        warn!("failed to speak service error prompt: {err:?}");
    }
}

async fn cleanup_turn_leds(device: &Device, led: &DeviceConfig) {
    for id in [
        led.led_speaking,
        led.led_thinking,
        led.led_user_speaking,
        led.led_listening,
    ] {
        device.shut_led(id).await;
    }
}

fn is_capture_timeout(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("timed out waiting for user speech")
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::Mutex;

    use crate::config::AppConfig;
    use crate::shell::RecordingShell;
    use crate::speech::{SpeechInput, TestCommand};

    /// 一个完全离线的 `TurnState`，不需要设备也不联网：
    ///
    /// - 默认的 `AsrProvider::OpenAi` 让 `start_streaming_transcription` 直接返回
    ///   `Ok(None)`，于是走本地 VAD 的 `record_utterance`，一次都不发请求；
    /// - `agent.tools_enabled=false` 让 `AgentRuntime::new` 走
    ///   `McpConnections::empty()`，不建 MCP 连接；
    /// - `airplay.enabled=false` 让 `AirPlayService::start` 只构造内存状态。
    /// 测试基线配置：离线（不建 MCP、不建 realtime 流、不监听 AirPlay）。
    fn base_test_config() -> AppConfig {
        let mut config = AppConfig::default();
        config.agent.tools_enabled = false;
        config.airplay.enabled = false;
        // record_utterance 内部会把上限抬到至少 1s，所以超时用例实跑约 1s。
        config.runtime.session_idle_timeout_s = 1.0;
        config
    }

    async fn test_state(shell: Arc<RecordingShell>) -> TurnState {
        test_state_with(base_test_config(), shell).await
    }

    async fn test_state_with(config: AppConfig, shell: Arc<RecordingShell>) -> TurnState {
        let config = ConfigHandle::new(Arc::new(config));
        let initial = config.get().await;
        let device = Device::with_runner(initial.device.clone(), shell);
        let asr = AsrClient::new(initial.asr.clone()).expect("默认 ASR 配置应当可用");
        let music = Arc::new(
            MusicService::new(config.clone(), device.clone()).expect("音乐服务构造不应失败"),
        );
        let airplay = AirPlayService::start(initial.airplay.clone())
            .await
            .expect("airplay.enabled=false 时不应失败");
        let agent = Arc::new(
            AgentRuntime::new(config.clone(), device.clone(), music.clone())
                .await
                .expect("离线构造 agent 不应失败"),
        );
        TurnState {
            config,
            led: initial.device.clone(),
            device,
            asr,
            agent,
            music,
            airplay,
            failures: Arc::new(AtomicU32::new(0)),
        }
    }

    async fn commands(recorder: &Arc<Mutex<Vec<TestCommand>>>) -> Vec<TestCommand> {
        recorder.lock().await.clone()
    }

    fn followup_channel() -> mpsc::Receiver<SpeechTurn> {
        let (_tx, rx) = mpsc::channel(2);
        rx
    }

    /// 唤醒打断：旧对话必须被 finish 掉，否则 mipns 那侧留着上一轮的对话状态。
    #[tokio::test]
    async fn wake_interrupt_finishes_the_old_dialog_and_resets_the_session() {
        let state = test_state(Arc::new(RecordingShell::always_ok())).await;
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(7);

        interrupt_turn(&state, &handle, turn.id(), InterruptCause::Wake).await;

        assert_eq!(
            commands(&recorder).await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "唤醒打断必须 finish 旧对话"
        );
        assert_eq!(state.agent.reset_reason_for_test().await, "wake keyword");
    }

    /// 空闲超时的收尾不许补发 `stop_capture`：停采集是 capture.rs 的职责，
    /// 而且它有不对称契约——超时已经补发过一次，`End`/`Cancelled` 则禁止补发。
    /// 在这一层再补一条会把那个契约踩掉（mipns 侧会收到自己结束后的 stop）。
    #[tokio::test]
    async fn idle_timeout_interrupt_never_reissues_stop_capture() {
        let state = test_state(Arc::new(RecordingShell::always_ok())).await;
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(8);

        interrupt_turn(&state, &handle, turn.id(), InterruptCause::IdleTimeout).await;

        assert_eq!(
            commands(&recorder).await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "中断收尾只发 finish_dialog，不碰 stop_capture"
        );
        assert_eq!(
            state.agent.reset_reason_for_test().await,
            "session idle timeout"
        );
    }

    /// 采集被 mipns 取消：整轮按失败返回，但收尾一步都不能少
    /// （finish 对话一次、四个 LED 全灭、会话重置）。
    #[tokio::test]
    async fn cancelled_stream_fails_the_turn_but_still_cleans_up() {
        let shell = Arc::new(RecordingShell::always_ok());
        let state = test_state(shell.clone()).await;
        let (turn, input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(11);
        input_tx.send(SpeechInput::Cancelled).await.unwrap();
        let turn_id = Arc::new(AtomicU64::new(turn.id()));

        let result = run_turn(state.clone(), handle, turn, followup_channel(), turn_id).await;

        assert!(result.is_err(), "mipns 取消流应当让这一轮失败");
        assert_eq!(
            commands(&recorder).await,
            vec![TestCommand::Finish {
                continue_dialog: false
            }],
            "失败路径只 finish 一次：run_session 里的采集错误不再各自补一条"
        );
        assert_eq!(
            shell.count_matching("/bin/shut_led"),
            4,
            "四个 LED 必须全灭，否则会留一盏常亮灯"
        );
        assert_eq!(state.agent.reset_reason_for_test().await, "turn failed");
        assert_eq!(
            state.failures.load(Ordering::Relaxed),
            1,
            "失败轮数要累计：连续 3 轮才让位"
        );
    }

    /// 会话空闲超时是**正常结束**：先由 capture.rs 补发 stop_capture，
    /// 再由收尾发 finish_dialog，顺序不能反（反了 mipns 的 stream 会挂着）。
    #[tokio::test]
    async fn idle_timeout_ends_the_session_without_a_failure() {
        let shell = Arc::new(RecordingShell::always_ok());
        let state = test_state(shell).await;
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(12);
        let turn_id = Arc::new(AtomicU64::new(turn.id()));

        let result = run_turn(state.clone(), handle, turn, followup_channel(), turn_id).await;

        assert!(result.is_ok(), "空闲超时不是失败: {result:?}");
        assert_eq!(
            commands(&recorder).await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                }
            ],
            "先 stop_capture（capture.rs）再 finish_dialog（收尾）"
        );
        assert_eq!(
            state.agent.reset_reason_for_test().await,
            "session idle timeout"
        );
        assert_eq!(
            state.failures.load(Ordering::Relaxed),
            0,
            "正常结束要清零，否则偶发失败会累积成让位"
        );
    }

    #[tokio::test]
    async fn followup_request_command_goes_through_the_seam_and_succeeds() {
        let shell = Arc::new(RecordingShell::answering(r#"{"code":0}"#));
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());

        request_followup_capture(&device).await.unwrap();

        let script = shell.last_script().expect("必须真的调 pnshelper");
        assert!(script.contains("pnshelper event_notify"), "{script}");
        assert!(script.contains(r#"{"src":1,"event":0}"#), "{script}");
    }

    #[tokio::test]
    async fn followup_request_surfaces_command_failures() {
        let shell = Arc::new(RecordingShell::exiting_with(2, "ubus: not found"));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit=2"), "{err}");
        assert!(err.contains("ubus: not found"), "{err}");

        let shell = Arc::new(RecordingShell::cannot_run(
            "shell command timed out after 10s: ubus",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed to request follow-up capture"), "{err}");
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn followup_request_rejects_bad_pnshelper_answers() {
        let device = Device::with_runner(
            DeviceConfig::default(),
            Arc::new(RecordingShell::answering("not json")),
        );
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid follow-up response"), "{err}");

        let device = Device::with_runner(
            DeviceConfig::default(),
            Arc::new(RecordingShell::answering(r#"{"code":7}"#)),
        );
        let err = request_followup_capture(&device)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rejected follow-up capture"), "{err}");
    }

    /// 收尾的日志级别由「是不是预期内的正常结束」决定，不是由「谁触发的」决定。
    /// 唤醒打断 / 空闲超时 / 前端正常关流都是正常收尾，只有 Failure 才是真故障——
    /// 这条分类错了，正常结束会以 warn 写进设备日志，现场排查就全是噪音。
    #[test]
    fn interrupt_causes_mark_normal_endings_as_expected() {
        assert!(InterruptCause::Wake.expected());
        assert!(InterruptCause::IdleTimeout.expected());
        assert!(InterruptCause::FrontendStopped.expected());
        assert!(!InterruptCause::Failure("turn failed").expected());

        assert_eq!(
            InterruptCause::FrontendStopped.reason(),
            "speech frontend stopped"
        );
        assert_eq!(InterruptCause::IdleTimeout.reason(), "session idle timeout");
        assert_eq!(
            InterruptCause::Failure("turn failed").reason(),
            "turn failed"
        );
    }

    /// 只认采集空闲超时。ASR 转写超时同样是 "timed out"，但那是服务故障，
    /// 不能当成空闲超时静默结束（否则用户说完话只会看到灯灭）。
    #[test]
    fn is_capture_timeout_matches_only_the_idle_timeout_error() {
        assert!(is_capture_timeout(&anyhow::anyhow!(
            "timed out waiting for user speech"
        )));
        assert!(!is_capture_timeout(&anyhow::anyhow!(
            "timed out waiting for Realtime ASR transcript"
        )));
        assert!(!is_capture_timeout(&anyhow::anyhow!(
            "mipns cancelled the speech stream"
        )));
    }

    /// 没有活跃会话却收到 turn：顺序必须是「先停采集，再挂断」。
    #[tokio::test]
    async fn orphaned_turn_stops_capture_before_finishing_the_dialog() {
        let (turn, _input_tx, recorder, handle) = SpeechTurn::for_test_with_handle(21);

        reject_orphaned_turn(turn, &handle).await;

        assert_eq!(
            commands(&recorder).await,
            vec![
                TestCommand::StopCapture,
                TestCommand::Finish {
                    continue_dialog: false
                }
            ],
            "顺序不能反：先停采集再挂断，否则 mipns 的 stream 一直挂着"
        );
    }

    /// 让位只有一个动作：调 launcher 的 `stop`。还原顺序（停 pns → 停 watchdog
    /// → 停 mico → 卸保护 → 还原文件 → 起原生服务）全在脚本里，别在 agent 里重写。
    #[tokio::test]
    async fn yield_to_native_asks_the_launcher_to_stop() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-ok");

        assert!(yield_to_native(&device, Some(launcher), &owned, "fatal")
            .await
            .succeeded());

        let scripts = shell.detached_scripts();
        assert_eq!(scripts.len(), 1, "让位就一条命令");
        assert!(
            scripts[0].ends_with("start-agent.sh stop"),
            "必须是 launcher 的 stop（restore_native_frontend 在它里面）: {}",
            scripts[0]
        );
        assert!(
            shell.scripts().is_empty(),
            "让位必须走 detached 通道：普通通道的超时会杀掉正在 restore 的 sh"
        );
    }

    /// launcher 用 `mkdir` 锁和 watchdog 互斥：watchdog 正在 restart 时我们的
    /// stop 会撞锁失败。这种情况必须如实返回 false（调用方据此定退出码），
    /// 不能假装让位成功。
    #[tokio::test]
    async fn yield_to_native_reports_failure_instead_of_pretending() {
        let shell = Arc::new(RecordingShell::exiting_with(
            1,
            "another launcher operation is in progress",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        // socket 还在我们手里 → 脚本报失败就是真失败
        let owned = still_owned_socket("yield-fail");

        assert!(!yield_to_native(&device, Some(launcher), &owned, "fatal")
            .await
            .succeeded());
    }

    /// 让位命令**本身没跑成**（超时 / 无法 fork）也必须如实返回 false。
    /// 退出码 1 是运维唯一能把「让位了」和「以为让位了」区分开的地方。
    #[tokio::test]
    async fn yield_to_native_reports_an_unexecutable_command() {
        let shell = Arc::new(RecordingShell::cannot_run(
            "shell command timed out after 180s: start-agent.sh stop",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-timeout");

        assert!(!yield_to_native(&device, Some(launcher), &owned, "fatal")
            .await
            .succeeded());
    }

    /// 脚本报失败、但 socket 已经不是我们绑的那个 inode：原生其实已接管，
    /// 必须按成功处理——否则运维会看到退出码 1，去查一件已经成功的事。
    #[tokio::test]
    async fn yield_treats_a_taken_over_socket_as_success_even_when_the_command_failed() {
        let shell = Arc::new(RecordingShell::exiting_with(
            1,
            "another launcher operation is in progress",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");

        let owned = taken_over_socket("yield-taken-over");
        let outcome = yield_to_native(&device, Some(launcher), &owned, "fatal").await;

        assert!(!outcome.issued, "命令本身确实失败了");
        assert!(outcome.taken_over, "但 socket 已经被接管走");
        assert!(outcome.succeeded(), "这种情况必须算让位成功");
        let _ = std::fs::remove_file(owned.path());
    }

    /// 命令干净退出、但 socket 还在我们手里：仍算成功（命令确实执行了），
    /// 矛盾态由 `yield_to_native` 里的 warn 留痕，不由 `succeeded()` 表达。
    #[tokio::test]
    async fn yield_succeeds_when_the_command_succeeded_even_when_still_owned() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let launcher = Path::new("/data/open-xiaoai/start-agent.sh");
        let owned = still_owned_socket("yield-still-mine");

        let outcome = yield_to_native(&device, Some(launcher), &owned, "fatal").await;

        assert!(outcome.issued);
        assert!(!outcome.taken_over);
        assert!(outcome.succeeded(), "命令成功就算让位达成");
        let _ = std::fs::remove_file(owned.path());
    }

    /// 连续失败后的处置：让位成功 → 退出；让位失败 → 继续跑（C-lite 策略）。
    #[tokio::test]
    async fn consecutive_failures_exit_only_when_the_yield_succeeded() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("streak-exit");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(code, Some(0), "让位成功才退出");
        let _ = std::fs::remove_file(owned.path());
    }

    /// 已经动过前端（4 = pns/watchdog 停了但 mico 没停；5 = 保护没卸或原生没恢复）
    /// 必须退出：此时继续跑的表现是「LED 正常闪、麦克风不工作、watchdog 已停」。
    #[tokio::test]
    async fn consecutive_failures_exit_when_the_launcher_stopped_halfway() {
        for exit in [4_i32, 5, 1] {
            let shell = Arc::new(RecordingShell::exiting_with(exit, "restore failed"));
            let device = Device::with_runner(DeviceConfig::default(), shell);
            let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
            let owned = still_owned_socket(&format!("streak-half-{exit}"));

            let code = yield_after_consecutive_failures(
                &device,
                Some(Path::new("/data/open-xiaoai/start-agent.sh")),
                &owned,
                &failures,
            )
            .await;

            assert_eq!(code, Some(1), "退出码 {exit} 说明前端已被动过，必须退出");
            let _ = std::fs::remove_file(owned.path());
        }
    }

    /// `exit 5` 必须压过「inode 已换」：`rm` + `mv` 成功后 mico/pns 没起来，
    /// socket 在位但麦克风不工作——这条在当前实现下曾经返回 `Some(0)`。
    #[tokio::test]
    async fn exit_code_5_beats_a_replaced_inode() {
        let shell = Arc::new(RecordingShell::exiting_with(
            LAUNCHER_EXIT_HALF_DISMANTLED,
            "failed to restart native mico",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let taken = taken_over_socket("half-dismantled");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/x/start-agent.sh")),
            &taken,
            &failures,
        )
        .await;

        assert_eq!(
            code,
            Some(1),
            "inode 换了但原生服务没起来，不能算成功（退出码 0 = 哑且无任何信号）"
        );
        let _ = std::fs::remove_file(taken.path());
    }

    /// `succeeded()` 是「让位是否达成」的唯一答案：`exit 5` 必须压过 inode 事实。
    /// 三个调用点（Fatal ×2、连续失败 ×1）共用这一条，钉住它就等于三处都钉住。
    #[test]
    fn exit_code_5_is_never_success_even_with_a_replaced_inode() {
        let half = YieldOutcome {
            issued: false,
            taken_over: true,
            timed_out: false,
            exit_code: Some(LAUNCHER_EXIT_HALF_DISMANTLED),
        };
        assert!(!half.succeeded(), "inode 换了但原生服务没起来，不能算成功");

        // 对照：同一个「inode 已换」的事实，脚本说成功时才算成功。
        let restored = YieldOutcome {
            exit_code: Some(0),
            ..half
        };
        assert!(restored.succeeded());
    }

    /// 让位**超时**不能算「脚本没动前端」：脚本可能正停在还原中途，此时
    /// watchdog 已停、保护已卸，继续运行会表现为「LED 正常闪但麦克风不工作」。
    #[tokio::test]
    async fn consecutive_failures_exit_when_the_yield_timed_out() {
        let shell = Arc::new(RecordingShell::cannot_run(
            "shell command timed out after 180s: /data/open-xiaoai/start-agent.sh stop",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("streak-timeout");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(
            code,
            Some(1),
            "超时说明脚本可能停在还原中途，必须退出而不是假装健康"
        );
        let _ = std::fs::remove_file(owned.path());
    }

    /// 只有「脚本根本没动前端」（锁被占用，退出码 3）才继续运行。
    #[tokio::test]
    async fn consecutive_failures_keep_running_only_when_the_frontend_is_untouched() {
        let shell = Arc::new(RecordingShell::exiting_with(
            LAUNCHER_EXIT_UNTOUCHED,
            "another launcher operation is in progress",
        ));
        let device = Device::with_runner(DeviceConfig::default(), shell);
        let failures = Arc::new(AtomicU32::new(YIELD_AFTER_CONSECUTIVE_FAILURES));
        let owned = still_owned_socket("streak-keep");

        let code = yield_after_consecutive_failures(
            &device,
            Some(Path::new("/data/open-xiaoai/start-agent.sh")),
            &owned,
            &failures,
        )
        .await;

        assert_eq!(
            code, None,
            "前端未受影响时不该退出：让位这次没跑起来而已，麦克风还在我们手里"
        );
        assert_eq!(
            failures.load(Ordering::Relaxed),
            0,
            "计数必须清零，否则每个 tick 都会再试一次让位"
        );
        let _ = std::fs::remove_file(owned.path());
    }

    /// 一次热加载的全量用例配置：同时含「热加载段」和「需重启段」。
    const RELOAD_YAML: &str = r#"runtime:
  session_idle_timeout_s: 42
llm:
  model: reloaded-model
device:
  tts_command: "/usr/sbin/tts_play.sh --changed {text}"
asr:
  open_ai:
    model: reloaded-asr
agent:
  tools_enabled: false
"#;

    /// 半截 yaml：故意破坏，用来验证「重载失败保留旧配置」。
    const BROKEN_YAML: &str = "runtime: [not a mapping";

    /// 改 socket + 一个合法热加载段：验证「拒绝 socket、但其余段照常生效」。
    const SOCKET_YAML: &str = r#"runtime:
  speech_socket: /tmp/other/speech.usock
  session_idle_timeout_s: 7
"#;

    fn temp_config_path(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-reload-{tag}-{}-{stamp}.yaml",
            std::process::id()
        ))
    }

    /// 「socket 还在我们手里」：临时文件 + 它自己的 inode。
    fn still_owned_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let path = temp_config_path(tag).with_extension("usock");
        std::fs::write(&path, b"x").expect("写临时 socket 占位文件");
        let inode = std::fs::metadata(&path).expect("读 inode").ino();
        SocketOwnership::for_test(path, inode)
    }

    /// 「已经被接管走」：复刻 launcher 的 `rm -f` + `mv`，让路径指向另一个 inode。
    ///
    /// 不能用「路径不存在」来代表已接管——那正是 rm→mv 之间窗口的样子，
    /// 也可能是 mv 失败或脚本没跑到 restore。
    fn taken_over_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let live = temp_config_path(tag).with_extension("usock");
        let native = live.with_extension("native.usock");
        std::fs::write(&live, b"agent").expect("写 agent socket 占位");
        std::fs::write(&native, b"native").expect("写原生 socket 占位");
        let mine = std::fs::metadata(&live).expect("读 inode").ino();
        std::fs::remove_file(&live).expect("rm");
        std::fs::rename(&native, &live).expect("mv");
        SocketOwnership::for_test(live, mine)
    }

    /// 重载期间的 SIGHUP 必须「合并成下一轮」，不能静默丢弃：否则用户改错再改回来、
    /// 再发一次信号时，最终生效的还是第一次那份（写错的）。
    #[test]
    fn reload_gate_coalesces_requests_arriving_mid_reload() {
        let gate = ReloadGate::default();
        assert!(gate.begin(), "第一个请求拿到执行权");
        assert!(!gate.begin(), "重载进行中：本次请求被并入下一轮");
        assert!(gate.finish_round(), "有合并进来的请求，需要再跑一轮");
        assert!(!gate.finish_round(), "没有新请求就结束");
        gate.release();
        assert!(gate.begin(), "释放之后又能接新请求");
    }

    /// N2 的回归：`running` 靠 RAII 复位，不能手动 store。
    #[test]
    fn reload_guard_releases_the_gate_on_drop() {
        let gate = Arc::new(ReloadGate::default());
        assert!(gate.begin());
        {
            let _guard = ReloadGuard(gate.clone());
            assert!(!gate.begin(), "持锁期间不该再接新任务");
        }
        assert!(gate.begin(), "guard drop 之后必须已复位");
    }

    /// 真正会咬人的那一档：任务 panic 展开时也要复位。手动 `store(false)` 的写法
    /// 在这里会把 running 永久留在 true，此后每次 SIGHUP 都静默失效。
    #[test]
    fn reload_guard_releases_the_gate_even_when_the_task_panics() {
        let gate = Arc::new(ReloadGate::default());
        assert!(gate.begin());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ReloadGuard(gate.clone());
            panic!("simulated reload panic");
        }));
        assert!(outcome.is_err(), "这条用例本身要制造一次 panic");
        assert!(
            gate.begin(),
            "panic 展开后 running 必须复位，否则热加载永久失效"
        );
    }

    /// 热加载的核心承诺：能热加载的段立刻生效，需要重启的段在句柄里保持旧值。
    /// 后者是「句柄 == 运行中真正生效的配置」这条不变量的实现方式。
    #[tokio::test]
    async fn config_reload_applies_hot_sections_and_keeps_pending_ones() {
        let state = test_state(Arc::new(RecordingShell::always_ok())).await;
        let before = state.config.get().await;
        let path = temp_config_path("apply");
        std::fs::write(&path, RELOAD_YAML).unwrap();

        apply_config_reload(&state.config, &path, &state.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = state.config.get().await;
        assert_eq!(
            after.runtime.session_idle_timeout_s, 42.0,
            "runtime.session_idle_timeout_s 是热加载段"
        );
        assert_eq!(after.llm.model, "reloaded-model", "llm 段是热加载段");
        assert_eq!(
            after.device.tts_command, before.device.tts_command,
            "device 段要重启才生效，句柄里必须还是旧值"
        );
        assert_eq!(
            after.asr.open_ai.model, before.asr.open_ai.model,
            "asr 段要重启才生效，句柄里必须还是旧值"
        );
    }

    /// 一份写坏的 yaml 不能让音箱因为一次 SIGHUP 变哑：失败保留旧配置。
    #[tokio::test]
    async fn config_reload_keeps_old_config_when_the_file_is_broken() {
        let state = test_state(Arc::new(RecordingShell::always_ok())).await;
        let before = state.config.get().await;
        let path = temp_config_path("broken");
        std::fs::write(&path, BROKEN_YAML).unwrap();

        apply_config_reload(&state.config, &path, &state.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = state.config.get().await;
        assert_eq!(after.llm.model, before.llm.model);
        assert_eq!(
            after.runtime.session_idle_timeout_s,
            before.runtime.session_idle_timeout_s
        );
    }

    /// 只改了 device（需重启）段：不该触发任何重载动作，句柄原样不动。
    /// 这是 P3 那条「每发一次 SIGHUP 就白重建一次工具/MCP」的回归测试。
    #[tokio::test]
    async fn config_reload_is_a_noop_when_only_restart_sections_changed() {
        let state = test_state(Arc::new(RecordingShell::always_ok())).await;
        let before = state.config.get().await;
        let path = temp_config_path("device-only");
        std::fs::write(
            &path,
            // 热加载段保持与测试基线一致，只让 device 段不同——否则
            // session_idle_timeout_s / agent.tools_enabled 会各自成为热加载变化。
            "runtime:
  session_idle_timeout_s: 1
agent:
  tools_enabled: false
device:
  tts_command: \"/usr/sbin/tts_play.sh --changed {text}\"
",
        )
        .unwrap();

        apply_config_reload(&state.config, &path, &state.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = state.config.get().await;
        assert_eq!(
            after.device.tts_command, before.device.tts_command,
            "device 段需要重启，句柄必须原样不动"
        );
        assert_eq!(
            after.runtime.session_idle_timeout_s,
            before.runtime.session_idle_timeout_s
        );
    }

    /// socket 运行中不能换（SpeechService 已经绑在上面），但同一次重载里的
    /// 其它段照常生效——不要因为一个字段就把整份配置丢掉。
    #[tokio::test]
    async fn config_reload_ignores_speech_socket_changes() {
        let state = test_state(Arc::new(RecordingShell::always_ok())).await;
        let before = state.config.get().await.runtime.speech_socket.clone();
        let path = temp_config_path("socket");
        std::fs::write(&path, SOCKET_YAML).unwrap();

        apply_config_reload(&state.config, &path, &state.agent).await;
        let _ = std::fs::remove_file(&path);

        let after = state.config.get().await;
        assert_eq!(
            after.runtime.speech_socket, before,
            "运行中换 socket 会让 mipns 和 agent 各说各话，必须拒绝"
        );
        assert_eq!(
            after.runtime.session_idle_timeout_s, 7.0,
            "同一份配置里的其它段不该被这个拒绝连累"
        );
    }

    #[tokio::test]
    async fn yield_to_native_without_a_launcher_runs_nothing() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());

        assert!(
            !yield_to_native(&device, None, &still_owned_socket("no-launcher"), "fatal")
                .await
                .succeeded()
        );

        assert_eq!(shell.command_count(), 0, "找不到 launcher 就别瞎跑命令");
        assert!(shell.detached_scripts().is_empty());
    }
}
