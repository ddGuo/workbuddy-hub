use hub_core::{
    account_store, auth_crypto::AtRestKey, auth_seen, checkin, client_compat, credit, daemon,
    model_probe, portable, profile, rate_limit, switch_engine, task_index, task_unify, transcript,
};
use serde::Serialize;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// `sync_accounts` 的返回：当前账号 uid，以及从旧版 DSH 导入了几条。
/// 新限流事件：推给界面做右下角提醒。
#[derive(Serialize, Clone)]
pub struct LimitAlert {
    signal: rate_limit::LimitSignal,
    /// 建议切换到的账号（已经过滤掉当前账号与被限流账号）
    next_uid: Option<String>,
}

#[derive(Serialize)]
pub struct SyncAccountsOut {
    current: Option<String>,
    imported: usize,
}

#[derive(Serialize)]
pub struct ProbeOut {
    exe: Option<String>,
    auth_file: Option<String>,
    data_root: String,
    db_ok: bool,
    accounts_dir: String,
    at_rest_key_id: Option<String>,
}


const ERR_NO_LOGIN: &str = "NO_LOGIN:";
const ERR_EXPIRED: &str = "EXPIRED:";

fn no_login() -> String {
    format!("{ERR_NO_LOGIN}客户端未登录，请先在 WorkBuddy 客户端登录至少一个账号")
}

fn expired(uid: &str) -> String {
    let short = &uid[..uid.len().min(8)];
    format!("{ERR_EXPIRED}账号 {short} 的凭据已过期，请先在客户端重新登录")
}

/// 把阻塞逻辑丢到 Tauri 的阻塞线程池。
///
/// 同步的 `#[tauri::command]` 会在 WebView2 的 IPC 回调（主线程）里内联执行，
/// 中间任何 sleep / 子进程等待都会让窗口「未响应」，进度事件也发不出去。
async fn on_blocking<T, F>(f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("后台任务异常退出：{e}"))?
}

static SWITCH_RUNNING: AtomicBool = AtomicBool::new(false);

