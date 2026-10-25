use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use serde_json::json;
use tracing::{debug, error, warn};

use crate::config::{OpenAiTtsConfig, TtsConfig, TtsProvider};
use crate::device::Device;

/// 播放本地音频文件的默认命令。
///
/// 三条都是真机实测的结论（OH2P 1.62.2，见 `docs/device-scripts.md`）：
/// - `miplayer -f <本地文件>` 可用（日志 `event(Playing)` → `EndReached`）；
/// - `miplayer` **播完不会自己退出**，所以下次播放前必须 `killall`；
/// - 后台起播（`&`），否则一条命令就吃掉整个超时预算。
///
/// 已知副作用：这句 `killall miplayer` 也会杀掉 `device.play_url_command` 放音乐
/// 用的那个 `miplayer`——换到外部 TTS 之后「放歌时说话，歌没了」。native 路径
/// （`tts_play.sh`）不碰 `miplayer`，所以两条路径行为不同。统一的正确做法是
/// 「duck → 播 → unduck」（`device.duck_audio_command` / `unduck_audio_command`），
/// 但那需要先确认压音量对 TTS 播放通道也生效，留待后续。
pub const DEFAULT_PLAYBACK_COMMAND: &str =
    "killall miplayer 2>/dev/null; miplayer -f {path} >/tmp/xiaoai-tts-player.log 2>&1 &";

/// 播放会话令牌：**打断的判据不是「杀掉任务」，而是「令牌变了」**。
///
/// 当前实现里唤醒打断靠 `task.abort()`，但 `tts_play.sh` / `miplayer` 是**独立进程**，
/// abort 掉 future 不等于杀掉它们——所以「说了闭嘴还多播一两句」。
/// 令牌解决的是另一个方向：任何**新**会话（唤醒、`/stop`、下一轮）一开始就 `begin()`
/// 让旧令牌失效，正在进行的合成/播放每完成一步先查一次，失效就闭嘴。
///
/// 两条必须一起用：令牌负责「不要再开始新的播放」，`abort_current_output()`
/// 负责「把已经在响的那句掐掉」。只有令牌，当前这句还会播完；只有 kill，
/// 已排队但还没起播的句子仍会漏出来。
#[derive(Debug, Default)]
pub struct PlaybackGate {
    token: AtomicU64,
}

impl PlaybackGate {
    pub fn current(&self) -> u64 {
        self.token.load(Ordering::Relaxed)
    }

    /// 让所有在飞的播放会话失效，返回新令牌。
    pub fn begin(&self) -> u64 {
        self.token.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 抓到的令牌还是当前的吗。
    pub fn is_active(&self, token: u64) -> bool {
        self.token.load(Ordering::Relaxed) == token
    }
}

/// 连续多少次 TTS 失败后升级成 `error`。
///
/// 取 3：一次网络抖动不该刷红日志，但「每轮一条 warn」的长期故障必须能一眼看出来。
pub const TTS_FAILURES_BEFORE_ALARM: u32 = 3;

/// TTS 健康度：只做计数与日志分档，**不接让位机制**。
///
/// 让位是为了处理「能唤醒但答不上」的空转（大脑侧）；TTS 是输出侧，挂了不该把整个
/// agent 下线。但摆到「永远只 warn」那一端又会让运维分不出「刚抖了一下」和
/// 「已经坏了三天」——所以连续失败到阈值时升成 `error`。
#[derive(Debug, Default)]
pub struct TtsHealth {
    consecutive_failures: AtomicU32,
}

impl TtsHealth {
    pub fn on_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// 记一次失败并打日志，返回当前连续失败次数。
    pub fn on_failure(&self, err: &anyhow::Error) -> u32 {
        let streak = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if streak >= TTS_FAILURES_BEFORE_ALARM {
            error!(
                consecutive = streak,
                error = %err,
                "语音合成连续失败：检查 tts 后端/配置，或把 provider 改回 native（不影响对话，只是没声音）"
            );
        } else {
            warn!(
                consecutive = streak,
                error = %err,
                "回答的语音合成失败：不计入「连续失败让位」，本轮无声音但流程继续"
            );
        }
        streak
    }
}

/// 回答的音频出口。
///
/// `provider: native`（默认）与直调 `device.speak` 完全等价——同一个模板、同样的
/// 空文本短路、同样的非零退出码变 Err。外部 provider 走「拿音频字节 → 落
/// `/tmp` → 播放器命令」这条链路，任何一步失败都按 `fallback_to_native` 决定
/// 是回退系统 TTS 还是报错。
pub struct TtsRouter {
    device: Device,
    gate: Arc<PlaybackGate>,
}

impl TtsRouter {
    pub fn new(device: Device, gate: Arc<PlaybackGate>) -> Self {
        Self { device, gate }
    }

