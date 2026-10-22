//! workbuddy-hub 验证工具。
//!
//! 用于在接 UI 之前先把两件事验证掉：
//! - M0：能否只读聚合出全部账号的任务
//! - M1：能否用 at-rest 密钥解开账号库里的凭据

use anyhow::{bail, Context, Result};
use hub_core::account_store::{self, Account};
use hub_core::auth_crypto::{self, AtRestKey};
use hub_core::checkin;
use hub_core::client_compat;
use hub_core::daemon;
use hub_core::profile;
use hub_core::switch_engine;
use hub_core::task_index;
use hub_core::task_unify;
use hub_core::transcript;
use hub_core::{format_date, format_ts, now_secs};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        "probe" => cmd_probe(),
        "accounts" => cmd_accounts(),
        "tasks" => cmd_tasks(&args[1..]),
        "verify" => cmd_verify(),
        "switch" => cmd_switch(&args[1..]),
        "unify" => cmd_unify(&args[1..]),
        "watch" => cmd_watch(&args[1..]),
        "transcript" => cmd_transcript(&args[1..]),
        "checkin" => cmd_checkin(&args[1..]),
        "profile" => cmd_profile(&args[1..]),
        "authhash" => cmd_authhash(),
        "enc-test" => cmd_enc_test(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            print_help();
            bail!("未知命令：{other}")
        }
    }
}

fn print_help() {
    println!("workbuddy-hub 验证工具");
    println!();
    println!("用法：hub <命令>");
    println!();
    println!("  probe      探测客户端位置、凭据文件、数据目录");
    println!("  accounts   列出账号库（含任务数、凭据到期）");
    println!("  tasks      列出所有账号的任务（可加 --uid <前缀> 过滤）");
    println!("  verify     用 at-rest 密钥逐个解密账号凭据，验证可用性");
    println!("  switch     切号：hub switch --to <uid前缀> [--force]（默认只演练）");
    println!("  unify      任务归集：hub unify [--to current|<uid>] [--force]");
    println!("             hub unify --status | --rollback");
    println!("  watch      限流守护：hub watch [--once] [--auto] [--interval 秒]");
    println!("  transcript 预览会话正文：hub transcript <session-id 前缀> [--tail N]");
    println!("  checkin    每日签到：hub checkin [--status] [--force-claim] [--uid <前缀>]");
    println!("  profile    读取账号资料（昵称 / 脱敏手机号）：hub profile [--uid <前缀>]");
    println!("  authhash   输出客户端凭据的哈希，用于与 TS 实现对拍");
}

fn cmd_probe() -> Result<()> {
    println!("== 客户端探测 ==");
    match client_compat::find_exe() {
        Some(p) => println!("  可执行文件 : {}", p.display()),
        None => println!("  可执行文件 : 未找到"),
    }
    match client_compat::auth_file() {
        Some(p) => println!("  凭据文件   : {}", p.display()),
        None => println!("  凭据文件   : 未找到"),
    }
    let root = client_compat::data_root()?;
    println!(
        "  数据根目录 : {} {}",
        root.display(),
        if root.is_dir() { "" } else { "(不存在)" }
    );
    let db = root.join("workbuddy.db");
    println!(
        "  会话数据库 : {} {}",
        db.display(),
        if db.is_file() { "" } else { "(不存在)" }
    );
    let accounts = client_compat::dsh_accounts_dir()?;
    println!(
        "  账号库目录 : {} {}",
        accounts.display(),
        if accounts.is_dir() { "" } else { "(不存在)" }
    );

    if let Some(exe) = client_compat::find_exe() {
        println!();
        println!("== at-rest 密钥 ==");
        match client_compat::at_rest_secret(&exe) {
            Ok(secret) => {
                let key = AtRestKey::from_secret(&secret);
                println!("  取密钥成功，keyId = {}", key.key_id());
            }
            Err(e) => println!("  取密钥失败：{e:#}"),
        }
    }
    Ok(())
}