/// 切号 / 归集互斥。Drop 释放，闭包 panic 也不会把锁留在手里。
struct BusyGuard(&'static AtomicBool);

impl BusyGuard {
    fn acquire(flag: &'static AtomicBool, msg: &str) -> Result<Self, String> {
        if flag.swap(true, Ordering::SeqCst) {
            Err(msg.to_string())
        } else {
            Ok(Self(flag))
        }
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn match_account<'a>(
    accounts: &'a [account_store::Account],
    target: &str,
) -> Result<&'a account_store::Account, String> {
    let hits: Vec<_> = accounts
        .iter()
        .filter(|a| a.uid.starts_with(target))
        .collect();
    match hits.len() {
        0 => Err(format!("找不到 uid 以 {target} 开头的账号")),
        1 => Ok(hits[0]),
        _ => Err(format!("{target} 匹配到多个账号，请写长一点")),
    }
}

/// 客户端登录态。永不抛错，前端可以安全地放进 Promise.all。
#[derive(Serialize)]
pub struct AuthState {
    logged_in: bool,
    current_uid: Option<String>,
    auth_file: Option<String>,
    /// no_auth_file / empty_uid / unreadable
    reason: Option<&'static str>,
}

#[tauri::command]
fn auth_state() -> AuthState {
    let Some(path) = client_compat::auth_file() else {
        return AuthState {
            logged_in: false,
            current_uid: None,
            auth_file: None,
            reason: Some("no_auth_file"),
        };
    };
    let file = Some(path.display().to_string());
    match switch_engine::current_uid(&path) {
        Ok(uid) if !uid.trim().is_empty() => AuthState {
            logged_in: true,
            current_uid: Some(uid),
            auth_file: file,
            reason: None,
        },
        Ok(_) => AuthState {
            logged_in: false,
            current_uid: None,
            auth_file: file,
            reason: Some("empty_uid"),
        },
        Err(_) => AuthState {
            logged_in: false,
            current_uid: None,
            auth_file: file,
            reason: Some("unreadable"),
        },
    }
}

fn db_path() -> Result<std::path::PathBuf, String> {
    let root = client_compat::data_root().map_err(|e| e.to_string())?;
    Ok(root.join("workbuddy.db"))
}

/// 同步账号库：把客户端当前登录的账号存一份，并把旧版 DSH 里没有的存档导进来。
#[tauri::command]
fn sync_accounts() -> Result<SyncAccountsOut, String> {
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let current = match client_compat::auth_file() {
        Some(auth) => account_store::save_current(&auth, &dir).map_err(|e| e.to_string())?,
        None => None,
    };
    let imported = match client_compat::legacy_accounts_dir() {
        Ok(dsh) => account_store::import_from_dsh(&dsh, &dir).map_err(|e| e.to_string())?,
        Err(_) => 0,
    };
    Ok(SyncAccountsOut { current, imported })
}

#[tauri::command]
async fn probe() -> Result<ProbeOut, String> {
    on_blocking(probe_blocking).await
}

fn probe_blocking() -> Result<ProbeOut, String> {
    let root = client_compat::data_root().map_err(|e| e.to_string())?;
    let accounts_dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;

    // 取密钥会启动一个短命的 Node 进程，放在这里只为在界面上显示 keyId；
    // 失败不影响其它功能。
    let exe = client_compat::find_exe();
    let at_rest_key_id = exe
        .as_deref()
        .and_then(|path| client_compat::at_rest_secret(path).ok())
        .map(|s| AtRestKey::from_secret(&s).key_id().to_string());

    Ok(ProbeOut {
        exe: exe.map(|p| p.display().to_string()),
        auth_file: client_compat::auth_file().map(|p| p.display().to_string()),
        data_root: root.display().to_string(),
        db_ok: db_path().map(|p| p.is_file()).unwrap_or(false),
        accounts_dir: accounts_dir.display().to_string(),
        at_rest_key_id,
    })
}

#[tauri::command]
fn list_tasks() -> Result<Vec<task_index::TaskRow>, String> {
    let db = db_path()?;
    task_index::load_tasks(&db).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_accounts() -> Result<Vec<account_store::AccountSummary>, String> {
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    Ok(accounts.iter().map(|a| a.summary()).collect())
}

#[tauri::command]
fn current_uid() -> Result<String, String> {
    let path = client_compat::auth_file().ok_or_else(no_login)?;
    switch_engine::current_uid(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_client() -> Result<(), String> {
    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe")?;
    Command::new(exe)
        .spawn()
        .map_err(|e| format!("启动客户端失败：{e}"))?;
    Ok(())
}

#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("路径为空".into());
    }
    if !std::path::Path::new(&path).exists() {
        return Err(format!("路径不存在：{path}"));
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("explorer")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("打开目录失败：{e}"))?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("打开目录失败：{e}"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("打开目录失败：{e}"))?;
    }
    Ok(())
}

/// 生成切号计划。纯只读，不写任何文件。
#[tauri::command]
async fn switch_plan(target: String) -> Result<switch_engine::SwitchPlan, String> {
    on_blocking(move || switch_plan_blocking(&target)).await
}

fn switch_plan_blocking(target: &str) -> Result<switch_engine::SwitchPlan, String> {
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let account = match_account(&accounts, target)?;

    // 先判登录态与有效期，别等用户确认完、客户端都退了才发现不可用
    let auth = client_compat::auth_file().ok_or_else(no_login)?;
    if account.refresh_expires_at <= hub_core::now_secs() {
        return Err(expired(&account.uid));
    }

    let current = switch_engine::current_uid(&auth).map_err(|e| e.to_string())?;
    let tasks = task_index::load_tasks(&db_path()?).map_err(|e| e.to_string())?;
    Ok(switch_engine::plan(account, &current, &tasks))
}

/// 归集计划：把多少条会话改到目标账号名下。纯只读。
#[tauri::command]
async fn unify_plan(target: Option<String>) -> Result<task_unify::UnifyPlan, String> {
    on_blocking(move || unify_plan_blocking(target)).await
}

fn unify_plan_blocking(target: Option<String>) -> Result<task_unify::UnifyPlan, String> {
    let db = db_path()?;
    let (_, ownership_path, _) = daemon::hub_paths().map_err(|e| e.to_string())?;
    let ownership = task_unify::Ownership::load(&ownership_path).map_err(|e| e.to_string())?;
    let target_uid = resolve_target_uid(target)?;
    task_unify::plan(&db, &ownership, &target_uid).map_err(|e| e.to_string())
}

/// 执行归集。会写数据库，写前自动做在线快照。
#[tauri::command]
async fn unify_apply(target: Option<String>) -> Result<task_unify::UnifyReport, String> {
    on_blocking(move || unify_apply_blocking(target)).await
}

fn unify_apply_blocking(target: Option<String>) -> Result<task_unify::UnifyReport, String> {
    let _guard = BusyGuard::acquire(&SWITCH_RUNNING, "正在切换或同步，请稍候")?;
    let db = db_path()?;
    let (_, ownership_path, backup_dir) = daemon::hub_paths().map_err(|e| e.to_string())?;
    let target_uid = resolve_target_uid(target)?;
    task_unify::unify(&db, &ownership_path, &target_uid, &backup_dir, false)
        .map_err(|e| e.to_string())
}

/// 按原始归属表恢复。
#[tauri::command]
async fn unify_rollback() -> Result<usize, String> {
    on_blocking(unify_rollback_blocking).await
}

fn unify_rollback_blocking() -> Result<usize, String> {
    let _guard = BusyGuard::acquire(&SWITCH_RUNNING, "正在切换或同步，请稍候")?;
    let db = db_path()?;
    let (_, ownership_path, backup_dir) = daemon::hub_paths().map_err(|e| e.to_string())?;
    task_unify::rollback(&db, &ownership_path, &backup_dir).map_err(|e| e.to_string())
}

/// 当前归集状态。
#[tauri::command]
fn unify_status() -> Result<task_unify::Ownership, String> {
    let (_, ownership_path, _) = daemon::hub_paths().map_err(|e| e.to_string())?;
    task_unify::status(&ownership_path).map_err(|e| e.to_string())
}

/// 读取一个会话的正文预览。
#[tauri::command]
async fn session_detail(id: String) -> Result<transcript::Transcript, String> {
    on_blocking(move || session_detail_blocking(&id)).await
}

fn session_detail_blocking(id: &str) -> Result<transcript::Transcript, String> {
    let tasks = task_index::load_tasks(&db_path()?).map_err(|e| e.to_string())?;
    let task = tasks
        .iter()
        .find(|t| t.id == id)
        .ok_or_else(|| format!("任务列表里没有 {id}"))?;
    let projects = client_compat::data_root()
        .map_err(|e| e.to_string())?
        .join("projects");
    let path = transcript::find_session_file(&projects, &id, &task.cwd)
        .ok_or_else(|| format!("找不到会话正文：{}", projects.display()))?;
    transcript::parse(&path).map_err(|e| format!("解析会话失败：{e}"))
}

/// 扫描最近的限流信号（只读，不改冷却表）。
#[tauri::command]
async fn scan_limits() -> Result<Vec<rate_limit::LimitSignal>, String> {
    on_blocking(scan_limits_blocking).await
}

fn scan_limits_blocking() -> Result<Vec<rate_limit::LimitSignal>, String> {
    let tasks = task_index::load_tasks(&db_path()?).map_err(|e| e.to_string())?;
    let projects = client_compat::data_root()
        .map_err(|e| e.to_string())?
        .join("projects");
    let (runtime_path, _, _) = daemon::hub_paths().map_err(|e| e.to_string())?;
    let mut state = daemon::RuntimeState::load(&runtime_path);
    // 用一份临时状态扫，避免把界面上的「看一眼」变成「已处理」
    state.handled.clear();
    let mut signals = daemon::scan(&tasks, &projects, &mut state);

    // 切号时任务会被归集到新账号，旧限流不该算到新账号头上：
    // 按「消息时间当时是谁在线」重新归属，而不是丢弃。
    if let Some(auth) = client_compat::auth_file() {
        let _ = auth_seen::refresh(&auth);
        if let Ok(path) = auth_seen::path() {
            let history = auth_seen::load(&path);
            for sig in &mut signals {
                if let Some(uid) = history.owner_at(sig.detected_at) {
                    sig.uid = uid;
                }
            }
        }
    }
    Ok(signals)
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "WorkBuddyHub";

/// 同一时间只允许一轮签到：调度线程和用户点「立即签到」可能撞在一起。
static CHECKIN_RUNNING: AtomicBool = AtomicBool::new(false);

/// 账号重新登录后，把当天残留的签到失败记录清掉，让下一轮立即重试。
fn clear_checkin_error(uid: &str) {
    let Ok(path) = checkin_store_path() else {
        return;
    };
    let mut store = checkin::CheckinStore::load(&path);
    let today = hub_core::format_date(hub_core::now_secs());
    let stale = store
        .records
        .get(uid)
        .map(|r| r.date == today && r.state == "error")
        .unwrap_or(false);
    if stale {
        store.records.remove(uid);
        let _ = store.save(&path);
    }
}

fn checkin_store_path() -> Result<std::path::PathBuf, String> {
    let hub = client_compat::hub_dir().map_err(|e| e.to_string())?;
    Ok(hub.join("checkin.json"))
}

/// 逐账号签到。已经签过的直接跳过，不再打接口。
///
/// `only` 给定时只处理匹配的账号（界面上单个账号补签）。
fn run_checkins(
    app: Option<&tauri::AppHandle>,
    only: Option<String>,
    force: bool,
) -> Result<checkin::CheckinStore, String> {
    use tauri::Emitter;

    if CHECKIN_RUNNING.swap(true, Ordering::SeqCst) {
        return Err("正在签到，请稍候".into());
    }
    let result = (|| -> Result<checkin::CheckinStore, String> {
        let path = checkin_store_path()?;
        let mut store = checkin::CheckinStore::load(&path);
        let today = hub_core::format_date(hub_core::now_secs());

        let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe，无法取账号密钥")?;
        let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
        let key = AtRestKey::from_secret(&secret);
        let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
        let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;

        let mut claimed = 0usize;
        let mut already = 0usize;
        let mut failed = 0usize;

        for a in &accounts {
            if let Some(f) = only.as_deref() {
                if !a.uid.starts_with(f) {
                    continue;
                }
            }
            // 已签过的账号也要拉一次状态：剩余积分会随使用变化，列表要显示最新值。
            // 只有「今天失败过」的账号在 6 小时内不重试（手动点击除外）。
            if let Some(r) = store.records.get(&a.uid) {
                // 401 不适用 6 小时退避：重新登录后令牌就变了，下一轮应该立刻重试
                if !force
                    && r.date == today
                    && r.state == "error"
                    && !r.message.contains("401")
                    && hub_core::now_secs() - r.updated_at < 6 * 3600
                {
                    failed += 1;
                    continue;
                }
            }
            if a.refresh_expires_at <= hub_core::now_secs() {
                continue;
            }
            if let Some(app) = app {
                let _ = app.emit("checkin-progress", format!("正在签到 {}", &a.uid[..8.min(a.uid.len())]));
            }
            let token = match a.access_token(&key) {
                Ok(t) => t,
                Err(e) => {
                    failed += 1;
                    store.records.insert(
                        a.uid.clone(),
                        checkin::CheckinRecord {
                            uid: a.uid.clone(),
                            date: today.clone(),
                            state: "error".into(),
                            credit: 0,
                            streak_days: 0,
                            total_credits: -1,
                            message: format!("解密凭据失败：{e}"),
                            updated_at: hub_core::now_secs(),
                        },
                    );
                    continue;
                }
            };
            let creds = checkin::Credentials {
                token: &token,
                uid: &a.uid,
                domain: &a.domain,
            };
            let mut rec = checkin::check_and_claim(&creds, &today);
            if rec.state == "error" && rec.message.contains("401") {
                rec.message = "登录态已失效，请重新登录该账号".into();
            }
            match rec.state.as_str() {
                "claimed" => claimed += 1,
                "already" => already += 1,
                "inactive" => {}
                _ => failed += 1,
            }
            store.records.insert(a.uid.clone(), rec);
        }

        store.last_run = hub_core::now_secs();
        store.last_summary = if failed > 0 {
            format!("新签 {claimed} · 已签 {already} · 失败 {failed}")
        } else {
            format!("新签 {claimed} · 已签 {already}")
        };
        store.save(&path).map_err(|e| e.to_string())?;
        if let Some(app) = app {
            let _ = app.emit("checkin-updated", &store);
        }
        Ok(store)
    })();

    CHECKIN_RUNNING.store(false, Ordering::SeqCst);
    result
}

fn credits_store_path() -> Result<std::path::PathBuf, String> {
    let hub = client_compat::hub_dir().map_err(|e| e.to_string())?;
    Ok(hub.join("credits.json"))
}

#[derive(Serialize)]
pub struct ImportOut {
    imported: usize,
    skipped: usize,
}

/// 把账号库里所有可用账号导出成一个密码加密的文件。
#[tauri::command]
async fn export_credentials(path: String, password: String) -> Result<usize, String> {
    on_blocking(move || export_credentials_blocking(&path, &password)).await
}

fn export_credentials_blocking(path: &str, password: &str) -> Result<usize, String> {
    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe，无法取账号密钥")?;
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let now = hub_core::now_secs();

    let mut payload = portable::ExportPayload::default();
    for a in &accounts {
        if a.refresh_expires_at <= now {
            continue;
        }
        let (Ok(access), Ok(refresh)) = (a.access_token(&key), a.refresh_token(&key)) else {
            continue;
        };
        payload.accounts.push(portable::ExportedAccount {
            uid: a.uid.clone(),
            domain: a.domain.clone(),
            expires_at: a.expires_at,
            refresh_expires_at: a.refresh_expires_at,
            access_token: access,
            refresh_token: refresh,
        });
    }
    if payload.accounts.is_empty() {
        return Err("没有可导出的账号：令牌都读不出来，或者都已过期".into());
    }

    let text = portable::seal(&payload, password).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("写入文件失败：{e}"))?;
    Ok(payload.accounts.len())
}

/// 导入密码加密的凭据文件，用本机 at-rest 密钥重新封装后写入账号库。
#[tauri::command]
async fn import_credentials(
    app: tauri::AppHandle,
    path: String,
    password: String,
) -> Result<ImportOut, String> {
    on_blocking(move || import_credentials_blocking(&app, &path, &password)).await
}

fn import_credentials_blocking(
    app: &tauri::AppHandle,
    path: &str,
    password: &str,
) -> Result<ImportOut, String> {
    use tauri::Emitter;

    let text = std::fs::read_to_string(path).map_err(|e| format!("读取文件失败：{e}"))?;
    let payload = portable::open(&text, password).map_err(|e| e.to_string())?;

    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe，无法取账号密钥")?;
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建账号库目录失败：{e}"))?;
    let existing = account_store::load(&dir).map_err(|e| e.to_string())?;

    let mut imported = 0usize;
    let mut skipped = 0usize;
    for acc in payload.accounts {
        let uid = acc.uid.trim();
        if uid.is_empty() || acc.access_token.trim().is_empty() || acc.refresh_token.trim().is_empty()
        {
            skipped += 1;
            continue;
        }
        // 本机已有更新的令牌就不要覆盖
        if let Some(old) = existing.iter().find(|a| a.uid == uid) {
            if old.refresh_expires_at >= acc.refresh_expires_at {
                skipped += 1;
                continue;
            }
        }

        let access_env = hub_core::auth_crypto::encrypt_field(&acc.access_token, &key, None)
            .map_err(|e| format!("封装令牌失败：{e}"))?;
        let refresh_env = hub_core::auth_crypto::encrypt_field(&acc.refresh_token, &key, None)
            .map_err(|e| format!("封装令牌失败：{e}"))?;
        let doc = serde_json::json!({
            "account": { "uid": uid },
            "auth": {
                "accessToken": { "$wbEncrypted": 1, "envelope": access_env },
                "refreshToken": { "$wbEncrypted": 1, "envelope": refresh_env },
                "expiresAt": acc.expires_at,
                "refreshExpiresAt": acc.refresh_expires_at,
                "domain": acc.domain,
            }
        });
        let target = dir.join(format!("{uid}.json"));
        std::fs::write(&target, serde_json::to_string_pretty(&doc).map_err(|e| e.to_string())?)
            .map_err(|e| format!("写入账号库失败：{e}"))?;
        imported += 1;
    }

    if imported > 0 {
        let _ = app.emit("accounts-updated", "");
    }
    Ok(ImportOut {
        imported,
        skipped,
    })
}

/// 用账号凭据 + 指定模型发一次最小请求，直接问服务端是否限流。
#[tauri::command]
async fn probe_model(uid: String, model: String) -> Result<model_probe::ProbeResult, String> {
    on_blocking(move || probe_model_blocking(&uid, &model)).await
}

fn probe_model_blocking(uid: &str, model: &str) -> Result<model_probe::ProbeResult, String> {
    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe，无法取账号密钥")?;
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let account = accounts
        .iter()
        .find(|a| a.uid.starts_with(uid))
        .ok_or_else(|| format!("找不到 uid 以 {uid} 开头的账号"))?;
    if account.refresh_expires_at <= hub_core::now_secs() {
        return Err(expired(&account.uid));
    }
    let token = account.access_token(&key).map_err(|e| e.to_string())?;
    let creds = checkin::Credentials {
        token: &token,
        uid: &account.uid,
        domain: &account.domain,
    };
    model_probe::probe(&creds, model).map_err(|e| e.to_string())
}

/// 剩余积分缓存（只读本地文件）。
#[tauri::command]
fn credits_overview() -> Result<credit::CreditStore, String> {
    Ok(credit::CreditStore::load(&credits_store_path()?))
}

/// 拉取各账号剩余积分。`uid` 为空时处理全部账号。
#[tauri::command]
async fn credits_refresh(
    app: tauri::AppHandle,
    uid: Option<String>,
) -> Result<credit::CreditStore, String> {
    on_blocking(move || credits_refresh_blocking(Some(&app), uid)).await
}

fn credits_refresh_blocking(
    app: Option<&tauri::AppHandle>,
    only: Option<String>,
) -> Result<credit::CreditStore, String> {
    use tauri::Emitter;

    let path = credits_store_path()?;
    let mut store = credit::CreditStore::load(&path);

    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe，无法取账号密钥")?;
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let now = hub_core::now_secs();

    for a in &accounts {
        if let Some(f) = only.as_deref() {
            if !a.uid.starts_with(f) {
                continue;
            }
        }
        if a.refresh_expires_at <= now {
            continue;
        }
        let Ok(token) = a.access_token(&key) else {
            continue;
        };
        let creds = checkin::Credentials {
            token: &token,
            uid: &a.uid,
            domain: &a.domain,
        };
        match credit::fetch(&creds) {
            Ok(summary) => {
                store.credits.insert(a.uid.clone(), summary);
            }
            Err(e) if e.to_string().contains("401") => {
                // 服务端不认这个令牌了，界面上直接标「登录失效」
                store.credits.insert(
                    a.uid.clone(),
                    credit::CreditSummary {
                        remaining: 0.0,
                        total: 0.0,
                        packages: 0,
                        updated_at: hub_core::now_secs(),
                        valid: false,
                    },
                );
            }
            Err(_) => {}
        }
    }

    store.save(&path).map_err(|e| e.to_string())?;
    if let Some(app) = app {
        let _ = app.emit("credits-updated", &store);
    }
    Ok(store)
}

/// 今日签到概览（只读本地记录，不发请求）。
#[tauri::command]
fn checkin_overview() -> Result<checkin::CheckinStore, String> {
    Ok(checkin::CheckinStore::load(&checkin_store_path()?))
}

/// 立即签到。`uid` 为空时处理所有账号。
#[tauri::command]
async fn checkin_run(
    app: tauri::AppHandle,
    uid: Option<String>,
) -> Result<checkin::CheckinStore, String> {
    on_blocking(move || {
        // 手动点击：即使是今天失败过的账号也重试一次
        run_checkins(Some(&app), uid, true)
    })
    .await
}

/// 开关自动签到。
#[tauri::command]
fn checkin_set_auto(enabled: bool) -> Result<checkin::CheckinStore, String> {
    let path = checkin_store_path()?;
    let mut store = checkin::CheckinStore::load(&path);
    store.auto = enabled;
    store.save(&path).map_err(|e| e.to_string())?;
    Ok(store)
}

fn profile_store_path() -> Result<std::path::PathBuf, String> {
    let hub = client_compat::hub_dir().map_err(|e| e.to_string())?;
    Ok(hub.join("profiles.json"))
}

/// 账号资料缓存（文件里只有脱敏手机号）。
#[tauri::command]
fn profiles_overview() -> Result<profile::ProfileStore, String> {
    Ok(profile::ProfileStore::load(&profile_store_path()?))
}

/// 拉取账号资料。默认只补缺的和超过 7 天的；`force` 时全部重拉。
#[tauri::command]
async fn profiles_refresh(force: Option<bool>) -> Result<profile::ProfileStore, String> {
    on_blocking(move || profiles_refresh_blocking(force)).await
}

fn profiles_refresh_blocking(force: Option<bool>) -> Result<profile::ProfileStore, String> {
    let path = profile_store_path()?;
    let mut store = profile::ProfileStore::load(&path);
    let now = hub_core::now_secs();
    let force = force.unwrap_or(false);

    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe")?;
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;

    for a in &accounts {
        if !force {
            if let Some(p) = store.profiles.get(&a.uid) {
                // 手机号不常变，7 天更新一次就够
                if !p.phone.is_empty() && now - p.fetched_at < 7 * 86_400 {
                    continue;
                }
            }
        }
        if a.refresh_expires_at <= now {
            continue;
        }
        let Ok(token) = a.access_token(&key) else {
            continue;
        };
        if let Ok(p) = profile::fetch(&a.domain, &token, &a.uid) {
            store.profiles.insert(a.uid.clone(), p);
        }
    }

    store.last_run = now;
    store.save(&path).map_err(|e| e.to_string())?;
    Ok(store)
}

/// 是否已加入开机自启（读 HKCU Run 项，不写任何东西）。
#[tauri::command]
fn autostart_get() -> bool {
    #[cfg(target_os = "windows")]
    {
        hub_core::proc::hidden("reg")
            .args(["query", RUN_KEY, "/v", RUN_NAME])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// 加入 / 取消开机自启。
///
/// 直接用注册表 `HKCU\...\Run`，不引入额外插件：这个位置只影响当前用户，
/// 不需要管理员权限，卸载时也能一眼看到并手动删掉。
#[tauri::command]
fn autostart_set(enabled: bool) -> Result<bool, String> {
    #[cfg(target_os = "windows")]
    {
        if enabled {
            let exe = std::env::current_exe().map_err(|e| format!("取程序路径失败：{e}"))?;
            let value = format!("\"{}\"", exe.display());
            let ok = hub_core::proc::hidden("reg")
                .args([
                    "add",
                    RUN_KEY,
                    "/v",
                    RUN_NAME,
                    "/t",
                    "REG_SZ",
                    "/d",
                    value.as_str(),
                    "/f",
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !ok {
                return Err("写入注册表失败".into());
            }
        } else {
            // 不存在时 reg 会返回非零，这里不需要区分
            let _ = hub_core::proc::hidden("reg")
                .args(["delete", RUN_KEY, "/v", RUN_NAME, "/f"])
                .output();
        }
        Ok(autostart_get())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = enabled;
        Err("开机自启目前只支持 Windows".into())
    }
}

/// 完整切换：退出客户端 → 写凭据 → 归集任务 → 启动客户端。
///
/// 这会重启 WorkBuddy，正在跑的任务会被中断。界面上必须先做二次确认。
/// 每一步都会通过 `switch-progress` 事件推给前端，避免长时间无反馈。
#[tauri::command]
async fn switch_apply(app: tauri::AppHandle, target: String) -> Result<String, String> {
    on_blocking(move || switch_apply_blocking(&app, &target)).await
}

fn switch_apply_blocking(app: &tauri::AppHandle, target: &str) -> Result<String, String> {
    use tauri::Emitter;

    let _guard = BusyGuard::acquire(&SWITCH_RUNNING, "正在切换账号，请稍候")?;

    let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let account = match_account(&accounts, target)?;

    // 切走之前先把**当前账号**的最新令牌存进账号库。
    // 否则刚在客户端登录完就切走，后台 10 秒轮询还没跑到，存档里还是旧令牌，
    // 下次切回来就会因为令牌过期/被轮换而要求重新登录。
    if let Some(auth) = client_compat::auth_file() {
        let _ = account_store::save_current(&auth, &dir);
    }

    // 登录态与有效期要在退出客户端之前判掉
    let auth_path = client_compat::auth_file().ok_or_else(no_login)?;
    if account.refresh_expires_at <= hub_core::now_secs() {
        return Err(expired(&account.uid));
    }

    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe")?;

    let _ = app.emit("switch-progress", "正在读取账号密钥…");
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);

    // 切之前先问服务端这个令牌还认不认。失效的账号写进去，客户端启动后
    // 只会把用户踢回登录页；不如在这里拦下来，并告诉用户去登录一次。
    let _ = app.emit("switch-progress", "正在校验账号登录态…");
    let short = &account.uid[..account.uid.len().min(8)];
    let token = account
        .access_token(&key)
        .map_err(|e| format!("读取账号令牌失败：{e}"))?;
    let probe = checkin::Credentials {
        token: &token,
        uid: &account.uid,
        domain: &account.domain,
    };
    // access token 还没到期却被 401，说明会话已被服务端吊销（换机沿用旧存档、
    // 在别处登录过同一账号），切过去只会被踢回登录页。
    // 如果 access token 本来就过期了，客户端还能用 refresh token 续期，放行。
    let access_expired = account.expires_at <= hub_core::now_secs();
    if !access_expired
        && matches!(credit::check_token(&probe), credit::TokenState::Invalid)
    {
        return Err(format!(
            "TOKEN_EXPIRED:账号 {short} 的登录态已失效，请先在 WorkBuddy 客户端登录一次该账号，再回来切换"
        ));
    }

    let db = db_path()?;
    let (_, ownership_path, backup_dir) = daemon::hub_paths().map_err(|e| e.to_string())?;

    let app2 = app.clone();
    daemon::switch_now(
        account,
        &key,
        &auth_path,
        &db,
        &ownership_path,
        &backup_dir,
        &exe,
        move |msg| {
            let _ = app2.emit("switch-progress", msg);
        },
    )?;

    Ok(format!("已切换到 {short}，任务已归集。客户端正在重启。"))
}

/// 把 `None` / `"current"` 解析成当前登录账号的 uid。
fn resolve_target_uid(target: Option<String>) -> Result<String, String> {
    let auth = client_compat::auth_file().ok_or_else(no_login)?;
    match target.as_deref() {
        None | Some("") | Some("current") => {
            switch_engine::current_uid(&auth).map_err(|e| e.to_string())
        }
        Some(prefix) => {
            let dir = client_compat::accounts_dir().map_err(|e| e.to_string())?;
            let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
            let hits: Vec<_> = accounts
                .iter()
                .filter(|a| a.uid.starts_with(prefix))
                .collect();
            match hits.len() {
                0 => Err(format!("找不到 uid 以 {prefix} 开头的账号")),
                1 => Ok(hits[0].uid.clone()),
                _ => Err(format!("{prefix} 匹配到多个账号，请写长一点")),
            }
        }
    }
}

/// 把主窗口拉回前台（托盘菜单 / 双击托盘图标都走这里）。
fn show_main(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        nudge_repaint(&w);
    }
}

enum Instance {
    /// 抢到了端口，负责监听后续实例的「显示窗口」请求
    Primary(std::net::TcpListener),
    /// 已有实例在跑，已经通知它把窗口叫到前台
    AlreadyRunning,
    /// 端口被别的程序占用：照常启动，只是失去互斥
    PortBusy,
}

/// 用 127.0.0.1:47821 做互斥。
///
/// 必须在**建窗口之前**调用：放在 setup 里的话，第二个进程会先建出一个
/// 黑窗口，再被退出逻辑杀掉，用户看到的就是一次黑屏闪烁。
fn claim_instance() -> Instance {
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    const PORT: u16 = 47821;

    match TcpListener::bind(("127.0.0.1", PORT)) {
        Ok(listener) => Instance::Primary(listener),
        Err(_) => {
            // 端口被占：只有对面真的收下消息，才认定是已有实例；
            // 否则可能只是别的程序占了这个端口，照常启动。
            let signaled = TcpStream::connect(("127.0.0.1", PORT))
                .ok()
                .and_then(|mut s| s.write_all(b"show").ok())
                .is_some();
            if signaled {
                Instance::AlreadyRunning
            } else {
                Instance::PortBusy
            }
        }
    }
}

/// WebView2 在窗口隐藏/恢复后偶尔只画一帧黑屏。
/// 轻微改一次尺寸再改回来，强制它重新计算视口 —— 放在后台线程里，不挡 UI。
fn nudge_repaint(w: &tauri::WebviewWindow) {
    let w = w.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(80));
        // 最大化窗口改尺寸会被系统当成还原，跳过
        if w.is_maximized().unwrap_or(false) {
            return;
        }
        let Ok(size) = w.inner_size() else {
            return;
        };
        let scale = w.scale_factor().unwrap_or(1.0);
        let logical = size.to_logical::<f64>(scale);
        let _ = w.set_size(tauri::LogicalSize::new(logical.width + 1.0, logical.height));
        std::thread::sleep(std::time::Duration::from_millis(120));
        let _ = w.set_size(tauri::LogicalSize::new(logical.width, logical.height));
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 单实例判定要在建窗口之前，避免第二个进程闪一个黑窗口
    let instance = claim_instance();
    if matches!(&instance, Instance::AlreadyRunning) {
        return;
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(move |app| {
            use tauri::Manager;

            // 首次显示时 WebView2 偶尔不跟随窗口完成布局，延后修正一次
            if let Some(w) = app.get_webview_window("main") {
                let fix = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(800));
                    nudge_repaint(&fix);
                });
            }

            // 托盘常驻：这是后台应用，关掉窗口不等于退出
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;

            let mut builder = TrayIconBuilder::new()
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("WorkBuddy Hub");
            if let Ok(img) = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png")) {
                builder = builder.icon(img);
            } else if let Some(icon) = app.default_window_icon().cloned() {
                builder = builder.icon(icon);
            }

            builder
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // 主实例负责接受「显示窗口」的请求
            if let Instance::Primary(listener) = instance {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    for _ in listener.incoming().flatten() {
                        show_main(&handle);
                    }
                });
            }

            // 剩余积分：当前账号每 30 秒刷新（余额随使用变化），
            // 其余账号每 10 分钟全量刷新一次。
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(25));
                    let mut last_full = 0i64;
                    loop {
                        let now = hub_core::now_secs();
                        if let Some(auth) = client_compat::auth_file() {
                            if let Ok(Some(seen)) = auth_seen::refresh(&auth) {
                                let _ = credits_refresh_blocking(Some(&handle), Some(seen.uid));
                            }
                        }
                        if now - last_full >= 600 {
                            let _ = credits_refresh_blocking(Some(&handle), None);
                            last_full = now;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(30));
                    }
                });
            }

            // 限流监听：每 5 秒扫一次最近会话（有 mtime 缓存，基本只剩 stat），
            // 发现新的限流就推给界面做右下角提醒。
            {
                use std::collections::HashSet;
                use tauri::Emitter;
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let mut seen: HashSet<String> = HashSet::new();
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        let Ok(db) = db_path() else { continue };
                        let Ok(tasks) = task_index::load_tasks(&db) else { continue };
                        let Ok(projects) = client_compat::data_root().map(|r| r.join("projects"))
                        else {
                            continue;
                        };
                        let Ok((runtime_path, _, _)) = daemon::hub_paths() else {
                            continue;
                        };
                        let mut state = daemon::RuntimeState::load(&runtime_path);
                        state.handled.clear();
                        let signals = daemon::scan(&tasks, &projects, &mut state);

                        if let Some(auth) = client_compat::auth_file() {
                            let _ = auth_seen::refresh(&auth);
                        }
                        let history = auth_seen::path()
                            .ok()
                            .map(|p| auth_seen::load(&p))
                            .unwrap_or_default();
                        let accounts = client_compat::accounts_dir()
                            .ok()
                            .and_then(|d| account_store::load(&d).ok())
                            .unwrap_or_default();
                        let profiles = client_compat::hub_dir()
                            .ok()
                            .map(|d| profile::ProfileStore::load(&d.join("profiles.json")))
                            .unwrap_or_default();
                        let now = hub_core::now_secs();
                        let current = history
                            .current()
                            .map(|s| s.uid.clone())
                            .unwrap_or_default();

                        for mut sig in signals {
                            // 按消息时间归属到当时在线的账号
                            if let Some(uid) = history.owner_at(sig.detected_at) {
                                sig.uid = uid;
                            }
                            // 只提醒刚发生的，避免启动时把历史全弹一遍
                            if now - sig.detected_at > 600 {
                                continue;
                            }
                            let key = format!("{}@{}", sig.session_id, sig.detected_at);
                            if !seen.insert(key) {
                                continue;
                            }
                            let next_uid = daemon::pick_next(&accounts, &state, &current, now)
                                .filter(|a| a.uid != sig.uid)
                                .map(|a| a.uid);
                            // 窗口不在前台时补一条系统通知，托盘状态下也能看到
                            let focused = handle
                                .get_webview_window("main")
                                .and_then(|w| w.is_focused().ok())
                                .unwrap_or(false);
                            if !focused {
                                use tauri_plugin_notification::NotificationExt;
                                let label = profiles
                                    .profiles
                                    .get(&sig.uid)
                                    .map(|p| p.phone.clone())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| sig.uid.chars().take(8).collect());
                                let model = sig.model.clone().unwrap_or_else(|| {
                                    if sig.kind == rate_limit::LimitKind::Account {
                                        "账号额度".to_string()
                                    } else {
                                        "未知模型".to_string()
                                    }
                                });
                                let body = match sig.reset_at {
                                    Some(t) => format!(
                                        "{label} · {model} · {} 恢复",
                                        hub_core::format_ts(t)
                                    ),
                                    None => format!("{label} · {model} · 限流中"),
                                };
                                let _ = handle
                                    .notification()
                                    .builder()
                                    .title("账号被限流")
                                    .body(body)
                                    .show();
                            }

                            let _ = handle.emit(
                                "limit-detected",
                                LimitAlert {
                                    signal: sig,
                                    next_uid,
                                },
                            );
                        }
                    }
                });
            }

            // 客户端登录新账号时自动存档：轮询凭据文件的 mtime，
            // 变化就把它写进 hub 自己的账号库，并通知前端刷新列表。
            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let mut last = None;
                    loop {
                        if let Some(auth) = client_compat::auth_file() {
                            let stamp = std::fs::metadata(&auth)
                                .and_then(|m| m.modified())
                                .ok();
                            if stamp.is_some() && stamp != last {
                                if let Ok(dir) = client_compat::accounts_dir() {
                                    if let Ok(Some(uid)) = account_store::save_current(&auth, &dir)
                                    {
                                        clear_checkin_error(&uid);
                                        let _ = handle.emit("accounts-updated", uid);
                                    }
                                }
                                last = stamp;
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_secs(10));
                    }
                });
            }

            // 账号资料：启动 8 秒后补一次（只有缺失或超过 7 天的才会真请求）
            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    if let Ok(store) = profiles_refresh_blocking(None) {
                        let _ = handle.emit("profiles-updated", store);
                    }
                });
            }

            // 每日签到：启动 20 秒后先跑一轮，之后每 30 分钟看一次。
            // 今天已有结论的账号会直接跳过，不会反复打接口。
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(20));
                    loop {
                        let auto = checkin_store_path()
                            .map(|p| checkin::CheckinStore::load(&p).auto)
                            .unwrap_or(true);
                        if auto {
                            let _ = run_checkins(Some(&handle), None, false);
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1800));
                    }
                });
            }

            Ok(())
        })
        // 关窗只隐藏：切号、限流守护都需要进程继续活着。
        // 真要退出走托盘菜单的「退出」。
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            probe,
            auth_state,
            sync_accounts,
            list_tasks,
            list_accounts,
            current_uid,
            open_client,
            open_folder,
            switch_plan,
            switch_apply,
            unify_plan,
            unify_apply,
            unify_rollback,
            unify_status,
            scan_limits,
            session_detail,
            autostart_get,
            autostart_set,
            checkin_overview,
            credits_overview,
            credits_refresh,
            probe_model,
            export_credentials,
            import_credentials,
            checkin_run,
            checkin_set_auto,
            profiles_overview,
            profiles_refresh
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
