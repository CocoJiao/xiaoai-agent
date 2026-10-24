use anyhow::bail;
use tracing::warn;

use crate::config::{TtsConfig, TtsProvider};
use crate::device::Device;

/// 回答的音频出口。
///
/// 这一步只做一件事：把「谁来念这句话」从 `device.speak` 直调变成一个**可路由**
/// 的对象。`provider: native`（默认值）下行为与直调完全一致——同样的
/// `device.tts_command` 模板、同样的空文本短路、同样的非零退出码变 Err，
/// 所以接上它不改变任何现有行为。
///
/// 外部 provider（doubao / openai）留到下一步接：那时才会出现「拿音频字节、
/// 落盘、交给播放器」这条链路。现在配成外部 provider 只会得到一条 warn +
/// 回退系统 TTS（`fallback_to_native: true` 时），因为一个不出声的音箱比一个
/// 音色不统一的音箱糟糕得多。
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
        match config.provider {
            TtsProvider::Native => self.device.speak(text).await,
            provider => {
                if !config.fallback_to_native {
                    // 显式关掉回退 = 用户要求「宁可不说话，也别用系统音色」。
                    // 这条必须报错而不是静默跳过，否则表现为「答了但没声音」。
                    bail!("tts provider {provider:?} 尚未接入，且 fallback_to_native=false");
                }
                warn!(
                    ?provider,
                    "外部 TTS provider 尚未接入，本轮回答回退系统 TTS"
                );
                self.device.speak(text).await
            }
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

    fn config(provider: TtsProvider, fallback_to_native: bool) -> TtsConfig {
        TtsConfig {
            provider,
            fallback_to_native,
            playback_command: None,
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

    /// 外部 provider 还没接入 → 回退系统 TTS，且**确实念了**（不是静默跳过）。
    #[tokio::test]
    async fn unimplemented_provider_falls_back_to_native() {
        let shell = Arc::new(RecordingShell::always_ok());
        router(shell.clone())
            .speak(&config(TtsProvider::OpenAi, true), "回退测试")
            .await
            .unwrap();
        assert_eq!(
            shell.command_count(),
            1,
            "回退时也要真的念出来，否则表现为「答了但没声音」"
        );
        assert!(shell.last_script().unwrap().contains("回退测试"));
    }

    /// 关掉回退 = 明确要求「宁可不说话」，必须报错而不是静默成功。
    #[tokio::test]
    async fn unimplemented_provider_without_fallback_is_an_error() {
        let shell = Arc::new(RecordingShell::always_ok());
        let err = router(shell.clone())
            .speak(&config(TtsProvider::Doubao, false), "不该被念出来")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("fallback_to_native=false"), "{err}");
        assert_eq!(shell.command_count(), 0, "报错时不该动设备");
    }
}
