use hub_core::{
    account_store, auth_crypto::AtRestKey, checkin, client_compat, daemon, profile, rate_limit,
    switch_engine, task_index, task_unify, transcript,
};
use serde::Serialize;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Serialize)]
pub struct ProbeOut {
    exe: Option<String>,
    auth_file: Option<String>,
    data_root: String,
    db_ok: bool,
    accounts_dir: String,
    at_rest_key_id: Option<String>,
}

fn db_path() -> Result<std::path::PathBuf, String> {
    let root = client_compat::data_root().map_err(|e| e.to_string())?;
    Ok(root.join("workbuddy.db"))
}

#[tauri::command]
fn probe() -> Result<ProbeOut, String> {
    let root = client_compat::data_root().map_err(|e| e.to_string())?;
    let accounts_dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;

    // 取密钥会启动一个短命的 Node 进程，放在这里只为在界面上显示 keyId；
    // 失败不影响其它功能。
    let at_rest_key_id = client_compat::find_exe()
        .and_then(|exe| client_compat::at_rest_secret(&exe).ok())
        .map(|s| AtRestKey::from_secret(&s).key_id().to_string());

    Ok(ProbeOut {
        exe: client_compat::find_exe().map(|p| p.display().to_string()),
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
    let dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    Ok(accounts.iter().map(|a| a.summary()).collect())
}

#[tauri::command]
fn current_uid() -> Result<String, String> {    let path = client_compat::auth_file().ok_or("找不到凭据文件")?;
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
fn switch_plan(target: String) -> Result<switch_engine::SwitchPlan, String> {
    let dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let matches: Vec<_> = accounts
        .iter()
        .filter(|a| a.uid.starts_with(target.as_str()))
        .collect();
    let account = match matches.len() {
        0 => return Err(format!("找不到 uid 以 {target} 开头的账号")),
        1 => matches[0],
        _ => return Err(format!("{target} 匹配到多个账号，请写长一点")),
    };

    let auth = client_compat::auth_file().ok_or("找不到凭据文件")?;
    let current = switch_engine::current_uid(&auth).map_err(|e| e.to_string())?;
    let tasks = task_index::load_tasks(&db_path()?).map_err(|e| e.to_string())?;
    Ok(switch_engine::plan(account, &current, &tasks))
}

/// 归集计划：把多少条会话改到目标账号名下。纯只读。
#[tauri::command]
fn unify_plan(target: Option<String>) -> Result<task_unify::UnifyPlan, String> {
    let db = db_path()?;
    let (_, ownership_path, _) = daemon::hub_paths().map_err(|e| e.to_string())?;
    let ownership = task_unify::Ownership::load(&ownership_path).map_err(|e| e.to_string())?;
    let target_uid = resolve_target_uid(target)?;
    task_unify::plan(&db, &ownership, &target_uid).map_err(|e| e.to_string())
}

/// 执行归集。会写数据库，写前自动做在线快照。
#[tauri::command]
fn unify_apply(target: Option<String>) -> Result<task_unify::UnifyReport, String> {
    let db = db_path()?;
    let (_, ownership_path, backup_dir) = daemon::hub_paths().map_err(|e| e.to_string())?;
    let target_uid = resolve_target_uid(target)?;
    task_unify::unify(&db, &ownership_path, &target_uid, &backup_dir, false)
        .map_err(|e| e.to_string())
}

/// 按原始归属表恢复。
#[tauri::command]
fn unify_rollback() -> Result<usize, String> {
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
fn session_detail(id: String) -> Result<transcript::Transcript, String> {
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
fn scan_limits() -> Result<Vec<rate_limit::LimitSignal>, String> {
    let tasks = task_index::load_tasks(&db_path()?).map_err(|e| e.to_string())?;
    let projects = client_compat::data_root()
        .map_err(|e| e.to_string())?
        .join("projects");
    let (runtime_path, _, _) = daemon::hub_paths().map_err(|e| e.to_string())?;
    let mut state = daemon::RuntimeState::load(&runtime_path);
    // 用一份临时状态扫，避免把界面上的「看一眼」变成「已处理」
    state.handled.clear();
    Ok(daemon::scan(&tasks, &projects, &mut state))
}

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "WorkBuddyHub";

/// 同一时间只允许一轮签到：调度线程和用户点「立即签到」可能撞在一起。
static CHECKIN_RUNNING: AtomicBool = AtomicBool::new(false);

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
        let dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;
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
            // 今天已经有结论的账号不再请求
            if let Some(r) = store.records.get(&a.uid) {
                if r.date == today && matches!(r.state.as_str(), "claimed" | "already" | "inactive") {
                    if r.state == "claimed" {
                        claimed += 1;
                    } else {
                        already += 1;
                    }
                    continue;
                }
                // 失败的不反复重试：同一账号 6 小时内只试一次（手动点击除外）
                if !force
                    && r.date == today
                    && r.state == "error"
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
            let rec = checkin::check_and_claim(&creds, &today);
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
        Ok(store)
    })();

    CHECKIN_RUNNING.store(false, Ordering::SeqCst);
    result
}

/// 今日签到概览（只读本地记录，不发请求）。
#[tauri::command]
fn checkin_overview() -> Result<checkin::CheckinStore, String> {
    Ok(checkin::CheckinStore::load(&checkin_store_path()?))
}

/// 立即签到。`uid` 为空时处理所有账号。
#[tauri::command]
fn checkin_run(app: tauri::AppHandle, uid: Option<String>) -> Result<checkin::CheckinStore, String> {
    // 手动点击：即使是今天失败过的账号也重试一次
    run_checkins(Some(&app), uid, true)
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
fn profiles_refresh(force: Option<bool>) -> Result<profile::ProfileStore, String> {
    let path = profile_store_path()?;
    let mut store = profile::ProfileStore::load(&path);
    let now = hub_core::now_secs();
    let force = force.unwrap_or(false);

    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe")?;
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;
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
fn switch_apply(app: tauri::AppHandle, target: String) -> Result<String, String> {
    use tauri::Emitter;

    let dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;
    let accounts = account_store::load(&dir).map_err(|e| e.to_string())?;
    let hits: Vec<_> = accounts
        .iter()
        .filter(|a| a.uid.starts_with(target.as_str()))
        .collect();
    let account = match hits.len() {
        0 => return Err(format!("找不到 uid 以 {target} 开头的账号")),
        1 => hits[0],
        _ => return Err(format!("{target} 匹配到多个账号，请写长一点")),
    };

    let exe = client_compat::find_exe().ok_or("找不到 WorkBuddy.exe")?;
    let auth_path = client_compat::auth_file().ok_or("找不到凭据文件")?;

    let _ = app.emit("switch-progress", "正在读取账号密钥…");
    let secret = client_compat::at_rest_secret(&exe).map_err(|e| e.to_string())?;
    let key = AtRestKey::from_secret(&secret);

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

    Ok(format!(
        "已切换到 {}，任务已归集。客户端正在重启。",
        &account.uid[..8]
    ))
}

/// 把 `None` / `"current"` 解析成当前登录账号的 uid。
fn resolve_target_uid(target: Option<String>) -> Result<String, String> {
    let auth = client_compat::auth_file().ok_or("找不到凭据文件")?;
    match target.as_deref() {
        None | Some("") | Some("current") => {
            switch_engine::current_uid(&auth).map_err(|e| e.to_string())
        }
        Some(prefix) => {
            let dir = client_compat::dsh_accounts_dir().map_err(|e| e.to_string())?;
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
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            use tauri::Manager;

            // WebView2 在窗口首次显示时偶尔不跟随窗口完成布局，
            // 显示后轻微改变一次尺寸能强制它重新计算视口。
            if let Some(w) = app.get_webview_window("main") {
                let fix = w.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(900));
                    let _ = fix.set_size(tauri::LogicalSize::new(1181.0, 761.0));
                    std::thread::sleep(std::time::Duration::from_millis(150));
                    let _ = fix.set_size(tauri::LogicalSize::new(1180.0, 760.0));
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

            // 单实例：托盘应用被重复启动会出现两个图标，而且用户点第二次
            // 往往以为「没反应」。这里用一个本地端口做互斥 —— 抢到端口的
            // 是主实例，抢不到的把已有窗口叫到前台再退出。
            {
                use std::io::Write;
                use std::net::{TcpListener, TcpStream};
                const PORT: u16 = 47821;
                match TcpListener::bind(("127.0.0.1", PORT)) {
                    Ok(listener) => {
                        let handle = app.handle().clone();
                        std::thread::spawn(move || {
                            for _ in listener.incoming().flatten() {
                                show_main(&handle);
                            }
                        });
                    }
                    Err(_) => {
                        // 端口被占：要么是已有实例，要么是别的程序。只有确认
                        // 对面能收下这条消息，才认定「已有实例」并退出；
                        // 否则照常启动，不要因为端口冲突把自己挡在门外。
                        let signaled = TcpStream::connect(("127.0.0.1", PORT))
                            .ok()
                            .and_then(|mut s| s.write_all(b"show").ok())
                            .is_some();
                        if signaled {
                            std::process::exit(0);
                        }
                    }
                }
            }

            // 账号资料：启动 8 秒后补一次（只有缺失或超过 7 天的才会真请求）
            {
                use tauri::Emitter;
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(8));
                    if let Ok(store) = profiles_refresh(None) {
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
            checkin_run,
            checkin_set_auto,
            profiles_overview,
            profiles_refresh
        ])
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}
