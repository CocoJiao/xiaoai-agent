use std::sync::Arc;

use anyhow::Context;
use chrono::Datelike;
use chrono_tz::Tz;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::agent::AgentControl;
use crate::config::{timeout_duration, AppConfig};
use crate::music::MusicService;
use crate::weather::WeatherService;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolCallError(String);

impl From<anyhow::Error> for ToolCallError {
    fn from(value: anyhow::Error) -> Self {
        Self(value.to_string())
    }
}

/// 构造工具的 JSON Schema 参数定义。
///
/// `required` 必须显式给出（无必填参数时传空数组）：
/// 部分严格实现（OpenAI 的 strict tools、若干中转代理）要求每个函数都带
/// `required` 字段，缺失会被直接拒绝或静默降级；而属性类型必须是字符串，
/// 写成 `["string", "null"]` 这种数组形式同样会被拒。
/// 可选参数靠「不列进 required」表达，不要用 null 联合类型。
pub fn definition(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters: json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

/// 校验单个工具定义的参数 schema 是否满足上面的约定。
///
/// 返回错误信息而不是直接 panic，便于测试里带上工具名。
///
/// 已知边界：这套 schema **还不满足 OpenAI strict mode** —— 那边要求
/// `additionalProperties: false` 且所有属性都必须进 `required`。现在没有接
/// strict 端点是对的（多数中转不支持那两个约束），以后要接需在这里补校验。
///
/// `cfg(test)`：唯一调用点在下面测试里，生产构建用不到。挂了它 `cargo check`
/// 才回到零 warning（不挂是既有基线里的 1 个 dead_code）。
#[cfg(test)]
pub fn validate_definition(def: &ToolDefinition) -> Result<(), String> {
    let params = &def.parameters;
    if params.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!("{}: parameters.type 必须是 object", def.name));
    }
    let required = params
        .get("required")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{}: 缺少 required 字段", def.name))?;
    let empty = Map::new();
    let properties = params
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    for key in required {
        let key = key
            .as_str()
            .ok_or_else(|| format!("{}: required 里出现非字符串项", def.name))?;
        if !properties.contains_key(key) {
            return Err(format!(
                "{}: required 里的 {key} 不在 properties 中",
                def.name
            ));
        }
    }
    for (prop, schema) in properties {
        match schema.get("type") {
            Some(Value::String(_)) => {}
            Some(other) => {
                return Err(format!(
                    "{}: 属性 {prop} 的 type 必须写成字符串，实际是 {other}",
                    def.name
                ))
            }
            None => return Err(format!("{}: 属性 {prop} 缺少 type", def.name)),
        }
    }
    Ok(())
}

