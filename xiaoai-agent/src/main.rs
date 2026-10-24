mod agent;
mod airplay;
mod asr;
mod capture;
mod config;
mod device;
mod mcp;
mod mcp_legacy_sse;
mod music;
mod shell;
mod speech;
mod tools;
mod vad;
mod weather;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
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
use crate::config::{AppConfig, DeviceConfig};
use crate::device::Device;
use crate::music::MusicService;
use crate::speech::{SpeechHandle, SpeechService, SpeechServiceEvent, SpeechTurn};

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
    let config = Arc::new(
        AppConfig::load(&cli.config)
            .with_context(|| format!("failed to load config {}", cli.config.display()))?,
    );
    let mut speech = SpeechService::bind(&config.runtime.speech_socket).await?;
    let speech_handle = speech.handle();

    let device = Device::new(config.device.clone());
    let asr = AsrClient::new(config.asr.clone())?;
    let music = Arc::new(MusicService::new(config.clone(), device.clone())?);
    let airplay = AirPlayService::start(config.airplay.clone()).await?;
    let agent = Arc::new(AgentRuntime::new(config.clone(), device.clone(), music.clone()).await?);
    let failures = Arc::new(AtomicU32::new(0));
    let launcher = launcher_path();
    if launcher.is_none() {
        warn!("找不到 start-agent.sh：故障时无法主动让位，只能退出等 watchdog 重启");
    }

    info!(socket = %config.runtime.speech_socket, "xiaoai-agent ready");
    device
        .blink_ready(config.device.led_listening, Duration::from_millis(250))
        .await;

    let mut active_session: Option<ActiveSession> = None;
    let mut session_check = interval(Duration::from_millis(250));
    session_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            event = speech.recv() => {
                let Some(event) = event else {
                    // 服务任务自己结束了：和 Fatal 同一类故障，先让位再退出。
                    let yielded = yield_to_native(
                        &device,
                        launcher.as_deref(),
                        "speech.usock service stopped",
                    )
                    .await;
                    std::process::exit(if yielded { 0 } else { 1 });
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
                                &config, &device, &asr, &agent, &music, &airplay, &failures,
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
                        let yielded =
                            yield_to_native(&device, launcher.as_deref(), "speech.usock fatal")
                                .await;
                        std::process::exit(if yielded { 0 } else { 1 });
                    }
                    SpeechServiceEvent::Turn(turn) if turn.activation().is_wakeup() => {
                        info!(
                            turn_id = turn.id(),
                            interaction_mode = turn.interaction_mode(),
                            "WAKE native mipns"
                        );
                        let state = build_turn_state(
                            &config, &device, &asr, &agent, &music, &airplay, &failures,
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
                        cleanup_turn_leds(&device, &config.device).await;

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
                        // 「能唤醒但答不上」已经连续出现整段会话。继续留在接管
                        // 状态只会让用户一直对着一个不说话的音箱喊，让位更划算。
                        error!(
                            consecutive_failures = streak,
                            "连续多轮会话失败，故障让位给原厂小爱"
                        );
                        let yielded =
                            yield_to_native(&device, launcher.as_deref(), "consecutive failures")
                                .await;
                        std::process::exit(if yielded { 0 } else { 1 });
                    }
                    device
                        .blink_ready(config.device.led_listening, Duration::from_millis(250))
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

/// 中断原因。它只决定两件事：session 重置的日志理由，以及清理时该用 warn
/// 还是 debug 记录（唤醒打断的 stale finish_dialog 是预期，不是故障）。
#[derive(Debug, Clone, Copy)]
enum InterruptCause {
    /// 新的唤醒词打断了上一轮。
    Wake,
    /// 会话空闲超时（采集等不到人说话、等不到 follow-up 流）。
    IdleTimeout,
    /// 采集中途失败（mipns 结束/取消流、ASR 出错等）。
    Failure(&'static str),
}

impl InterruptCause {
    fn reason(self) -> &'static str {
        match self {
            Self::Wake => "wake keyword",
            Self::IdleTimeout => "session idle timeout",
            Self::Failure(reason) => reason,
        }
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
        match cause {
            InterruptCause::Wake => {
                debug!(turn_id, error = %err, "旧 turn 已被新唤醒取代，finish_dialog 按 stale 跳过")
            }
            _ => warn!(turn_id, error = %err, "中断收尾时 finish_dialog 失败"),
        }
    }
    state.agent.reset_session(cause.reason()).await;
}

#[derive(Clone)]
struct TurnState {
    config: Arc<AppConfig>,
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
    config: &Arc<AppConfig>,
    device: &Device,
    asr: &AsrClient,
    agent: &Arc<AgentRuntime>,
    music: &Arc<MusicService>,
    airplay: &AirPlayService,
    failures: &Arc<AtomicU32>,
) -> TurnState {
    TurnState {
        config: config.clone(),
        device: device.clone(),
        asr: asr.clone(),
        agent: agent.clone(),
        music: music.clone(),
        airplay: airplay.clone(),
        failures: failures.clone(),
    }
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
async fn yield_to_native(device: &Device, launcher: Option<&Path>, reason: &str) -> bool {
    let Some(launcher) = launcher else {
        error!(
            reason,
            "找不到 start-agent.sh，无法让位；只能退出交给 watchdog 处理"
        );
        return false;
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
    // 还原序列内部有多次 sleep（stop_pns 最多 5s、stop_mico 最多 10s、
    // 重启原生服务再等 START_TIMEOUT），给足 180s，别在脚本干到一半时判超时。
    match device.run_raw(&script, Duration::from_secs(180)).await {
        Ok(result) if result.exit_code == 0 => {
            info!("launcher stop 已执行，原厂前端应当已恢复");
            true
        }
        Ok(result) => {
            error!(
                exit = result.exit_code,
                stderr = result.stderr.trim(),
                "让位命令执行失败"
            );
            false
        }
        Err(err) => {
            error!(error = %err, "让位命令没能执行");
            false
        }
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
    cleanup_turn_leds(&state.device, &state.config.device).await;
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
    let led = &state.config.device;
    let idle_timeout =
        Duration::from_secs_f64(state.config.runtime.session_idle_timeout_s.max(1.0));

    loop {
        let turn_id = turn.id();
        active_turn_id.store(turn_id, Ordering::Relaxed);
        state.device.show_led(led.led_listening).await;
        let device_for_speech = state.device.clone();
        let maybe_stream = match state
            .asr
            .start_streaming_transcription(state.config.capture.sample_rate)
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
                    state.config.capture.clone(),
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
                    state.config.capture.clone(),
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
            let pcm =
                match record_utterance(state.config.capture.clone(), idle_timeout, &mut turn, {
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
                .transcribe_pcm(&pcm, state.config.capture.sample_rate)
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
            state.device.speak(&reply.text).await?;
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
                interrupt_turn(
                    &state,
                    &speech,
                    turn_id,
                    InterruptCause::Failure("speech frontend stopped"),
                )
                .await;
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
    async fn test_state(shell: Arc<RecordingShell>) -> TurnState {
        let mut config = AppConfig::default();
        config.agent.tools_enabled = false;
        config.airplay.enabled = false;
        // record_utterance 内部会把上限抬到至少 1s，所以超时用例实跑约 1s。
        config.runtime.session_idle_timeout_s = 1.0;
        let config = Arc::new(config);
        let device = Device::with_runner(config.device.clone(), shell);
        let asr = AsrClient::new(config.asr.clone()).expect("默认 ASR 配置应当可用");
        let music = Arc::new(
            MusicService::new(config.clone(), device.clone()).expect("音乐服务构造不应失败"),
        );
        let airplay = AirPlayService::start(config.airplay.clone())
            .await
            .expect("airplay.enabled=false 时不应失败");
        let agent = Arc::new(
            AgentRuntime::new(config.clone(), device.clone(), music.clone())
                .await
                .expect("离线构造 agent 不应失败"),
        );
        TurnState {
            config,
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

        assert!(yield_to_native(&device, Some(launcher), "fatal").await);

        assert_eq!(shell.command_count(), 1, "让位就一条命令");
        let script = shell.last_script().expect("必须调 launcher");
        assert!(
            script.ends_with("start-agent.sh stop"),
            "必须是 launcher 的 stop（restore_native_frontend 在它里面）: {script}"
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

        assert!(!yield_to_native(&device, Some(launcher), "fatal").await);
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

        assert!(!yield_to_native(&device, Some(launcher), "fatal").await);
    }

    #[tokio::test]
    async fn yield_to_native_without_a_launcher_runs_nothing() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = Device::with_runner(DeviceConfig::default(), shell.clone());

        assert!(!yield_to_native(&device, None, "fatal").await);

        assert_eq!(shell.command_count(), 0, "找不到 launcher 就别瞎跑命令");
    }
}
