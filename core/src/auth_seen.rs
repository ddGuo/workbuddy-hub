//! 当前账号的生效时间线。
//!
//! 限流原文里没有账号身份，归属只能靠会话的 `user_id`。但切号时 hub 会把任务
//! 归集到新账号名下，旧会话里的旧限流就会被算到新账号头上。这里记录每次
//! 「当前账号变成谁」的时间点，按消息时间反查当时是谁在线：
//!
//! - 09:51 的限流 → 当时是 9287 → 算 9287
//! - 17:35 切到 4303 → 这条历史不影响 4303

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthSeen {
    pub uid: String,
    /// 这个账号成为当前账号的时间（unix 秒）
    pub since: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthHistory {
    pub entries: Vec<AuthSeen>,
}

impl AuthHistory {
    /// 距离 `ts` 最近的一次账号变更；早于所有记录时取最早的那条。
    pub fn owner_at(&self, ts: i64) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut owner = &self.entries[0];
        for e in &self.entries {
            if e.since <= ts {
                owner = e;
            } else {
                break;
            }
        }
        Some(owner.uid.clone())
    }

    pub fn current(&self) -> Option<&AuthSeen> {
        self.entries.last()
    }
}

pub fn path() -> Result<PathBuf> {
    Ok(crate::client_compat::hub_dir()?.join("auth_seen.json"))
}

/// 兼容旧格式：单条 `{uid, since}`。
pub fn load(path: &Path) -> AuthHistory {
    let Ok(text) = fs::read_to_string(path) else {
        return AuthHistory::default();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return AuthHistory::default();
    };
    if let Some(list) = v.get("entries").and_then(Value::as_array) {
        let entries: Vec<AuthSeen> = list
            .iter()
            .filter_map(|e| serde_json::from_value(e.clone()).ok())
            .collect();
        return AuthHistory { entries };
    }
    if let Ok(one) = serde_json::from_value::<AuthSeen>(v) {
        return AuthHistory { entries: vec![one] };
    }
    AuthHistory::default()
}

/// 读取凭据文件并追加时间线；同一账号不会重复追加。
pub fn refresh(auth_file: &Path) -> Result<Option<AuthSeen>> {
    refresh_at(auth_file, &path()?)
}

fn refresh_at(auth_file: &Path, state: &Path) -> Result<Option<AuthSeen>> {
    let text = fs::read_to_string(auth_file)
        .with_context(|| format!("读取凭据文件失败：{}", auth_file.display()))?;
    let doc: Value = serde_json::from_str(&text).context("凭据文件不是合法 JSON")?;
    let Some(uid) = doc
        .get("account")
        .and_then(|a| a.get("uid"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    let mut history = load(state);
    if let Some(last) = history.entries.last() {
        if last.uid == uid {
            return Ok(Some(last.clone()));
        }
    }

    let mtime = fs::metadata(auth_file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(crate::now_secs);
    let since = history
        .entries
        .last()
        .map(|e| mtime.max(e.since + 1))
        .unwrap_or(mtime);

    let entry = AuthSeen {
        uid: uid.to_string(),
        since,
    };
    history.entries.push(entry.clone());
    if history.entries.len() > 40 {
        let drop = history.entries.len() - 40;
        history.entries.drain(0..drop);
    }
    if let Some(dir) = state.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(state, serde_json::to_string_pretty(&history)?);
    Ok(Some(entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_since_until_uid_changes() {
        let root = std::env::temp_dir().join(format!("hub-seen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let auth = root.join("workbuddy-desktop.info");
        let state = root.join("auth_seen.json");

        fs::write(&auth, r#"{"account":{"uid":"u1"}}"#).unwrap();
        let first = refresh_at(&auth, &state).unwrap().unwrap();
        assert_eq!(first.uid, "u1");

        // 同一个 uid 不重复追加
        let same = refresh_at(&auth, &state).unwrap().unwrap();
        assert_eq!(same, first);
        assert_eq!(load(&state).entries.len(), 1);

        // 换账号追加一条，时间单调递增
        fs::write(&auth, r#"{"account":{"uid":"u2"}}"#).unwrap();
        let next = refresh_at(&auth, &state).unwrap().unwrap();
        assert_eq!(next.uid, "u2");
        assert!(next.since > first.since);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn owner_lookup_by_timestamp() {
        let h = AuthHistory {
            entries: vec![
                AuthSeen {
                    uid: "a".into(),
                    since: 100,
                },
                AuthSeen {
                    uid: "b".into(),
                    since: 200,
                },
            ],
        };
        assert_eq!(h.owner_at(50).as_deref(), Some("a"), "早于记录时按最早账号算");
        assert_eq!(h.owner_at(150).as_deref(), Some("a"));
        assert_eq!(h.owner_at(250).as_deref(), Some("b"));
    }
}
