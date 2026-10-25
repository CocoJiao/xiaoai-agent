# 本地控制 API（`server` 段）

默认**关闭**：`server.enabled: false` 时进程不 bind 任何端口。开给 Home Assistant 或运维用，才打开。

```yaml
server:
  enabled: true
  host: 127.0.0.1     # 只绑回环；要给 HA 用就填内网地址
  port: 8765
  token: "换成一个足够长的随机串"
```

`server` 段属于**改完要重启**的段（监听器启动时 bind）。`token` 为空时：

| host | 结果 |
|---|---|
| 回环（`127.0.0.1` / `localhost` / `::1`） | 允许启动，不鉴权 |
| 非回环（含 `0.0.0.0`） | **拒绝启动控制面**（语音不受影响，见下） |

## 端点

| 端点 | 说明 |
|---|---|
| `GET /health` | 活体判据：`speech.usock` 是否仍归本进程所有 |
| `GET /status` | 同上 + `pid` + 配置路径 |

鉴权：`Authorization: Bearer <token>`（`token` 为空时不校验）。

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8765/health
# {"status":"ok","uptime_s":128,"socket":{"path":"/tmp/.../speech.usock","owned":true},
#  "speech":{"last_packet_age_s":3,"service_restarts":0}}
```

`/health` 判的是 **inode 归属**，不是「进程还在」：让位、被原生抢回、socket 被删掉重建，三种「管线已死但进程活着」都会让它返回 **503**（`status: socket_not_owned`）。

`last_packet_age_s` / `service_restarts` 是**诊断字段**，不参与 200/503 判定——没人说话时长时间没有音频包是正常的。

控制台页面把「10 分钟内有过音频包」当作**活跃**，超过则显示「在线 · 静默」（空心绿点）。这个 600 秒阈值**只存在于页面里**（`panel.html` 的 `QUIET_S`），服务端不返回、也不做任何判断——改它只影响文案，不影响 API 行为。

## 排障：三种「控制面不可用」的含义不同

| 现象 | 含义 | 往哪查 |
|---|---|---|
| **connection refused** | 控制面**没启动**（`enabled: false`，或配置非法被拒） | `agent.yaml` 的 `server` 段；日志里找 `控制面启动失败` / `控制面已禁用` |
| **503**（`socket_not_owned`） | 控制面活着，但 `speech.usock` 不归本进程了（让位 / 被原生抢回 / socket 被删） | 语音链路，不是控制面配置 |
| **401** | token 不匹配或没带 | `Authorization: Bearer <token>` |

**配置写错不会让音箱失能**：控制面起不来时只记一条 `error!` + 一条 `warn!`，语音照常工作。这是刻意的取舍——安全上没有损失（端口压根没被监听），而故障半径从「整机哑」缩到「控制功能不可用」。`main.rs` 的 `a_misconfigured_control_plane_does_not_take_the_agent_down` 钉住了这个决定。

## 未实现

`/play/*`、`/wakeup`、`/stop` 还没做：它们需要 `playback_token` 的打断语义（issue #6 第一步已落地），属于 #9 的后续步骤。
