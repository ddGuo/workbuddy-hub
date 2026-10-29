//! 账号库：复用 DSH 的 `~/.dsh/workbuddy-accounts/`，格式保持原样。
//!
//! 目录里混杂着 `.bak` / `.imported-*` / `baseline-*` 之类的副本文档，
//! 只有文件名是 `<uuid>.json` 的才是规范条目；同一 uid 出现多次时保留规范文件。
//!
//! 另外一个实测坑：不同存档的 `expiresAt` 单位不统一（有的秒，有的毫秒），
//! 直接按秒解析会把有效期算成五万多年，因此统一归一化后再对外暴露。

use crate::auth_crypto::{decrypt_field, is_encrypted, AtRestKey};
use crate::normalize_ts;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Account {
    pub uid: String,
    pub file: String,
    pub path: PathBuf,
    pub domain: String,
    pub expires_at: i64,
    pub refresh_expires_at: i64,
    pub doc: Value,
}

/// 对外展示用的脱敏视图，不含任何令牌。
#[derive(Debug, Serialize, Clone)]
pub struct AccountSummary {
    pub uid: String,
    pub short_uid: String,
    pub file: String,
    pub domain: String,
    /// 已归一化为秒级时间戳
    pub expires_at: i64,
    pub refresh_expires_at: i64,
    pub access_encrypted: bool,
    pub refresh_encrypted: bool,
}

impl Account {
    pub fn summary(&self) -> AccountSummary {
        let auth = self.doc.get("auth").cloned().unwrap_or(Value::Null);
        AccountSummary {
            uid: self.uid.clone(),
            short_uid: self.uid.chars().take(8).collect(),
            file: self.file.clone(),
            domain: self.domain.clone(),
            expires_at: self.expires_at,
            refresh_expires_at: self.refresh_expires_at,
            access_encrypted: auth
                .get("accessToken")
                .map(is_encrypted)
                .unwrap_or(false),
            refresh_encrypted: auth
                .get("refreshToken")
                .map(is_encrypted)
                .unwrap_or(false),
        }
    }

    fn auth_value(&self, field: &str) -> Result<&Value> {
        self.doc
            .get("auth")
            .and_then(|a| a.get(field))
            .with_context(|| format!("账号 {} 缺少 auth.{field}", self.file))
    }

    /// 取明文 accessToken；若字段是加密信封则用 at-rest 密钥解开。
    pub fn access_token(&self, key: &AtRestKey) -> Result<String> {
        read_token(self.auth_value("accessToken")?, key)
    }

    pub fn refresh_token(&self, key: &AtRestKey) -> Result<String> {
        read_token(self.auth_value("refreshToken")?, key)
    }
}

/// 把客户端当前登录的账号写进 hub 自己的账号库。
///
/// 数据源是客户端的凭据文件：客户端登录一个账号，这里就存一份；令牌刷新后
/// 再次登录会用新值覆盖。内容没变时不落盘，调用方可以安全地反复调用。
pub fn save_current(auth_path: &Path, dir: &Path) -> Result<Option<String>> {
    let text = fs::read_to_string(auth_path)
        .with_context(|| format!("读取凭据文件失败：{}", auth_path.display()))?;
    let doc: Value = serde_json::from_str(&text).context("凭据文件不是合法 JSON")?;

    let uid = doc
        .get("account")
        .and_then(|a| a.get("uid"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("凭据文件里没有账号 uid"))?
        .to_string();

    let auth = match doc.get("auth") {
        Some(v) if v.is_object() => v,
        _ => bail!("凭据文件里没有 auth 字段"),
    };
    // 客户端登出后可能留下空的 auth；这种不能覆盖掉账号库里有效的令牌
    let has_token = |key: &str| {
        auth.get(key)
            .map(|v| {
                v.is_object()
                    || v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false)
            })
            .unwrap_or(false)
    };
    if !has_token("accessToken") || !has_token("refreshToken") {
        return Ok(None);
    }

    fs::create_dir_all(dir)
        .with_context(|| format!("创建账号库目录失败：{}", dir.display()))?;
    let target = dir.join(format!("{uid}.json"));
    clear_deleted(dir, &uid);

    // 令牌可能是明文，也可能是客户端自己的 $wbEncrypted 信封；
    // 原样拷贝，读取时由 read_token 统一处理，不在这里解密。
    let out = serde_json::json!({
        "account": { "uid": uid },
        "auth": {
            "accessToken": auth.get("accessToken").cloned().unwrap_or(Value::Null),
            "refreshToken": auth.get("refreshToken").cloned().unwrap_or(Value::Null),
            "expiresAt": auth.get("expiresAt").cloned().unwrap_or(Value::Null),
            "refreshExpiresAt": auth.get("refreshExpiresAt").cloned().unwrap_or(Value::Null),
            "domain": auth.get("domain").cloned().unwrap_or(Value::Null),
        }
    });

    if let Ok(old) = fs::read_to_string(&target) {
        if serde_json::from_str::<Value>(&old).map(|v| v == out).unwrap_or(false) {
            return Ok(None);
        }
    }

    let body = serde_json::to_string_pretty(&out)?;
    let tmp = target.with_extension("json.tmp");
    fs::write(&tmp, body).with_context(|| format!("写入临时存档失败：{}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        // 并发写入时另一个调用可能已经写好，不算失败
        if target.exists() {
            return Ok(None);
        }
        return Err(e).with_context(|| format!("写入账号存档失败：{}", target.display()));
    }
    Ok(Some(uid))
}

/// 把旧版 DSH 账号库（`~/.dsh/workbuddy-accounts`）里**不存在于** hub 账号库的存档
/// 拷过来。hub 自己的存档不覆盖 —— 它的令牌更新。
///
/// 一次性兼容：目录不存在时返回 0，调用方可以无条件调用。
/// 被用户删除过的账号。重新登录会清除标记，之后旧账号库可以再次导入。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeletedAccounts {
    #[serde(default)]
    pub uids: Vec<String>,
}

