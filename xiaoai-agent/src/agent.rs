use std::sync::Arc;

use anyhow::Context;
use rig_core::client::CompletionClient;
use rig_core::completion::{Message, Prompt};
use rig_core::providers::{anthropic, openai};
use rig_core::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use crate::config::{timeout_duration, AppConfig, ConfigHandle, LlmProtocol, LlmThinkingMode};
use crate::device::Device;
use crate::mcp::McpConnections;
use crate::music::MusicService;
use crate::tools::*;

pub const SPEAKER_AGENT_INSTRUCTIONS: &str = concat!(
    "你是小爱同学，一个运行在小爱音箱上的中文语音助手。",
    "你运行在小爱音箱端侧；端侧程序负责语音唤醒、云端 ASR、TTS 和播放控制。",
    "你负责根据用户文本请求完成任务并返回适合朗读的文本。",
    "直接给最终答案，不要输出思考过程。",
    "回答要自然、简短、适合直接朗读。",
    "通常只说一到两句话，除非用户明确要求详细解释。",
    "不要使用 Markdown，不要列太长的清单。",
    "当用户询问当前时间、日期、天气或预报时，必须调用工具获得实时信息。",
    "当用户询问新闻、最近发生的事、实时资料，或明确要求上网搜索时，必须调用 web_search 工具。",
    "基于搜索结果回答时，要简短说明信息来源；搜索结果不足或互相矛盾时，要说无法确认。",
    "当用户要求控制智能家居设备时，必须调用 Home Assistant 或 MCP 工具；",
    "控制智能家居前优先调用 ha_search，根据自然名称和区域查找目标；不要仅凭设备类别猜测 Home Assistant domain，风扇、灯等设备也可能属于 switch。",
    "ha_search 返回目标后必须使用完整 entity_id；查询当前状态调用 ha_get_state，控制设备调用 ha_call_service，多设备操作可调用 ha_bulk_control。",
    "调用 ha_call_service 时使用 entity_id、实际 domain 和明确 service，不要把 friendly_name 当作 entity_id。",
    "如果目标 state 是 unavailable 或 unknown，要告诉用户设备当前不可用，不要继续假装成功。",
    "只有工具返回明确成功时才可以说操作完成。",
    "如果工具返回错误、没有匹配设备或结果不明确，要说明无法确认，不要假装成功。",
    "当用户要求播放、搜索、停止音乐或询问当前播放内容时，必须调用音乐工具。",
    "用户只说播放某首歌或某位歌手时，优先调用 play_music_query，",
    "这会替换当前队列并开始播放；用户说放点音乐、随便放点歌、随机播放时，调用 play_random_music；",
    "用户说加入、添加到队列、稍后播放时，调用 add_music_to_play_queue；",
    "用户明确说随机加几首到队列时，调用 add_random_music_to_play_queue。",
    "用户说下一首、上一首、暂停、继续播放时，调用对应的音乐播放控制工具。",
    "如果候选不明确或播放失败，再简短追问或说明原因。",
    "当音乐工具提示需要网易云验证码登录，或用户要求登录网易云时，",
    "先调用 request_music_login_code 发送验证码；用户报出短信数字后，",
    "调用 submit_music_login_code 完成登录；登录 cookie 只在本次进程内保存。",
    "当用户表达再见、拜拜、不用了、结束对话、先这样等结束意图时，",
    "必须调用 end_conversation 工具，然后用一句简短自然的告别作为最终回复。",
    "如果工具返回错误或信息不足，要坦率说明无法确认。"
);

/// 关闭工具时使用的 preamble。
/// 原来的 preamble 会命令模型"必须调用工具"，而 tools_enabled=false 时那些工具
/// 根本不存在，模型会谎称"正在为你播放"或直接说"我无法查询"，体验更差。
pub const SPEAKER_AGENT_INSTRUCTIONS_NO_TOOLS: &str = concat!(
    "你是小爱同学，一个运行在小爱音箱上的中文语音助手。",
    "你运行在小爱音箱端侧；端侧程序负责语音唤醒、云端 ASR、TTS 和播放控制。",
    "你负责根据用户文本请求完成任务并返回适合朗读的文本。",
    "直接给最终答案，不要输出思考过程。",
    "回答要自然、简短、适合直接朗读。",
    "通常只说一到两句话，除非用户明确要求详细解释。",
    "不要使用 Markdown，不要列太长的清单。",
    "【重要】当前没有任何工具可用：你无法播放音乐、无法控制智能家居、无法联网搜索、无法查询实时数据。",
    "遇到这类请求，直接简短说明你现在做不到；不要提『管理员』『工具开关』之类内部概念；",
    "绝对不要声称你已经执行了某个动作。",
    "用户消息开头会给出当前时间；用户问时间或日期时直接使用它。",
    "对于天气、新闻、股价等需要实时数据的请求，说明你拿不到实时数据。",
    "如果用户表达再见、拜拜、不用了、先这样等结束意图，直接用一句简短告别回复。"
);

