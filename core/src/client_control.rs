//! 客户端进程控制：优雅退出、等待、启动。
//!
//! 切号必须重启客户端（`app/lockfile` 说明有单实例锁），而重启前要让客户端
//! 自己走完退出流程，否则 WAL 可能停在半写状态。所以先发关闭请求，
//! 轮询到进程消失；只有超时才强制结束。
//!
//! 进程探测走 [`crate::proc::hidden`]，不弹控制台窗口；间隔 500ms，
//! 超时也会有限次探测，不会出现「刷屏式闪黑框」。

use crate::proc;
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// 轮询间隔。
const POLL: Duration = Duration::from_millis(500);

/// 客户端是否在运行。
pub fn is_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        proc::hidden("tasklist")
            .args(["/FI", "IMAGENAME eq WorkBuddy.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("WorkBuddy.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// 请求客户端退出，并等待它真的消失。
///
/// 返回 `true` 表示已正常退出；`false` 表示超时后强制结束。
pub fn quit(timeout: Duration) -> Result<bool> {
    if !is_running() {
        return Ok(true);
    }

    #[cfg(target_os = "windows")]
    {
        // 不带 /F：发 WM_CLOSE，让 Electron 走正常退出流程
        let _ = proc::run_hidden("taskkill", &["/IM", "WorkBuddy.exe", "/T"]);
    }

    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        sleep(POLL);
        if !is_running() {
            return Ok(true);
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = proc::run_hidden("taskkill", &["/F", "/IM", "WorkBuddy.exe", "/T"]);
    }
    for _ in 0..10 {
        sleep(POLL);
        if !is_running() {
            return Ok(false);
        }
    }
    bail!("无法结束 WorkBuddy 进程");
}

/// 启动客户端。
pub fn start(exe: &Path) -> Result<()> {
    if is_running() {
        return Ok(());
    }
    Command::new(exe)
        .spawn()
        .with_context(|| format!("启动客户端失败：{}", exe.display()))?;
    Ok(())
}

/// 等客户端进程出现。
pub fn wait_started(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if is_running() {
            return true;
        }
        sleep(POLL);
    }
    false
}
