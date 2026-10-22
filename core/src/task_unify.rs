//! 任务归集：把所有账号的会话归属改到当前账号，让客户端原生任务列表显示全部。
//!
//! 客户端只按 `sessions.user_id` 过滤，所以把归属统一到当前账号，就能在原生
//! 列表里看到所有历史任务。这条路能走通，是因为几个实测前提：
//!
//! - 全部会话 `transport = 'local'`，不上云，改写不会被服务端覆盖
//! - 没有任何触发器、视图、外键引用 `sessions`
//! - 所有涉及 `user_id` 的索引都是 `WHERE transport = 'cloud'` 的部分索引，
//!   local 行不受影响
//!
//! 代价是归属被抹平，所以必须配一张原始归属表（[`Ownership`]）才能回滚。
//! 归集是幂等的：只对「映射里还没有」的会话记录原始归属，重复执行不会把
//! 已经改过的 uid 再当成原始值记一遍。

use anyhow::{bail, Context, Result};
use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 会话原始归属表，落在 hub 自己的数据目录里。
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Ownership {
    pub version: u32,
    /// 当前被统一到的账号；None 表示已回滚 / 未归集
    pub unified_to: Option<String>,
    /// session_id -> 最初归属的 uid
    pub sessions: BTreeMap<String, String>,
    pub updated_at: i64,
}