fn deleted_path(dir: &Path) -> PathBuf {
    dir.parent().unwrap_or(dir).join("deleted.json")
}

pub fn deleted_uids(dir: &Path) -> Vec<String> {
    fs::read_to_string(deleted_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<DeletedAccounts>(&t).ok())
        .map(|d| d.uids)
        .unwrap_or_default()
}

fn save_deleted(dir: &Path, list: &DeletedAccounts) -> Result<()> {
    let path = deleted_path(dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(list)?)?;
    Ok(())
}

/// 删除账号存档，并记下墓碑，避免旧账号库的导入把它带回来。
pub fn delete_account(dir: &Path, uid: &str) -> Result<bool> {
    let target = dir.join(format!("{uid}.json"));
    let removed = if target.is_file() {
        fs::remove_file(&target)?;
        true
    } else {
        false
    };

    let mut list = DeletedAccounts {
        uids: deleted_uids(dir),
    };
    if !list.uids.iter().any(|u| u == uid) {
        list.uids.push(uid.to_string());
        save_deleted(dir, &list)?;
    }
    Ok(removed)
}

fn clear_deleted(dir: &Path, uid: &str) {
    let mut list = DeletedAccounts {
        uids: deleted_uids(dir),
    };
    if list.uids.iter().any(|u| u == uid) {
        list.uids.retain(|u| u != uid);
        let _ = save_deleted(dir, &list);
    }
}

pub fn import_from_dsh(dsh_dir: &Path, dir: &Path) -> Result<usize> {
    if !dsh_dir.is_dir() {
        return Ok(0);
    }
    let mut imported = 0usize;
    let deleted = deleted_uids(dir);
    for entry in fs::read_dir(dsh_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_canonical(name) || !path.is_file() {
            continue;
        }
        let target = dir.join(name);
        if target.exists() {
            continue;
        }
        let stem = name.strip_suffix(".json").unwrap_or(name);
        if deleted.iter().any(|u| u == stem) {
            continue;
        }
        fs::create_dir_all(dir)
            .with_context(|| format!("创建账号库目录失败：{}", dir.display()))?;
        fs::copy(&path, &target)
            .with_context(|| format!("导入存档失败：{}", path.display()))?;
        imported += 1;
    }
    Ok(imported)
}

fn read_token(value: &Value, key: &AtRestKey) -> Result<String> {
    if let Some(s) = value.as_str() {
        return Ok(s.to_string());
    }
    if is_encrypted(value) {
        let envelope = value
            .get("envelope")
            .and_then(|v| v.as_str())
            .context("加密字段缺少 envelope")?;
        return decrypt_field(envelope, key);
    }
    bail!("字段既不是字符串也不是可识别的加密信封")
}

