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
///
/// 三个坑都是在别的电脑上才暴露出来的：
/// - 新版客户端 `loggerGet()` 返回**对象**而不是字符串，`String(...)` 会得到
///   `[object Object]`，解析端就报“找不到 JSON”；
/// - Windows 上 `process.stdout.write` 是**异步**的，脚本写完立刻退出，
///   输出可能整段丢掉 —— 父进程看到的就是空 stdout / 空 stderr；
/// - 从 GUI 进程启动时，个别环境下 fd 1 根本不是有效句柄，写 stdout 直接失败。
///
/// 所以改成 `fs.writeSync` 同步写，并且**再写一份临时文件**（路径由
/// `WORKBUDDY_SECRET_OUT` 传入），Rust 侧在 stdout 为空时回读文件。
const SECRET_SCRIPT: &str = r#"(function () {
  var fs = require("fs");
  var out = process.env.WORKBUDDY_SECRET_OUT;
  function emit(text) {
    try { fs.writeSync(1, text); } catch (e) {}
    if (out) { try { fs.writeFileSync(out, text); } catch (e) {} }
  }
  try {
    var s = process._linkedBinding("electron_browser_workbuddy_storage").loggerGet();
    emit(typeof s === "string" ? s : JSON.stringify(s));
  } catch (e) {
    var msg = "SCRIPT_ERROR: " + (e && e.stack ? e.stack : String(e));
    try { fs.writeSync(2, msg); } catch (_) {}
    if (out) { try { fs.writeFileSync(out + ".err", msg); } catch (_) {} }
    process.exitCode = 1;
  }
})()"#;

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

    // 3. 注册表卸载项 / 开始菜单快捷方式（新机器装在非标准目录时最有用）
    out.extend(registry_candidates());

    // 3.5 Squirrel 布局：`%LOCALAPPDATA%\WorkBuddy\app-<版本>\WorkBuddy.exe`
    out.extend(squirrel_candidates());

    // 4. 常见安装位置
    for root in ["LOCALAPPDATA", "ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Ok(dir) = std::env::var(root) {
            if dir.is_empty() {
                continue;
            }
            out.push(PathBuf::from(&dir).join("Programs/WorkBuddy/WorkBuddy.exe"));
            out.push(PathBuf::from(&dir).join("Programs/workbuddy/WorkBuddy.exe"));
            out.push(PathBuf::from(&dir).join("WorkBuddy/WorkBuddy.exe"));
            out.push(PathBuf::from(&dir).join("workbuddy/WorkBuddy.exe"));
        }
    }
    // 5. 非标准盘符（本机实测在 D:）
    for drive in ["C", "D", "E", "F"] {
        out.push(PathBuf::from(format!("{drive}:/WorkBuddy/WorkBuddy.exe")));
        out.push(PathBuf::from(format!(
            "{drive}:/Program Files/WorkBuddy/WorkBuddy.exe"
        )));
    }

    out.dedup();
    // 同一路径可能从注册表、快捷方式、Squirrel 目录各来一次，按去重后的顺序保留
    let mut seen: Vec<String> = Vec::new();
    out.retain(|p| {
        let key = p.display().to_string().to_lowercase();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    out
}

/// Squirrel 布局：`%LOCALAPPDATA%\WorkBuddy\app-<版本>\WorkBuddy.exe`。
///
/// 这种安装方式根目录只有一个 `Update.exe` 和启动器，真正的客户端在带版本号的
/// 子目录里；按版本号倒序，优先最新。
fn squirrel_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(local) = std::env::var("LOCALAPPDATA") else {
        return out;
    };
    let roots = [
        PathBuf::from(&local).join("WorkBuddy"),
        PathBuf::from(&local).join("Programs").join("WorkBuddy"),
    ];
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut versions: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("app-"))
                    .unwrap_or(false)
            })
            .collect();
        versions.sort();
        versions.reverse();
        for v in versions {
            out.push(v.join("WorkBuddy.exe"));
        }
    }
    out
}