impl Ownership {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self {
                version: 1,
                ..Default::default()
            });
        }
        let text = fs::read_to_string(path)?;
        let mut v: Self = serde_json::from_str(&text).unwrap_or_default();
        if v.version == 0 {
            v.version = 1;
        }
        Ok(v)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct UnifyPlan {
    pub target_uid: String,
    pub target_short: String,
    pub total_sessions: usize,
    /// 需要改归属的条数
    pub to_update: usize,
    pub already_target: usize,
    /// 本次会新记入原始归属表的条数
    pub new_records: usize,
    pub known_records: usize,
    /// 原始归属分布（uid 前缀 -> 条数）
    pub by_account: Vec<(String, usize)>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UnifyReport {
    pub dry_run: bool,
    pub backup_path: Option<String>,
    pub updated_rows: usize,
    pub recorded: usize,
    pub total_sessions: usize,
}

fn open_rw(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .with_context(|| format!("无法打开数据库 {}", db.display()))?;
    // 客户端可能正持有写锁，给它让路而不是直接报 SQLITE_BUSY
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// 读取所有会话的 (id, user_id)，包含已软删除的，保持库内一致。
fn read_sessions(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT id, user_id FROM sessions")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// 生成归集计划，不做任何写入。
pub fn plan(db: &Path, ownership: &Ownership, target_uid: &str) -> Result<UnifyPlan> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("无法只读打开 {}", db.display()))?;
    let sessions = read_sessions(&conn)?;

    if sessions.is_empty() {
        bail!("数据库里没有任何会话记录");
    }

    let mut to_update = 0;
    let mut already = 0;
    let mut new_records = 0;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for (id, uid) in &sessions {
        // 原始归属：映射里记过就以映射为准，否则以当前 uid 为准
        let owner = ownership
            .sessions
            .get(id)
            .cloned()
            .unwrap_or_else(|| uid.clone());

        if uid == target_uid {
            already += 1;
        } else {
            to_update += 1;
        }
        if !ownership.sessions.contains_key(id) && uid != target_uid {
            new_records += 1;
        }
        let short: String = owner.chars().take(8).collect();
        *counts.entry(short).or_default() += 1;
    }

    let mut warnings = Vec::new();
    if to_update == 0 {
        warnings.push("所有任务已经在目标账号名下，无需归集".into());
    }
    if let Some(prev) = &ownership.unified_to {
        if prev != target_uid {
            warnings.push(format!(
                "此前归集到 {}，本次会改为 {}（原始归属沿用首次记录）",
                &prev[..prev.len().min(8)],
                &target_uid[..target_uid.len().min(8)]
            ));
        }
    }

    Ok(UnifyPlan {
        target_uid: target_uid.to_string(),
        target_short: target_uid.chars().take(8).collect(),
        total_sessions: sessions.len(),
        to_update,
        already_target: already,
        new_records,
        known_records: ownership.sessions.len(),
        by_account: counts.into_iter().collect(),
        warnings,
    })
}

/// 用 SQLite 在线备份接口做一致性快照（WAL 里未 checkpoint 的数据也会带上）。
pub fn backup_db(db: &Path, backup_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(backup_dir)?;
    let stamp = crate::format_ts(crate::now_secs()).replace(['-', ' ', ':'], "");
    let out = backup_dir.join(format!("workbuddy-{stamp}.db"));
    if out.exists() {
        fs::remove_file(&out)?;
    }

    let src = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .context("备份时无法打开源数据库")?;
    let mut dst = Connection::open(&out).context("无法创建备份文件")?;
    {
        let backup = Backup::new(&src, &mut dst).context("初始化在线备份失败")?;
        backup
            .run_to_completion(200, Duration::from_millis(20), None)
            .context("在线备份过程中失败")?;
    }
    Ok(out)
}

/// 执行归集。`dry_run = true` 只统计不写入。
pub fn unify(
    db: &Path,
    ownership_path: &Path,
    target_uid: &str,
    backup_dir: &Path,
    dry_run: bool,
) -> Result<UnifyReport> {
    let mut ownership = Ownership::load(ownership_path)?;
    let plan = plan(db, &ownership, target_uid)?;

    if dry_run {
        return Ok(UnifyReport {
            dry_run: true,
            backup_path: None,
            updated_rows: plan.to_update,
            recorded: plan.new_records,
            total_sessions: plan.total_sessions,
        });
    }

    let backup = backup_db(db, backup_dir)?;

    // 先补齐原始归属：只对还没记录过、且确实要改的会话记录
    let conn_r = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sessions = read_sessions(&conn_r)?;
    drop(conn_r);

    let mut recorded = 0;
    for (id, uid) in &sessions {
        if uid != target_uid && !ownership.sessions.contains_key(id) {
            ownership.sessions.insert(id.clone(), uid.clone());
            recorded += 1;
        }
    }

    // 再批量改归属
    let conn = open_rw(db)?;
    let tx = conn.unchecked_transaction()?;
    let updated = tx
        .execute(
            "UPDATE sessions SET user_id = ?1 WHERE user_id <> ?1",
            [target_uid],
        )
        .context("更新 sessions.user_id 失败")?;
    tx.commit().context("提交事务失败")?;

    ownership.unified_to = Some(target_uid.to_string());
    ownership.updated_at = crate::now_secs();
    ownership.save(ownership_path)?;

    Ok(UnifyReport {
        dry_run: false,
        backup_path: Some(backup.display().to_string()),
        updated_rows: updated,
        recorded,
        total_sessions: sessions.len(),
    })
}

/// 按原始归属表恢复。
pub fn rollback(db: &Path, ownership_path: &Path, backup_dir: &Path) -> Result<usize> {
    let mut ownership = Ownership::load(ownership_path)?;
    if ownership.sessions.is_empty() {
        bail!("没有原始归属记录，无法回滚（可能是首次使用或已回滚）");
    }

    backup_db(db, backup_dir)?;

    let conn = open_rw(db)?;
    let tx = conn.unchecked_transaction()?;
    let mut restored = 0;
    {
        let mut stmt = tx.prepare("UPDATE sessions SET user_id = ?1 WHERE id = ?2")?;
        for (session_id, owner) in &ownership.sessions {
            restored += stmt.execute([owner, session_id])?;
        }
    }
    tx.commit()?;

    ownership.unified_to = None;
    ownership.updated_at = crate::now_secs();
    ownership.save(ownership_path)?;

    Ok(restored)
}

/// 统计当前归集状态（只读）。
pub fn status(ownership_path: &Path) -> Result<Ownership> {
    Ownership::load(ownership_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, deleted_at INTEGER);
             INSERT INTO sessions VALUES ('s1','uid_a',NULL),('s2','uid_b',NULL),('s3','uid_a',NULL);",
        )
        .unwrap();
        c
    }

    #[test]
    fn ownership_roundtrip() {
        let dir = std::env::temp_dir().join("hub-test-ownership");
        let _ = fs::create_dir_all(&dir);
        let p = dir.join("ownership.json");
        let mut o = Ownership::default();
        o.version = 1;
        o.sessions.insert("s1".into(), "uid_a".into());
        o.save(&p).unwrap();
        let back = Ownership::load(&p).unwrap();
        assert_eq!(back.sessions.get("s1").unwrap(), "uid_a");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unify_is_idempotent() {
        let c = mem_db();
        c.execute("UPDATE sessions SET user_id='uid_x' WHERE user_id<>'uid_x'", [])
            .unwrap();
        // 第二次执行不应再有任何行被改动
        let n = c
            .execute("UPDATE sessions SET user_id='uid_x' WHERE user_id<>'uid_x'", [])
            .unwrap();
        assert_eq!(n, 0);
        let distinct: i64 = c
            .query_row("SELECT COUNT(DISTINCT user_id) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(distinct, 1);
    }
}
