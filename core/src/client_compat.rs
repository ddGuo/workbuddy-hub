//! 客户端适配层：探测安装位置、定位凭据文件、获取 at-rest 密钥。
//!
//! 客户端装在非默认路径（这台机器是 `D:\WorkBuddy\`），所以探测要多路兜底。
//! 拿到 `atRestSecretKey` 的唯一可靠方式是让客户端自己以 Node 模式跑一段脚本，
//! 走它内部的 linked binding —— 这也是现有插件验证过的路径。

use crate::proc;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

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
pub fn dsh_accounts_dir() -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| anyhow!("无法确定用户主目录"))?;
    Ok(PathBuf::from(home).join(".dsh/workbuddy-accounts"))
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

/// 让客户端吐出 at-rest 密钥。
pub fn at_rest_secret(exe: &Path) -> Result<String> {
    // 这条命令会起一个 Node 模式的客户端进程，必须隐藏窗口，
    // 否则切换账号时会闪一个黑框。
    let out = proc::hidden(exe)
        .arg("-e")
        .arg(SECRET_SCRIPT)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .output()
        .with_context(|| format!("无法启动客户端以获取密钥：{}", exe.display()))?;

    if !out.status.success() {
        bail!(
            "客户端返回非零退出码 {}：{}",
            out.status.code().unwrap_or(-1),
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