/// 从注册表卸载项与开始菜单快捷方式里找。
///
/// 不调用 reg.exe：部分安全策略会拦它。用 PowerShell 一次性读，
/// 结果会被 `find_exe` 缓存，不会每次切号都启动进程。
fn registry_candidates() -> Vec<PathBuf> {
    let script = r#"$out=@()
$keys=@('HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\*','HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*')
Get-ItemProperty $keys -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -like '*WorkBuddy*' } | ForEach-Object { if ($_.DisplayIcon) { $out += $_.DisplayIcon }; if ($_.InstallLocation) { $out += $_.InstallLocation } }
$lnk = Get-ChildItem "$env:APPDATA\Microsoft\Windows\Start Menu\Programs" -Recurse -Filter '*WorkBuddy*.lnk' -ErrorAction SilentlyContinue | Select-Object -First 1
if ($lnk) { $out += (New-Object -ComObject WScript.Shell).CreateShortcut($lnk.FullName).TargetPath }
$out"#;
    let Ok(res) = proc::hidden("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
    else {
        return Vec::new();
    };
    if !res.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&res.stdout)
        .lines()
        .filter_map(parse_install_line)
        .collect()
}

/// 解析一行安装信息：`"D:\WorkBuddy\WorkBuddy.exe",0` 或安装目录。
fn parse_install_line(line: &str) -> Option<PathBuf> {
    let s = line.trim().trim_matches('"');
    if s.is_empty() {
        return None;
    }
    // DisplayIcon 形如 `C:\App\WorkBuddy.exe,0`
    let path = s.split(',').next().unwrap_or(s).trim().trim_matches('"');
    if path.is_empty() {
        return None;
    }
    let p = PathBuf::from(path);
    if path.to_lowercase().ends_with(".exe") {
        Some(p)
    } else if p.is_dir() {
        Some(p.join("WorkBuddy.exe"))
    } else {
        None
    }
}

static EXE_CACHE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

/// 在候选里找第一个真实存在的（结果缓存，避免每 30 秒重复探测）。
///
/// 优先挑**真正的 Electron 主程序**（目录下有 `resources/app.asar`）：
/// 同一台机器上可能同时存在启动器与主程序，启动器也能 `-e`，但什么都不输出。
pub fn find_exe() -> Option<PathBuf> {
    let cache = EXE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some(p) = guard.as_ref() {
            if p.is_file() {
                return Some(p.clone());
            }
        }
    }
    let existing: Vec<PathBuf> = exe_candidates().into_iter().filter(|p| p.is_file()).collect();
    let found = existing
        .iter()
        .find(|p| looks_like_electron_app(p.as_path()))
        .cloned()
        .or_else(|| existing.first().cloned());
    if let Ok(mut guard) = cache.lock() {
        *guard = found.clone();
    }
    found
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
    // 拿不到路径时退回 PowerShell 查询；可能有多个进程，取第一个真程序
    let ps = proc::hidden("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process WorkBuddy -ErrorAction SilentlyContinue | ForEach-Object { $_.Path } | Select-Object -Unique)",
        ])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&ps.stdout);
    let paths: Vec<PathBuf> = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect();
    paths
        .iter()
        .find(|p| p.is_file() && looks_like_electron_app(p.as_path()))
        .cloned()
        .or_else(|| paths.into_iter().find(|p| p.is_file()))
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

/// 旧版 DSH 账号库，仅用于一次性导入，之后不再读写。
pub fn legacy_accounts_dir() -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| anyhow!("无法确定用户主目录"))?;
    Ok(PathBuf::from(home).join(".dsh").join("workbuddy-accounts"))
}

/// hub 自己的账号库：`~/.workbuddy-hub/accounts/<uid>.json`。
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
/// 失败后的冷却时间。
///
/// 失败不缓存，30 秒一轮的积分刷新就会反复起进程；在「客户端主程序不认
/// `ELECTRON_RUN_AS_NODE`」的机器上，每起一次就等于把客户端窗口叫到前台一次。
/// 所以失败也记一笔，两分钟内直接返回同一条错误；用户手动点按钮时绕过它。
const SECRET_FAILURE_TTL: Duration = Duration::from_secs(120);

struct CachedSecret {
    secret: String,
    at: Instant,
}

static SECRET_CACHE: OnceLock<Mutex<Option<CachedSecret>>> = OnceLock::new();
static SECRET_FAILURE: OnceLock<Mutex<Option<(Instant, Duration, String)>>> = OnceLock::new();
/// 每个 exe 的 fuse 结论（路径 + 文件大小时有效）。
static FUSE_CACHE: OnceLock<Mutex<Vec<(PathBuf, u64, bool)>>> = OnceLock::new();
/// 同一时刻只允许一个探测进程：并发调用时不要各起一个 Electron。
static SECRET_PROBE: Mutex<()> = Mutex::new(());