#[derive(Debug, Clone)]
pub struct AgentTurnResult {
    pub text: String,
    pub should_end: bool,
    pub end_reason: String,
}

#[derive(Default)]
pub struct AgentControl {
    pub should_end: bool,
    pub end_reason: String,
}

pub struct AgentRuntime {
    /// 可整体替换的配置句柄（SIGHUP 热加载）。每轮取一次快照，所以一轮对话
    /// 全程用同一份配置，不会中途换模型。
    config: ConfigHandle,
    /// 重建工具服务器时需要（音乐工具要它）。
    music: Arc<MusicService>,
    /// 工具服务器与 MCP 连接都可以在 `reload` 里整体替换，所以放 RwLock 后。
    tool_server: RwLock<ToolServerHandle>,
    mcp: RwLock<McpConnections>,
    history: Mutex<Vec<HistoryTurn>>,
    control: Arc<Mutex<AgentControl>>,
}

/// 按一份配置快照构建内置工具服务器。
///
/// 从 `new` 里提出来是为了 `reload` 能复用：工具结构体各自持有 `Arc<AppConfig>`
/// 快照，所以「配置变了」只能在重建工具时生效，不能靠改字段。
fn build_tool_server(
    config: Arc<AppConfig>,
    music: Arc<MusicService>,
    control: Arc<Mutex<AgentControl>>,
) -> ToolServerHandle {
    let mut tool_server = ToolServer::new();
    if config.agent.tools_enabled {
        // 新增/删除工具时，必须同步 tools.rs 测试里的 all_definitions()：
        // 那边是按这份列表手工抄的，数量断言靠它兜住。
        tool_server = tool_server
            .tool(GetCurrentTime::new(config.clone()))
            .tool(GetWeather::new(config.clone()))
            .tool(WebSearch::new(config.clone()))
            .tool(EndConversation::new(control))
            .tool(SearchMusic::new(music.clone()))
            .tool(RequestMusicLoginCode::new(music.clone()))
            .tool(SubmitMusicLoginCode::new(music.clone()))
            .tool(PlayMusicQuery::new(music.clone()))
            .tool(AddMusicToQueue::new(music.clone()))
            .tool(AddRandomMusicToQueue::new(music.clone()))
            .tool(PlayRandomMusic::new(music.clone()))
            .tool(StopMusicPlayback::new(music.clone()))
            .tool(PauseMusicPlayback::new(music.clone()))
            .tool(ResumeMusicPlayback::new(music.clone()))
            .tool(PlayNextMusic::new(music.clone()))
            .tool(PlayPreviousMusic::new(music.clone()))
            .tool(GetMusicStatus::new(music));
    }
    tool_server.run()
}

/// 一轮问答的结构化历史。
///
/// 只存「用户说了什么 / 助手答了什么」两条文本，发请求时再转换成 rig 的
/// `Message`。不直接存 `Message` 是为了让截断策略（轮数 + 字符数）能作用在
/// 可读文本上，而不是在 `OneOrMany<Content>` 里翻找。
#[derive(Debug, Clone, PartialEq, Eq)]
struct HistoryTurn {
    user: String,
    assistant: String,
}

impl HistoryTurn {
    fn chars(&self) -> usize {
        self.user.chars().count() + self.assistant.chars().count()
    }
}

/// 历史最多保留 10 轮问答；同时限制总字符数，避免长回答把上下文吃掉。
/// 两者取先到者，超出的从最旧的一轮开始丢。
const HISTORY_MAX_TURNS: usize = 10;
const HISTORY_MAX_CHARS: usize = 4000;

