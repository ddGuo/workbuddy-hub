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

/// 请求客户端正常退出的等待时间。正常 Electron 退出 1–3 秒。
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(10);
/// 强制结束后的等待时间。
pub const FORCE_WAIT: Duration = Duration::from_secs(3);
/// 等客户端启动的时间。主窗口通常 3–6 秒出现。
pub const START_TIMEOUT: Duration = Duration::from_secs(12);

/// 反复探测直到成立或超时。`POLL` 与剩余时间取小，短超时的单测也能快速返回。
fn wait_until(mut probe: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        sleep(POLL.min(remaining));
    }
}

/// 客户端是否在运行。
pub fn is_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        proc::hidden("tasklist")
            .args(["/FI", "IMAGENAME eq WorkBuddy.exe", "/NH"])
            .output()
            .map(|o| output_has_client(&o.stdout))
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

fn output_has_client(bytes: &[u8]) -> bool {
    String::from_utf8_lossy(bytes).contains("WorkBuddy.exe")
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

    if wait_until(|| !is_running(), timeout) {
        return Ok(true);
    }

    #[cfg(target_os = "windows")]
    {
        let _ = proc::run_hidden("taskkill", &["/F", "/IM", "WorkBuddy.exe", "/T"]);
    }
    if wait_until(|| !is_running(), FORCE_WAIT) {
        return Ok(false);
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
    wait_until(is_running, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_until_succeeds_after_retry() {
        let mut calls = 0;
        assert!(wait_until(
            || {
                calls += 1;
                calls >= 2
            },
            Duration::from_millis(800)
        ));
    }

    #[test]
    fn wait_until_times_out() {
        let started = Instant::now();
        assert!(!wait_until(|| false, Duration::from_millis(60)));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn detects_client_in_tasklist_output() {
        let hit = br#""WorkBuddy.exe","1234","Console","1","120,000 K""#;
        let miss = b"INFO: No tasks are running which match the specified criteria.";
        assert!(output_has_client(hit));
        assert!(!output_has_client(miss));
    }
}
