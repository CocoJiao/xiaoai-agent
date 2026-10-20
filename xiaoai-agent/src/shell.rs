use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use crate::base::AppError;

/// 单条 shell 命令的最长执行时间。
/// 设备上 tts_play.sh / miplayer 一旦卡住（ALSA 占用、播放队列堵），
/// 没有超时就会把整轮对话永久挂住。
const SHELL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

pub async fn run_shell(script: &str) -> Result<CommandResult, AppError> {
    // kill_on_drop: 上层 abort 掉本轮任务时，sh 及其子进程必须一起死，
    // 否则旧 TTS 会继续出声、与新回答叠在一起（双播）。
    let fut = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .kill_on_drop(true)
        .output();

    let output = match timeout(SHELL_TIMEOUT, fut).await {
        Ok(res) => res?,
        Err(_) => {
            let brief: String = script.chars().take(80).collect();
            return Err(format!("shell command timed out after {SHELL_TIMEOUT:?}: {brief}").into());
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