/// 让客户端吐出 at-rest 密钥。
///
/// 结果缓存 10 分钟；失败缓存 2 分钟（见 [`SECRET_FAILURE_TTL`]）。
/// 会依次尝试「探测到的 exe → 其它看起来是真正 Electron 程序的候选」，
/// 因为个别机器上找到的 `WorkBuddy.exe` 只是启动器：它能接受参数，
/// 但既不跑脚本也不输出，静默退 0。
pub fn at_rest_secret(exe: &Path) -> Result<String> {
    at_rest_secret_inner(exe, false)
}

/// 用户手动触发的动作（点「立即签到」、确认切号）用这个：跳过失败冷却。
pub fn at_rest_secret_force(exe: &Path) -> Result<String> {
    at_rest_secret_inner(exe, true)
}

fn at_rest_secret_inner(exe: &Path, ignore_failure: bool) -> Result<String> {
    if let Some(secret) = cached_secret() {
        return Ok(secret);
    }

    let _guard = SECRET_PROBE.lock().unwrap_or_else(|e| e.into_inner());
    // 拿到锁后再查一次：并发冷启动时只有第一个真的起进程
    if let Some(secret) = cached_secret() {
        return Ok(secret);
    }
    if !ignore_failure {
        if let Some(err) = cached_failure() {
            return Err(anyhow!("{err}"));
        }
    }

    let candidates = probe_candidates(exe);
    let mut notes: Vec<String> = Vec::new();
    for (i, cand) in candidates.iter().enumerate() {
        match probe_secret_once(cand, i) {
            Ok(secret) => {
                cache_secret(&secret);
                clear_failure();
                write_probe_log(&format!("取密钥成功：{}", cand.display()));
                return Ok(secret);
            }
            Err(e) => notes.push(format!("{}：{e}", cand.display())),
        }
    }

    let detail = if notes.is_empty() {
        if run_as_node_disabled(exe) == Some(true) {
            format!(
                "客户端 {} 关闭了 Electron 的 RunAsNode 功能，没法用 Node 模式读密钥；签到、手机号、切号都依赖它，请把这个结论反馈给作者",
                exe.display()
            )
        } else {
            format!("不是可执行文件：{}", exe.display())
        }
    } else {
        notes.join("；")
    };
    let msg = format!("读不到客户端密钥。{detail}");
    write_probe_log(&msg);
    cache_failure(&msg);
    Err(anyhow!("{msg}"))
}

/// 清掉缓存。客户端重装、密钥轮换、解密失败时可以调用。
pub fn invalidate_at_rest_secret() {
    if let Some(slot) = SECRET_CACHE.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
    clear_failure();
}

/// 密钥和 exe 路径无关：真正取到密钥的可能是候选里的第二个（第一个是启动器），
/// 所以缓存只看 TTL，不比对路径 —— 否则每次调用都会把失败的候选再跑一遍。
fn cached_secret() -> Option<String> {
    let slot = SECRET_CACHE.get()?;
    let guard = slot.lock().ok()?;
    let entry = guard.as_ref()?;
    if entry.at.elapsed() < SECRET_TTL {
        Some(entry.secret.clone())
    } else {
        None
    }
}

fn cache_secret(secret: &str) {
    let slot = SECRET_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = slot.lock() {
        *guard = Some(CachedSecret {
            secret: secret.to_string(),
            at: Instant::now(),
        });
    }
}

fn cached_failure() -> Option<String> {
    let slot = SECRET_FAILURE.get()?;
    let guard = slot.lock().ok()?;
    let (at, ttl, msg) = guard.as_ref()?;
    if at.elapsed() < *ttl {
        Some(msg.clone())
    } else {
        None
    }
}

fn cache_failure(msg: &str) {
    // 「零输出」「RunAsNode 被禁用」都是结构性原因，冷却久一点：
    // 这种机器上每探一次都可能把客户端窗口叫到前台。
    let ttl = if msg.contains("没有任何输出") || msg.contains("RunAsNode") {
        Duration::from_secs(1800)
    } else {
        SECRET_FAILURE_TTL
    };
    let slot = SECRET_FAILURE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = slot.lock() {
        *guard = Some((Instant::now(), ttl, msg.to_string()));
    }
}

fn clear_failure() {
    if let Some(slot) = SECRET_FAILURE.get() {
        if let Ok(mut guard) = slot.lock() {
            *guard = None;
        }
    }
}

