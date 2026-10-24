# 蚂蚁百灵（Ant Ling）TTS / ASR 接入笔记

实测环境：本机 curl → `https://api.ant-ling.com/v1`，2026-10-25。
目的：给 `xiaoai-agent` 找一套「外部 TTS + 外部 ASR」的后端。协议与 OpenAI 兼容，
所以 agent 侧只改配置，不改代码。

## 一、语音合成（TTS）

| 项 | 值 |
|---|---|
| 端点 | `POST https://api.ant-ling.com/v1/audio/speech` |
| 模型 | `Ming-3.0-tts`（当前唯一可选值） |
| 输出格式 | `wav` / `mp3` / `pcm` / `opus` / `aac`（同步默认 wav，流式默认 pcm） |
| 采样率 | `8000` / `16000` / `22050` / `24000`（默认 22050） |
| 其它参数 | `speed` 0.25–4.0、`volume` 0.5–2.0、`pitch` 0.5–2.0 |
| 文本上限 | 2048 字符 |
| 响应 | `Content-Type` 随 `response_format` 变化，body 就是音频二进制 |

### 两个坑（都实测撞上了）

1. **`voice` / `references` / `instructions` 至少给一个**，否则
   `400 {"message":"voice or references or instructions  are required"}`。
   文档把 `voice` 写成选填，实际不给会报错。
2. **不存在的 `voice` 名字 → `HTTP 200` + **空 body**（0 字节）**，不是 4xx。
   例如 `zh-CN-XiaoxiaoNeural` / `female` 都是这个结果。
   所以客户端必须把「200 但空音频」当失败处理——`tts.rs` 里的
   `bail!("TTS 接口返回空音频")` 就是为这条准备的。

可用的最小请求（实测 200，56 KB wav）：

```json
{"model":"Ming-3.0-tts","input":"你好，我是小爱音箱。","response_format":"wav","sample_rate":16000,"instructions":"用温柔的女声，语速稍慢"}
```

`voice: "alloy"` 也返回 200。

### 音色：实测 `voice` / `instructions` 都不改变输出

同一段文本跑了四组变体，比字节与简单声学统计：

| 参数组合 | 字节数 | 过零数 | RMS |
|---|---|---|---|
| 只给 `voice: alloy` | 56364 | 4134 | 3024 |
| 只给 `instructions: 用低沉男声，语速很快` | 56364 | 4144 | 3023 |
| 同时给 `voice: alloy` + 同上 instructions | 56364 | 4150 | 3023 |
| 只给 `instructions: 用温柔的女声，语速稍慢` | 56364 | 4144 | 3023 |

结论：**换 `voice` 名字、换 `instructions` 描述，输出几乎没差别**（差异 <0.5%，且同参数两次跑的哈希也不同——合成本身有随机性）。`Ming-3.0-tts` 目前实际只有一套默认音色，`voice` / `instructions` 的现实作用是**满足接口的参数要求**（三者都不给会 400），不是调音色。

真要换音色只能用 `references`（声音克隆：10–30 秒 WAV 的 base64 + 逐字文本），需要用户自己提供样本，不在当前配置面里。

### WAV 头是流式写法（`data` 长度 = 0x7FFFFFFF）

拿到的 wav 头里 data chunk 长度字段是 `0x7FFFFFFF`，不是真实长度。后果：

- **播放没问题**（真机实测：`miplayer` 打出 `notify PLAYER STARTED` → `event(EndReached)`；`aplay` `rc=0`，识别为 `Signed 16 bit Little Endian, Rate 16000 Hz, Mono`）；
- 但**别用这个头算时长**——按头算会得到 67108 秒这种荒谬值。要时长就用字节数 ÷ (采样率 × 声道 × 2)。

## 二、语音识别（ASR）

| 项 | 值 |
|---|---|
| 端点 | `POST https://api.ant-ling.com/v1/audio/transcriptions` |
| 模型 | `Ming-3.0-asr-tiny` |
| 输入 | `multipart/form-data`，字段 `file` + `model` |
| 格式 | `.wav` / `.mp3` |
| 限制 | 文件 ≤ 10 MB、**音频 ≤ 30 秒** |
| 返回 | 只有 `json`（`text` + `usage`） |
| 不支持 | `stream`、`timestamp_granularities`、`chunking_strategy`、`keywords` 等 |

回环验证：TTS 合成「你好，我是小爱音箱，这是百灵语音合成的测试。」→ 直接喂给
ASR → 识别结果 `你好。我是小艾音箱。`（同音字「艾」，属正常识别偏差）。

## 三、写进 agent 配置的样子

```yaml
tts:
  provider: openai
  fallback_to_native: true     # 后端挂了就退回设备自带 tts_play.sh
  openai:
    base_url: https://api.ant-ling.com/v1
    api_key: sk-...            # 不要提交进仓库
    model: Ming-3.0-tts
    instructions: 用温柔的女声，语速稍慢
    response_format: wav
    sample_rate: 16000
    timeout_s: 30

asr:
  provider: open_ai
  open_ai:
    base_url: https://api.ant-ling.com/v1
    api_key: sk-...
    model: Ming-3.0-asr-tiny
    language: zh
```

注意两点：

- `tts` 是**热段**（SIGHUP 生效）；`asr` 是**需要重启**的段（`AsrClient` 启动时定型）。
- `sample_rate` 是**百灵的扩展字段**：官方 OpenAI 的 `/v1/audio/speech` 没有它。留着指向官方不会报错，但字段会被静默忽略（采样率由 `response_format` 与模型决定）。默认 `None` 不发。
- ASR 单段 ≤30 秒。agent 一轮的录音超过 30 秒会被拒——这种情况会走到 ASR 错误
  分支并提示用户，不会静默吞掉。

## 四、为什么是它

对比过的三个候选：

| 候选 | 结论 |
|---|---|
| NAS 上的 MiMo `:9130` | `/v1/audio/speech` 对所有模型名都 `502 upstream_failed`，`/v1/models` 也没有 TTS 模型——不是 TTS 端点 |
| ebondai 中转 | 没有 `/v1/audio/speech` 路由（404） |
| NAS 自建 `edge-tts` + shim | 可行但要额外维护一个服务；有现成 API 时没必要 |

蚂蚁百灵同时提供 TTS 和 ASR，一套 key、一套 base_url，省掉「TTS 一个供应商、
ASR 另一个」的组合成本。

## 五、真机验证进度

| 项 | 状态 |
|---|---|
| TTS 合成（curl） | ✅ 200 / 56364 字节 wav / 16 kHz mono |
| ASR 回环（curl） | ✅ 识别出原文 |
| 这段 wav 在音箱上播放 | ✅ `miplayer`：`PLAYER STARTED` → `EndReached`；`aplay`：`rc=0` |
| agent 侧 `provider: openai` 完整问答 | ❌ 未做（需要把新二进制部署到音箱 + 改 `agent.yaml` 重启） |