fn as_json_text(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

#[derive(Clone)]
pub struct GetCurrentTime {
    config: Arc<AppConfig>,
}

impl GetCurrentTime {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

#[derive(Debug, Deserialize)]
pub struct TimeArgs {
    #[serde(default)]
    timezone: String,
}

impl Tool for GetCurrentTime {
    const NAME: &'static str = "get_current_time";
    type Error = ToolCallError;
    type Args = TimeArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "查询当前时间。timezone 可以传 IANA 时区名，例如 Asia/Shanghai。",
            json!({"timezone": {"type": "string"}}),
            &[],
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let tz_name = if args.timezone.trim().is_empty() {
            self.config.agent.timezone.clone()
        } else {
            args.timezone
        };
        let tz: Tz = tz_name.parse().unwrap_or(chrono_tz::Asia::Shanghai);
        let now = chrono::Utc::now().with_timezone(&tz);
        Ok(as_json_text(json!({
            "timezone": tz.name(),
            "date": now.format("%Y-%m-%d").to_string(),
            "time": now.format("%H:%M:%S").to_string(),
            "weekday": format!("{:?}", now.weekday()),
            "iso": now.to_rfc3339(),
        })))
    }
}

#[derive(Clone)]
pub struct GetWeather {
    service: WeatherService,
}

impl GetWeather {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            service: WeatherService::new(config),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WeatherArgs {
    #[serde(default)]
    location: Option<String>,
}

impl Tool for GetWeather {
    const NAME: &'static str = "get_weather";
    type Error = ToolCallError;
    type Args = WeatherArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "查询天气预报。location 可传 QWeather location，例如 114.1,30.52；留空按当前 IP 定位。",
            // 可选参数靠「不列进 required」表达，不要写 ["string","null"]：
            // 严格后端会拒绝数组式 type。Rust 侧是 Option<String>，模型显式
            // 发 null 时 serde 仍会映射成 None，所以这里写单一类型是安全的。
            json!({"location": {"type": "string"}}),
            &[],
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(as_json_text(
            self.service
                .get_weather(args.location.as_deref().unwrap_or(""))
                .await,
        ))
    }
}

#[derive(Clone)]
pub struct WebSearch {
    config: Arc<AppConfig>,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WebSearchArgs {
    query: String,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    time_range: String,
    #[serde(default)]
    max_results: Option<usize>,
}

impl Tool for WebSearch {
    const NAME: &'static str = "web_search";
    type Error = ToolCallError;
    type Args = WebSearchArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "联网搜索网页信息。用于用户询问新闻、最近发生的事、实时资料，或明确要求上网搜索时。topic 可传 general 或 news；time_range 可传 day、week、month、year。",
            json!({
                "query": {"type": "string"},
                "topic": {"type": "string", "enum": ["general", "news"]},
                "time_range": {"type": "string", "enum": ["", "day", "week", "month", "year"]},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 10}
            }),
            &["query"],
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let settings = &self.config.agent.web_search;
        if !settings.enabled {
            return Err(anyhow::anyhow!("agent.web_search.enabled is false").into());
        }
        if settings.api_key.trim().is_empty() {
            return Err(anyhow::anyhow!("agent.web_search.api_key is empty").into());
        }

        let query = args.query.trim();
        if query.is_empty() {
            return Err(anyhow::anyhow!("web search query is empty").into());
        }
        let topic = match args.topic.trim() {
            "news" => "news",
            _ => "general",
        };
        let max_results = args
            .max_results
            .unwrap_or(settings.max_results)
            .clamp(1, 10);
        let mut request = Map::new();
        request.insert("query".to_string(), json!(query));
        request.insert("topic".to_string(), json!(topic));
        request.insert(
            "search_depth".to_string(),
            json!(normalize_search_depth(&settings.search_depth)),
        );
        request.insert("max_results".to_string(), json!(max_results));
        request.insert("include_answer".to_string(), json!(true));
        request.insert("include_raw_content".to_string(), json!(false));
        request.insert("include_images".to_string(), json!(false));
        if matches!(args.time_range.trim(), "day" | "week" | "month" | "year") {
            request.insert(
                "time_range".to_string(),
                json!(args.time_range.trim().to_string()),
            );
        }

        let response = self
            .client
            .post(settings.tavily_url.trim())
            .bearer_auth(settings.api_key.trim())
            .json(&request)
            .timeout(timeout_duration(settings.timeout_s))
            .send()
            .await
            .context("Tavily search request failed")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Tavily search response")?;
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Tavily search request failed with status {status}: {}",
                truncate_chars(&body, 300)
            )
            .into());
        }
        let data: Value =
            serde_json::from_str(&body).context("invalid Tavily search response JSON")?;
        Ok(as_json_text(summarize_tavily_response(
            query,
            data,
            max_results,
        )))
    }
}

fn normalize_search_depth(value: &str) -> &'static str {
    match value.trim() {
        "advanced" => "advanced",
        _ => "basic",
    }
}