/// 按「轮数 + 字符数」双上限裁剪历史。
///
/// 字符数裁剪至少保留一轮：一轮就超预算时，宁可留这一轮，也不要让模型在
/// 连续追问里彻底失忆（「它刚才说的那个」这类指代会当场断掉）。
fn trim_history(turns: &mut Vec<HistoryTurn>) {
    while turns.len() > HISTORY_MAX_TURNS {
        turns.remove(0);
    }
    let mut total: usize = turns.iter().map(HistoryTurn::chars).sum();
    while total > HISTORY_MAX_CHARS && turns.len() > 1 {
        total -= turns[0].chars();
        turns.remove(0);
    }
}

/// 把历史展开成 rig 能直接发出去的消息序列（user / assistant 交替）。
fn history_to_messages(turns: &[HistoryTurn]) -> Vec<Message> {
    let mut messages = Vec::with_capacity(turns.len() * 2);
    for turn in turns {
        messages.push(Message::user(turn.user.clone()));
        messages.push(Message::assistant(turn.assistant.clone()));
    }
    messages
}

impl AgentRuntime {
    pub async fn new(
        config: ConfigHandle,
        device: Device,
        music: Arc<MusicService>,
    ) -> anyhow::Result<Self> {
        let control = Arc::new(Mutex::new(AgentControl::default()));
        let snapshot = config.get().await;
        let tool_server = build_tool_server(snapshot.clone(), music.clone(), control.clone());

        let mcp = if snapshot.agent.tools_enabled {
            McpConnections::connect(snapshot.clone(), tool_server.clone()).await
        } else {
            McpConnections::empty()
        };
        if let Err(err) = device.stop_audio().await {
            warn!("initial stop_audio failed, continuing: {err:?}");
        }
        info!(
            protocol = ?snapshot.llm.protocol,
            thinking = ?snapshot.llm.thinking.mode,
            model = %snapshot.llm.model,
            "LLM client configured"
        );

        Ok(Self {
            config,
            music,
            tool_server: RwLock::new(tool_server),
            mcp: RwLock::new(mcp),
            history: Mutex::new(Vec::new()),
            control,
        })
    }

    /// 配置热加载：换配置快照，并重建工具服务器与 MCP 连接。
    ///
    /// 必须重建工具：17 个内置工具各自持有 `Arc<AppConfig>` 快照（见
    /// `build_tool_server`），不重建就只有 agent 主体换配置、工具还用旧的。
    /// 而 MCP 工具也是注册在同一个 `ToolServer` 上的，所以换服务器必须连带重连，
    /// 否则新服务器里一个 MCP 工具都没有。
    ///
    /// 不重建的是历史与 `control`：热加载不该把用户正在进行的对话抹掉。
    pub async fn reload(&self) {
        let snapshot = self.config.get().await;
        let tool_server =
            build_tool_server(snapshot.clone(), self.music.clone(), self.control.clone());
        let mcp = if snapshot.agent.tools_enabled {
            McpConnections::connect(snapshot.clone(), tool_server.clone()).await
        } else {
            McpConnections::empty()
        };

        *self.tool_server.write().await = tool_server;
        // 旧连接一定要 drop：McpConnections 的 Drop 会 abort 各个 supervisor，
        // 不 drop 旧连接会一直在后台重连。
        let previous = std::mem::replace(&mut *self.mcp.write().await, mcp);
        drop(previous);

        info!(
            tools_enabled = snapshot.agent.tools_enabled,
            model = %snapshot.llm.model,
            "agent 已按新配置重建工具服务器与 MCP 连接"
        );
    }

    pub async fn run_turn(&self, message: &str) -> anyhow::Result<AgentTurnResult> {
        // 一轮对话只取一次配置快照：中途发生 SIGHUP 也不会「前半句旧模型、后半句新模型」。
        let config = self.config.get().await;
        {
            let mut control = self.control.lock().await;
            control.should_end = false;
            control.end_reason.clear();
        }

        // 关闭工具时（例如后端代理不支持 tools API），模型拿不到当前时间，
        // 问"现在几点"会答"不方便查询"。默认只在没有工具时注入；
        // inject_current_time=true 可强制注入（工具存在时也能省一次工具调用）。
        let inject_time = config.agent.inject_current_time || !config.agent.tools_enabled;
        let message_with_time = if inject_time {
            format!("{}{}", self.now_prefix(&config.agent.timezone), message)
        } else {
            message.to_string()
        };

        // 历史按结构化消息传给模型，不再拼成「最近对话历史：…」那一段文字。
        let history = self.history_snapshot().await;
        let text = self.prompt_once(message_with_time, history).await?;

        let control = self.control.lock().await;
        self.push_history(message, &text).await;
        Ok(AgentTurnResult {
            text,
            should_end: control.should_end,
            end_reason: control.end_reason.clone(),
        })
    }

