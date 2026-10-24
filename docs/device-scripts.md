# 设备脚本速查

`device.*` 里的命令并**不是**随便写的 shell，而是这台音箱上真实存在的几个可执行文件。
换机型时最容易踩的坑就在这里：脚本名字一样，参数不一样。

标记：✅ 在 OH2P 1.62.2 上实测过 ｜ ⚠️ 有线索但未在本项目验证

## 一、agent 用到 / 推荐用的命令

| 用途 | 命令 | 对应配置 | 说明 |
|---|---|---|---|
| ✅ 念一句话 | `/usr/sbin/tts_play.sh --notify-pns {text}` | `device.tts_command` | **同步返回、异步播放**：`exit 0` 只证明投递成功，不证明播完、更不证明人听到 |
| ✅ 念一句话（不通知 pns） | `/usr/sbin/tts_play.sh {text}` | `device.tts_command`（L06A 等） | 部分机型没有 `--notify-pns` 参数，见第四节 |
| ✅ 主动播报 | `/usr/sbin/tts_play.sh "<文本>" -n` | — | `-n` 发 pns 事件，适合提醒/简报这类「它自己开口」（参数出处：参考实现仓库里拷贝的设备脚本 `docs/shells/tts_play.sh`；本仓库没有该脚本的副本） |
| ✅ 放网络音频 | `killall miplayer 2>/dev/null; miplayer -f {url} >/tmp/xiaoai-miplayer.log 2>&1 &` | `device.play_url_command` | 后台起播；不要等它返回，否则一条命令就吃掉整个超时预算 |
| ✅ 停止出声 | `killall tts_play.sh miplayer 2>/dev/null; mphelper pause` | `device.stop_audio_command` | 打断用户说话时最先调这个 |
| ✅ 暂停/继续音乐 | `mphelper pause` / `mphelper play` | `device.pause_audio_command` / `resume_audio_command` | 音乐通道；对 TTS 无效 |
| ✅ 压低音乐 | `amixer sset mysoftvol 25%`，恢复时写回原值 | `device.duck_audio_command` / `unduck_audio_command` | `mysoftvol` 是这台机器的软件音量控件；改前先读原值存 `/tmp` |
| ✅ 硬打断 | `killall tts_play.sh miplayer; mphelper pause; ubus call mediaplayer media_control ...` | `device.abort_command` | 连续对话里「用户又说话了」时兜底 |
| ✅ LED | `/bin/show_led {id}` / `/bin/shut_led {id}` | `device.led_enabled` + `led_listening` / `led_thinking` / `led_speaking` | agent 内部调用，不是配置项；灯号是数字，机型不同含义可能不同 |

## 二、接管语音相关的脚本与路径

| 路径 | 作用 | 注意 |
|---|---|---|
| `/tmp/mico_aivs_lab/usock/speech.usock` | 真麦克风的唯一出口 | agent 绑它；`mico_aivs_lab` 必须停掉才绑得上 |
| `/tmp/mico_aivs_lab/usock/common.usock` | 系统 TTS / 网络服务用 | 这个不能停，`mico_aivs_lab` 要留着跑 |
| `/etc/init.d/pns` | 启动 `mipns-*` 前端 | 里面的 `-r opus32` 决定送 Opus 还是 PCM；launcher 会把它复制到 `/tmp` 改掉再 bind-mount |
| `/etc/init.d/mico_aivs_lab` | 系统 TTS 与网络服务 | `restart` 有 1–2 秒黑洞（来源：本项目 OH2P 1.62.2 实测），**禁止每轮对话都调**（会把 ASR 事件流掐死，之后无法连续对话） |
| `/data/mipns/dialog_continuous` | 原生全双工标记 | 本项目刻意不启用，见仓库根目录 `AGENTS.md` |
| `ubus call pnshelper event_notify '{"src":1,"event":0}'` | 请求下一次采集 | 连续对话靠它；只发事件、不读 socket 的话音箱「能唤醒但没人答」 |

## 三、动手前先确认的三件事

```sh
# 1) 脚本真的存在，参数真的支持
ls -l /usr/sbin/tts_play.sh /usr/bin/mphelper /usr/bin/miplayer 2>&1
grep -n "notify-pns" /usr/sbin/tts_play.sh          # 没输出就是这机型不支持

# 2) 出声的到底是不是这条路
grep -aE "QueryTrack.tts_play|set tts_play_start_flag" /var/log/messages | tail -3
#   这两条只在 mico_aivs_lab / mipns-xiaomi 的日志里出现，是「TTS 真的被投递了」的机器旁证

# 3) 音乐通道状态（注意：它不反映 TTS）
ubus call mediaplayer player_get_play_status '{}'
```

## 四、机型差异：`--notify-pns`

- **OH2P 1.62.2**：`tts_play.sh --notify-pns {text}` 正常 ✅
- **L06A**：社区反馈该脚本**不认** `--notify-pns`，直接照抄默认配置会报错 ⚠️
  → 把 `device.tts_command` 改成 `/usr/sbin/tts_play.sh {text}`
- 判断方法：`grep -n "notify-pns" /usr/sbin/tts_play.sh`，没命中就说明不支持；
  再手工跑一次 `tts_play.sh --notify-pns "测试"` 看返回。

## 五、不要用的东西

| 命令 | 结论 |
|---|---|
| `/bin/wakeup.sh WuW` | 外部调用返回 `{"code":1}`、未进入唤醒态；**原因尚未定位**。参考实现在其 `docs/oh2p-roadmap.md` 记录过同样现象（输出含 `code: 1` 与 `idle`，`instruction.log` 没有新的识别结果），原文也强调「不能假定 `code: 1` 一定无害，也不能假定 `idle` 就代表录音已经打开」 |
| `arecord -D noop` 做长期耳朵 | `noop` 是单读者虚拟设备，且远场电平极低（来源：本项目 OH2P 1.62.2 实测，人声峰值仅 105–171；判据是停掉其它读者后用 `arecord -D noop` 读几秒看峰值），识别差 |
| `mico_record`、`hw:0,0/0,1/0,2` | 恒定静音或 dummy，任何方案都别用 |
| `ubus call mibrain ...`（当万能接口） | 语音事件不走它；LED 也不是它——LED 用的是 `/bin/show_led`、`/bin/shut_led` |

## 六、写脚本时的两个硬约束

- **超时预算**：每条设备命令都受 `device.command_timeout_s`（默认 30 秒）约束，超时那一轮对话直接结束。
  播放类命令一律后台起播（`&`）或 `killall` 先停旧的，不要写成「等播放结束」。
- **NAND 磨损**：日志、PID、临时索引都写 `/tmp`（内存盘），不要写 `/data`。