fn summarize_tavily_response(query: &str, data: Value, max_results: usize) -> Value {
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(max_results)
                .map(|item| {
                    json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                        "url": item.get("url").and_then(Value::as_str).unwrap_or(""),
                        "content": truncate_chars(
                            item.get("content").and_then(Value::as_str).unwrap_or(""),
                            160,
                        ),
                        "score": item.get("score"),
                        "published_date": item.get("published_date"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "query": query,
        "answer": data.get("answer").and_then(Value::as_str).unwrap_or(""),
        "results": results,
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in text.trim().chars().take(max_chars) {
        output.push(ch);
    }
    if text.trim().chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

#[derive(Clone)]
pub struct EndConversation {
    control: Arc<Mutex<AgentControl>>,
}

impl EndConversation {
    pub fn new(control: Arc<Mutex<AgentControl>>) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
pub struct EndArgs {
    #[serde(default)]
    reason: String,
}

impl Tool for EndConversation {
    const NAME: &'static str = "end_conversation";
    type Error = ToolCallError;
    type Args = EndArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "当用户表示再见、拜拜、不用帮忙了、结束对话、先这样时，结束当前音箱对话 session。",
            json!({"reason": {"type": "string"}}),
            &[],
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut control = self.control.lock().await;
        control.should_end = true;
        control.end_reason = args.reason.clone();
        Ok(as_json_text(json!({
            "should_end": true,
            "reason": if args.reason.is_empty() { "user ended the conversation" } else { &args.reason },
        })))
    }
}

macro_rules! music_tool {
    ($tool:ident, $args:ty, $name:literal, $desc:literal, $schema:expr, $required:expr, $body:expr) => {
        #[derive(Clone)]
        pub struct $tool {
            music: Arc<MusicService>,
        }

        impl $tool {
            pub fn new(music: Arc<MusicService>) -> Self {
                Self { music }
            }
        }

        impl Tool for $tool {
            const NAME: &'static str = $name;
            type Error = ToolCallError;
            type Args = $args;
            type Output = String;

            async fn definition(&self, _: String) -> ToolDefinition {
                definition(Self::NAME, $desc, $schema, $required)
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                let value = ($body)(self.music.clone(), args).await;
                Ok(as_json_text(value))
            }
        }
    };
}

#[derive(Debug, Deserialize)]
pub struct SearchMusicArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    5
}

music_tool!(
    SearchMusic,
    SearchMusicArgs,
    "search_music_library",
    "搜索音乐曲库，返回候选歌曲。用于用户问有没有某首歌、想听某歌手或需要先确认版本时。",
    json!({
        "query": {"type": "string"},
        "limit": {"type": "integer", "minimum": 1, "maximum": 20}
    }),
    &["query"],
    |music: Arc<MusicService>, args: SearchMusicArgs| async move {
        music.search(&args.query, args.limit).await
    }
);

#[derive(Debug, Deserialize)]
pub struct EmptyArgs {}

music_tool!(
    RequestMusicLoginCode,
    EmptyArgs,
    "request_music_login_code",
    "向配置的网易云手机号发送短信验证码。用户想登录网易云、授权音乐会员或获取验证码时调用。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.request_login_code().await }
);

#[derive(Debug, Deserialize)]
pub struct LoginCodeArgs {
    code: String,
}

music_tool!(
    SubmitMusicLoginCode,
    LoginCodeArgs,
    "submit_music_login_code",
    "提交用户报出的网易云短信验证码，完成登录；cookie 只在本次进程内保存，不写磁盘。",
    json!({"code": {"type": "string"}}),
    &["code"],
    |music: Arc<MusicService>, args: LoginCodeArgs| async move {
        music.submit_login_code(&args.code).await
    }
);

#[derive(Debug, Deserialize)]
pub struct PlayMusicArgs {
    #[serde(default)]
    query: String,
    #[serde(default)]
    song_id: String,
}

music_tool!(
    PlayMusicQuery,
    PlayMusicArgs,
    "play_music_query",
    "替换当前队列并开始循环播放。可以传自然语言查询，例如 周杰伦 晴天；也可以传 search_music_library 返回的 song_id。",
    json!({
        "query": {"type": "string"},
        "song_id": {"type": "string"}
    }),
    &[],
    |music: Arc<MusicService>, args: PlayMusicArgs| async move { music.play_query(&args.query, &args.song_id).await }
);

#[derive(Debug, Deserialize)]
pub struct AddMusicArgs {
    #[serde(default)]
    queries: String,
    #[serde(default)]
    song_ids: String,
}

music_tool!(
    AddMusicToQueue,
    AddMusicArgs,
    "add_music_to_play_queue",
    "把一个或多个指定歌曲加入当前播放队列，不打断正在播放的歌曲。queries 用换行或分号分隔；song_ids 也用换行或分号分隔。",
    json!({
        "queries": {"type": "string"},
        "song_ids": {"type": "string"}
    }),
    &[],
    |music: Arc<MusicService>, args: AddMusicArgs| async move { music.add_to_queue(&args.queries, &args.song_ids).await }
);

#[derive(Debug, Deserialize)]
pub struct RandomMusicArgs {
    #[serde(default = "default_random_count")]
    count: usize,
}

fn default_random_count() -> usize {
    10
}

music_tool!(
    AddRandomMusicToQueue,
    RandomMusicArgs,
    "add_random_music_to_play_queue",
    "从当前音乐库随机选择指定数量的歌曲加入播放队列，不打断正在播放的歌曲。",
    json!({"count": {"type": "integer", "minimum": 1, "maximum": 100}}),
    &[],
    |music: Arc<MusicService>, args: RandomMusicArgs| async move { music.add_random(args.count).await }
);

music_tool!(
    PlayRandomMusic,
    RandomMusicArgs,
    "play_random_music",
    "从当前音乐库随机选择歌曲，替换当前队列，并立即开始播放。用于用户说放点音乐、随便放点歌、随机播放。",
    json!({"count": {"type": "integer", "minimum": 1, "maximum": 100}}),
    &[],
    |music: Arc<MusicService>, args: RandomMusicArgs| async move { music.play_random(args.count).await }
);

music_tool!(
    StopMusicPlayback,
    EmptyArgs,
    "stop_music_playback",
    "停止当前音乐播放并清空播放队列。用于用户说停止、别放了、关掉音乐、清空队列。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.stop().await }
);

music_tool!(
    PauseMusicPlayback,
    EmptyArgs,
    "pause_music_playback",
    "暂停当前音乐播放，但保留当前歌曲和播放队列。用于用户说暂停、停一下、先暂停。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.pause().await }
);

music_tool!(
    ResumeMusicPlayback,
    EmptyArgs,
    "resume_music_playback",
    "继续播放刚才暂停的音乐。用于用户说继续播放、恢复播放、接着放。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.resume().await }
);

music_tool!(
    PlayNextMusic,
    EmptyArgs,
    "play_next_music",
    "播放队列里的下一首歌。用于用户说下一首、换一首、跳过这首。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.next().await }
);

music_tool!(
    PlayPreviousMusic,
    EmptyArgs,
    "play_previous_music",
    "播放历史里的上一首歌。用于用户说上一首、回到刚才那首。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.previous().await }
);

music_tool!(
    GetMusicStatus,
    EmptyArgs,
    "get_music_status",
    "查询当前是否正在播放音乐，以及当前歌曲信息。",
    json!({}),
    &[],
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.status().await }
);

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct ToolOutput(Value);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::Device;
    use rig_core::tool::Tool;

    /// 用示例配置构造，避免依赖真实 agent.yaml（可能含密钥）。
    fn example_config() -> Arc<AppConfig> {
        Arc::new(
            serde_yaml::from_str(include_str!("../agent.example.yaml")).expect("示例配置应可解析"),
        )
    }

    /// 把 agent.rs 里注册的全部工具定义收集起来，供下面三条断言共用。
    ///
    /// 工具列表和 AgentRuntime::new 里注册的一致；新增工具时这里也要加一行，
    /// 否则 schema 回归检查会漏掉它。
    async fn all_definitions() -> Vec<ToolDefinition> {
        let config = example_config();
        let music = Arc::new(
            MusicService::new(config.clone(), Device::new(config.device.clone()))
                .expect("music service"),
        );
        let control = Arc::new(Mutex::new(AgentControl::default()));
        vec![
            GetCurrentTime::new(config.clone())
                .definition(String::new())
                .await,
            GetWeather::new(config.clone())
                .definition(String::new())
                .await,
            WebSearch::new(config.clone())
                .definition(String::new())
                .await,
            EndConversation::new(control)
                .definition(String::new())
                .await,
            SearchMusic::new(music.clone())
                .definition(String::new())
                .await,
            RequestMusicLoginCode::new(music.clone())
                .definition(String::new())
                .await,
            SubmitMusicLoginCode::new(music.clone())
                .definition(String::new())
                .await,
            PlayMusicQuery::new(music.clone())
                .definition(String::new())
                .await,
            AddMusicToQueue::new(music.clone())
                .definition(String::new())
                .await,
            AddRandomMusicToQueue::new(music.clone())
                .definition(String::new())
                .await,
            PlayRandomMusic::new(music.clone())
                .definition(String::new())
                .await,
            StopMusicPlayback::new(music.clone())
                .definition(String::new())
                .await,
            PauseMusicPlayback::new(music.clone())
                .definition(String::new())
                .await,
            ResumeMusicPlayback::new(music.clone())
                .definition(String::new())
                .await,
            PlayNextMusic::new(music.clone())
                .definition(String::new())
                .await,
            PlayPreviousMusic::new(music.clone())
                .definition(String::new())
                .await,
            GetMusicStatus::new(music).definition(String::new()).await,
        ]
    }

    fn required_of(defs: &[ToolDefinition], name: &str) -> Vec<String> {
        let def = defs
            .iter()
            .find(|def| def.name == name)
            .unwrap_or_else(|| panic!("缺少工具定义: {name}"));
        def.parameters["required"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: required 不是数组"))
            .iter()
            .map(|value| value.as_str().expect("required 项应为字符串").to_string())
            .collect()
    }

    /// 回归：换模型/换中转代理曾经因为 schema 不规范直接报错或静默失效。
    /// 每个工具都必须带 required、属性 type 必须是字符串、required ⊆ properties。
    #[tokio::test]
    async fn every_tool_definition_is_strict_backend_compatible() {
        let defs = all_definitions().await;
        assert_eq!(
            defs.len(),
            17,
            "工具数量变了，请同步 AgentRuntime 的注册列表"
        );
        for def in &defs {
            validate_definition(def).unwrap_or_else(|err| panic!("{err}"));
        }
    }

    #[tokio::test]
    async fn required_lists_exactly_the_mandatory_arguments() {
        let defs = all_definitions().await;
        assert_eq!(required_of(&defs, "web_search"), vec!["query"]);
        assert_eq!(required_of(&defs, "search_music_library"), vec!["query"]);
        assert_eq!(required_of(&defs, "submit_music_login_code"), vec!["code"]);
        // 下面这些工具有可选参数或无参数：required 必须是空数组，不能缺字段。
        for name in [
            "get_current_time",
            "get_weather",
            "end_conversation",
            "play_music_query",
            "add_music_to_play_queue",
            "add_random_music_to_play_queue",
            "play_random_music",
            "stop_music_playback",
            "pause_music_playback",
            "resume_music_playback",
            "play_next_music",
            "play_previous_music",
            "get_music_status",
            "request_music_login_code",
        ] {
            assert!(
                required_of(&defs, name).is_empty(),
                "{name}: 所有参数都是可选的，required 应为空数组"
            );
        }
    }

    #[tokio::test]
    async fn optional_arguments_are_optional_not_nullable() {
        let defs = all_definitions().await;
        for def in &defs {
            let properties = def.parameters["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{}: properties 不是对象", def.name));
            for (prop, schema) in properties {
                assert!(
                    schema["type"].is_string(),
                    "{}: 属性 {prop} 的 type 写成了数组，严格后端会拒绝",
                    def.name
                );
            }
        }
    }
}