    /// 念一句话。
    ///
    /// 注意错误提示（`speak_service_error`）不走这里，是有意为之：那条路径要念的
    /// 是「服务出问题了」，此时外部 TTS 大概率同样不可用，只有设备自带的
    /// `tts_play.sh` 还靠得住。
    pub async fn speak(&self, config: &TtsConfig, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        // 抓一次令牌：本轮从合成到播放用的都是它。中途有人喊唤醒（或 `/stop`）就会
        // begin() 一次，这里在真正出声前发现自己过期，于是闭嘴而不是补播一句。
        let token = self.gate.current();
        match config.provider {
            TtsProvider::Native => self.device.speak(text).await,
            provider => self.speak_remote(config, provider, text, token).await,
        }
    }

    async fn speak_remote(
        &self,
        config: &TtsConfig,
        provider: TtsProvider,
        text: &str,
        token: u64,
    ) -> anyhow::Result<()> {
        let attempt = match provider {
            TtsProvider::OpenAi => self.speak_openai(config, text, token).await,
            TtsProvider::Doubao => Err(anyhow!("tts provider Doubao 尚未接入")),
            TtsProvider::Native => unreachable!("native 不会走到这里"),
        };
        match attempt {
            Ok(()) => Ok(()),
            Err(err) if config.fallback_to_native => {
                // 音色会变，但「能说话」比「音色统一」重要。
                warn!(?provider, error = %err, "外部 TTS 失败，回退系统 TTS");
                self.device.speak(text).await
            }
            Err(err) => Err(err.context(format!(
                "tts provider {provider:?} 失败，且 fallback_to_native=false"
            ))),
        }
    }

    async fn speak_openai(&self, config: &TtsConfig, text: &str, token: u64) -> anyhow::Result<()> {
        let tts = &config.openai;
        if tts.api_key.trim().is_empty() {
            bail!("tts.openai.api_key 为空");
        }

        prune_old_speech_files(&tts.dir());
        let path = speech_output_path(&tts.dir(), &tts.response_format);
        let bytes = self.fetch_openai_speech(tts, text).await?;

        // 合成这一步可能要几百毫秒到几秒：期间用户可能已经喊了唤醒词。
        // 这时候**不要**再出声——旧行为是照样播完，用户听到的是「他已经问下一句了，
        // 音箱还在念上一句的答案」。
        if !self.gate.is_active(token) {
            debug!("本轮回答在合成期间被打断，跳过播放");
            return Ok(());
        }
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("写音频文件失败: {}", path.display()))?;
        debug!(bytes = bytes.len(), path = %path.display(), "TTS 音频已落盘");

        let template = config
            .playback_command
            .as_deref()
            .unwrap_or(DEFAULT_PLAYBACK_COMMAND);
        let script = render_playback_command(template, &path)?;
        let result = self
            .device
            .run_raw(&script, Duration::from_secs(10))
            .await?;
        if result.exit_code != 0 {
            bail!(
                "播放命令失败 exit={} stderr={}",
                result.exit_code,
                result.stderr.trim()
            );
        }
        Ok(())
    }

