use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use serde_json::json;
use tracing::{debug, warn};

use crate::config::{OpenAiTtsConfig, TtsConfig, TtsProvider};
use crate::device::Device;

/// 播放本地音频文件的默认命令。
///
/// 三条都是真机实测的结论（OH2P 1.62.2，见 `docs/device-scripts.md`）：
/// - `miplayer -f <本地文件>` 可用（日志 `event(Playing)` → `EndReached`）；
/// - `miplayer` **播完不会自己退出**，所以下次播放前必须 `killall`；
/// - 后台起播（`&`），否则一条命令就吃掉整个超时预算。
pub const DEFAULT_PLAYBACK_COMMAND: &str =
    "killall miplayer 2>/dev/null; miplayer -f {path} >/tmp/xiaoai-tts-player.log 2>&1 &";

/// 回答的音频出口。
///
/// `provider: native`（默认）与直调 `device.speak` 完全等价——同一个模板、同样的
/// 空文本短路、同样的非零退出码变 Err。外部 provider 走「拿音频字节 → 落
/// `/tmp` → 播放器命令」这条链路，任何一步失败都按 `fallback_to_native` 决定
/// 是回退系统 TTS 还是报错。
pub struct TtsRouter {
    device: Device,
}

impl TtsRouter {
    pub fn new(device: Device) -> Self {
        Self { device }
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
        match config.provider {
            TtsProvider::Native => self.device.speak(text).await,
            provider => self.speak_remote(config, provider, text).await,
        }
    }

    async fn speak_remote(
        &self,
        config: &TtsConfig,
        provider: TtsProvider,
        text: &str,
    ) -> anyhow::Result<()> {
        let attempt = match provider {
            TtsProvider::OpenAi => self.speak_openai(config, text).await,
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

    async fn speak_openai(&self, config: &TtsConfig, text: &str) -> anyhow::Result<()> {
        let tts = &config.openai;
        if tts.api_key.trim().is_empty() {
            bail!("tts.openai.api_key 为空");
        }

        prune_old_speech_files(&tts.dir());
        let path = speech_output_path(&tts.dir(), &tts.response_format);
        let bytes = self.fetch_openai_speech(tts, text).await?;
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

        let request = reqwest::Client::new()
            .post(tts.endpoint())
            .bearer_auth(&tts.api_key)
            .json(&body);
        let response = tokio::time::timeout(
            Duration::from_secs_f64(tts.timeout_s.max(1.0)),
            request.send(),
        )
        .await
        .context("TTS 请求超时")??;

        let status = response.status();
        let bytes = response.bytes().await.context("读取 TTS 响应体失败")?;
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
        TtsRouter::new(Device::with_runner(DeviceConfig::default(), shell))
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
