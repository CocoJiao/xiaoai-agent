use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tracing::info;

use crate::config::ServerConfig;
use crate::speech::{SocketOwnership, SpeechLiveness};

/// 控制面能看到的运行时事实。
///
/// 刻意只放「客观可核对」的东西（socket 归属、收包时间、重启次数、进程年龄），
/// 不放「我认为我在正常工作」这类自评——`/health` 的价值就在于它比
/// 「进程还在吗」细一级。
pub struct ControlState {
    pub token: String,
    pub socket: SocketOwnership,
    pub liveness: SpeechLiveness,
    pub started_at: Instant,
    pub config_path: Option<PathBuf>,
}

/// 启动控制面。**关闭时返回 `Ok(None)`，不 bind 任何 socket**——issue #9 的
/// 验收条件之一就是「关闭时零监听」，所以这里不能先 bind 再判断。
pub async fn spawn(
    config: &ServerConfig,
    state: Arc<ControlState>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    if !config.enabled {
        info!("本地控制面未启用（server.enabled=false），不监听任何端口");
        return Ok(None);
    }

    // 决定不启动，就别先把端口占了：检查放在 bind 之前。
    if config.token.trim().is_empty() && !is_loopback(&config.host) {
        return Err(anyhow::anyhow!(
            "控制面绑在非回环地址（{}）却没有配置 token：拒绝启动。请设 server.token，或把 host 改回 127.0.0.1",
            config.host
        ));
    }

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|err| anyhow::anyhow!("绑定控制面 {addr} 失败: {err}"))?;
    let local = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or(addr);

    info!(
        listen = %local,
        auth = !state.token.trim().is_empty(),
        "本地控制面已启动"
    );

    let app = build_router(state);

    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "控制面退出");
        }
    });
    Ok(Some(handle))
}

fn build_router(state: Arc<ControlState>) -> Router {
    // 鉴权写成 layer，而不是让每个 handler 自己记得调 authorize：
    // #9 下一步就是 /stop、/play/*，新增端点的人只要漏抄那几行就是一个
    // **无鉴权的控制端点**，而路由结构不会提醒他。现在默认受保护，
    // 要公开必须显式挂到下面这条 Router 上。
    let api = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));

    Router::new()
        .route("/", get(panel))
        .merge(api)
        .with_state(state)
}

/// 统一的 Bearer 校验。**所有非公开路由都过这里**。
///
/// 这套设计成立的前提只有一条：跨站请求带不上自定义头。所以任何会改变状态的
/// 端点都必须要求 `Authorization`，且不该用 GET（`<img>`/`<a>`/`<form>` 都能触发
/// GET，但都带不上 Bearer——GET + 不鉴权 = CSRF 跳板）。
async fn require_token(
    State(state): State<Arc<ControlState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match authorize(&state.token, request.headers()) {
        Ok(()) => next.run(request).await,
        Err(status) => (status, Json(json!({ "error": "unauthorized" }))).into_response(),
    }
}

