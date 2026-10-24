# 生态盘点：谁还活着、谁能抄、哪些不能碰

这个赛道项目多、关系乱，而且**一半已经归档**。这份文档记录一次可复现的盘点，避免每次重新调研。

- 数据日期：**2026-10-24**
- 取数命令（任何人都能重跑）：

```sh
gh api repos/<owner>/<repo> \
  --jq '{full_name,stars:.stargazers_count,archived,pushed_at,license:.license.spdx_id}'
```

## 一、现状表

| 项目 | Star | 状态 | 最近提交 | 许可 | 定位 |
|---|---|---|---|---|---|
| `idootop/mi-gpt` | 12495 | 🔴 **已归档** | 2026-04-04 | MIT | 账号桥接：不进设备，把小爱云端对话接到 ChatGPT/豆包 |
| `idootop/open-xiaoai` | 2625 | 🔴 **已归档** | 2026-04-04 | MIT | 原版：TS 客户端 + 固件补丁 + 刷机工具，本仓库的 vendor 来源 |
| `duhow/xiaoai-patch` | 602 | 🟢 活跃 | 2026-09-29 | **GPL-3.0** | 固件层改造：往 rootfs 塞 MPD/Snapcast/Shairport，**型号覆盖最全**（LX06/LX01/LX05/L09A） |
| `coderzc/open-xiaoai-bridge` | 423 | 🟢 活跃 | 2026-10-10 | MIT | 服务端大脑 + 音箱端 Rust（静态 Opus、symphonia、playback_token） |
| `FelixQiu1/XiaoAi-LLM-Router` | 73 | 🟡 少量提交 | 2026-10-09 | MIT | 零改造旁路：挂 MiService 转发 |
| `stevenjoezhang/xiaoai-agent` | 53 | 🟢 活跃 | 2026-10-04 | LGPL-3.0 | **我们的上游**：Rust 端侧 Agent，接管原生语音前端 |
| `cxjava/open-xiaoai` | 22 | 🟢 活跃 | 2026-10-20 | MIT | Go 重写：设备 client + 服务端 + 配置热更 + 本地曲库 |

> 星数会变，**状态和许可才是重点**。两个 2k+ 星的头部项目都在 2026-04 归档了，
> 意味着「跟着最火的那个走」这条路已经断了。

## 二、四条路线（先分清前提，再谈借鉴）

| 路线 | 代表 | 前提 | 代价 | 适合谁 |
|---|---|---|---|---|
| **端侧 Agent** | 本项目、`xiaoai-agent` | 能拿到固件原生语音前端（mipns → `speech.usock`） | 强依赖固件版本与型号；改协议要刷机 | 想要「零服务器、离线可控」 |
| **服务端大脑** | `open-xiaoai-bridge`、`cxjava/open-xiaoai` | 设备上跑客户端，家里要有常驻服务 | 多一跳网络；隐私出境 | 想要多设备、多模型随便换 |
| **固件层改造** | `duhow/xiaoai-patch` | 能刷机、能接受型号差异 | 型号适配工作量大；**GPL-3.0** | 只想要 MPD/Snapcast 这类现成服务 |
| **账号桥接** | `mi-gpt` | 小米账号 + 云端可用 | 云端一改就失效；归档后无人维护 | 完全不想动设备 |

**我们选的是第一条**，原因是它能复用固件自带唤醒/AEC/波束，不引入常驻服务器。
代价写在 `docs/models.md`：换机型要逐条过 6 个前提。

## 三、可借鉴清单（每条都已核实文件存在）

### `coderzc/open-xiaoai-bridge`（MIT，最值得对读）

| 借鉴点 | 文件 | 对应我们的 issue |
|---|---|---|
| 静态链接 libopus、一包一解 + FEC | `native/src/opus.rs`、`native/Cargo.toml` | #7 Opus 输入 |
| `playback_token` 流式打断 | `native/src/tts/mod.rs` | #6 流式 TTS 第一步 |
| TTS provider 路由 + 任何异常回退原生 | `core/services/tts/router.py` | #8 TTS provider |
| HTTP 控制面（`/play/text`、`/status`、`/wakeup`、`/stop`、`/health`） | `core/services/api_server.py` | #9 本地控制 API |

注意：它是**服务端**架构，设备侧只是瘦客户端；我们抄的是**单点实现思路**（token、解码、回退），
不是整体架构。

### `cxjava/open-xiaoai`（MIT）

| 借鉴点 | 文件 | 对应我们的 issue |
|---|---|---|
| 本地曲库索引 + 文件服务 | `pkg/music/indexer.go` | #12 本地曲库 |
| 配置热更新（RWMutex 整体替换） | `apps/chat/engine.go` | #10 SIGHUP 热加载 |
| QEMU 无刷机调试环境 | `tools/runtime/Dockerfile` | #14 QEMU 环境 |
| 免账号 OTA 下载思路 | 见 `docs/account-free-firmware-download.md` | 已落地（#14 残项） |
| `stat` 取文件大小的跨平台回退 | `tools/client-patch/src/squashfs.sh` | 已落地（fork PR #9）——**这条思路来自 cxjava，不是原创**：我们按它的「先试 GNU、再退 BSD」扩成四级回退，并加了「取不到数字就报错退出」 |

### `duhow/xiaoai-patch`（GPL-3.0）—— **只读方法，不并代码**

多型号适配的方法论值得学（按型号分补丁、失败回滚、型号矩阵），但：

> **GPL-3.0 与本仓库 LGPL-3.0 不兼容。** 任何一行代码并进来，整个项目就得按 GPL 分发。
> 只允许「读完自己写」，并且不要在提交信息里引用它的代码片段。

### `idootop/*`（MIT，已归档）

可以用来对齐概念（`speech.usock` 协议、固件补丁清单），但**不要指望有人修 bug**，
也不要把它当成「最新做法」的来源。

## 四、协议红线（抄之前先看这一栏）

| 许可 | 能不能并代码 | 条件 |
|---|---|---|
| MIT | ✅ 能 | 保留版权声明与许可文本；建议在文件头注明来源 |
| LGPL-3.0（本仓库） | — | 自身分发遵守 LGPL；可以吸收 MIT 代码（MIT → LGPL 兼容） |
| **GPL-3.0** | ❌ **不能** | 传染：并进来就得整项目 GPL-3.0。只能读方法、重写实现 |

## 五、维护约定

- 表里的 Star/状态**每次大改前重跑一次命令**，并把日期改掉；
- 新借鉴一条实现时，在第三节补一行（含 `文件路径` 与对应 issue），
  这样后来人不用再翻一遍隔壁仓库；
- 归档项目在表里保持 🔴，不要因为「它方案好」就当成活的主线。