    /// 按是否注册工具选择 preamble，避免"命令模型调用不存在的工具"。
    ///
    /// 参数化而不是读 `self.config`：热加载后工具可能刚被重建，preamble 必须跟
    /// 同一份快照走，否则会出现「preamble 说没工具、工具其实在」。
    fn preamble(tools_enabled: bool) -> &'static str {
        if tools_enabled {
            SPEAKER_AGENT_INSTRUCTIONS
        } else {
            SPEAKER_AGENT_INSTRUCTIONS_NO_TOOLS
        }
    }

    /// 把当前时间写进用户消息，供关闭工具时的模型使用。
    fn now_prefix(&self, timezone: &str) -> String {
        use chrono::Utc;
        let tz: chrono_tz::Tz = timezone.parse().unwrap_or(chrono_tz::Asia::Shanghai);
        let now = Utc::now().with_timezone(&tz);
        let weekday = match now.format("%u").to_string().as_str() {
            "1" => "一",
            "2" => "二",
            "3" => "三",
            "4" => "四",
            "5" => "五",
            "6" => "六",
            _ => "日",
        };
        format!(
            "【当前时间：{} 星期{}】\n",
            now.format("%Y年%m月%d日 %H:%M"),
            weekday
        )
    }

    async fn prompt_once(&self, prompt: String, history: Vec<Message>) -> anyhow::Result<String> {
        let config = self.config.get().await;
        let http_client = reqwest::Client::builder()
            .timeout(timeout_duration(config.llm.timeout_s))
            .build()
            .context("failed to create LLM HTTP client")?;

        match config.llm.protocol {
            LlmProtocol::OpenAiChat => self.prompt_openai(http_client, prompt, history).await,
            LlmProtocol::Anthropic => self.prompt_anthropic(http_client, prompt, history).await,
        }
    }

    async fn prompt_openai(
        &self,
        http_client: reqwest::Client,
        prompt: String,
        history: Vec<Message>,
    ) -> anyhow::Result<String> {
        let config = self.config.get().await;
        let tool_server = self.tool_server.read().await.clone();
        let client = openai::Client::builder()
            .api_key(config.llm.api_key.as_str())
            .base_url(config.llm.base_url.as_str())
            .http_client(http_client)
            .build()
            .context("failed to create OpenAI-compatible Rig client")?
            .completions_api();

        let agent = client
            .agent(config.llm.model.as_str())
            .preamble(Self::preamble(config.agent.tools_enabled))
            .temperature(config.llm.temperature)
            .max_tokens(config.llm.max_tokens)
            .default_max_turns(3)
            .additional_params(openai_thinking_params(&config.llm.thinking.mode))
            .tool_server_handle(tool_server.clone())
            .build();

        // 历史以结构化消息挂到请求上；空历史不必多挂一层。
        let mut request = agent.prompt(prompt);
        if !history.is_empty() {
            request = request.with_history(history);
        }
        Ok(request.await?.trim().to_string())
    }

    async fn prompt_anthropic(
        &self,
        http_client: reqwest::Client,
        prompt: String,
        history: Vec<Message>,
    ) -> anyhow::Result<String> {
        let config = self.config.get().await;
        let tool_server = self.tool_server.read().await.clone();
        // 老版 Claude 要求 1024 <= budget_tokens < max_tokens；默认配置是
        // budget 1024 / max_tokens 300，直接开 thinking 会每轮失败并播"大模型服务遇到问题"。
        // 这里改成自动收敛并告警，不让纯配置陷阱把音箱弄哑。
        let mut thinking_params =
            anthropic_thinking_params(&config.llm.thinking.mode, config.llm.thinking.budget_tokens);
        if config.llm.thinking.mode == LlmThinkingMode::Enabled {
            const MIN_BUDGET: u64 = 1024;
            let max = config.llm.max_tokens;
            let wanted = config.llm.thinking.budget_tokens;
            if wanted < MIN_BUDGET || wanted >= max {
                // 合法区间是 [1024, max_tokens)。区间为空说明 max_tokens 太小，
                // 静默给一个非法值只会换来 400，这里直接报出人话错误。
                anyhow::ensure!(
                    max > MIN_BUDGET,
                    "llm.thinking.mode=enabled 需要 llm.thinking.budget_tokens 落在                      [1024, llm.max_tokens) 区间，但 llm.max_tokens={max} 太小；                     请把 llm.max_tokens 调到 2048 以上（或把 thinking.mode 设回 auto）"
                );
                let clamped = wanted.clamp(MIN_BUDGET, max - 1);
                warn!(
                    wanted,
                    max_tokens = max,
                    clamped,
                    "llm.thinking.budget_tokens out of range, clamped into [1024, max_tokens)"
                );
                thinking_params = anthropic_thinking_params(&LlmThinkingMode::Enabled, clamped);
            }
        }

        let client = anthropic::Client::builder()
            .api_key(config.llm.api_key.as_str())
            .base_url(config.llm.base_url.as_str())
            .http_client(http_client)
            .anthropic_version("2023-06-01")
            .build()
            .context("failed to create Anthropic-compatible Rig client")?;

        let mut builder = client
            .agent(config.llm.model.as_str())
            .preamble(Self::preamble(config.agent.tools_enabled))
            .max_tokens(config.llm.max_tokens)
            .default_max_turns(3)
            .additional_params(thinking_params);
        if config.llm.thinking.mode != LlmThinkingMode::Enabled {
            builder = builder.temperature(config.llm.temperature);
        }
        let agent = builder.tool_server_handle(tool_server.clone()).build();

        // 历史以结构化消息挂到请求上；空历史不必多挂一层。
        let mut request = agent.prompt(prompt);
        if !history.is_empty() {
            request = request.with_history(history);
        }
        Ok(request.await?.trim().to_string())
    }

    pub async fn reset_session(&self, reason: &str) {
        self.history.lock().await.clear();
        let mut control = self.control.lock().await;
        control.should_end = false;
        control.end_reason = reason.to_string();
    }

    /// 测试用：读回上一次 `reset_session` 写入的理由。
    ///
    /// 生产代码不需要这个读口，但中断清理的测试需要——`reset_session` 是
    /// 「这条路径真的走过收尾」的唯一直接证据，否则只能靠日志断言。
    #[cfg(test)]
    pub async fn reset_reason_for_test(&self) -> String {
        self.control.lock().await.end_reason.clone()
    }

    /// 取一份历史快照，转成 rig 的消息序列。
    ///
    /// 拿的是克隆而不是锁本身：请求期间不能一直占着 `history`，否则同一轮里
    /// 的 `push_history` 会等到请求结束才写进去。
    async fn history_snapshot(&self) -> Vec<Message> {
        let turns = self.history.lock().await;
        history_to_messages(&turns)
    }

    async fn push_history(&self, user: &str, assistant: &str) {
        let mut history = self.history.lock().await;
        history.push(HistoryTurn {
            user: user.to_string(),
            assistant: assistant.to_string(),
        });
        trim_history(&mut history);
    }
}

