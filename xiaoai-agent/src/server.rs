use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
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

    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|err| anyhow::anyhow!("绑定控制面 {addr} 失败: {err}"))?;
    let local = listener
        .local_addr()
        .map(|addr| addr.to_string())
        .unwrap_or(addr);

    // 边界要在暴露面还小的时候划：当前只有只读端点，但 #9 的下一步就是
    // /play/*、/wakeup、/stop —— 那是能让音箱开口、能打断播放的端点。
    // 「改 host 忘了填 token」是最常见的翻车方式，所以这里直接拒绝启动，
    // 而不是打一条 warn 让它继续跑。
    if config.token.trim().is_empty() && !is_loopback(&config.host) {
        return Err(anyhow::anyhow!(
            "控制面绑在非回环地址（{}）却没有配置 token：拒绝启动。请设 server.token，或把 host 改回 127.0.0.1",
            config.host
        ));
    }
    info!(
        listen = %local,
        auth = !state.token.trim().is_empty(),
        "本地控制面已启动"
    );

    let app = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .with_state(state);

    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "控制面退出");
        }
    });
    Ok(Some(handle))
}

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

    /// 判据一：路径还是我们绑的那个 inode → 200 / owned: true。
    #[test]
    fn health_is_ok_only_while_the_socket_is_still_ours() {
        let socket = still_owned_socket("healthy");
        let (code, body) = health_response(&state_with_ownership("", socket));
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["socket"]["owned"], true);
        assert_eq!(
            status_body(&state_with_ownership("t", unattributed()))["pid"],
            std::process::id()
        );
    }

    /// 判据二：inode 换成了原生的（被让位/被抢回）→ 503。
    #[test]
    fn health_is_503_when_the_socket_was_taken_over() {
        let socket = taken_over_socket("taken");
        let (code, body) = health_response(&state_with_ownership("", socket));
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "socket_not_owned");
        assert_eq!(body["socket"]["owned"], false);
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
        assert!(spawn(&config, state_with_ownership("", unattributed()))
            .await
            .unwrap()
            .is_some());
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