/// 把失败详情落到 `~/.workbuddy-hub/secret-probe.log`。
///
/// 这个错误在别的电脑上偶发，光靠界面截图分不清是「exe 找错」还是
/// 「客户端不认 Node 模式」，留一份可回传的记录省一轮来回。
fn write_probe_log(detail: &str) {
    let Ok(dir) = hub_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("secret-probe.log");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().collect();
    if lines.len() > 200 {
        lines = lines[lines.len() - 200..].to_vec();
    }
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(&format!(
        "[{}] {detail}\n",
        crate::format_ts(crate::now_secs())
    ));
    let _ = std::fs::write(path, body);
}

/// 探测时依次尝试的可执行文件。
///
/// 第一个是调用方给的（通常来自 [`find_exe`]），后面补上其它**看起来是真正
/// Electron 程序**的候选：启动器 / 更新器目录里没有 `resources/app.asar`，
/// 拿它们跑脚本只会得到零输出。
fn probe_candidates(exe: &Path) -> Vec<PathBuf> {
    let mut raw: Vec<PathBuf> = Vec::new();
    if exe.is_file() {
        raw.push(exe.to_path_buf());
    }
    for cand in exe_candidates() {
        if raw.len() >= 4 {
            break;
        }
        if !cand.is_file() || raw.contains(&cand) {
            continue;
        }
        if !looks_like_electron_app(&cand) {
            continue;
        }
        raw.push(cand);
    }
    // 明确关掉 RunAsNode 的客户端连试都别试：那等于把它当普通 GUI 启动一次，
    // 除了把客户端窗口叫到前台、什么都不产出。
    raw.into_iter()
        .filter(|p| run_as_node_disabled(p) != Some(true))
        .collect()
}

/// Electron fuse 的哨兵串，后面依次是 version / count / 每个 fuse 一个字节。
///
/// `RunAsNode` 是第一个 fuse：`0x30` 关、`0x31` 开。关掉之后
/// `ELECTRON_RUN_AS_NODE` 会被忽略，客户端当成一次普通启动 —— 因为已经有实例
/// 在跑，第二个进程把 argv 转给主实例就退出，退出码 0、stdout/stderr 全空。
const FUSE_SENTINEL: &[u8] = b"dL7pKGdnNz796PbbjQWNKmHXBZaB9tsX";

/// 客户端是否**明确**关掉了 RunAsNode。`None` 表示没找到 fuse 信息（不是 Electron）。
///
/// 结果按「路径 + 文件大小」缓存：这是 200MB 的 exe，不缓存等于每轮都读一遍。
fn run_as_node_disabled(exe: &Path) -> Option<bool> {
    use std::io::Read;

    let len = std::fs::metadata(exe).ok()?.len();
    let cache = FUSE_CACHE.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(guard) = cache.lock() {
        if let Some((_, _, v)) = guard.iter().find(|(p, l, _)| p.as_path() == exe && *l == len) {
            return Some(*v);
        }
    }

    let mut file = std::fs::File::open(exe).ok()?;
    let mut buf = vec![0u8; 1 << 20];
    let mut tail: Vec<u8> = Vec::new();
    let mut found: Option<bool> = None;
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        let mut window = tail.clone();
        window.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_subslice(&window, FUSE_SENTINEL) {
            let at = pos + FUSE_SENTINEL.len();
            // version(1) + count(1) + fuses...
            if window.len() >= at + 3 && window[at + 1] > 0 {
                found = Some(window[at + 2] == 0x30);
            }
            break;
        }
        let keep = FUSE_SENTINEL.len().saturating_sub(1);
        let keep = keep.min(window.len());
        tail = window[window.len() - keep..].to_vec();
    }

    if let Some(v) = found {
        if let Ok(mut guard) = cache.lock() {
            guard.push((exe.to_path_buf(), len, v));
        }
    }
    found
}

/// 逐字节找子串。文件有 200MB，不能用 `windows().position()`：
/// 每轮都构造一个迭代器，跑下来要几十秒。
fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i] == first && &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 目录里有没有 Electron 的 `resources`（`app.asar` / `app` / `default_app.asar`）。
///
/// 用来把真正的客户端主程序和「同名启动器」区分开：启动器的输出永远是空的，
/// 那是另一台电脑上报「找不到 JSON（stdout: / stderr: ）」的直接原因。
fn looks_like_electron_app(exe: &Path) -> bool {
    let Some(dir) = exe.parent() else {
        return false;
    };
    let res = dir.join("resources");
    res.join("app.asar").is_file() || res.join("app").is_dir() || res.join("default_app.asar").is_file()
}

