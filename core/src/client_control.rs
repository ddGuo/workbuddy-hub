//! 客户端进程控制：优雅退出、等待、启动。
//!
//! 切号必须重启客户端（`app/lockfile` 说明有单实例锁），而重启前要让客户端
//! 自己走完退出流程，否则 WAL 可能停在半写状态。所以先发关闭请求，
//! 轮询到进程消失；只有超时才强制结束。
//!
//! 进程探测走 [`crate::proc::hidden`]，不弹控制台窗口；间隔 500ms，
//! 超时也会有限次探测，不会出现「刷屏式闪黑框」。
//!
//! 强制结束按 **PID** 逐个来，而不是 `taskkill /IM` 按名字一次打完：
//! Electron 的渲染进程、GPU 进程都叫 `WorkBuddy.exe`，按名字杀经常留下
//! 一两个残留，`is_running()` 就永远为真，看起来像「无法结束进程」。
//! 另外客户端可能被开机启动项/更新程序重新拉起（PID 全换一批），
//! 所以分轮重取、逐轮清掉。

use crate::proc;
use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// 轮询间隔。
const POLL: Duration = Duration::from_millis(500);

/// 请求客户端正常退出的等待时间。正常 Electron 退出 1–3 秒。
pub const QUIT_TIMEOUT: Duration = Duration::from_secs(10);
/// 每一轮强制结束后的等待时间。
pub const FORCE_WAIT: Duration = Duration::from_secs(3);
/// 强制结束最多做几轮（客户端被自动拉起时会换一批新 PID）。
const FORCE_ROUNDS: usize = 3;
/// 等客户端启动的时间。主窗口通常 3–6 秒出现。
pub const START_TIMEOUT: Duration = Duration::from_secs(12);

/// 客户端主程序名。渲染进程、GPU 进程也用这个名字。
#[cfg(target_os = "windows")]
const IMAGE: &str = "WorkBuddy.exe";

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

/// 当前所有客户端进程的 PID。
pub fn pids() -> Vec<u32> {
    #[cfg(target_os = "windows")]
    {
        let filter = format!("IMAGENAME eq {IMAGE}");
        let Ok(out) = proc::hidden("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
        else {
            return Vec::new();
        };
        parse_tasklist_csv(&out.stdout)
    }

    #[cfg(target_os = "macos")]
    {
        let Ok(out) = proc::hidden("pgrep")
            .args(["-f", "WorkBuddy.app/Contents/MacOS/WorkBuddy"])
            .output()
        else {
            return Vec::new();
        };
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .collect()
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// 解析 `tasklist /FO CSV /NH` 的输出。
///
/// 一行形如 `"WorkBuddy.exe","11268","Console","1","120,000 K"`；
/// 没有匹配时是本地化的「信息: 没有运行的任务…」，不以引号开头，直接忽略。
#[cfg(target_os = "windows")]
fn parse_tasklist_csv(bytes: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with('"') {
                return None;
            }
            let cols: Vec<&str> = line.split("\",\"").collect();
            let name = cols.first()?.trim_matches('"');
            if !name.eq_ignore_ascii_case(IMAGE) {
                return None;
            }
            cols.get(1)?.trim_matches('"').trim().parse::<u32>().ok()
        })
        .collect()
}

/// 客户端是否在运行。
pub fn is_running() -> bool {
    !pids().is_empty()
}

/// 请求退出：只发关闭请求，不强制。
fn request_quit() {
    #[cfg(target_os = "windows")]
    {
        // 不带 /F：发 WM_CLOSE，让 Electron 走正常退出流程。
        // 不带 /T：子进程没有窗口，带 /T 只会刷一堆「只能强制终止」的报错。
        let _ = proc::run_hidden("taskkill", &["/IM", IMAGE]);
    }
    #[cfg(target_os = "macos")]
    {
        // AppleScript 的 quit 等价于 Cmd+Q，让 Electron 正常收尾
        let _ = proc::run_hidden(
            "osascript",
            &["-e", r#"tell application "WorkBuddy" to quit"#],
        );
    }
}

/// 强制结束给定的 PID；返回 taskkill 的报错文本（用于判断是不是权限问题）。
fn force_kill(pids: &[u32]) -> Option<String> {
    let mut errors = String::new();

    #[cfg(target_os = "windows")]
    {
        for pid in pids {
            let Ok(out) = proc::hidden("taskkill")
                .args(["/F", "/PID", &pid.to_string(), "/T"])
                .output()
            else {
                continue;
            };
            if !out.status.success() {
                errors.push_str(&String::from_utf8_lossy(&out.stderr));
                errors.push('\n');
            }
        }
        // 兜底扫一遍：万一上面某个 PID 结束时又派生了一个子进程
        let _ = proc::run_hidden("taskkill", &["/F", "/IM", IMAGE, "/T"]);
    }

    #[cfg(target_os = "macos")]
    {
        let _ = pids;
        let _ = proc::run_hidden(
            "pkill",
            &["-9", "-f", "WorkBuddy.app/Contents/MacOS/WorkBuddy"],
        );
    }

    if errors.trim().is_empty() {
        None
    } else {
        Some(errors)
    }
}

/// 请求客户端退出，并等待它真的消失。
///
/// 返回 `true` 表示已正常退出；`false` 表示超时后强制结束。
pub fn quit(timeout: Duration) -> Result<bool> {
    if pids().is_empty() {
        return Ok(true);
    }

    request_quit();
    if wait_until(|| pids().is_empty(), timeout) {
        return Ok(true);
    }

    let mut errors = String::new();
    for _ in 0..FORCE_ROUNDS {
        let alive = pids();
        if alive.is_empty() {
            return Ok(false);
        }
        if let Some(err) = force_kill(&alive) {
            errors.push_str(&err);
        }
        if wait_until(|| pids().is_empty(), FORCE_WAIT) {
            return Ok(false);
        }
    }

    let left = pids().len();
    let denied = ["拒绝访问", "Access is denied", "Acceso denegado"]
        .iter()
        .any(|k| errors.contains(k));
    if denied {
        bail!(
            "客户端拒绝被结束（拒绝访问）。如果 WorkBuddy 是以管理员身份启动的，\
             请右键以管理员身份运行 hub，或先手动退出客户端再切换"
        );
    }
    bail!(
        "无法结束 WorkBuddy 进程（仍有 {left} 个进程在运行）。\
         可能被开机启动项或更新程序自动拉起，请手动退出客户端后重试"
    );
}

/// 启动客户端。
pub fn start(exe: &Path) -> Result<()> {
    if is_running() {
        return Ok(());
    }
    Command::new(exe)
        .spawn()
        .map_err(|e| anyhow::anyhow!("启动客户端失败：{}：{e}", exe.display()))?;
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

    #[cfg(target_os = "windows")]
    #[test]
    fn parses_tasklist_pids() {
        let out = br#"
"WorkBuddy.exe","11268","Console","1","120,000 K"
"WorkBuddy.exe","37292","Console","1","88,000 K"
"#;
        assert_eq!(parse_tasklist_csv(out), vec![11268, 37292]);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn ignores_no_match_and_other_images() {
        let localized = "信息: 没有运行的任务匹配指定标准。".as_bytes();
        assert!(parse_tasklist_csv(localized).is_empty());
        let other = br#""WorkBuddyHelper.exe","1","Console","1","10 K""#;
        assert!(parse_tasklist_csv(other).is_empty());
    }
}