fn openai_thinking_params(mode: &LlmThinkingMode) -> Value {
    match mode {
        // Auto / Disabled 都不发参数：reasoning_effort、enable_thinking、
        // chat_template_kwargs 都是特定厂商（OpenAI 推理模型 / Qwen 系）的方言，
        // 换成严格代理或其它模型时会被 400 拒绝。确实需要时再按目标模型单独加回。
        LlmThinkingMode::Auto | LlmThinkingMode::Disabled => json!({}),
        LlmThinkingMode::Enabled => json!({ "enable_thinking": true }),
    }
}

fn anthropic_thinking_params(mode: &LlmThinkingMode, budget_tokens: u64) -> Value {
    match mode {
        // 新版 Claude 已移除 budget_tokens，连 {"type":"disabled"} 都会 400；
        // 默认什么都不发，代际通吃。
        LlmThinkingMode::Auto | LlmThinkingMode::Disabled => json!({}),
        LlmThinkingMode::Enabled => {
            json!({"thinking": {"type": "enabled", "budget_tokens": budget_tokens}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        anthropic_thinking_params, history_to_messages, openai_thinking_params, trim_history,
        HistoryTurn, HISTORY_MAX_CHARS, HISTORY_MAX_TURNS,
    };
    use crate::config::LlmThinkingMode;
    use rig_core::completion::Message;
    use serde_json::json;

    fn turn(user: &str, assistant: &str) -> HistoryTurn {
        HistoryTurn {
            user: user.to_string(),
            assistant: assistant.to_string(),
        }
    }

    #[test]
    fn history_is_sent_as_structured_messages() {
        // 回归：历史曾经被拼成「最近对话历史：\n用户：…\n助手：…」一整段文字塞进
        // 用户消息，导致重复计费、模型把旧话当新指令。
        let turns = vec![
            turn("今天天气怎么样", "晴，22 度"),
            turn("那明天呢", "明天有小雨"),
        ];
        let messages = history_to_messages(&turns);
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[0], Message::User { .. }));
        assert!(matches!(&messages[1], Message::Assistant { .. }));
        assert!(matches!(&messages[2], Message::User { .. }));
        assert!(matches!(&messages[3], Message::Assistant { .. }));
    }

    #[test]
    fn trim_history_drops_oldest_turns_beyond_the_turn_limit() {
        let mut turns = (0..HISTORY_MAX_TURNS + 3)
            .map(|index| turn(&format!("问{index}"), &format!("答{index}")))
            .collect::<Vec<_>>();
        trim_history(&mut turns);
        assert_eq!(turns.len(), HISTORY_MAX_TURNS);
        // 留下的必须是最近的，最旧的三轮要丢掉
        assert_eq!(turns[0].user, "问3");
        assert_eq!(
            turns.last().map(|turn| turn.user.clone()),
            Some(format!("问{}", HISTORY_MAX_TURNS + 2))
        );
    }

    #[test]
    fn trim_history_drops_oldest_turns_beyond_the_char_budget() {
        // 每轮 1000 字符，5 轮就超过 4000 的预算 → 只应留 4 轮
        let mut turns = (0..5)
            .map(|index| {
                turn(
                    &format!("{index}").repeat(500),
                    &format!("{index}").repeat(500),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(turns.iter().map(HistoryTurn::chars).sum::<usize>(), 5000);
        trim_history(&mut turns);
        assert_eq!(turns.len(), 4);
        assert!(turns.iter().map(HistoryTurn::chars).sum::<usize>() <= HISTORY_MAX_CHARS);
        assert!(
            turns[0].user.starts_with('1'),
            "最旧的一轮（以 0 开头）应被丢弃"
        );
    }

    #[test]
    fn trim_history_keeps_a_single_oversized_turn() {
        // 一轮就超预算时不能把历史清空到「连这轮都没有」——宁可留一条，
        // 也不要让模型在连续追问里完全失忆。
        let mut turns = vec![turn(&"问".repeat(3000), &"答".repeat(3000))];
        trim_history(&mut turns);
        assert_eq!(turns.len(), 1);
    }

    #[test]
    fn disables_thinking_for_anthropic_messages() {
        // 默认不发任何 thinking 参数：新版 Claude 连 {"type":"disabled"} 都会 400
        assert_eq!(
            anthropic_thinking_params(&LlmThinkingMode::Disabled, 1024),
            json!({})
        );
        assert_eq!(
            anthropic_thinking_params(&LlmThinkingMode::Auto, 1024),
            json!({})
        );
    }

    #[test]
    fn enables_thinking_with_budget_for_anthropic_messages() {
        assert_eq!(
            anthropic_thinking_params(&LlmThinkingMode::Enabled, 2048),
            json!({"thinking": {"type": "enabled", "budget_tokens": 2048}})
        );
    }

    #[test]
    fn omits_vendor_specific_params_by_default() {
        // reasoning_effort / chat_template_kwargs 是厂商方言，默认不能发
        assert_eq!(
            openai_thinking_params(&LlmThinkingMode::Disabled),
            json!({})
        );
        assert_eq!(openai_thinking_params(&LlmThinkingMode::Auto), json!({}));
        assert_eq!(
            openai_thinking_params(&LlmThinkingMode::Enabled),
            json!({"enable_thinking": true})
        );
    }
}
