use bytes::Bytes;
use std::future::Future;
use std::time::Duration;

use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use crate::asr::{RealtimeAsrEvent, RealtimeAsrSession};
use crate::config::CaptureConfig;
use crate::speech::{SpeechInput, SpeechTurn};
use crate::vad::{SpeechCollector, SpeechEvent};

pub async fn record_utterance<F, Fut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    turn: &mut SpeechTurn,
    on_speech_start: F,
) -> anyhow::Result<Bytes>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;

    info!(
        turn_id = turn.id(),
        "CAPTURE_BACKEND backend=speech_usock endpoint=energy"
    );
    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                collector.log_noise_floor("timeout");
                let _ = turn.stop_capture().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            input = turn.recv() => {
                match input {
                    Some(SpeechInput::Pcm(bytes)) => {
                        for event in collector.push(&bytes) {
                            match event {
                                SpeechEvent::SpeechStart(_) => {
                                    speech_started = true;
                                    on_speech_start().await;
                                }
                                SpeechEvent::SpeechChunk(_) => {}
                                SpeechEvent::SpeechRejected => {
                                    speech_started = false;
                                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                                }
                                SpeechEvent::Utterance(pcm) => {
                                    turn.stop_capture().await?;
                                    return Ok(pcm);
                                }
                            }
                        }
                    }
                    Some(SpeechInput::End) => {
                        anyhow::bail!("mipns ended the stream before an utterance was captured");
                    }
                    Some(SpeechInput::Cancelled) => {
                        anyhow::bail!("mipns cancelled the speech stream");
                    }
                    None => anyhow::bail!("speech.usock service stopped during capture"),
                }
            }
        }
    }
}

pub async fn record_utterance_streaming<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    turn: &mut SpeechTurn,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
) -> anyhow::Result<Bytes>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
    C: Fn(Bytes) -> CFut + Send + Sync,
    CFut: Future<Output = anyhow::Result<()>> + Send,
    R: Fn() -> RFut + Send + Sync,
    RFut: Future<Output = anyhow::Result<()>> + Send,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;

    info!(
        turn_id = turn.id(),
        "CAPTURE_BACKEND backend=speech_usock endpoint=energy streaming=true"
    );
    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                collector.log_noise_floor("timeout");
                let _ = turn.stop_capture().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            input = turn.recv() => {
                match input {
                    Some(SpeechInput::Pcm(bytes)) => {
                        for event in collector.push(&bytes) {
                            match event {
                                SpeechEvent::SpeechStart(prefix) => {
                                    speech_started = true;
                                    on_speech_start().await;
                                    on_audio_chunk(prefix).await?;
                                }
                                SpeechEvent::SpeechChunk(chunk) => {
                                    on_audio_chunk(chunk).await?;
                                }
                                SpeechEvent::SpeechRejected => {
                                    speech_started = false;
                                    on_speech_rejected().await?;
                                    idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                                }
                                SpeechEvent::Utterance(pcm) => {
                                    turn.stop_capture().await?;
                                    return Ok(pcm);
                                }
                            }
                        }
                    }
                    Some(SpeechInput::End) => {
                        anyhow::bail!("mipns ended the stream before an utterance was captured");
                    }
                    Some(SpeechInput::Cancelled) => {
                        anyhow::bail!("mipns cancelled the speech stream");
                    }
                    None => anyhow::bail!("speech.usock service stopped during capture"),
                }
            }
        }
    }
}