    async fn fetch_openai_speech(
        &self,
        tts: &OpenAiTtsConfig,
        text: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let mut body = json!({
            "model": tts.model,
            "input": text,
            "voice": tts.voice,
            "response_format": tts.response_format,
        });
        if let Some(speed) = tts.speed {
            body["speed"] = json!(speed);
        }
        if let Some(sample_rate) = tts.sample_rate {
            body["sample_rate"] = json!(sample_rate);
        }
        if let Some(instructions) = tts.instructions.as_deref() {
            if !instructions.trim().is_empty() {
                body["instructions"] = json!(instructions);
            }
        }

        let request = reqwest::Client::new()
            .post(tts.endpoint())
            .bearer_auth(&tts.api_key)
            .json(&body);
        // timeout_s 是「整体上限」，不是「等到响应头为止」：reqwest 的 send()
        // 在收到响应头就返回，body 是 bytes() 才读的。一个「200 之后几乎不吐
        // 数据」的后端会让这一轮 turn 永久卡住——LED 停在说话状态、空闲超时也
        // 要等这一轮结束才生效。所以两层都用同一个 limit 兜住。
        let limit = Duration::from_secs_f64(tts.timeout_s.max(1.0));
        let response = tokio::time::timeout(limit, request.send())
            .await
            .context("TTS 请求超时")??;

        let status = response.status();
        let bytes = tokio::time::timeout(limit, response.bytes())
            .await
            .context("TTS 响应体读取超时")?
            .context("读取 TTS 响应体失败")?;
        if !status.is_success() {
            // 错误体可能是 JSON、也可能是中转的 HTML 页，取前 200 字节止血。
            bail!(
                "TTS 接口返回 {status}: {}",
                String::from_utf8_lossy(&bytes[..bytes.len().min(200)])
            );
        }
        if bytes.is_empty() {
            bail!("TTS 接口返回空音频");
        }
        Ok(bytes.to_vec())
    }
}

/// 把 `{path}` 渲染成被 shell 引号化的路径。
///
/// 模板里没有 `{path}` 直接报错：写成 `miplayer -f`（忘了占位符）会变成
/// 「播放器空转、什么都没响」，比启动时炸掉难查得多。
pub fn render_playback_command(template: &str, path: &Path) -> anyhow::Result<String> {
    if !template.contains("{path}") {
        bail!("playback_command 缺少 {{path}} 占位符: {template}");
    }
    let displayed = path.to_string_lossy();
    let quoted = shell_words::quote(&displayed);
    Ok(template.replace("{path}", quoted.as_ref()))
}

fn extension_for(response_format: &str) -> &str {
    match response_format {
        "wav" => "wav",
        "mp3" => "mp3",
        "opus" => "opus",
        "aac" => "aac",
        "flac" => "flac",
        "pcm" => "pcm",
        // 未知格式也让它落盘，别在这里拦——真正的判据是播放器能不能放。
        // 结局是「播放失败 → 回退系统 TTS」，不是静默成功。
        _ => "bin",
    }
}

fn speech_output_path(dir: &Path, response_format: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "xiaoai-tts-{}-{}.{}",
        std::process::id(),
        stamp,
        extension_for(response_format)
    ))
}