/// 文件名是否是规范条目：`<uuid>.json`
fn is_canonical(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".json") else {
        return false;
    };
    if stem.len() != 36 {
        return false;
    }
    let groups: Vec<&str> = stem.split('-').collect();
    if groups.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    groups
        .iter()
        .zip(lens.iter())
        .all(|(g, &l)| g.len() == l && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// 读取整个账号库，按 uid 去重。
pub fn load(dir: &Path) -> Result<Vec<Account>> {
    // 新机器上还没跑过导入，目录本来就不存在：这是「空库」不是错误
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut by_uid: std::collections::BTreeMap<String, Account> = Default::default();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_canonical(&name) {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let doc: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(uid) = doc
            .get("account")
            .and_then(|a| a.get("uid"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let uid = uid.to_string();

        let auth = doc.get("auth").cloned().unwrap_or(Value::Null);
        let expires_at = normalize_ts(auth.get("expiresAt").and_then(|v| v.as_i64()).unwrap_or(0));
        let refresh_expires_at = normalize_ts(
            auth.get("refreshExpiresAt")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        );
        let domain = auth
            .get("domain")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let account = Account {
            uid: uid.clone(),
            file: name,
            path,
            domain,
            expires_at,
            refresh_expires_at,
            doc,
        };

        // 规范文件名优先；已存在时不覆盖
        match by_uid.get(&uid) {
            Some(existing) if existing.file == format!("{uid}.json") => {}
            _ => {
                by_uid.insert(uid, account);
            }
        }
    }

    Ok(by_uid.into_values().collect())
}

/// 供测试与诊断：列出被跳过的非规范文件。
pub fn skipped_files(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_canonical(&name) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn accepts_uuid_json() {
        assert!(is_canonical("697af213-6a63-4edb-ad9c-1ff588ee434d.json"));
    }

    #[test]
    fn missing_dir_loads_empty() {
        let dir = std::env::temp_dir().join("hub-missing-account-dir-for-test");
        let _ = fs::remove_dir_all(&dir);
        assert!(load(&dir).unwrap().is_empty());
    }

    #[test]
    fn saves_current_account_and_updates_changed_tokens() {
        let root = std::env::temp_dir().join(format!("hub-import-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        let auth = root.join("workbuddy-desktop.info");
        fs::write(
            &auth,
            serde_json::json!({
                "account": { "uid": "abc-123" },
                "auth": {
                    "accessToken": { "$wbEncrypted": 1, "envelope": "e1" },
                    "refreshToken": "rt",
                    "expiresAt": 123,
                    "refreshExpiresAt": 456,
                    "domain": "www.workbuddy.cn"
                }
            })
            .to_string(),
        )
        .unwrap();

        let dir = root.join("accounts");
        assert_eq!(save_current(&auth, &dir).unwrap().as_deref(), Some("abc-123"));

        let saved: Value =
            serde_json::from_str(&fs::read_to_string(dir.join("abc-123.json")).unwrap()).unwrap();
        assert_eq!(saved["account"]["uid"], "abc-123");
        assert_eq!(saved["auth"]["refreshToken"], "rt");
        assert_eq!(saved["auth"]["domain"], "www.workbuddy.cn");
        assert!(saved["auth"]["accessToken"].get("envelope").is_some());

        // 第二次不覆盖
        assert!(save_current(&auth, &dir).unwrap().is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn imports_missing_dsh_archives_only() {
        let root = std::env::temp_dir().join(format!("hub-dsh-import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dsh = root.join("dsh");
        let hub = root.join("accounts");
        fs::create_dir_all(&dsh).unwrap();

        let a = dsh.join("11111111-1111-1111-1111-111111111111.json");
        fs::write(&a, r#"{"account":{"uid":"11111111-1111-1111-1111-111111111111"}}"#).unwrap();
        fs::write(
            dsh.join("22222222-2222-2222-2222-222222222222.json.bak"),
            "noise",
        )
        .unwrap();

        assert_eq!(import_from_dsh(&dsh, &hub).unwrap(), 1);
        assert!(hub.join("11111111-1111-1111-1111-111111111111.json").exists());
        assert!(!hub.join("22222222-2222-2222-2222-222222222222.json.bak").exists());

        // hub 里已有的不覆盖
        let mine = hub.join("11111111-1111-1111-1111-111111111111.json");
        fs::write(&mine, "hub-own").unwrap();
        assert_eq!(import_from_dsh(&dsh, &hub).unwrap(), 0);
        assert_eq!(fs::read_to_string(&mine).unwrap(), "hub-own");

        // 目录不存在时静默返回 0
        assert_eq!(import_from_dsh(&root.join("nope"), &hub).unwrap(), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_copies() {
        assert!(!is_canonical("697af213-6a63-4edb-ad9c-1ff588ee434d.json.bak"));
        assert!(!is_canonical("baseline-A-697af213.json"));
        assert!(!is_canonical("697af213-6a63-4edb-ad9c-1ff588ee434d.json.imported-1792468402525"));
    }
}