fn load_accounts_with_counts() -> Result<(Vec<Account>, HashMap<String, usize>)> {
    let dir = client_compat::dsh_accounts_dir()?;
    let accounts = account_store::load(&dir)?;
    let db = client_compat::data_root()?.join("workbuddy.db");
    let mut counts = HashMap::new();
    if db.is_file() {
        for t in task_index::load_tasks(&db)? {
            *counts.entry(t.uid).or_insert(0) += 1;
        }
    }
    Ok((accounts, counts))
}

fn cmd_accounts() -> Result<()> {
    let (accounts, counts) = load_accounts_with_counts()?;
    let now = now_secs();

    println!("账号库共 {} 个唯一账号", accounts.len());
    println!();
    println!(
        "{:<10}{:<8}{:<14}{:<14}{:<8}{}",
        "uid", "任务", "access 到期", "refresh 到期", "状态", "备注"
    );
    println!("{}", "-".repeat(74));

    let mut with_tasks = 0;
    for a in &accounts {
        let n = counts.get(&a.uid).copied().unwrap_or(0);
        if n > 0 {
            with_tasks += 1;
        }
        let status = if a.refresh_expires_at <= now {
            "已失效"
        } else if a.expires_at <= now {
            "AT 过期"
        } else {
            "有效"
        };
        let mut note = String::new();
        if n == 0 {
            note.push_str("空账号");
        }
        if a.domain != "www.workbuddy.cn" {
            note.push_str(&format!(" {}", a.domain));
        }
        println!(
            "{:<10}{:<8}{:<14}{:<14}{:<8}{}",
            a.summary().short_uid,
            n,
            format_date(a.expires_at),
            format_date(a.refresh_expires_at),
            status,
            note
        );
    }

    println!();
    println!(
        "合计：{} 个账号，其中 {} 个有任务，共 {} 个任务",
        accounts.len(),
        with_tasks,
        counts.values().sum::<usize>()
    );
    Ok(())
}

fn cmd_tasks(args: &[String]) -> Result<()> {
    let filter = args
        .iter()
        .position(|a| a == "--uid")
        .and_then(|i| args.get(i + 1))
        .cloned();

    let db = find_db()?;
    let mut tasks = task_index::load_tasks(&db)?;
    if let Some(f) = &filter {
        tasks.retain(|t| t.uid.starts_with(f.as_str()));
    }

    let stats = task_index::stats(&tasks);
    println!(
        "共 {} 个任务，分布在 {} 个账号",
        stats.total, stats.accounts_with_tasks
    );
    println!(
        "状态分布：{}",
        stats
            .by_status
            .iter()
            .map(|(s, n)| format!("{s}={n}"))
            .collect::<Vec<_>>()
            .join("  ")
    );
    println!();
    println!(
        "{:<10}{:<10}{:<16}{}",
        "账号", "状态", "更新时间", "标题"
    );
    println!("{}", "-".repeat(88));
    for t in &tasks {
        let title = if t.title.chars().count() > 34 {
            let head: String = t.title.chars().take(34).collect();
            format!("{head}…")
        } else {
            t.title.clone()
        };
        println!(
            "{:<10}{:<10}{:<16}{}",
            t.short_uid,
            t.status,
            format_ts(t.updated_at),
            title
        );
    }
    Ok(())
}

fn find_db() -> Result<PathBuf> {
    let db = client_compat::data_root()?.join("workbuddy.db");
    if !db.is_file() {
        bail!("找不到会话数据库：{}", db.display());
    }
    Ok(db)
}

fn cmd_verify() -> Result<()> {
    let exe = client_compat::find_exe()
        .ok_or_else(|| anyhow::anyhow!("找不到 WorkBuddy.exe，可设 WORKBUDDY_ELECTRON_BIN 指定"))?;
    println!("客户端：{}", exe.display());
    let secret = client_compat::at_rest_secret(&exe)?;
    let key = AtRestKey::from_secret(&secret);
    println!("at-rest keyId：{}", key.key_id());
    println!();

    let (accounts, _) = load_accounts_with_counts()?;
    println!(
        "{:<10}{:<10}{:<10}{}",
        "uid", "access", "refresh", "结果"
    );
    println!("{}", "-".repeat(66));

    let mut ok = 0;
    for a in &accounts {
        let at = a.access_token(&key);
        let rt = a.refresh_token(&key);
        let u = a.summary().short_uid;
        match (&at, &rt) {
            (Ok(at), Ok(rt)) => {
                ok += 1;
                println!(
                    "{:<10}{:<10}{:<10}{}",
                    u,
                    format!("{} 字符", at.len()),
                    format!("{} 字符", rt.len()),
                    "解密成功"
                );
            }
            _ => {
                let err = at.err().or_else(|| rt.err()).unwrap();
                println!("{:<10}{:<10}{:<10}解密失败：{err:#}", u, "-", "-");
            }
        }
    }
    println!();
    println!("{}/{} 个账号凭据解密成功", ok, accounts.len());
    Ok(())
}

