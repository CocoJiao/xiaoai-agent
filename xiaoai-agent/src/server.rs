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
    Router::new()
        .route("/", get(panel))
        .route("/health", get(health))
        .route("/status", get(status))
        .with_state(state)
}

/// 控制台页面。**故意不鉴权**：它只是一张静态 HTML，本身不含任何设备信息，
/// token 由页面里输入一次、存 localStorage，之后的请求才带 Authorization。
/// 这样运维不用记 curl；同时也不会把 token 写进 URL（历史、日志都会留痕）。
async fn panel() -> Html<&'static str> {
    Html(PANEL_HTML)
}

/// 单文件控制台：无外部依赖（设备上没有 CDN 也要能开）。
const PANEL_HTML: &str = r#"<!doctype html>
<html lang="zh-CN"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>XiaoAI 控制台</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 15px/1.5 system-ui, -apple-system, "Segoe UI", sans-serif; margin: 0; padding: 24px; max-width: 760px; }
  h1 { font-size: 18px; margin: 0 0 16px; }
  .row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin-bottom: 16px; }
  input[type=password] { flex: 1 1 260px; padding: 8px 10px; border: 1px solid #8884; border-radius: 8px; background: transparent; color: inherit; font: inherit; }
  button { padding: 8px 14px; border: 1px solid #8884; border-radius: 8px; background: #8881; color: inherit; font: inherit; cursor: pointer; }
  button:hover { background: #8882; }
  .card { border: 1px solid #8884; border-radius: 12px; padding: 14px 16px; margin-bottom: 12px; }
  .k { color: #8889; font-size: 13px; }
  .v { font-variant-numeric: tabular-nums; font-weight: 600; }
  .ok { color: #17864a; } .bad { color: #c22a2a; }
  pre { background: #8881; padding: 12px; border-radius: 8px; overflow: auto; font-size: 12px; margin: 0; }
  .hint { color: #8889; font-size: 13px; }
</style>
<h1>XiaoAI 控制台 <span class="hint" id="who"></span></h1>
<div class="row">
  <input id="token" type="password" placeholder="server.token（只存在本机浏览器）">
  <button onclick="save()">保存令牌</button>
  <button onclick="refresh()">刷新状态</button>
</div>
<div class="card">
  <div class="k">探活</div>
  <div class="v" id="health">—</div>
  <div class="k" id="healthWhy"></div>
</div>
<div class="card">
  <div class="k">原始响应</div>
  <pre id="raw">点「刷新状态」</pre>
</div>
<p class="hint">地址栏打开本页不需要令牌；令牌只用于页面发出的 /health、/status 请求。</p>
<script>
const $ = (id) => document.getElementById(id);
$("token").value = localStorage.getItem("xiaoai_token") || "";
$("who").textContent = location.host;
function save() { localStorage.setItem("xiaoai_token", $("token").value.trim()); refresh(); }
async function get(path) {
  const r = await fetch(path, { headers: { Authorization: "Bearer " + ($("token").value.trim() || "") } });
  const text = await r.text();
  let body = null; try { body = JSON.parse(text); } catch (e) {}
  return { status: r.status, body, text };
}
async function refresh() {
  try {
    const h = await get("/health");
    const ok = h.status === 200;
    $("health").textContent = h.status + " " + ((h.body && h.body.status) || "");
    $("health").className = "v " + (ok ? "ok" : "bad");
    if (h.body) {
      const s = h.body.speech || {}, so = h.body.socket || {};
      $("healthWhy").textContent =
        "socket " + (so.owned ? "归本进程" : "不在本进程手里") +
        " · 上次收包 " + (s.last_packet_age_s == null ? "从未" : s.last_packet_age_s + "s 前") +
        " · 重启 " + (s.service_restarts ?? 0) + " 次 · 运行 " + (h.body.uptime_s ?? 0) + "s";
    }
    const st = await get("/status");
    $("raw").textContent = JSON.stringify({ health: h.body, status: st.body }, null, 2);
  } catch (e) {
    $("health").textContent = "请求失败";
    $("health").className = "v bad";
    $("healthWhy").textContent = String(e);
  }
}
refresh(); setInterval(refresh, 5000);
</script></html>"#;

async fn health(State(state): State<Arc<ControlState>>, headers: HeaderMap) -> Response {
    let (status, body) = health_response(&state);
    match authorize(&state.token, &headers) {
        Ok(()) => (status, Json(body)).into_response(),
        Err(status) => (status, Json(json!({ "error": "unauthorized" }))).into_response(),
    }
}

async fn status(State(state): State<Arc<ControlState>>, headers: HeaderMap) -> Response {
    match authorize(&state.token, &headers) {
        Ok(()) => Json(status_body(&state)).into_response(),
        Err(status) => (status, Json(json!({ "error": "unauthorized" }))).into_response(),
    }
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

        let unauth = reqwest::get(format!("http://127.0.0.1:{port}/health"))
            .await
            .unwrap();
        assert_eq!(unauth.status(), 401, "页面不鉴权，但 API 必须鉴权");

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
        cleanup(&state);
    }

    /// 判据一：路径还是我们绑的那个 inode → 200 / owned: true。    /// 判据一：路径还是我们绑的那个 inode → 200 / owned: true。
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