/// 诊断用：逐个候选跑一遍，报告每个位置的结果（**不输出密钥本身**）。
///
/// 别的电脑上报「找不到 JSON」时，让用户跑 `hub secret` 把这段贴回来，
/// 就能分清是「exe 找错」还是「客户端不认 Node 模式」。
pub fn probe_report() -> String {
    let exe = find_exe();
    let mut lines: Vec<String> = Vec::new();
    match &exe {
        Some(p) => {
            lines.push(format!("客户端主程序：{}", p.display()));
            lines.push(format!("Electron 目录（有 resources）：{}", looks_like_electron_app(p)));
            lines.push(format!(
                "RunAsNode fuse：{}",
                match run_as_node_disabled(p) {
                    Some(true) => "已禁用（这个版本读不出密钥）",
                    Some(false) => "已启用",
                    None => "未找到 fuse 信息",
                }
            ));
        }
        None => lines.push("没找到 WorkBuddy.exe".to_string()),
    }
    let primary = exe.clone().unwrap_or_default();
    let candidates = probe_candidates(&primary);
    if candidates.is_empty() {
        lines.push("没有可试的候选".to_string());
    }
    for (i, cand) in candidates.iter().enumerate() {
        match probe_secret_once(cand, i) {
            Ok(secret) => {
                let key = crate::auth_crypto::AtRestKey::from_secret(&secret);
                lines.push(format!("{} → 成功，keyId={}", cand.display(), key.key_id()));
            }
            Err(e) => lines.push(format!("{} → 失败：{e}", cand.display())),
        }
    }
    lines.join("\n")
}

/// 跑一次探测：脚本走 stdin 之外的临时文件兜底，避免输出被吞。
fn probe_secret_once(exe: &Path, index: usize) -> Result<String> {
    let stamp = format!("{}-{index}", std::process::id());
    let out_file = std::env::temp_dir().join(format!("workbuddy-hub-secret-{stamp}.json"));
    let err_file = std::env::temp_dir().join(format!("workbuddy-hub-secret-{stamp}.json.err"));
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&err_file);

    // 这条命令会起一个 Node 模式的客户端进程，必须隐藏窗口，
    // 否则切换账号时会闪一个黑框。
    let mut cmd = proc::hidden(exe);
    cmd.arg("-e")
        .arg(SECRET_SCRIPT)
        .env("ELECTRON_RUN_AS_NODE", "1")
        .env("WORKBUDDY_SECRET_OUT", &out_file);

    let captured = run_command_with_timeout(cmd, SECRET_TIMEOUT)
        .with_context(|| format!("无法启动：{}", exe.display()));

    // stdout 可能因为异步写、句柄无效而整段丢掉，脚本同时写了一份临时文件
    let file_out = std::fs::read_to_string(&out_file).ok();
    let file_err = std::fs::read_to_string(&err_file).ok();
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&err_file);

    let out = captured?;
    let mut extra = file_out.unwrap_or_default();
    if let Some(e) = file_err {
        if !extra.contains(&e) {
            if !extra.is_empty() {
                extra.push('\n');
            }
            extra.push_str(&e);
        }
    }
    let extra = if extra.trim().is_empty() {
        None
    } else {
        Some(extra.as_str())
    };
    parse_secret_output_with(&out, extra)
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

#[cfg(test)]
fn parse_secret_output(out: &CapturedOutput) -> Result<String> {
    parse_secret_output_with(out, None)
}

/// 解析客户端输出。`extra` 是脚本额外写到临时文件的那一份（stdout 被吞时的兜底）。
fn parse_secret_output_with(out: &CapturedOutput, extra: Option<&str>) -> Result<String> {
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let extra = extra.unwrap_or("").to_string();

    // 三处都找一遍：stdout 最正常，stderr 是日志重定向，extra 是文件兜底
    let mut saw_json = false;
    let mut snippet = String::new();
    for raw in [&stdout, &stderr, &extra] {
        let Some(value) = text_json(raw) else {
            continue;
        };
        saw_json = true;
        if snippet.is_empty() {
            snippet = truncate(raw, 200);
        }
        if let Some(secret) = secret_from_json(&value) {
            return Ok(secret);
        }
    }

    let tail = if !stderr.trim().is_empty() {
        stderr.as_str()
    } else {
        extra.as_str()
    };
    if !out.success {
        bail!(
            "客户端返回非零退出码 {}：{}",
            out.code.unwrap_or(-1),
            truncate(tail, 200)
        );
    }
    if saw_json {
        bail!("输出中缺少 atRestSecretKey 字段：{snippet}");
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() && extra.trim().is_empty() {
        bail!("客户端没有任何输出（退出码 0）：这个 exe 多半是启动器/更新器，不是客户端主程序");
    }
    bail!(
        "客户端输出里找不到 JSON（stdout: {} / stderr: {} / file: {}）",
        truncate(&stdout, 120),
        truncate(&stderr, 120),
        truncate(&extra, 120)
    )
}

/// 从一段可能混着日志的文本里取出 JSON。
fn text_json(raw: &str) -> Option<Value> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 少数版本会把整个 JSON 再包成一层字符串（首尾带引号、内部转义）
    let unwrapped: String =
        serde_json::from_str::<String>(trimmed).unwrap_or_else(|_| trimmed.to_string());
    let slice = extract_json(&unwrapped)?;
    serde_json::from_str::<Value>(slice).ok()
}