/// Streams mipns PCM until the remote ASR service closes the utterance with
/// its own VAD. Local timing is only used before speech and as a hard fail-safe.
pub async fn record_utterance_server_vad_streaming<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    turn: &mut SpeechTurn,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
) -> anyhow::Result<String>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
    E: Fn() -> EFut + Send + Sync,
    EFut: Future<Output = ()> + Send,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let max_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(max_timer);
    let transcript_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(transcript_timer);
    let appender = session.appender();
    let mut speech_started = false;
    let mut input_closed = false;
    let mut chunks = 0_u64;

    info!(
        turn_id = turn.id(),
        "CAPTURE_BACKEND backend=speech_usock endpoint=server_vad streaming=true"
    );
    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                let _ = turn.stop_capture().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            _ = &mut max_timer, if speech_started && !input_closed => {
                warn!(max_utterance_s = config.max_utterance_s, "server VAD did not end utterance; committing at local safety limit");
                turn.stop_capture().await?;
                input_closed = true;
                transcript_timer.as_mut().reset(Instant::now() + idle_timeout);
                session.commit().await?;
                on_speech_stop().await;
            }
            _ = &mut transcript_timer, if input_closed => {
                anyhow::bail!("timed out waiting for Realtime ASR transcript");
            }
            event = session.next_event() => {
                match event? {
                    RealtimeAsrEvent::SpeechStarted { audio_start_ms } => {
                        info!(audio_start_ms, "ASR_VAD_SPEECH_STARTED");
                        if !speech_started {
                            speech_started = true;
                            max_timer.as_mut().reset(Instant::now() + max_utterance_duration(&config));
                            on_speech_start().await;
                        }
                    }
                    RealtimeAsrEvent::SpeechStopped { audio_end_ms } => {
                        info!(audio_end_ms, "ASR_VAD_SPEECH_STOPPED");
                        if !input_closed {
                            turn.stop_capture().await?;
                            input_closed = true;
                            transcript_timer.as_mut().reset(Instant::now() + idle_timeout);
                            on_speech_stop().await;
                        }
                    }
                    RealtimeAsrEvent::Transcript(text) => {
                        if !input_closed {
                            turn.stop_capture().await?;
                        }
                        return Ok(text);
                    }
                }
            }
            input = turn.recv(), if !input_closed => {
                match input {
                    Some(SpeechInput::Pcm(bytes)) => {
                        chunks += 1;
                        if chunks == 1 {
                            debug!(bytes = bytes.len(), "SPEECH_USOCK_FIRST_ASR_CHUNK");
                        }
                        appender.append_pcm(bytes).await?;
                    }
                    Some(SpeechInput::End) => {
                        if !speech_started {
                            anyhow::bail!("mipns ended the stream before server VAD detected speech");
                        }
                        input_closed = true;
                        transcript_timer.as_mut().reset(Instant::now() + idle_timeout);
                        session.commit().await?;
                        on_speech_stop().await;
                    }
                    Some(SpeechInput::Cancelled) => {
                        anyhow::bail!("mipns cancelled the speech stream");
                    }
                    None => anyhow::bail!("speech.usock service stopped during capture"),
                }
            }
        }
    }
}

