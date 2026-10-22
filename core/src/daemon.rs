//! L1 半自动守护：发现限流 → 冷却当前账号 → 换号 → 归集 → 重启 → 提示继续。
//!
//! 它做不到「无感」，但能省掉「自己发现被限流、自己找账号、自己切」这三步。
//! 有意保留两个安全阀：
//!
//! - **有任务在跑就不切**。重启会打断所有进行中的任务，等它们结束再说。
//! - **冷却表持久化**。同一个账号不会因为同一条错误被反复切走。

use crate::client_control;
use crate::rate_limit::{self, LimitSignal};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cooldown {
    pub until: i64,
    pub strikes: u32,
    pub reason: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RuntimeState {
    pub version: u32,
    /// uid -> 冷却信息
    pub cooldowns: BTreeMap<String, Cooldown>,
    /// 已处理过的 session id，避免重复动作
    pub handled: Vec<String>,
    pub last_scan: i64,
}

impl RuntimeState {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Self {
                version: 1,
                ..Default::default()
            })
    }

    pub fn save(&self, path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, text);
        }
    }

    pub fn is_cooling(&self, uid: &str, now: i64) -> bool {
        self.cooldowns
            .get(uid)
            .map(|c| c.until > now)
            .unwrap_or(false)
    }

    pub fn mark(&mut self, uid: &str, until: i64, reason: &str) {
        let e = self.cooldowns.entry(uid.to_string()).or_insert(Cooldown {
            until,
            strikes: 0,
            reason: String::new(),
        });
        e.strikes += 1;
        e.until = until;
        e.reason = reason.to_string();
    }
}

/// 把 `cwd` 编码成 `projects/` 下的目录名。
///
/// 实测规则：`C:/Users/x` → `c-Users-x`，即 `:/` `:\` 先折叠成一个 `-`，
/// 其余分隔符各自变成 `-`，盘符转小写。
pub fn encode_cwd(cwd: &str) -> String {
    let s = cwd
        .replace(":/", "-")
        .replace(":\\", "-")
        .replace(['/', '\\'], "-");
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() => format!("{}{}", c.to_ascii_lowercase(), chars.as_str()),
        _ => s,
    }
}

/// 扫描所有 `status='error'` 的会话，返回其中的限流信号。
pub fn scan(
    tasks: &[crate::task_index::TaskRow],
    projects_dir: &Path,
    state: &mut RuntimeState,
) -> Vec<LimitSignal> {
    let mut out = Vec::new();
    for t in tasks.iter().filter(|t| t.status == "error") {
        if state.handled.iter().any(|h| h == &t.id) {
            continue;
        }
        let jsonl = projects_dir
            .join(encode_cwd(&t.cwd))
            .join(format!("{}.jsonl", t.id));
        if let Ok(Some(mut sig)) = rate_limit::scan_session(&jsonl, &t.id, &t.uid) {
            // 会话正文里没有「错误发生时间」，用任务的更新时间代替：
            // 界面据此区分「刚才被限流」和「几天前的旧记录」。
            sig.detected_at = t.updated_at;
            state.handled.push(t.id.clone());
            out.push(sig);
        }
    }
    out
}

/// 从账号里挑下一个可用的：不在冷却、不是当前账号、凭据未过期。
pub fn pick_next(
    accounts: &[crate::account_store::Account],
    state: &RuntimeState,
    current_uid: &str,
    now: i64,
) -> Option<crate::account_store::Account> {
    accounts
        .iter()
        .filter(|a| a.uid != current_uid)
        .filter(|a| a.refresh_expires_at > now)
        .filter(|a| !state.is_cooling(&a.uid, now))
        .max_by_key(|a| a.refresh_expires_at)
        .cloned()
}

/// 一次完整的换号动作。调用方负责确保此刻没有任务在跑。
///
/// 顺序：退出客户端 → 写凭据 → 归集任务 → 启动客户端。
/// 之所以先退再写，是避免与客户端争抢 auth 文件和数据库写锁。
///
/// `report` 会在每一步之前被调用，界面据此显示「正在切换…」的进度，
/// 而不是让用户对着一个没有反馈的按钮干等十几秒。
pub fn switch_now(
    target: &crate::account_store::Account,
    at_rest: &crate::auth_crypto::AtRestKey,
    auth_path: &Path,
    db: &Path,
    ownership_path: &Path,
    backup_dir: &Path,
    exe: &Path,
    mut report: impl FnMut(&str),
) -> Result<(), String> {
    report("正在退出客户端…");
    client_control::quit(std::time::Duration::from_secs(15))
        .map_err(|e| format!("退出客户端失败：{e}"))?;

    report("正在写入凭据…");
    crate::switch_engine::apply(target, at_rest, auth_path, backup_dir, false)
        .map_err(|e| format!("写入凭据失败：{e}"))?;

    report("正在同步任务到当前账号…");
    crate::task_unify::unify(db, ownership_path, &target.uid, backup_dir, false)
        .map_err(|e| format!("任务归集失败：{e}"))?;

    report("正在启动客户端…");
    client_control::start(exe).map_err(|e| format!("启动客户端失败：{e}"))?;

    report("等待客户端就绪…");
    let ok = client_control::wait_started(std::time::Duration::from_secs(20));
    report(if ok { "切换完成" } else { "已切换，但客户端启动较慢，请稍候" });
    Ok(())
}

pub fn hub_paths() -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let hub = crate::client_compat::hub_dir()?;
    Ok((
        hub.join("runtime.json"),
        hub.join("ownership.json"),
        hub.join("backups"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_windows_paths() {
        assert_eq!(
            encode_cwd("C:/Users/Administrator/WorkBuddy/2026-10-18-13-07-08"),
            "c-Users-Administrator-WorkBuddy-2026-10-18-13-07-08"
        );
        assert_eq!(
            encode_cwd(r"C:\Users\Administrator\WorkBuddy\2026-06-29-15-31-17"),
            "c-Users-Administrator-WorkBuddy-2026-06-29-15-31-17"
        );
        assert_eq!(encode_cwd(r"D:\workbuddy_p\yuyin"), "d-workbuddy_p-yuyin");
    }

    #[test]
    fn cooling_respects_until() {
        let mut s = RuntimeState::default();
        s.mark("uid_a", 2000, "429");
        assert!(s.is_cooling("uid_a", 1000));
        assert!(!s.is_cooling("uid_a", 3000));
        assert!(!s.is_cooling("uid_b", 1000));
    }
}
