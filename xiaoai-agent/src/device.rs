use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::config::DeviceConfig;
use crate::shell::{CommandResult, ShellRunner, SystemShell};

#[derive(Clone)]
pub struct Device {
    config: DeviceConfig,
    /// 唯一的 shell 出口。生产是 `SystemShell`，测试换成记录型 fake，
    /// 于是「设备侧到底发了什么命令」变成可断言的事实。
    runner: Arc<dyn ShellRunner>,
}

impl Device {
    pub fn new(config: DeviceConfig) -> Self {
        Self::with_runner(config, Arc::new(SystemShell))
    }

    /// 换掉 shell 执行体，其余设备语义（模板渲染、退出码判定、LED 开关）不变。
    pub fn with_runner(config: DeviceConfig, runner: Arc<dyn ShellRunner>) -> Self {
        Self { config, runner }
    }

    pub async fn speak(&self, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.run_template(&self.config.tts_command, &[("text", text)])
            .await
    }

    pub async fn play_url(&self, url: &str) -> anyhow::Result<()> {
        self.run_template(&self.config.play_url_command, &[("url", url)])
            .await
    }

    pub async fn stop_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.stop_audio_command).await
    }

    pub async fn pause_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.pause_audio_command).await
    }

    pub async fn resume_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.resume_audio_command).await
    }

    pub async fn duck_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.duck_audio_command).await
    }

    pub async fn unduck_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.unduck_audio_command).await
    }

    pub async fn abort_current_output(&self) {
        if let Err(err) = self.run_script(&self.config.abort_command).await {
            warn!("failed to abort current device output: {err:?}");
        }
    }

    pub async fn show_led(&self, id: u8) {
        if !self.config.led_enabled || id == 0 {
            return;
        }
        if let Err(err) = self
            .run_script(&format!("/bin/show_led {id} >/dev/null 2>&1"))
            .await
        {
            warn!("failed to show LED {id}: {err:?}");
        }
    }

    pub async fn shut_led(&self, id: u8) {
        if !self.config.led_enabled || id == 0 {
            return;
        }
        if let Err(err) = self
            .run_script(&format!("/bin/shut_led {id} >/dev/null 2>&1"))
            .await
        {
            warn!("failed to shut LED {id}: {err:?}");
        }
    }

    pub async fn blink_ready(&self, id: u8, duration: Duration) {
        self.show_led(id).await;
        sleep(duration).await;
        self.shut_led(id).await;
    }

    async fn run_template(&self, template: &str, values: &[(&str, &str)]) -> anyhow::Result<()> {
        let mut script = template.to_string();
        for (key, value) in values {
            let quoted = shell_words::quote(value);
            script = script.replace(&format!("{{{key}}}"), quoted.as_ref());
        }
        self.run_script(&script).await
    }

    /// 直接执行一条脚本。
    ///
    /// 给「不属于设备语义、但同样必须能拦住」的命令用：`main.rs` 的
    /// `pnshelper` 追问、故障让位都属于这一类。绕开这里直接调
    /// `shell::run_shell` 会让那条路径重新变成不可测。
    pub async fn run_raw(&self, script: &str, limit: Duration) -> anyhow::Result<CommandResult> {
        self.runner
            .run(script, limit)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
    }

    async fn run_script(&self, script: &str) -> anyhow::Result<()> {
        let limit = std::time::Duration::from_secs_f64(self.config.command_timeout_s.max(1.0));
        let result = self.run_raw(script, limit).await?;
        if result.exit_code != 0 {
            anyhow::bail!(
                "command failed exit={} stderr={}",
                result.exit_code,
                result.stderr.trim()
            );
        }
        debug!(
            "device command ok: {}",
            script.lines().next().unwrap_or(script)
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::RecordingShell;

    fn device_with(shell: Arc<RecordingShell>) -> Device {
        Device::with_runner(DeviceConfig::default(), shell)
    }

    #[tokio::test]
    async fn speak_renders_the_template_through_the_runner() {
        let shell = Arc::new(RecordingShell::always_ok());
        device_with(shell.clone()).speak("你好 世界").await.unwrap();
        let script = shell.last_script().expect("speak 必须走 shell");
        assert!(
            script.starts_with("/usr/sbin/tts_play.sh"),
            "模板没渲染: {script}"
        );
        assert!(
            script.contains("'你好 世界'"),
            "带空格的中文必须被 shell_words 引号化，否则 tts_play.sh 会当成多个参数: {script}"
        );
    }

    #[tokio::test]
    async fn empty_speech_does_not_touch_the_shell() {
        // 空回复不该触发 TTS：设备上每多一次 tts_play.sh 就多一次 pns 通知。
        let shell = Arc::new(RecordingShell::always_ok());
        device_with(shell.clone()).speak("   ").await.unwrap();
        assert_eq!(shell.command_count(), 0);
    }

    #[tokio::test]
    async fn non_zero_exit_becomes_an_error() {
        // 设备脚本用返回码表达失败，上层（run_session 的错误分支）靠这个 Err
        // 决定要不要提示用户，吞掉就等于静默失败。
        let shell = Arc::new(RecordingShell::exiting_with(3, "no such device"));
        let err = device_with(shell)
            .speak("hi")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit=3"), "{err}");
        assert!(err.contains("no such device"), "{err}");
    }

    #[tokio::test]
    async fn led_commands_are_skipped_when_disabled() {
        let shell = Arc::new(RecordingShell::always_ok());
        let mut config = DeviceConfig::default();
        config.led_enabled = false;
        let device = Device::with_runner(config, shell.clone());
        device.show_led(3).await;
        device.shut_led(3).await;
        assert_eq!(
            shell.command_count(),
            0,
            "led_enabled=false 时不该有 show_led/shut_led 子进程"
        );
    }

    #[tokio::test]
    async fn led_commands_use_the_configured_ids() {
        let shell = Arc::new(RecordingShell::always_ok());
        let device = device_with(shell.clone());
        device.show_led(4).await;
        device.shut_led(5).await;
        assert_eq!(shell.count_matching("/bin/show_led 4"), 1);
        assert_eq!(shell.count_matching("/bin/shut_led 5"), 1);
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").finish_non_exhaustive()
    }
}
