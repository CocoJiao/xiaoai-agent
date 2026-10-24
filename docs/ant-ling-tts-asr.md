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

`voice: "alloy"` 也返回 200（音色与默认一致）。

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
