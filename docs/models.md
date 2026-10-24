# 机型支持矩阵

本仓库的 `xiaoai-agent` 不是通用客户端：它接管的是小米音箱固件里**原生语音前端**（`mipns-*`）
送出来的音频，因此能不能用，取决于目标机型是否具备下面这 6 个前提，而不是「刷上 open-xiaoai 就行」。

图例：✅ 本项目实测可用 ｜ ⚠️ 有线索但本项目未验证 ｜ ❓ 完全未验证

| 机型 | 固件 | 状态 | 说明 |
|---|---|---|---|
| **OH2P**（小米智能音箱 Pro） | 1.62.2 | ✅ | 本项目唯一实测目标。CI 产物、`start-agent.sh`、全部验收步骤都在这个组合上跑过 |
| LX06 | — | ⚠️ | 参考实现（`cxjava/open-xiaoai`）的 `squashfs.sh` 里有 `LX06` 分支，固件补丁侧出现过该型号；本项目**没有**真机验证 |
| L06A | — | ⚠️ | 社区反馈：`tts_play.sh` **不认** `--notify-pns`，默认 `device.tts_command` 会直接报错，需改成 `/usr/sbin/tts_play.sh {text}`（见 [`device-scripts.md`](device-scripts.md)）。未在本项目验证 |
| LX01 / LX04 / LX05 / LX07 / X08A 等 | — | ❓ | 无任何验证，也没有社区数据 |

**不要**把「能刷 open-xiaoai 官方客户端」当成「能用本 agent」：官方客户端自己抓麦克风、
自己播 PCM，不依赖 `speech.usock`；本 agent 恰恰依赖它。两者对机型的要求不一样。

## 6 个前提（换机型时逐条自查）

```sh
# 1) 用户态 ABI：需要 armv7 hard-float + glibc >= 2.25
uname -m                       # 内核可能报 aarch64，但用户态是 32 位 armhf
ldd --version | head -1        # 必须 >= 2.25，否则二进制报 GLIBC_2.28 之类缺失

# 2) 原生语音前端存在，且麦克风是它的
ls -l /tmp/mico_aivs_lab/usock/          # 要有 speech.usock 与 common.usock
ps | grep -E "[m]ipns|[m]ico_aivs"       # mipns-* 必须是真麦克风的唯一读者

# 3) 送出来的是 PCM 而不是 Opus（launcher 会自动改，手工接管时要自己确认）
tr '\0' '\n' < "/proc/$(pidof mipns-xiaomi)/cmdline" | grep -n "opus32"
#   有输出 = 还在送 Opus；必须去掉 -r opus32，agent 才会收到 16k/mono/S16_LE PCM

# 4) pnshelper 的 ubus 事件存在（连续对话靠它请求下一次采集）
ubus -v list pnshelper | head

# 5) TTS 脚本的参数差异
head -30 /usr/sbin/tts_play.sh | grep -n "notify-pns\|usage\|getopts"

# 6) 有没有可用的块设备/网络文件系统（想做本地曲库时）
ls /proc/filesystems | grep -E "cifs|nfs"   # 大多数机型没有 → 不能直接挂 NAS
```

六条全过才谈「能跑」；任何一条不成立，就不是配置问题，是这个机型不适配。

## 已知的机型差异清单

| 差异点 | OH2P 1.62.2 | 其他机型 |
|---|---|---|
| `speech.usock` 协议 | 有 | 未知，需按上面的第 2 条自查 |
| `tts_play.sh --notify-pns` | 支持 | L06A 不支持（社区反馈，未验证）→ 改 `device.tts_command` |
| `wakeup.sh WuW` | 外部调用返回 `{"code":1}`、未进入唤醒态（原因未定位，见参考实现 `docs/oh2p-roadmap.md` 的排查记录） | 未知 |
| `/data/mipns/dialog_continuous` | 存在，但本项目刻意不启用（见 `AGENTS.md`） | 未知 |
| 固件大小上限 | `0x02800000` | 参考实现对 LX06 用同一个上限，未验证 |

## 新增机型的正确做法

1. 先只跑 [`flash-troubleshooting.md`](flash-troubleshooting.md) 里的只读自查，**不要**先装 agent；
2. 六条前提里 1/2/3/4 都过了，再考虑接管；
3. 接管前准备好回滚（`sh start-agent.sh stop`），并在本文件补一行「机型 / 固件 / 验证到哪一步」；
4. 只过了部分前提的，如实写「未验证」，不要写成「支持」。