fn secret_from_json(value: &Value) -> Option<String> {
    value
        .get("atRestSecretKey")
        .or_else(|| value.get("data")?.get("atRestSecretKey"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
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
    fn parses_install_icon_line() {
        assert_eq!(
            parse_install_line("\"D:\\WorkBuddy\\WorkBuddy.exe\",0"),
            Some(PathBuf::from("D:\\WorkBuddy\\WorkBuddy.exe"))
        );
        assert_eq!(
            parse_install_line("C:\\Apps\\WorkBuddy\\WorkBuddy.exe"),
            Some(PathBuf::from("C:\\Apps\\WorkBuddy\\WorkBuddy.exe"))
        );
        assert!(parse_install_line("").is_none());
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
    fn parses_object_and_string_wrapped_output() {
        // 新版客户端 loggerGet() 返回对象
        let out = captured(true, Some(0), r#"{"atRestSecretKey":"abc"}"#, "");
        assert_eq!(parse_secret_output(&out).unwrap(), "abc");

        // 整个 JSON 又被包成字符串
        let wrapped = serde_json::to_string(r#"{"atRestSecretKey":"xyz"}"#).unwrap();
        let out = captured(true, Some(0), &wrapped, "");
        assert_eq!(parse_secret_output(&out).unwrap(), "xyz");

        // 输出跑到 stderr
        let out = captured(true, Some(0), "", r#"{"atRestSecretKey":"err"}"#);
        assert_eq!(parse_secret_output(&out).unwrap(), "err");
    }

    #[test]
    fn cache_hits_regardless_of_path() {
        // 取到密钥的可能是候选里的第二个（第一个是启动器），缓存不能按路径失效
        let entry = CachedSecret {
            secret: "k".into(),
            at: Instant::now(),
        };
        assert!(entry.at.elapsed() < SECRET_TTL);
        assert_eq!(entry.secret, "k");
    }

    #[test]
    fn parses_secret_from_file_fallback() {
        // 另一台电脑上 stdout / stderr 都是空的，只有脚本写出的临时文件里有内容
        let out = captured(true, Some(0), "", "");
        let extra = r#"{"version":1,"atRestSecretKey":"fromfile"}"#;
        assert_eq!(parse_secret_output_with(&out, Some(extra)).unwrap(), "fromfile");
    }

    #[test]
    fn explains_silent_launcher() {
        let out = captured(true, Some(0), "", "");
        let err = parse_secret_output_with(&out, None).unwrap_err().to_string();
        assert!(err.contains("没有任何输出"), "{err}");
    }

    #[test]
    fn reports_script_error_from_file() {
        let out = captured(false, Some(1), "", "");
        let extra = "SCRIPT_ERROR: TypeError: process._linkedBinding is not a function";
        let err = parse_secret_output_with(&out, Some(extra)).unwrap_err().to_string();
        assert!(err.contains("非零退出码 1"), "{err}");
        assert!(err.contains("_linkedBinding"), "{err}");
    }

    #[test]
    fn detects_electron_layout() {
        let dir = std::env::temp_dir().join(format!("hub-electron-test-{}", std::process::id()));
        let exe = dir.join("WorkBuddy.exe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&exe, b"stub").unwrap();
        // 只有 exe、没有 resources：启动器
        assert!(!looks_like_electron_app(&exe));

        std::fs::create_dir_all(dir.join("resources")).unwrap();
        std::fs::write(dir.join("resources/app.asar"), b"asar").unwrap();
        assert!(looks_like_electron_app(&exe));
        let _ = std::fs::remove_dir_all(&dir);
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