fn max_utterance_duration(config: &CaptureConfig) -> Duration {
    Duration::from_secs_f64(config.max_utterance_s.max(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CaptureConfig;
    use crate::speech::{SpeechInput, SpeechTurn, TestCommand};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// 100ms @ 16kHz mono S16
    const BLOCK_BYTES: usize = 3200;

    fn test_config() -> CaptureConfig {
        CaptureConfig {
            silence_ms: 200,
            min_speech_ms: 100,
            cooldown_s: 0.0,
            ..CaptureConfig::default()
        }
    }

    fn pcm_block(sample: i16) -> Bytes {
        Bytes::from(
            (0..1600)
                .flat_map(|_| sample.to_le_bytes())
                .collect::<Vec<u8>>(),
        )
    }

    async fn commands(recorder: &Arc<Mutex<Vec<TestCommand>>>) -> Vec<TestCommand> {
        recorder.lock().await.clone()
    }

    /// 采集超时必须把 stop_capture 发出去再返回错误——只返回错误不发命令会让
    /// mipns 侧的 stream 一直挂着，下一次唤醒绑不上新 stream。
    ///
    /// 注意：`record_utterance` 内部把上限钳到至少 1 秒（`capture.rs:23`），
    /// 所以这条测试实跑约 1s。CI 里可接受，暂时不为此引入 `tokio::time::pause()`
    /// （paused clock 会让「谁在等时间」变模糊）；等这类慢测试累计超过 5s 再优化。
    #[tokio::test]
    async fn idle_timeout_stops_capture_and_reports_timeout() {
        let (mut turn, _input_tx, recorder) = SpeechTurn::for_test(1);
        let err = record_utterance(
            test_config(),
            // record_utterance 内部会把上限抬到至少 1s
            Duration::from_secs(1),
            &mut turn,
            || async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out"), "错误信息应说明超时: {err}");
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    #[tokio::test]
    async fn end_before_speech_is_an_error_without_stop_command() {
        // 这里只固定「mipns 自己结束流时不补发 stop_capture」这条契约。
        // 用户体感的问题（说完话一片沉默、没有任何提示）在 main.rs 的错误分支里
        // 仍然存在，属于另一条待办；不要因为这条测试绿了就以为那条路径已经处理好。
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(2);
        input_tx.send(SpeechInput::End).await.unwrap();
        let err = record_utterance(
            test_config(),
            Duration::from_secs(30),
            &mut turn,
            || async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("ended the stream"), "{err}");
        assert!(
            commands(&recorder).await.is_empty(),
            "mipns 自己结束了流，不需要我们补一条 stop_capture"
        );
    }

    #[tokio::test]
    async fn cancelled_stream_is_an_error() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(3);
        input_tx.send(SpeechInput::Cancelled).await.unwrap();
        let err = record_utterance(
            test_config(),
            Duration::from_secs(30),
            &mut turn,
            || async {},
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("cancelled"), "{err}");
        assert!(commands(&recorder).await.is_empty());
    }

    #[tokio::test]
    async fn voiced_then_silence_returns_pcm_and_stops_capture() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(4);
        let handle = tokio::spawn(async move {
            record_utterance(
                test_config(),
                Duration::from_secs(30),
                &mut turn,
                || async {},
            )
            .await
        });

        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        // silence_ms=200 → 两个静音块就够收尾
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("应当完成一次采集");
        assert!(
            pcm.len() >= 4 * BLOCK_BYTES,
            "至少应包含 4 个有声块（含 300ms pre-roll 会更多），实际 {} 字节",
            pcm.len()
        );
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }

    /// 流式路径除了返回整段 PCM，还要把每个块实时转给 ASR：
    /// 只返回不发块，ASR 侧会一直等不到首包。
    #[tokio::test]
    async fn streaming_path_forwards_chunks_then_returns_utterance() {
        let (mut turn, input_tx, recorder) = SpeechTurn::for_test(5);
        let forwarded = Arc::new(Mutex::new(Vec::<usize>::new()));
        let collect = forwarded.clone();
        let handle = tokio::spawn(async move {
            record_utterance_streaming(
                test_config(),
                Duration::from_secs(30),
                &mut turn,
                || async {},
                move |chunk: Bytes| {
                    let collect = collect.clone();
                    async move {
                        collect.lock().await.push(chunk.len());
                        Ok(())
                    }
                },
                || async { Ok(()) },
            )
            .await
        });

        for _ in 0..4 {
            input_tx
                .send(SpeechInput::Pcm(pcm_block(10_000)))
                .await
                .unwrap();
        }
        for _ in 0..3 {
            input_tx.send(SpeechInput::Pcm(pcm_block(0))).await.unwrap();
        }

        let pcm = handle.await.unwrap().expect("应当完成一次采集");
        let forwarded = forwarded.lock().await.clone();
        assert!(
            !forwarded.is_empty(),
            "流式路径必须把块转给 ASR，否则 ASR 收不到音频"
        );
        assert!(
            forwarded.iter().all(|len| *len > 0),
            "转发的块不应为空: {forwarded:?}"
        );
        assert!(pcm.len() >= 4 * BLOCK_BYTES);
        assert_eq!(commands(&recorder).await, vec![TestCommand::StopCapture]);
    }
}
