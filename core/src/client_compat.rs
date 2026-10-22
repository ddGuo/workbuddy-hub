//! 客户端适配层：探测安装位置、定位凭据文件、获取 at-rest 密钥。
//!
//! 客户端装在非默认路径（这台机器是 `D:\WorkBuddy\`），所以探测要多路兜底。
//! 拿到 `atRestSecretKey` 的唯一可靠方式是让客户端自己以 Node 模式跑一段脚本，
//! 走它内部的 linked binding —— 这也是现有插件验证过的路径。

use crate::proc;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const AUTH_SUBPATH: &str = "CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info";

/// 用来取出 at-rest 密钥的脚本。输出是 JSON，其中含 `atRestSecretKey`。
const SECRET_SCRIPT: &str =
    r#"process.stdout.write(String(process._linkedBinding("electron_browser_workbuddy_storage").loggerGet()))"#;

/// 按优先级列出候选可执行文件路径。
pub fn exe_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    // 1. 显式覆盖
    if let Ok(v) = std::env::var("WORKBUDDY_ELECTRON_BIN") {
        if !v.trim().is_empty() {
            out.push(PathBuf::from(v));
        }
    }

    // 2. 正在运行的进程（最可靠：用户当前用的就是它）
    if let Some(p) = running_process_path() {
        out.push(p);
    }

    // 3. 常见安装位置
    for root in ["LOCALAPPDATA", "ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(dir) = std::env::var(root) {
            if dir.is_empty() {
                continue;
            }
            out.push(PathBuf::from(&dir).join("Programs/WorkBuddy/WorkBuddy.exe"));
            out.push(PathBuf::from(&dir).join("WorkBuddy/WorkBuddy.exe"));
        }
    }
    // 4. 非标准盘符（本机实测在 D:）
    for drive in ["C", "D", "E", "F"] {
        out.push(PathBuf::from(format!("{drive}:/WorkBuddy/WorkBuddy.exe")));
    }

    out.dedup();
    out
}

/// 在候选里找第一个真实存在的。
pub fn find_exe() -> Option<PathBuf> {
    exe_candidates().into_iter().find(|p| p.is_file())
}

/// 通过 tasklist 找正在运行的 WorkBuddy 进程路径。
/// 不引第三方依赖，解析 CSV 输出即可。
fn running_process_path() -> Option<PathBuf> {
    let out = proc::hidden("tasklist")
        .args(["/FI", "IMAGENAME eq WorkBuddy.exe", "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.contains("WorkBuddy.exe") {
        return None;
    }
    // 拿不到路径时退回 PowerShell 查询
    let ps = proc::hidden("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process WorkBuddy -ErrorAction SilentlyContinue | Select-Object -First 1).Path",
        ])
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&ps.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// 凭据文件路径（Local 优先，Roaming 兜底）。
pub fn auth_file() -> Option<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(v) = std::env::var("LOCALAPPDATA") {
        roots.push(PathBuf::from(v));
    }
    if let Ok(v) = std::env::var("APPDATA") {
        roots.push(PathBuf::from(v));
    }
    roots
        .into_iter()
        .map(|r| r.join(AUTH_SUBPATH))
        .find(|p| p.is_file())
}

/// 数据根目录（`~/.workbuddy`）。
pub fn data_root() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("WORKBUDDY_DATA_ROOT") {
        if !v.trim().is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| anyhow!("无法确定用户主目录"))?;
    Ok(PathBuf::from(home).join(".workbuddy"))
}

/// DSH 账号库目录（复用现有凭据池）。
pub fn accounts_dir() -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| anyhow!("无法确定用户主目录"))?;
    // hub 自己的账号库：客户端登录一个账号就自动存一份，不再依赖 DSH
    Ok(PathBuf::from(home).join(".workbuddy-hub").join("accounts"))
}

/// hub 自己的数据目录（状态、原始归属表、备份）。
pub fn hub_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("WORKBUDDY_HUB_DIR") {
        if !v.trim().is_empty() {
            return Ok(PathBuf::from(v));
        }
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| anyhow!("无法确定用户主目录"))?;
    Ok(PathBuf::from(home).join(".workbuddy-hub"))
}

/// 取密钥的硬超时。Electron-as-Node 冷启动通常 0.3–2s，8s 足够覆盖杀软扫描。
const SECRET_TIMEOUT: Duration = Duration::from_secs(8);
/// 密钥缓存有效期：只有客户端重装 / 密钥轮换才会变。
const SECRET_TTL: Duration = Duration::from_secs(600);

struct CachedSecret {
    exe: PathBuf,
    secret: String,
    at: Instant,
}

static SECRET_CACHE: OnceLock<Mutex<Option<CachedSecret>>> = OnceLock::new();
/// 同一时刻只允许一个探测进程：并发调用时不要各起一个 Electron。
static SECRET_PROBE: Mutex<()> = Mutex::new(());

