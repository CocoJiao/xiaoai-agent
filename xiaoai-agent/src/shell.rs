use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use crate::base::AppError;

/// 默认单条 shell 命令的最长执行时间（可由 device.command_timeout_s 覆盖）。
/// 设备上 tts_play.sh / miplayer 一旦卡住（ALSA 占用、播放队列堵），
/// 没有超时就会把整轮对话永久挂住。
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn run_shell(script: &str, limit: Duration) -> Result<CommandResult, AppError> {
    // kill_on_drop: 上层 abort 掉本轮任务时，sh 及其子进程必须一起死，
    // 否则旧 TTS 会继续出声、与新回答叠在一起（双播）。
    let fut = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .kill_on_drop(true)
        .output();

    let output = match timeout(limit, fut).await {
        Ok(res) => res?,
        Err(_) => {
            let brief: String = script.chars().take(80).collect();
            return Err(format!("shell command timed out after {limit:?}: {brief}").into());
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    Ok(CommandResult {
        stdout,
        stderr,
        exit_code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    fn pid_file(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "xiaoai-shell-test-{tag}-{}-{stamp}.pid",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        let result = run_shell("echo hello", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.stdout.trim(), "hello");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn non_zero_exit_is_reported_not_turned_into_error() {
        // 设备脚本用返回码表达失败（例如 tts_play.sh 参数不支持），
        // 调用方要靠 exit_code 判断，而不是让整轮对话直接炸。
        let result = run_shell("echo out; echo err >&2; exit 3", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.exit_code, 3);
        assert_eq!(result.stdout.trim(), "out");
        assert_eq!(result.stderr.trim(), "err");
    }

    #[tokio::test]
    async fn timeout_returns_error_instead_of_hanging() {
        // 回归（PR #8）：设备命令卡住时必须有上限，否则整轮对话永久挂住。
        let started = Instant::now();
        let err = run_shell("sleep 30", Duration::from_millis(150))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "错误信息应说明超时: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "超时后必须立刻返回，实际耗时 {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn dropping_the_future_kills_the_direct_child() {
        // 回归（PR #8 的 kill_on_drop）：唤醒打断时会 abort 本轮任务，
        // sh 必须跟着死，否则旧 TTS 会和新回答叠着播。
        //
        // 注意：这里只保证直接子进程（sh）被回收。sh 里再 fork 出去的孙进程
        // （tts_play.sh 内部的播放器）**不保证**跟着死——这正是 issue #6 要
        // 用 playback_token 解决的那一类问题，不要把这个测试当成孙进程安全。
        let tag = "drop";
        let file = pid_file(tag);
        let script = format!("echo $$ > {}; sleep 30", file.display());

        let handle = tokio::spawn(async move { run_shell(&script, Duration::from_secs(30)).await });

        // 等 pid 文件落盘，确认子进程真的起来了
        let mut waited = 0;
        while !file.exists() && waited < 200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 1;
        }
        assert!(file.exists(), "子进程没有按预期启动");

        let pid: i32 = std::fs::read_to_string(&file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        handle.abort();
        let _ = handle.await;

        // 给内核一点时间回收
        let mut alive = true;
        for _ in 0..100 {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let _ = std::fs::remove_file(&file);
        assert!(!alive, "abort 后直接子进程 {pid} 仍然活着");
    }
}