/// M1 对拍：输出客户端 auth 文件里两个令牌的长度与 SHA-256。
///
/// 与 `tools/parity.cjs` 的输出逐字段比对。只比哈希，不打印明文 ——
/// 对拍只需要确认两个实现结果一致，不需要把 token 落到终端里。
fn cmd_authhash() -> Result<()> {
    let exe = client_compat::find_exe()
        .ok_or_else(|| anyhow::anyhow!("找不到 WorkBuddy.exe，可设 WORKBUDDY_ELECTRON_BIN 指定"))?;
    let secret = client_compat::at_rest_secret(&exe)?;
    let key = AtRestKey::from_secret(&secret);

    let path = client_compat::auth_file()
        .ok_or_else(|| anyhow::anyhow!("找不到 workbuddy-desktop.info"))?;
    let text = std::fs::read_to_string(&path)?;
    let doc: Value = serde_json::from_str(&text)?;

    let access = read_auth_field(&doc, "accessToken", &key)?;
    let refresh = read_auth_field(&doc, "refreshToken", &key)?;
    let uid = doc
        .get("account")
        .and_then(|a| a.get("uid"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let out = serde_json::json!({
        "source": "rust",
        "uid": uid,
        "keyId": key.key_id(),
        "aadLen": auth_crypto::debug_aad_hex(&key, 1).len() / 2,
        "accessLen": access.chars().count(),
        "refreshLen": refresh.chars().count(),
        "accessSha256": sha256_hex(&access),
        "refreshSha256": sha256_hex(&refresh),
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

fn read_auth_field(doc: &Value, field: &str, key: &AtRestKey) -> Result<String> {
    let v = doc
        .get("auth")
        .and_then(|a| a.get(field))
        .ok_or_else(|| anyhow::anyhow!("缺少 auth.{field}"))?;
    if let Some(s) = v.as_str() {
        return Ok(s.to_string());
    }
    if auth_crypto::is_encrypted(v) {
        let envelope = v
            .get("envelope")
            .and_then(|e| e.as_str())
            .ok_or_else(|| anyhow::anyhow!("{field} 缺少 envelope"))?;
        return auth_crypto::decrypt_field(envelope, key);
    }
    bail!("{field} 既不是字符串也不是加密信封")
}

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// L1 半自动守护：发现限流 → 冷却 → 换号 → 归集 → 重启。
///
/// 默认只报告不动作；`--auto` 才会真的换号，而且必须满足「当前没有任务在跑」。
/// `--once` 扫一轮就退出，方便先确认检测是否准确。
/// 读取账号资料：昵称、脱敏手机号、账号类型。
///
/// 本地只有当前账号的资料，其余账号要各自打一次 `/console/accounts`。
fn cmd_profile(args: &[String]) -> Result<()> {
    let uid_filter = args
        .windows(2)
        .find(|w| w[0] == "--uid")
        .map(|w| w[1].clone());

    let exe = client_compat::find_exe().context("找不到 WorkBuddy.exe")?;
    let secret = client_compat::at_rest_secret(&exe)?;
    let key = AtRestKey::from_secret(&secret);
    let accounts = account_store::load(&client_compat::dsh_accounts_dir()?)?;
    let now = now_secs();

    println!("账号资料");
    for a in &accounts {
        if let Some(f) = &uid_filter {
            if !a.uid.starts_with(f.as_str()) {
                continue;
            }
        }
        let short = &a.uid[..a.uid.len().min(8)];
        if a.refresh_expires_at <= now {
            println!("  {short}  跳过：凭据已过期");
            continue;
        }
        let token = match a.access_token(&key) {
            Ok(t) => t,
            Err(e) => {
                println!("  {short}  解密失败：{e:#}");
                continue;
            }
        };
        match profile::fetch(&a.domain, &token, &a.uid) {
            Ok(p) => println!(
                "  {short}  {:<14} 昵称={:<10} 类型={}",
                p.label(),
                if p.nickname.is_empty() { "-" } else { &p.nickname },
                if p.account_type.is_empty() {
                    "-"
                } else {
                    &p.account_type
                }
            ),
            Err(e) => println!("  {short}  读取失败：{e:#}"),
        }
    }
    Ok(())
}

/// 每日签到：先用 --status 看一眼，再决定要不要真签。
fn cmd_checkin(args: &[String]) -> Result<()> {
    let status_only = args.iter().any(|a| a == "--status");
    // 诊断用：跳过本地判断，直接打领取接口，看服务端原始返回
    let force_claim = args.iter().any(|a| a == "--force-claim");
    let uid_filter = args
        .windows(2)
        .find(|w| w[0] == "--uid")
        .map(|w| w[1].clone());

    let exe = client_compat::find_exe().context("找不到 WorkBuddy.exe")?;
    let secret = client_compat::at_rest_secret(&exe)?;
    let key = AtRestKey::from_secret(&secret);
    let dir = client_compat::dsh_accounts_dir()?;
    let accounts = account_store::load(&dir)?;
    let now = now_secs();
    let today = format_date(now);

    println!(
        "{}签到 · {}",
        if status_only { "查询" } else { "" },
        today
    );
    for a in &accounts {
        if let Some(f) = &uid_filter {
            if !a.uid.starts_with(f.as_str()) {
                continue;
            }
        }
        let short = &a.uid[..a.uid.len().min(8)];
        if a.refresh_expires_at <= now {
            println!("  {short}  跳过：凭据已过期");
            continue;
        }
        let token = match a.access_token(&key) {
            Ok(t) => t,
            Err(e) => {
                println!("  {short}  解密失败：{e:#}");
                continue;
            }
        };
        let creds = checkin::Credentials {
            token: &token,
            uid: &a.uid,
            domain: &a.domain,
        };
        if force_claim {
            match checkin::claim(&creds) {
                Ok(o) => println!(
                    "  {short}  code={} credit={} streak={} msg={}",
                    o.code, o.credit, o.streak_days, o.msg
                ),
                Err(e) => println!("  {short}  请求失败：{e:#}"),
            }
            continue;
        }
        if status_only {
            match checkin::status(&creds) {
                Ok(st) => println!(
                    "  {short}  活动={} 今日已签={} 连签={}天 今日={} 累计={}",
                    st.active, st.today_checked_in, st.streak_days, st.today_credit, st.total_credits
                ),
                Err(e) => println!("  {short}  查询失败：{e:#}"),
            }
        } else {
            let r = checkin::check_and_claim(&creds, &today);
            println!("  {short}  {:<9} +{}  {}", r.state, r.credit, r.message);
        }
    }
    Ok(())
}

/// 预览一个会话的正文解析结果。
///
/// 界面上是抽屉，这里只打印统计和最后几条，用来确认解析规则没跑偏 ——
/// 尤其是 `<system-reminder>` 剥离和工具调用摘要，这两处最容易在真实数据上翻车。
fn cmd_transcript(args: &[String]) -> Result<()> {
    let prefix = args
        .first()
        .context("用法：hub transcript <session-id 前缀> [--tail N]")?;
    let tail: usize = args
        .iter()
        .position(|a| a == "--tail")
        .and_then(|i| args.get(i + 1))
        .and_then(|n| n.parse().ok())
        .unwrap_or(12);

    let db = client_compat::data_root()?.join("workbuddy.db");
    let tasks = task_index::load_tasks(&db)?;
    let hits: Vec<_> = tasks
        .iter()
        .filter(|t| t.id.starts_with(prefix.as_str()))
        .collect();
    let task = match hits.len() {
        0 => bail!("没有 id 以 {prefix} 开头的任务"),
        1 => hits[0],
        _ => bail!("{prefix} 匹配到 {} 个任务，请写长一点", hits.len()),
    };

    let projects = client_compat::data_root()?.join("projects");
    let path = transcript::find_session_file(&projects, &task.id, &task.cwd)
        .context("找不到会话正文文件")?;
    let t = transcript::parse(&path)?;

    println!("会话     : {}  {}", t.session_id, t.title.as_deref().unwrap_or("-"));
    println!("账号     : {}", &task.uid[..task.uid.len().min(8)]);
    println!("模型     : {}", t.model.as_deref().unwrap_or("-"));
    println!(
        "时间     : {} → {}",
        format_ts(t.started_at),
        format_ts(t.ended_at)
    );
    println!("文件     : {} ({:.1} KB)", t.path, t.size_bytes as f64 / 1024.0);
    println!(
        "对话     : {} 问 / {} 答；工具 {} 次；正文 {} 字，思考 {} 字",
        t.stats.user_turns,
        t.stats.assistant_turns,
        t.stats.tool_calls,
        t.stats.total_chars,
        t.stats.reasoning_chars
    );
    if !t.stats.tools.is_empty() {
        let top: Vec<String> = t
            .stats
            .tools
            .iter()
            .take(6)
            .map(|(n, c)| format!("{n}×{c}"))
            .collect();
        println!("工具分布 : {}", top.join("  "));
    }
    if t.stats.dropped_entries > 0 {
        println!(
            "注意     : 只返回最近 {} 条，{} 条更早记录已省略",
            t.entries.len(),
            t.stats.dropped_entries
        );
    }

    println!("\n最后 {} 条：", tail.min(t.entries.len()));
    for e in t.entries.iter().rev().take(tail).collect::<Vec<_>>().iter().rev() {
        let who = match e.kind.as_str() {
            "user" => "你".to_string(),
            "assistant" => "助手".to_string(),
            "tool" => format!("⚙ {}", e.name.as_deref().unwrap_or("tool")),
            "tool_result" => format!("  ↳ {}", e.status.as_deref().unwrap_or("result")),
            other => other.to_string(),
        };
        let text = e.text.replace('\n', " ");
        let text: String = text.chars().take(140).collect();
        let full = format_ts(e.ts);
        let clock = if full.len() >= 16 { &full[11..] } else { full.as_str() };
        println!("  [{}] {:<12} {}", clock, who, text);
    }
    Ok(())
}

fn cmd_watch(args: &[String]) -> Result<()> {
    let auto = args.iter().any(|a| a == "--auto");
    let once = args.iter().any(|a| a == "--once");
    let interval = args
        .iter()
        .position(|a| a == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(5);

    let (runtime_path, ownership_path, backup_dir) = daemon::hub_paths()?;
    let projects = client_compat::data_root()?.join("projects");
    let db = find_db()?;

    println!(
        "监控中（{} 秒一轮，{}）",
        interval,
        if auto { "自动换号" } else { "仅报告，加 --auto 才动作" }
    );

    loop {
        let now = now_secs();
        let tasks = task_index::load_tasks(&db)?;
        let working = tasks.iter().filter(|t| t.status == "working").count();
        let mut state = daemon::RuntimeState::load(&runtime_path);

        let signals = daemon::scan(&tasks, &projects, &mut state);

        for sig in &signals {
            let uid_short: String = sig.uid.chars().take(8).collect();
            let until = sig.cooldown_until(now, state.cooldowns.get(&sig.uid).map(|c| c.strikes).unwrap_or(0));
            println!();
            println!("[{}] 检测到限流", format_ts(now));
            println!("  账号   : {uid_short}");
            println!("  范围   : {}", match sig.kind {
                hub_core::rate_limit::LimitKind::Account => "账号额度",
                hub_core::rate_limit::LimitKind::Model => "单模型",
            });
            println!("  冷却至 : {}", format_ts(until));
            println!("  原文   : {}", sig.message);
            state.mark(&sig.uid, until, &sig.message);

            if !auto {
                continue;
            }
            if working > 0 {
                println!("  动作   : 跳过 —— 还有 {working} 个任务在跑，重启会打断它们");
                continue;
            }

            let accounts = account_store::load(&client_compat::dsh_accounts_dir()?)?;
            let auth_path = client_compat::auth_file()
                .ok_or_else(|| anyhow::anyhow!("找不到凭据文件"))?;
            let current = switch_engine::current_uid(&auth_path)?;
            let Some(target) = daemon::pick_next(&accounts, &state, &current, now) else {
                println!("  动作   : 没有可用账号，全部在冷却中");
                continue;
            };

            let exe = client_compat::find_exe()
                .ok_or_else(|| anyhow::anyhow!("找不到 WorkBuddy.exe"))?;
            let secret = client_compat::at_rest_secret(&exe)?;
            let key = AtRestKey::from_secret(&secret);

            println!("  动作   : 切换到 {}", &target.uid[..8]);
            match daemon::switch_now(
                &target,
                &key,
                &auth_path,
                &db,
                &ownership_path,
                &backup_dir,
                &exe,
                |msg| println!("  · {msg}"),
            ) {
                Ok(()) => println!("  结果   : 已切换并归集，重启后任务留在当前账号下"),
                Err(e) => println!("  结果   : 失败 —— {e}"),
            }
        }

        state.last_scan = now;
        state.save(&runtime_path);

        if once {
            if signals.is_empty() {
                println!("[{}] 未发现限流信号", format_ts(now));
            }
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}

/// 任务归集：把所有账号的会话归属改到目标账号，让客户端原生列表显示全部。
///
/// 默认只演练，`--force` 才写库；写库前会自动做一次一致性快照，
/// 原始归属记在 `~/.workbuddy-hub/ownership.json`，可随时 `--rollback` 恢复。
fn cmd_unify(args: &[String]) -> Result<()> {
    let hub = client_compat::hub_dir()?;
    let ownership_path = hub.join("ownership.json");
    let backup_dir = hub.join("backups");
    let db = find_db()?;

    if args.iter().any(|a| a == "--status") {
        let o = task_unify::status(&ownership_path)?;
        println!("== 归集状态 ==");
        match &o.unified_to {
            Some(uid) => println!("  当前归集到 : {}", &uid[..uid.len().min(8)]),
            None => println!("  当前归集到 : （未归集）"),
        }
        println!("  归属记录数 : {}", o.sessions.len());
        if o.updated_at > 0 {
            println!("  最近更新   : {}", format_ts(o.updated_at));
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--rollback") {
        let n = task_unify::rollback(&db, &ownership_path, &backup_dir)?;
        println!("已恢复 {n} 条记录的原始归属。重启客户端后生效。");
        return Ok(());
    }

    let to = args
        .iter()
        .position(|a| a == "--to")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "current".to_string());
    let force = args.iter().any(|a| a == "--force");
    let dry_run = !force;

    let auth = client_compat::auth_file()
        .ok_or_else(|| anyhow::anyhow!("找不到凭据文件"))?;
    let target_uid = if to == "current" {
        switch_engine::current_uid(&auth)?
    } else {
        let (accounts, _) = load_accounts_with_counts()?;
        let hits: Vec<&Account> = accounts
            .iter()
            .filter(|a| a.uid.starts_with(to.as_str()))
            .collect();
        match hits.len() {
            0 => bail!("找不到 uid 以 {to} 开头的账号"),
            1 => hits[0].uid.clone(),
            _ => bail!("{to} 匹配到多个账号，请写长一点"),
        }
    };

    let ownership = task_unify::Ownership::load(&ownership_path)?;
    let plan = task_unify::plan(&db, &ownership, &target_uid)?;

    println!("== 归集计划 ==");
    println!("  目标账号 : {}", plan.target_short);
    println!("  会话总数 : {}", plan.total_sessions);
    println!("  需改归属 : {} 条", plan.to_update);
    println!("  已在名下 : {} 条", plan.already_target);
    println!(
        "  归属表   : 已有 {} 条记录，本次新增 {} 条",
        plan.known_records, plan.new_records
    );
    println!("  原始分布 :");
    for (uid, n) in &plan.by_account {
        println!("    {uid}  {n} 条");
    }
    if !plan.warnings.is_empty() {
        println!("  提醒：");
        for w in &plan.warnings {
            println!("    · {w}");
        }
    }
    println!();

    let report = task_unify::unify(&db, &ownership_path, &target_uid, &backup_dir, dry_run)?;

    if report.dry_run {
        println!("演练完成，未改动数据库。");
        println!("真实执行请加 --force。重启客户端后，所有任务会出现在当前账号的列表里。");
    } else {
        println!("已归集 {} 条会话，新增 {} 条原始归属记录。", report.updated_rows, report.recorded);
        if let Some(b) = &report.backup_path {
            println!("快照：{b}");
        }
        println!("重启客户端后生效。回滚用：hub unify --rollback");
    }
    Ok(())
}

/// 切号。默认只演练，`--force` 才会真的改写凭据文件。
///
/// 注意：本命令只负责「写凭据」，不负责退出/重启客户端 ——
/// 那一步会中断正在跑的任务，留给上层在得到明确确认后再做。
fn cmd_switch(args: &[String]) -> Result<()> {
    let to = args
        .iter()
        .position(|a| a == "--to")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("用法：hub switch --to <uid前缀> [--force]"))?;
    let force = args.iter().any(|a| a == "--force");

    let (accounts, _) = load_accounts_with_counts()?;
    let matches: Vec<&Account> = accounts
        .iter()
        .filter(|a| a.uid.starts_with(to.as_str()))
        .collect();
    let target = match matches.len() {
        0 => bail!("找不到 uid 以 {to} 开头的账号"),
        1 => matches[0],
        _ => bail!("{to} 匹配到多个账号，请写长一点"),
    };

    let auth_path = client_compat::auth_file()
        .ok_or_else(|| anyhow::anyhow!("找不到 workbuddy-desktop.info"))?;
    let current = switch_engine::current_uid(&auth_path)?;
    let db = find_db()?;
    let tasks = task_index::load_tasks(&db)?;

    let plan = switch_engine::plan(target, &current, &tasks);
    println!("== 切号计划 ==");
    println!("  当前账号 : {}", plan.current_short);
    println!("  目标账号 : {}", plan.target_short);
    println!("  客户端   : {}", if plan.client_running { "运行中" } else { "未运行" });
    println!("  目标任务 : {} 个", plan.target_task_count);
    if !plan.warnings.is_empty() {
        println!("  提醒：");
        for w in &plan.warnings {
            println!("    · {w}");
        }
    }
    println!();

    let exe = client_compat::find_exe()
        .ok_or_else(|| anyhow::anyhow!("找不到 WorkBuddy.exe"))?;
    let secret = client_compat::at_rest_secret(&exe)?;
    let key = AtRestKey::from_secret(&secret);

    let backup_dir = client_compat::data_root()?
        .parent()
        .map(|p| p.join(".workbuddy-hub/backups"))
        .unwrap_or_else(|| PathBuf::from(".workbuddy-hub/backups"));

    let report = switch_engine::apply(target, &key, &auth_path, &backup_dir, !force)?;

    println!("== 执行 ==");
    for s in &report.steps {
        println!("  · {s}");
    }
    println!();
    if report.dry_run {
        println!("演练完成，未改动任何文件。");
        println!("真实执行请加 --force（会改写凭据文件，但不重启客户端）。");
    } else {
        println!("凭据已写入。请手动重启客户端使其生效。");
        if let Some(b) = &report.backup_path {
            println!("备份：{b}");
        }
    }
    Ok(())
}

/// M2 前的反向验证：用 Rust 加密一段固定明文，输出 envelope。
///
/// 交给 `tools/parity-dec.cjs` 用 TS 实现解开，两边明文哈希一致才说明
/// 「写回客户端凭据」这条路是通的。
fn cmd_enc_test() -> Result<()> {
    let exe = client_compat::find_exe()
        .ok_or_else(|| anyhow::anyhow!("找不到 WorkBuddy.exe，可设 WORKBUDDY_ELECTRON_BIN 指定"))?;
    let secret = client_compat::at_rest_secret(&exe)?;
    let key = AtRestKey::from_secret(&secret);

    let plain = "workbuddy-hub-parity-test-2026-10-21";
    let envelope = auth_crypto::encrypt_field(plain, &key, None)?;

    let out = serde_json::json!({
        "source": "rust",
        "envelope": envelope,
        "plainSha256": sha256_hex(plain),
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}