/// 清掉一小时前的落盘音频。
///
/// `/tmp` 是 tmpfs（不写 NAND），但它是内存：agent 跑几周、每轮都留一个音频文件
/// 的话，最终会把设备内存吃光。只删自己写的那种文件名，不碰别人的东西。
fn prune_old_speech_files(dir: &Path) {
    const MAX_AGE: Duration = Duration::from_secs(3600);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("xiaoai-tts-")
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if modified.elapsed().map(|age| age > MAX_AGE).unwrap_or(false) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::DeviceConfig;
    use crate::shell::RecordingShell;

    fn router(shell: Arc<RecordingShell>) -> TtsRouter {
        TtsRouter::new(
            Device::with_runner(DeviceConfig::default(), shell),
            Arc::new(PlaybackGate::default()),
        )
    }

    /// CRLF 用 13/10 拼：源码里不出现转义序列，避免被工具链改写（踩过一次）。
    fn crlf() -> String {
        [13_u8, 10_u8].iter().map(|byte| *byte as char).collect()
    }

    /// 带独立令牌的 router：测试要自己控制「什么时候被打断」。
    fn router_with_gate(shell: Arc<RecordingShell>, gate: Arc<PlaybackGate>) -> TtsRouter {
        TtsRouter::new(Device::with_runner(DeviceConfig::default(), shell), gate)
    }

    /// 一个必定连接被拒的 OpenAI 兼容端点：不碰外网、失败得又快又确定。
    fn dead_endpoint() -> OpenAiTtsConfig {
        OpenAiTtsConfig {
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: "sk-test".to_string(),
            timeout_s: 3.0,
            output_dir: Some(std::env::temp_dir()),
            ..OpenAiTtsConfig::default()
        }
    }

    /// 默认配置（native）必须走原来的 `tts_play.sh`，一条命令、不多不少。
    #[tokio::test]
    async fn native_provider_still_uses_the_device_tts_command() {
        let shell = Arc::new(RecordingShell::always_ok());
        router(shell.clone())
            .speak(&TtsConfig::default(), "你好")
            .await
            .unwrap();
        assert_eq!(shell.command_count(), 1);
        assert!(
            shell
                .last_script()
                .unwrap()
                .starts_with("/usr/sbin/tts_play.sh"),
            "{:?}",
            shell.last_script()
        );
    }

    /// 尚未接入的 provider → 回退系统 TTS，且**确实念了**（不是静默跳过）。
    #[tokio::test]
    async fn unimplemented_provider_falls_back_to_native() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::Doubao,
            ..TtsConfig::default()
        };
        router(shell.clone())
            .speak(&config, "回退测试")
            .await
            .unwrap();
        assert_eq!(
            shell.command_count(),
            1,
            "回退时也要真的念出来，否则表现为「答了但没声音」"
        );
        assert!(shell.last_script().unwrap().contains("回退测试"));
    }

    #[tokio::test]
    async fn unimplemented_provider_without_fallback_is_an_error() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::Doubao,
            fallback_to_native: false,
            ..TtsConfig::default()
        };
        let err = router(shell.clone())
            .speak(&config, "不该被念出来")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback_to_native=false"), "{err}");
        assert_eq!(shell.command_count(), 0, "报错时不该动设备");
    }

    /// 没配 key 就没必要发请求：直接回退。这类配置错误重试多少次都没用。
    #[tokio::test]
    async fn openai_without_api_key_falls_back_without_a_request() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            ..TtsConfig::default()
        };
        router(shell.clone())
            .speak(&config, "没配 key")
            .await
            .unwrap();
        assert_eq!(shell.count_matching("/usr/sbin/tts_play.sh"), 1);
        assert_eq!(shell.count_matching("miplayer"), 0, "不该尝试播放");
    }

    /// 后端不可达 → 回退系统 TTS（这是「音箱必须能出声」的底线）。
    #[tokio::test]
    async fn unreachable_tts_backend_falls_back_to_native() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: dead_endpoint(),
            ..TtsConfig::default()
        };
        router(shell.clone())
            .speak(&config, "后端挂了")
            .await
            .unwrap();
        assert_eq!(shell.count_matching("/usr/sbin/tts_play.sh"), 1);
    }

    /// 关掉回退 = 明确要求「宁可不说话」，必须报错且不动设备。
    #[tokio::test]
    async fn unreachable_tts_backend_without_fallback_is_an_error() {
        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            fallback_to_native: false,
            openai: dead_endpoint(),
            ..TtsConfig::default()
        };
        let err = router(shell.clone())
            .speak(&config, "不该被念出来")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback_to_native=false"), "{err}");
        assert_eq!(shell.command_count(), 0);
    }

    /// 令牌语义：`begin()` 之后旧令牌立刻失效，新令牌有效。
    #[test]
    fn beginning_a_session_invalidates_the_old_token() {
        let gate = PlaybackGate::default();
        let first = gate.current();
        assert!(gate.is_active(first));

        let second = gate.begin();
        assert!(!gate.is_active(first), "begin() 之后旧令牌必须失效");
        assert!(gate.is_active(second));
        assert_eq!(gate.begin(), second + 1, "令牌必须单调递增");
    }

    /// 合成期间被打断（用户喊了唤醒词）→ **不播**，但也不算失败。
    ///
    /// 这条钉的是「说了闭嘴还多播一句」那个体感：旧代码在这里照样把音频交给播放器。
    #[tokio::test]
    async fn a_reply_interrupted_during_synthesis_is_not_played() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // 慢后端：300ms 后才回音频，留出「合成期间被打断」的窗口。
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0_u8; 4096];
                let _ = socket.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                let body = b"RIFFfake-wav";
                let nl = crlf();
                let head = format!("HTTP/1.1 200 OK{nl}Content-Length: {}{nl}{nl}", body.len());
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body).await;
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let gate = Arc::new(PlaybackGate::default());
        let router = router_with_gate(shell.clone(), gate.clone());

        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 5.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        // 100ms 后「用户喊了唤醒词」。
        let interrupter = gate.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            interrupter.begin();
        });

        router.speak(&config, "这句不该被念出来").await.unwrap();
        assert_eq!(
            shell.count_matching("miplayer"),
            0,
            "合成期间被打断时不能再去起播放器"
        );
    }

    /// 失败计数：到阈值前只 warn，连续三次后升 error；成功一次就清零。
    #[test]
    fn tts_health_escalates_then_resets() {
        let health = TtsHealth::default();
        let err = anyhow!("fake backend down");

        assert_eq!(health.on_failure(&err), 1);
        assert_eq!(health.on_failure(&err), 2);
        assert_eq!(
            health.on_failure(&err),
            TTS_FAILURES_BEFORE_ALARM,
            "第三次应当达到升级阈值"
        );

        health.on_success();
        assert_eq!(
            health.on_failure(&err),
            1,
            "成功一次必须清零，否则「坏过三天」永远都是同一个计数"
        );
    }

    /// `timeout_s` 必须覆盖**响应体读取**，不只是等到响应头。
    ///
    /// 假后端：回 `200` + 一个巨大的 `Content-Length`，然后一个字节都不写。
    /// 如果 body 不受 timeout 约束，这个测试会挂到测试超时；受约束时应当在
    /// `timeout_s` 附近返回 Err 并回退系统 TTS。
    #[tokio::test]
    async fn tts_body_read_is_bounded_by_the_timeout() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                // 用 13/10 拼 CRLF：源码里不出现转义序列，避免被各种工具链改写。
                let nl: String = [13_u8, 10_u8].iter().map(|byte| *byte as char).collect();
                let head = format!("HTTP/1.1 200 OK{nl}Content-Length: 999999{nl}{nl}");
                let _ = socket.write_all(head.as_bytes()).await;
                // 头发完就装死，body 永不到齐。
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        let shell = Arc::new(RecordingShell::always_ok());
        let config = TtsConfig {
            provider: TtsProvider::OpenAi,
            openai: OpenAiTtsConfig {
                base_url: format!("http://127.0.0.1:{port}/v1"),
                api_key: "sk-test".to_string(),
                timeout_s: 1.0,
                output_dir: Some(std::env::temp_dir()),
                ..OpenAiTtsConfig::default()
            },
            ..TtsConfig::default()
        };

        let started = std::time::Instant::now();
        router(shell.clone())
            .speak(&config, "超时测试")
            .await
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "body 读取必须受 timeout_s 约束，实际等了 {:?}",
            started.elapsed()
        );
        assert_eq!(
            shell.count_matching("/usr/sbin/tts_play.sh"),
            1,
            "超时后必须回退系统 TTS，而不是这一轮直接没声音"
        );
    }

    /// 播放命令必须带 `{path}`，且路径要被引号化（带空格的文件名不会裂开）。
    #[test]
    fn playback_command_quotes_the_path_and_requires_the_placeholder() {
        let rendered =
            render_playback_command(DEFAULT_PLAYBACK_COMMAND, Path::new("/tmp/a b.wav")).unwrap();
        assert!(
            rendered.contains("miplayer -f '/tmp/a b.wav'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("killall miplayer"),
            "必须先停旧的: {rendered}"
        );

        let err = render_playback_command("miplayer -f", Path::new("/tmp/x.wav"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("{path}"), "{err}");
    }

    /// 落盘文件名要带格式后缀：播放器多数按后缀选解复用器。
    #[test]
    fn output_path_follows_the_response_format() {
        let dir = std::env::temp_dir();
        for (format, ext) in [("wav", "wav"), ("mp3", "mp3"), ("pcm", "pcm")] {
            let path = speech_output_path(&dir, format);
            assert_eq!(path.extension().unwrap(), ext);
            assert!(path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("xiaoai-tts-"));
        }
    }
}
