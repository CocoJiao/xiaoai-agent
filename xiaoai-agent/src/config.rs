use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::RwLock;

use crate::shell::DEFAULT_COMMAND_TIMEOUT;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub root: Option<PathBuf>,
    pub runtime: RuntimeConfig,
    pub device: DeviceConfig,
    pub capture: CaptureConfig,
    pub asr: AsrConfig,
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub mcp: McpConfig,
    pub music: MusicConfig,
    pub airplay: AirPlayConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut config: Self = serde_yaml::from_str(&text)?;
        if config.root.is_none() {
            config.root = Some(
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(".")),
            );
        }
        config.resolve_paths();
        config.llm.base_url = openai_base_url(&config.llm.base_url);
        config.asr.open_ai.base_url = openai_base_url(&config.asr.open_ai.base_url);
        config.asr.openai_realtime.base_url =
            openai_realtime_base_url(&config.asr.openai_realtime.base_url);
        Ok(config)
    }

    fn resolve_paths(&mut self) {
        let Some(root) = self.root.as_deref() else {
            return;
        };
        resolve_optional_path(root, &mut self.music.netease.cookie_file);
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub speech_socket: String,
    pub session_idle_timeout_s: f64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            speech_socket: "/tmp/mico_aivs_lab/usock/speech.usock".to_string(),
            session_idle_timeout_s: 20.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    pub tts_command: String,
    /// 单条设备命令（tts_play.sh / miplayer / ubus 等）的最长执行时间。
    /// 同步播放的长回复可能接近这个上限，按需调大。
    pub command_timeout_s: f64,
    pub play_url_command: String,
    pub stop_audio_command: String,
    pub pause_audio_command: String,
    pub resume_audio_command: String,
    pub duck_audio_command: String,
    pub unduck_audio_command: String,
    pub abort_command: String,
    pub led_enabled: bool,
    pub led_listening: u8,
    pub led_user_speaking: u8,
    pub led_thinking: u8,
    pub led_speaking: u8,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            tts_command: "/usr/sbin/tts_play.sh --notify-pns {text}".to_string(),
            command_timeout_s: DEFAULT_COMMAND_TIMEOUT.as_secs_f64(),
            play_url_command:
                "killall miplayer 2>/dev/null; miplayer -f {url} >/tmp/xiaoai-miplayer.log 2>&1 &"
                    .to_string(),
            stop_audio_command: "killall tts_play.sh miplayer 2>/dev/null; mphelper pause"
                .to_string(),
            pause_audio_command:
                "mphelper pause 2>/dev/null || true; for p in $(pidof miplayer 2>/dev/null); do kill -STOP \"$p\" 2>/dev/null || true; done"
                    .to_string(),
            resume_audio_command:
                "for p in $(pidof miplayer 2>/dev/null); do kill -CONT \"$p\" 2>/dev/null || true; done; mphelper play 2>/dev/null || true"
                    .to_string(),
            duck_audio_command:
                "amixer sget mysoftvol | awk -F'[][]' '/%/ {print $2; exit}' | tr -d '%' >/tmp/xiaoai-mysoftvol.before; amixer sset mysoftvol 25% >/dev/null 2>&1 || true"
                    .to_string(),
            unduck_audio_command:
                "v=$(cat /tmp/xiaoai-mysoftvol.before 2>/dev/null); if [ -n \"$v\" ]; then amixer sset mysoftvol \"${v}%\" >/dev/null 2>&1 || true; fi; rm -f /tmp/xiaoai-mysoftvol.before"
                    .to_string(),
            abort_command: "killall tts_play.sh miplayer 2>/dev/null; mphelper pause 2>/dev/null || true; ubus call mediaplayer media_control '{\"player\":\"mediaplayer\",\"action\":\"pause\",\"volume\":0}' 2>/dev/null || true; ubus call mediaplayer player_play_operation '{\"media\":\"app_ios\",\"action\":\"stop\"}' 2>/dev/null || true".to_string(),
            led_enabled: true,
            led_listening: 3,
            led_user_speaking: 4,
            led_thinking: 2,
            led_speaking: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub sample_rate: u32,
    pub threshold: f64,
    pub mic_gain: f64,
    pub block_ms: u64,
    pub pre_roll_ms: u64,
    pub silence_ms: u64,
    pub min_speech_ms: u64,
    pub max_utterance_s: f64,
    pub cooldown_s: f64,
    pub print_levels: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            threshold: 0.006,
            mic_gain: 1.0,
            block_ms: 100,
            pre_roll_ms: 300,
            silence_ms: 1500,
            min_speech_ms: 300,
            max_utterance_s: 15.0,
            cooldown_s: 0.7,
            print_levels: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AsrConfig {
    pub provider: AsrProvider,
    pub open_ai: OpenAiAsrConfig,
    pub openai_realtime: OpenAiRealtimeAsrConfig,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            provider: AsrProvider::OpenAi,
            open_ai: OpenAiAsrConfig::default(),
            openai_realtime: OpenAiRealtimeAsrConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AsrProvider {
    #[default]
    #[serde(alias = "openai")]
    OpenAi,
    #[serde(alias = "openai_realtime", alias = "open_ai_realtime")]
    OpenAiRealtime,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAiAsrConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub language: String,
    pub prompt: String,
    pub timeout_s: f64,
    pub retries: u32,
}

impl Default for OpenAiAsrConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "EMPTY".to_string(),
            model: "whisper-1".to_string(),
            language: "zh".to_string(),
            prompt: String::new(),
            timeout_s: 5.0,
            retries: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAiRealtimeAsrConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub target_sample_rate: u32,
    pub chunk_ms: u32,
    pub timeout_s: f64,
    pub retries: u32,
    pub server_vad: RealtimeServerVadConfig,
}

impl Default for OpenAiRealtimeAsrConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "EMPTY".to_string(),
            model: "gpt-realtime-whisper".to_string(),
            target_sample_rate: 16_000,
            chunk_ms: 200,
            timeout_s: 10.0,
            retries: 1,
            server_vad: RealtimeServerVadConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RealtimeServerVadConfig {
    pub enabled: bool,
    pub prefix_padding_ms: u32,
    pub silence_duration_ms: u32,
    pub threshold: f32,
}

impl Default for RealtimeServerVadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix_padding_ms: 300,
            silence_duration_ms: 1_200,
            threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub protocol: LlmProtocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_s: f64,
    pub max_tokens: u64,
    pub temperature: f64,
    pub thinking: LlmThinkingConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            protocol: LlmProtocol::OpenAiChat,
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "EMPTY".to_string(),
            model: "gpt-5.4-mini".to_string(),
            timeout_s: 5.0,
            max_tokens: 300,
            temperature: 0.5,
            thinking: LlmThinkingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmProtocol {
    #[serde(alias = "openai", alias = "chat_completions")]
    #[default]
    OpenAiChat,
    Anthropic,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmThinkingConfig {
    pub mode: LlmThinkingMode,
    pub budget_tokens: u64,
}

impl Default for LlmThinkingConfig {
    fn default() -> Self {
        Self {
            mode: LlmThinkingMode::Disabled,
            budget_tokens: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmThinkingMode {
    Auto,
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub timezone: String,
    pub weather: WeatherConfig,
    pub web_search: WebSearchConfig,
    /// 是否把当前时间注入用户消息（关闭工具时，模型仍能回答时间/日期类问题）。
    pub inject_current_time: bool,
    /// 是否向 LLM 注册工具（内置 + MCP）。
    /// 某些 OpenAI 兼容代理会丢弃 tool_calls 内容，表现为
    /// "Response contained no message or tool call (empty)"；
    /// 这类端点必须关掉工具才能正常对话。
    pub tools_enabled: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            timezone: "Asia/Shanghai".to_string(),
            weather: WeatherConfig::default(),
            web_search: WebSearchConfig::default(),
            inject_current_time: false,
            tools_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    pub qweather_url: String,
    pub default_location: String,
    pub ip_lookup_url: String,
    pub timeout_s: f64,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            qweather_url: String::new(),
            default_location: String::new(),
            ip_lookup_url: "https://ipapi.co/json/".to_string(),
            timeout_s: 5.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    pub enabled: bool,
    pub tavily_url: String,
    pub api_key: String,
    pub timeout_s: f64,
    pub max_results: usize,
    pub search_depth: String,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tavily_url: "https://api.tavily.com/search".to_string(),
            api_key: String::new(),
            timeout_s: 10.0,
            max_results: 3,
            search_depth: "basic".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
    // Backward-compatible single-server configuration.
    pub home_assistant: HomeAssistantMcpConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub url: String,
    pub token: String,
    pub timeout_s: f64,
    pub tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: "mcp".to_string(),
            enabled: false,
            url: String::new(),
            token: String::new(),
            timeout_s: 10.0,
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HomeAssistantMcpConfig {
    pub enabled: bool,
    pub url: String,
    pub token: String,
    pub timeout_s: f64,
}

impl Default for HomeAssistantMcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://homeassistant.local:8123/mcp_server/sse".to_string(),
            token: String::new(),
            timeout_s: 5.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MusicConfig {
    pub enabled: bool,
    pub provider: String,
    pub interruption: MusicInterruptionConfig,
    pub netease: NeteaseConfig,
    pub navidrome: NavidromeConfig,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "navidrome".to_string(),
            interruption: MusicInterruptionConfig::default(),
            netease: NeteaseConfig::default(),
            navidrome: NavidromeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlayConfig {
    pub enabled: bool,
    pub name: String,
    pub port: u16,
    pub password: String,
    pub hwaddr: String,
    pub output: AirPlayOutputConfig,
    pub interruption: AirPlayInterruptionConfig,
}

impl Default for AirPlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "XiaoAI AirPlay".to_string(),
            port: 5000,
            password: String::new(),
            hwaddr: String::new(),
            output: AirPlayOutputConfig::default(),
            interruption: AirPlayInterruptionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlayOutputConfig {
    pub backend: String,
    pub aplay_path: String,
    pub device: String,
    pub format: String,
}

impl Default for AirPlayOutputConfig {
    fn default() -> Self {
        Self {
            backend: "aplay".to_string(),
            aplay_path: "/usr/bin/aplay".to_string(),
            device: "default".to_string(),
            format: "S16_LE".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlayInterruptionConfig {
    pub mode: String,
    pub duck_gain: f32,
}

impl Default for AirPlayInterruptionConfig {
    fn default() -> Self {
        Self {
            mode: "duck".to_string(),
            duck_gain: 0.25,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MusicInterruptionConfig {
    pub mode: String,
}

impl Default for MusicInterruptionConfig {
    fn default() -> Self {
        Self {
            mode: "pause".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NeteaseConfig {
    pub api_base_url: String,
    pub login_mode: String,
    pub account: String,
    pub phone: String,
    pub password: String,
    pub md5_password: String,
    pub cookie_file: Option<PathBuf>,
    pub default_level: String,
    pub timeout_s: f64,
    pub login_on_start: bool,
}

impl Default for NeteaseConfig {
    fn default() -> Self {
        Self {
            api_base_url: "http://127.0.0.1:3300".to_string(),
            login_mode: "captcha".to_string(),
            account: String::new(),
            phone: String::new(),
            password: String::new(),
            md5_password: String::new(),
            cookie_file: None,
            default_level: "standard".to_string(),
            timeout_s: 5.0,
            login_on_start: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NavidromeConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub api_version: String,
    pub timeout_s: f64,
}

impl Default for NavidromeConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:4533".to_string(),
            username: String::new(),
            password: String::new(),
            api_version: "1.16.1".to_string(),
            timeout_s: 5.0,
        }
    }
}

/// URL 是否已带路径段（`https://host` 为 false，`https://host/api/v3` 为 true）。
fn url_has_path(base: &str) -> bool {
    base.split_once("://")
        .map(|(_, rest)| rest.contains('/'))
        .unwrap_or(false)
}

pub fn openai_base_url(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    // 只有在 URL 没有路径段时才补 /v1。
    // 例如 https://ark.cn-beijing.volces.com/api/v3 已有路径，硬补会变成
    // /api/v3/v1 → 404，且用户没有任何办法关掉。
    if base.is_empty() || base.ends_with("/v1") || url_has_path(base) {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

pub fn openai_realtime_base_url(raw: &str) -> String {
    // 与 openai_base_url 同一契约：带路径就原样用，不再强补 /v1
    // （否则 https://host/api/v3 → /api/v3/v1、https://host/v1beta → /v1beta/v1）。
    let base = raw.trim_end_matches('/');
    if base.is_empty() || base.ends_with("/v1") || base.ends_with("/realtime") || url_has_path(base)
    {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// 可整体替换的配置句柄。
///
/// 选 `RwLock<Arc<AppConfig>>` 而不是引 `arc-swap`：零新依赖，读侧只是
/// 「拿读锁 → clone 一个 Arc → 放锁」，锁不跨 await。每次取一份快照，
/// 于是「一轮对话全程用同一份配置」天然成立，不会查到半新半旧的状态。
#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<Arc<AppConfig>>>,
}

impl ConfigHandle {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
        }
    }

    /// 取一份当前配置快照（拿到就立刻放锁，凭 Arc 生存）。
    pub async fn get(&self) -> Arc<AppConfig> {
        self.inner.read().await.clone()
    }

    /// 整体替换，返回被换下来的旧配置。
    pub async fn replace(&self, next: Arc<AppConfig>) -> Arc<AppConfig> {
        std::mem::replace(&mut *self.inner.write().await, next)
    }
}

/// 一次配置重载里各配置段的去向。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConfigChanges {
    /// 热加载生效，下一次用到就是新值。
    pub applied: Vec<&'static str>,
    /// 只有重启才生效：本次保留旧值，调用方据此告警。
    pub needs_restart: Vec<&'static str>,
    /// 明确拒绝的段：运行中无法切换（目前只有 socket），其余段照常生效。
    pub rejected: Vec<&'static str>,
}

impl ConfigChanges {
    pub fn is_noop(&self) -> bool {
        self.applied.is_empty() && self.needs_restart.is_empty() && self.rejected.is_empty()
    }
}

/// 比较两份配置，按「能不能热加载」分类。
///
/// 用 Debug 快照比较，而不是给 25 个配置结构逐个补 `PartialEq`：它们全是
/// `#[derive(Debug, Deserialize)]` 的纯数据，Debug 输出现即字段全集，比较结果
/// 与逐字段 Eq 等价；这样既不用改 25 处 derive，也没有「新加字段忘了比较」的
/// 风险——字段进了结构就自动进比较。
fn section_changed<T: std::fmt::Debug>(old: &T, new: &T) -> bool {
    format!("{old:?}") != format!("{new:?}")
}

pub fn classify_config_change(old: &AppConfig, new: &AppConfig) -> ConfigChanges {
    let mut changes = ConfigChanges::default();

    // 运行中不可能换 speech socket：SpeechService 已经绑在这个路径上了，
    // 换路径等于让 mipns 和 agent 各说各话。
    if section_changed(&old.runtime.speech_socket, &new.runtime.speech_socket) {
        changes.rejected.push("runtime.speech_socket");
    }
    if section_changed(
        &old.runtime.session_idle_timeout_s,
        &new.runtime.session_idle_timeout_s,
    ) {
        changes.applied.push("runtime.session_idle_timeout_s");
    }
    // 下面这些段都是「用的时候才读字段」，换掉句柄里的 Arc 就生效。
    if section_changed(&old.capture, &new.capture) {
        changes.applied.push("capture");
    }
    if section_changed(&old.llm, &new.llm) {
        changes.applied.push("llm");
    }
    if section_changed(&old.agent, &new.agent) {
        changes.applied.push("agent");
    }
    if section_changed(&old.music, &new.music) {
        changes.applied.push("music");
    }
    if section_changed(&old.mcp, &new.mcp) {
        changes.applied.push("mcp");
    }
    // device: Device 启动时把 DeviceConfig 克隆走了（TTS 模板、命令超时、LED id），
    // 只换配置会让「模板还是旧命令、ID 是新的」，所以整段按需重启处理。
    if section_changed(&old.device, &new.device) {
        changes.needs_restart.push("device");
    }
    // asr: AsrClient 构造时选定了 provider 并克隆了整段配置（含 realtime 客户端）。
    if section_changed(&old.asr, &new.asr) {
        changes.needs_restart.push("asr");
    }
    // airplay: 接收端在启动时建好监听，重启有端口/句柄占用风险。
    if section_changed(&old.airplay, &new.airplay) {
        changes.needs_restart.push("airplay");
    }

    changes
}

pub fn require_music_enabled(config: &AppConfig) -> anyhow::Result<()> {
    anyhow::ensure!(config.music.enabled, "music.enabled is false");
    Ok(())
}

pub fn timeout_duration(seconds: f64) -> Duration {
    // .inf / NaN（yaml 写 .inf 或算出 infinity）会让 from_secs_f64 panic，
    // 而 release 编译是 panic = "abort"，等于配置写错就把进程打死。
    if !seconds.is_finite() {
        return Duration::from_secs_f64(30.0);
    }
    Duration::from_secs_f64(seconds.max(0.1))
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn resolve_optional_path(root: &Path, path: &mut Option<PathBuf>) {
    if let Some(value) = path {
        *value = resolve_path(root, value);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_config_change, openai_base_url, openai_realtime_base_url, timeout_duration,
        AppConfig, AsrConfig, AsrProvider, ConfigHandle, LlmConfig, LlmProtocol, LlmThinkingMode,
        McpConfig,
    };
    use std::sync::Arc;

    #[test]
    fn example_uses_external_asr_by_default() {
        let config: AppConfig =
            serde_yaml::from_str(include_str!("../agent.example.yaml")).unwrap();
        assert!(matches!(config.asr.provider, AsrProvider::OpenAi));
    }

    #[test]
    fn rejects_removed_xiaomi_aivs_provider() {
        let error = serde_yaml::from_str::<AsrConfig>("provider: xiaomi_aivs\n").unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn parses_openai_realtime_provider_alias() {
        let config: AsrConfig = serde_yaml::from_str("provider: openai_realtime\n").unwrap();
        assert!(matches!(config.provider, AsrProvider::OpenAiRealtime));
        assert_eq!(config.openai_realtime.target_sample_rate, 16_000);
        assert!(config.openai_realtime.server_vad.enabled);
    }

    #[test]
    fn parses_realtime_server_vad_settings() {
        let config: AsrConfig = serde_yaml::from_str(
            "openai_realtime:\n  target_sample_rate: 16000\n  server_vad:\n    enabled: false\n    prefix_padding_ms: 450\n    silence_duration_ms: 900\n    threshold: 0.7\n",
        )
        .unwrap();
        assert!(!config.openai_realtime.server_vad.enabled);
        assert_eq!(config.openai_realtime.server_vad.prefix_padding_ms, 450);
        assert_eq!(config.openai_realtime.server_vad.silence_duration_ms, 900);
        assert_eq!(config.openai_realtime.server_vad.threshold, 0.7);
    }

    #[test]
    fn keeps_server_vad_enabled_when_partially_overridden() {
        let config: AsrConfig =
            serde_yaml::from_str("openai_realtime:\n  server_vad:\n    silence_duration_ms: 900\n")
                .unwrap();
        let vad = config.openai_realtime.server_vad;
        assert!(vad.enabled);
        assert_eq!(vad.prefix_padding_ms, 300);
        assert_eq!(vad.silence_duration_ms, 900);
        assert_eq!(vad.threshold, 0.5);
    }

    #[test]
    fn parses_anthropic_with_disabled_thinking() {
        let config: LlmConfig = serde_yaml::from_str(
            "protocol: anthropic\nthinking:\n  mode: disabled\n  budget_tokens: 2048\n",
        )
        .unwrap();
        assert_eq!(config.protocol, LlmProtocol::Anthropic);
        assert_eq!(config.thinking.mode, LlmThinkingMode::Disabled);
        assert_eq!(config.thinking.budget_tokens, 2048);
    }

    #[test]
    fn openai_base_url_keeps_existing_path() {
        // 已带路径的兼容端点不能被强补 /v1（否则变成 /api/v3/v1 → 404）
        assert_eq!(
            openai_base_url("https://ark.cn-beijing.volces.com/api/v3"),
            "https://ark.cn-beijing.volces.com/api/v3"
        );
        assert_eq!(
            openai_base_url("http://192.168.1.106:8088/v1"),
            "http://192.168.1.106:8088/v1"
        );
    }

    #[test]
    fn openai_realtime_base_url_keeps_existing_path() {
        assert_eq!(
            openai_realtime_base_url("https://ark.cn-beijing.volces.com/api/v3"),
            "https://ark.cn-beijing.volces.com/api/v3"
        );
        assert_eq!(
            openai_realtime_base_url("https://api.example.com/v1beta"),
            "https://api.example.com/v1beta"
        );
        assert_eq!(
            openai_realtime_base_url("ws://192.168.1.10:8000/v1"),
            "ws://192.168.1.10:8000/v1"
        );
        assert_eq!(
            openai_realtime_base_url("http://192.168.1.10:8000/realtime"),
            "http://192.168.1.10:8000/realtime"
        );
    }

    #[test]
    fn base_url_keeps_empty_input_as_is() {
        assert_eq!(openai_base_url(""), "");
        assert_eq!(openai_realtime_base_url(""), "");
        assert_eq!(
            timeout_duration(f64::INFINITY),
            std::time::Duration::from_secs(30)
        );
        assert_eq!(
            timeout_duration(f64::NAN),
            std::time::Duration::from_secs(30)
        );
    }

    #[test]
    fn openai_realtime_base_url_appends_v1_for_bare_host() {
        assert_eq!(
            openai_realtime_base_url("http://192.168.1.10:8000"),
            "http://192.168.1.10:8000/v1"
        );
    }

    #[test]
    fn openai_base_url_appends_v1_for_bare_host() {
        assert_eq!(
            openai_base_url("http://192.168.1.10:8000"),
            "http://192.168.1.10:8000/v1"
        );
        assert_eq!(
            openai_base_url("https://api.openai.com/"),
            "https://api.openai.com/v1"
        );
    }

    #[test]
    fn parses_generic_mcp_server_with_tool_allowlist() {
        let config: McpConfig = serde_yaml::from_str(
            "servers:\n  - name: home_assistant\n    enabled: true\n    url: http://homeassistant.local/api/mcp\n    tools: [ha_search, ha_get_state]\n",
        )
        .unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "home_assistant");
        assert_eq!(config.servers[0].tools, ["ha_search", "ha_get_state"]);
    }

    /// 同一份配置重载不应被当成「有变化」（否则每次 SIGHUP 都白白重建工具和 MCP）。
    #[test]
    fn classify_reports_noop_for_identical_config() {
        let config = AppConfig::default();
        assert!(classify_config_change(&config, &config).is_noop());
    }

    /// socket 是唯一被拒绝的段：SpeechService 已经绑在上面，换路径没有意义。
    #[test]
    fn classify_rejects_speech_socket_change() {
        let old = AppConfig::default();
        let mut new = AppConfig::default();
        new.runtime.speech_socket = "/tmp/other/speech.usock".to_string();
        let changes = classify_config_change(&old, &new);
        assert_eq!(changes.rejected, vec!["runtime.speech_socket"]);
        assert!(changes.applied.is_empty());
        assert!(changes.needs_restart.is_empty());
    }

    /// 启动时被克隆走的段（device / asr / airplay）只能重启生效，不能假装热加载。
    #[test]
    fn classify_marks_snapshotted_sections_as_needing_restart() {
        let old = AppConfig::default();
        let mut new = AppConfig::default();
        new.device.tts_command = "/usr/sbin/tts_play.sh --notify-pns -v 90 {text}".to_string();
        new.asr.open_ai.model = "whisper-large-v3".to_string();
        new.airplay.enabled = true;
        let changes = classify_config_change(&old, &new);
        assert_eq!(changes.needs_restart, vec!["device", "asr", "airplay"]);
        assert!(
            changes.applied.is_empty(),
            "这三段不该被当成热加载生效: {changes:?}"
        );
    }

    /// 「用的时候才读」的段必须进 applied，否则改完 yaml 用户以为生效了其实没有。
    #[test]
    fn classify_marks_per_use_sections_as_applied() {
        let old = AppConfig::default();
        let mut new = AppConfig::default();
        new.llm.model = "another-model".to_string();
        new.capture.sample_rate = 8000;
        new.runtime.session_idle_timeout_s = 60.0;
        new.agent.tools_enabled = false;
        let changes = classify_config_change(&old, &new);
        assert_eq!(
            changes.applied,
            vec!["runtime.session_idle_timeout_s", "capture", "llm", "agent"]
        );
        assert!(changes.needs_restart.is_empty());
        assert!(changes.rejected.is_empty());
    }

    #[tokio::test]
    async fn config_handle_returns_snapshots_and_old_value() {
        let mut first = AppConfig::default();
        first.llm.model = "first".to_string();
        let handle = ConfigHandle::new(Arc::new(first));

        let mut second = AppConfig::default();
        second.llm.model = "second".to_string();
        let previous = handle.replace(Arc::new(second)).await;

        assert_eq!(previous.llm.model, "first", "replace 必须把旧配置交出来");
        assert_eq!(handle.get().await.llm.model, "second");
    }
}