/// 控制台页面。**故意不鉴权**：它只是一张静态 HTML，本身不含任何设备信息，
/// token 由页面里输入一次、存 localStorage，之后的请求才带 Authorization。
/// 这样运维不用记 curl；同时也不会把 token 写进 URL（历史、日志都会留痕）。
async fn panel() -> Response {
    // 面板只有内联样式与脚本：
    // - `default-src 'none'` + `connect-src 'self'`：页面永远不加载外部资源（和
    //   「无 CDN」的目标一致），请求只能打到本机；
    // - `frame-ancestors 'none'`：不能被嵌进 iframe —— 第二步加了「停止播放」
    //   这类按钮后，点击劫持就成立了。
    let csp = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';                connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
    (
        [
            (axum::http::header::CONTENT_SECURITY_POLICY, csp),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        Html(PANEL_HTML),
    )
        .into_response()
}

/// 单文件控制台：无外部依赖（设备上没有 CDN 也要能开）。
/// 放在独立的 `.html` 里而不是 raw string：编辑器有高亮、diff 只显示 HTML、
/// 也不会因为 JS 里出现 `"#` 而静默截断。
const PANEL_HTML: &str = include_str!("panel.html");

async fn health(State(state): State<Arc<ControlState>>) -> Response {
    let (status, body) = health_response(&state);
    (status, no_store(), Json(body)).into_response()
}

async fn status(State(state): State<Arc<ControlState>>) -> Response {
    (StatusCode::OK, no_store(), Json(status_body(&state))).into_response()
}

/// 监控端点被缓存没有任何好处（`fetch` 默认会走 HTTP 缓存），显式禁掉。
fn no_store() -> [(axum::http::header::HeaderName, &'static str); 2] {
    [
        (axum::http::header::CACHE_CONTROL, "no-store"),
        (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    ]
}

/// 探活判据：**只看一件不会撒谎的事实——speech socket 还是不是我们的**。
///
/// 让位、被原生抢回去、socket 被 unlink 重建，都会让这条从 true 变成 false，
/// 而进程本身还活得好好的。所以「进程在」不能当健康，inode 归属才能当健康。
/// 收包年龄只作为诊断字段返回，不参与判定：没人说话时长期没有音频包是正常的。
fn health_response(state: &ControlState) -> (StatusCode, Value) {
    let owned = state.socket.still_owned();
    let body = json!({
        "status": if owned { "ok" } else { "socket_not_owned" },
        "uptime_s": state.started_at.elapsed().as_secs(),
        "socket": {
            "path": state.socket.path().display().to_string(),
            "owned": owned,
        },
        "speech": speech_json(state),
    });
    let code = if owned {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, body)
}

fn status_body(state: &ControlState) -> Value {
    json!({
        "pid": std::process::id(),
        "uptime_s": state.started_at.elapsed().as_secs(),
        "config": state.config_path.as_ref().map(|p| p.display().to_string()),
        "socket": {
            "path": state.socket.path().display().to_string(),
            "owned": state.socket.still_owned(),
        },
        "speech": speech_json(state),
    })
}

fn speech_json(state: &ControlState) -> Value {
    json!({
        "last_packet_age_s": state.liveness.last_packet_age().map(|age| age.as_secs()),
        "service_restarts": state.liveness.restarts(),
    })
}

/// Bearer 鉴权。空 token = 不鉴权（配置默认只绑回环，此时风险可控）。
fn authorize(token: &str, headers: &HeaderMap) -> Result<(), StatusCode> {
    if token.trim().is_empty() {
        return Ok(());
    }
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let expected = format!("Bearer {token}");
    if constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// 定长比较：控制面在局域网里，token 泄露的代价是「任何人都能让音箱说话」，
/// 不值得为省十行代码留一个理论上可行的时序侧信道。
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// 回环判定用 `IpAddr` 解析，而不是字面量匹配：`127.0.0.2`、`[::1]`
/// 这类写法都该算回环，字面量匹配会把它们误判成「对外暴露」。
fn is_loopback(host: &str) -> bool {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unattributed() -> SocketOwnership {
        SocketOwnership::unattributed(temp_sock("unattributed"))
    }
    use std::sync::atomic::{AtomicU32, AtomicU64};
    use std::sync::Arc;

    fn state_with_ownership(token: &str, socket: SocketOwnership) -> Arc<ControlState> {
        let liveness = SpeechLiveness {
            last_packet_ms: Arc::new(AtomicU64::new(crate::speech::now_ms())),
            restarts: Arc::new(AtomicU32::new(0)),
        };
        Arc::new(ControlState {
            token: token.to_string(),
            socket,
            liveness,
            started_at: Instant::now(),
            config_path: None,
        })
    }

    /// 临时占位文件要自己收：每次跑测试都留一个 `xiaoai-health-*` 的话，开发机会越攒越多。
    fn cleanup(state: &Arc<ControlState>) {
        let _ = std::fs::remove_file(state.socket.path());
    }

    fn temp_sock(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("xiaoai-health-{}-{tag}.usock", std::process::id()))
    }

    /// 路径存在、inode 就是我们绑的那个。
    fn still_owned_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let path = temp_sock(tag);
        std::fs::write(&path, b"agent").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        SocketOwnership::for_test(path, inode)
    }

    /// 路径存在，但 inode 已经换成原生的（复刻 launcher 的 `rm` + `mv`）。
    fn taken_over_socket(tag: &str) -> SocketOwnership {
        use std::os::unix::fs::MetadataExt;
        let live = temp_sock(tag);
        let native = live.with_extension("native.usock");
        std::fs::write(&live, b"agent").unwrap();
        std::fs::write(&native, b"native").unwrap();
        let mine = std::fs::metadata(&live).unwrap().ino();
        std::fs::remove_file(&live).unwrap();
        std::fs::rename(&native, &live).unwrap();
        SocketOwnership::for_test(live, mine)
    }

    fn headers_with(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    /// 关闭时绝不 bind：这是 issue #9 的验收条件（netstat 里不该出现这个端口）。
    #[tokio::test]
    async fn disabled_server_binds_nothing() {
        let config = ServerConfig {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 8765,
            token: String::new(),
        };
        let handle = spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap();
        assert!(handle.is_none(), "关闭状态不该产生监听任务");
    }

    #[tokio::test]
    async fn enabled_server_listens_and_serves_health() {
        let config = ServerConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 0, // 交给内核挑端口，避免测试之间抢端口
            token: "t".to_string(),
        };
        let handle = spawn(&config, state_with_ownership("t", unattributed()))
            .await
            .unwrap();
        assert!(handle.is_some(), "启用后必须真的起监听");
        handle.unwrap().abort();
    }

    /// 空 token = 不鉴权；设了 token 就必须带对 `Bearer`。
    #[test]
    fn bearer_token_is_required_when_configured() {
        assert!(authorize("", &HeaderMap::new()).is_ok(), "空 token 不鉴权");

        assert!(authorize("s3cret", &HeaderMap::new()).is_err());
        assert!(authorize("s3cret", &headers_with("Bearer wrong")).is_err());
        assert!(
            authorize("s3cret", &headers_with("s3cret")).is_err(),
            "缺 Bearer 前缀也要拒"
        );
        assert!(authorize("s3cret", &headers_with("Bearer s3cret")).is_ok());
    }

    #[test]
    fn constant_time_eq_matches_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    /// 控制台页面：不带令牌也能打开（它只是一张静态页），但页面里的 API 仍然要鉴权。
    #[tokio::test]
    async fn panel_opens_without_a_token_but_the_api_still_requires_one() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = state_with_ownership("s3cret", still_owned_socket("panel"));
        let app = build_router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let page = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let body = page.text().await.unwrap();
        assert!(body.contains("XiaoAI 控制台"), "页面应当包含标题");
        assert!(
            body.contains("/health") && body.contains("Bearer"),
            "页面要真的去调 API，并且用 Bearer"
        );

        // 除了面板，**每一条**路由都必须要求鉴权。这是路由级断言：
        // 新增端点忘了挂 layer 会直接红。
        for path in ["/health", "/status"] {
            let unauth = reqwest::get(format!("http://127.0.0.1:{port}{path}"))
                .await
                .unwrap();
            assert_eq!(unauth.status(), 401, "{path} 无令牌必须 401");
        }

        let authed = reqwest::Client::new()
            .get(format!("http://127.0.0.1:{port}/health"))
            .bearer_auth("s3cret")
            .send()
            .await
            .unwrap();
        assert_eq!(
            authed.status(),
            200,
            "带对令牌要拿到 200（socket 仍归本进程）"
        );
        assert_eq!(
            authed
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
            "监控端点不能被缓存"
        );

        // 面板与 API 的字段契约：改名必须两边一起改，否则页面静默显示 0。
        let body: Value = authed.json().await.unwrap();
        assert!(body["uptime_s"].is_u64(), "uptime_s 必须是数字");
        assert!(
            body["socket"]["owned"].is_boolean(),
            "socket.owned 必须是布尔"
        );
        assert!(
            body["speech"]["service_restarts"].is_u64(),
            "speech.service_restarts 必须是数字"
        );
        assert!(
            body["speech"].get("last_packet_age_s").is_some(),
            "last_packet_age_s 允许是 null，但键必须在"
        );
        for field in ["uptime_s", "service_restarts", "last_packet_age_s", "owned"] {
            assert!(
                PANEL_HTML.contains(field),
                "页面读取的字段 `{field}` 在 panel.html 里找不到"
            );
        }
        cleanup(&state);
    }

    /// 判据一：路径还是我们绑的那个 inode → 200 / owned: true。
    #[test]
    fn health_is_ok_only_while_the_socket_is_still_ours() {
        let socket = still_owned_socket("healthy");
        let state = state_with_ownership("", socket);
        let (code, body) = health_response(&state);
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["socket"]["owned"], true);
        cleanup(&state);
    }

    /// `/status` 除了健康字段，还要给出排查用的 pid（与健康判据无关，单独一条）。
    #[test]
    fn status_reports_the_process_id() {
        let state = state_with_ownership("t", unattributed());
        assert_eq!(status_body(&state)["pid"], std::process::id());
    }

    /// 判据二：inode 换成了原生的（被让位/被抢回）→ 503。
    #[test]
    fn health_is_503_when_the_socket_was_taken_over() {
        let socket = taken_over_socket("taken");
        let state = state_with_ownership("", socket);
        let (code, body) = health_response(&state);
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "socket_not_owned");
        assert_eq!(body["socket"]["owned"], false);
        cleanup(&state);
    }

    /// 判据三（复核指出的那个洞）：**路径不存在**时不能算「还是我的」。
    /// `taken_over()` 在这里返回 false，用它取反就会误报 200 —— 所以必须用
    /// `still_owned()`：不知道归属时按不健康报。
    #[test]
    fn health_is_503_when_the_socket_path_is_gone() {
        let missing = temp_sock("gone");
        let _ = std::fs::remove_file(&missing);
        let socket = SocketOwnership::unattributed(missing);
        let (code, body) = health_response(&state_with_ownership("", socket));
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["socket"]["owned"], false);
    }

    /// 非回环 + 空 token → 拒绝启动（在写端点进来之前就把边界划死）。
    #[tokio::test]
    async fn non_loopback_without_a_token_is_refused() {
        let config = ServerConfig {
            enabled: true,
            host: "0.0.0.0".to_string(),
            port: 0,
            token: String::new(),
        };
        let err = spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("拒绝启动"), "{err}");
    }

    /// 回环 + 空 token 允许（默认值就是这个组合，只在本机可达）。
    #[tokio::test]
    async fn loopback_without_a_token_is_allowed() {
        let config = ServerConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port: 0,
            token: String::new(),
        };
        let handle = spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap()
            .expect("回环 + 空 token 应当允许启动");
        // JoinHandle 的 drop 不会取消任务，测试里必须自己 abort。
        handle.abort();
    }

    #[test]
    fn loopback_detection_accepts_the_usual_spellings() {
        for host in ["127.0.0.1", "127.0.0.2", "localhost", "::1", "[::1]"] {
            assert!(is_loopback(host), "{host} 应判为回环");
        }
        for host in ["0.0.0.0", "192.168.1.181", "[::]"] {
            assert!(!is_loopback(host), "{host} 不该判为回环");
        }
    }
}