/// 让客户端吐出 at-rest 密钥。
///
/// 结果缓存 10 分钟；失败**不缓存** —— 客户端可能正在启动，下次重试还该有机会。
pub fn at_rest_secret(exe: &Path) -> Result<String> {
    if let Some(secret) = cached_secret(exe) {
        return Ok(secret);
    }

    let _guard = SECRET_PROBE.lock().unwrap_or_else(|e| e.into_inner());
    // 拿到锁后再查一次：并发冷启动时只有第一个真的起进程
    if let Some(secret) = cached_secret(exe) {
        return Ok(secret);
    }

    // 这条命令会起一个 Node 模式的客户端进程，必须隐藏窗口，
    // 否则切换账号时会闪一个黑框。
    let mut cmd = proc::hidden(exe);
    cmd.arg("-e")
        .arg(SECRET_SCRIPT)
        .env("ELECTRON_RUN_AS_NODE", "1");

    let out = run_command_with_timeout(cmd, SECRET_TIMEOUT)
        .with_context(|| format!("无法启动客户端以获取密钥：{}", exe.display()))?;
    let secret = parse_secret_output(&out)?;

    let slot = SECRET_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(CachedSecret {
            exe: exe.to_path_buf(),
            secret: secret.clone(),
            at: Instant::now(),
        });
    }
    Ok(secret)
}

/// 清掉缓存。客户端重装、密钥轮换、解密失败时可以调用。
pub fn invalidate_at_rest_secret() {
    if let Some(slot) = SECRET_CACHE.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

fn cached_secret(exe: &Path) -> Option<String> {
    let slot = SECRET_CACHE.get()?;
    let guard = slot.lock().ok()?;
    let entry = guard.as_ref()?;
    if entry.exe == exe && entry.at.elapsed() < SECRET_TTL {
        Some(entry.secret.clone())
    } else {
        None
    }
}

struct CapturedOutput {
    success: bool,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// 带硬超时地跑一个子进程。
///
/// 必须自己持有 `Child` 而不是用 `Command::output()`：后者没有超时，
/// 客户端一旦卡住，调用线程（以前是主线程）就永远回不来。
/// stdout/stderr 各起一个读线程，防止子进程写满管道后阻塞。
fn run_command_with_timeout(mut cmd: std::process::Command, timeout: Duration) -> Result<CapturedOutput> {
    use std::io::Read;
    use std::process::Stdio;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("启动子进程失败")?;

    let stdout = child.stdout.take().context("子进程没有 stdout")?;
    let stderr = child.stderr.take().context("子进程没有 stderr")?;
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = stdout;
        let _ = reader.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = stderr;
        let _ = reader.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("等待子进程失败：{e}"));
            }
        }
        if Instant::now() >= deadline {
            // 只 kill 直接子进程不够：孙进程（Electron 的 zygote 等）还握着
            // stdout/stderr 管道，reader 线程 join 会一直等到它自己退出。
            #[cfg(target_os = "windows")]
            {
                let pid = child.id().to_string();
                let _ = proc::run_hidden("taskkill", &["/F", "/T", "/PID", pid.as_str()]);
            }
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            bail!(
                "获取客户端密钥超时（超过 {} 秒），已结束子进程",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Ok(CapturedOutput {
        success: status.success(),
        code: status.code(),
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

fn parse_secret_output(out: &CapturedOutput) -> Result<String> {
    if !out.success {
        bail!(
            "客户端返回非零退出码 {}：{}",
            out.code.unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let json_slice = extract_json(&stdout)
        .ok_or_else(|| anyhow!("客户端输出里找不到 JSON：{}", truncate(&stdout, 200)))?;
    let value: Value = serde_json::from_str(json_slice).context("密钥输出不是合法 JSON")?;

    value
        .get("atRestSecretKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("输出中缺少 atRestSecretKey 字段"))
}

/// 从可能混有日志的输出里截出第一个 `{` 到最后一个 `}`。
fn extract_json(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(&text[start..=end])
    } else {
        None
    }
}

fn truncate(s: &str, n: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= n {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(n).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn captured(success: bool, code: Option<i32>, stdout: &str, stderr: &str) -> CapturedOutput {
        CapturedOutput {
            success,
            code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parses_secret_with_log_noise() {
        let out = captured(true, Some(0), "boot log\n{\"atRestSecretKey\":\"s3cret\"}\ntail", "");
        assert_eq!(parse_secret_output(&out).unwrap(), "s3cret");
    }

    #[test]
    fn rejects_non_zero_exit() {
        let out = captured(false, Some(3), "", "boom");
        let err = parse_secret_output(&out).unwrap_err().to_string();
        assert!(err.contains("非零退出码 3"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn rejects_missing_field() {
        let out = captured(true, Some(0), "{\"other\":1}", "");
        assert!(parse_secret_output(&out).unwrap_err().to_string().contains("atRestSecretKey"));
    }

    #[test]
    fn cache_matches_exe_and_ttl() {
        let exe = PathBuf::from("C:/x/WorkBuddy.exe");
        let entry = CachedSecret {
            exe: exe.clone(),
            secret: "k".into(),
            at: Instant::now(),
        };
        assert!(entry.exe == exe && entry.at.elapsed() < SECRET_TTL);
        assert!(entry.exe != PathBuf::from("C:/y/WorkBuddy.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn kills_child_on_timeout() {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "ping -n 6 127.0.0.1 >nul"]);
        let started = Instant::now();
        assert!(run_command_with_timeout(cmd, Duration::from_millis(300)).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
