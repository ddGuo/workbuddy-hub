//! 记录「当前账号从什么时候开始生效」。
//!
//! 限流原文里没有账号身份，归属只能靠会话的 `user_id`。但切号时 hub 会把任务
//! 归集到新账号名下，旧会话里的旧限流就会被算到新账号头上。用这份记录做时间线：
//! 只有发生在 `since` 之后的限流，才算当前账号的。

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

pub fn path() -> Result<PathBuf> {
    Ok(crate::client_compat::hub_dir()?.join("auth_seen.json"))
}

pub fn load(path: &Path) -> Option<AuthSeen> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 读取凭据文件并刷新记录：uid 变化时把 `since` 设为文件修改时间。
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

    if let Some(prev) = load(state) {
        if prev.uid == uid {
            return Ok(Some(prev));
        }
    }

    let since = fs::metadata(auth_file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(crate::now_secs);

    let seen = AuthSeen {
        uid: uid.to_string(),
        since,
    };
    if let Some(dir) = state.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(state, serde_json::to_string_pretty(&seen)?);
    Ok(Some(seen))
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

        // 同一个 uid：保留原来的 since
        fs::write(
            &state,
            serde_json::to_string(&AuthSeen {
                uid: "u1".into(),
                since: 100,
            })
            .unwrap(),
        )
        .unwrap();
        let same = refresh_at(&auth, &state).unwrap().unwrap();
        assert_eq!(same.since, 100);

        // 换账号：since 跟着变
        fs::write(&auth, r#"{"account":{"uid":"u2"}}"#).unwrap();
        let next = refresh_at(&auth, &state).unwrap().unwrap();
        assert_eq!(next.uid, "u2");
        assert_ne!(next.since, 100);

        let _ = fs::remove_dir_all(&root);
    }
}
