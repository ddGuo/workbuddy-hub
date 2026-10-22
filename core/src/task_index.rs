//! 任务索引：只读聚合 `~/.workbuddy/workbuddy.db` 中所有账号的会话。
//!
//! 客户端按 `user_id` 过滤任务列表，这里去掉那个过滤条件，得到全部账号的任务。
//! 全程只读，不写库、不改 `sessions.user_id`。

use crate::normalize_ts;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize, Clone)]
pub struct TaskRow {
    pub id: String,
    pub uid: String,
    pub short_uid: String,
    pub cwd: String,
    pub title: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Serialize)]
pub struct TaskStats {
    pub total: usize,
    pub accounts_with_tasks: usize,
    pub by_status: Vec<(String, usize)>,
}

/// 打开数据库（只读）。WAL 模式下要能读到尚未 checkpoint 的最新写入，
/// 因此不能用 immutable=1，只保留 mode=ro。
fn open_ro(db: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    Connection::open_with_flags(db, flags)
        .with_context(|| format!("无法只读打开 {}", db.display()))
}

/// 读取全部未删除的会话。
pub fn load_tasks(db: &Path) -> Result<Vec<TaskRow>> {
    let conn = open_ro(db)?;
    let mut stmt = conn.prepare(
        "SELECT id, user_id, cwd, title, status, created_at, updated_at
         FROM sessions
         WHERE deleted_at IS NULL
         ORDER BY updated_at DESC",
    )?;

    let rows = stmt
        .query_map([], |r| {
            let uid: String = r.get(1)?;
            let title: Option<String> = r.get(3)?;
            let cwd: Option<String> = r.get(2)?;
            Ok(TaskRow {
                id: r.get(0)?,
                short_uid: uid.chars().take(8).collect(),
                uid,
                cwd: cwd.unwrap_or_default(),
                title: title.unwrap_or_else(|| "(未命名任务)".into()),
                status: r.get(4)?,
                created_at: normalize_ts(r.get::<_, i64>(5)?),
                updated_at: normalize_ts(r.get::<_, i64>(6)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

pub fn stats(tasks: &[TaskRow]) -> TaskStats {
    let mut by_status: std::collections::BTreeMap<String, usize> = Default::default();
    let mut uids: std::collections::BTreeSet<&str> = Default::default();
    for t in tasks {
        *by_status.entry(t.status.clone()).or_default() += 1;
        uids.insert(t.uid.as_str());
    }
    TaskStats {
        total: tasks.len(),
        accounts_with_tasks: uids.len(),
        by_status: by_status.into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_real_db_if_present() {
        let root = match crate::client_compat::data_root() {
            Ok(p) => p.join("workbuddy.db"),
            Err(_) => return,
        };
        if !root.is_file() {
            return;
        }
        let tasks = load_tasks(&root).expect("应能只读读取任务");
        assert!(!tasks.is_empty());
        // 所有时间戳都应是合理的秒级值（2001 年之后）
        for t in &tasks {
            assert!(t.updated_at > 1_000_000_000, "时间戳未归一化：{}", t.updated_at);
        }
    }
}
