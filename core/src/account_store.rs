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
use serde::Serialize;
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
    if !dir.is_dir() {
        bail!("账号库目录不存在：{}", dir.display());
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
    use super::is_canonical;

    #[test]
    fn accepts_uuid_json() {
        assert!(is_canonical("697af213-6a63-4edb-ad9c-1ff588ee434d.json"));
    }

    #[test]
    fn rejects_copies() {
        assert!(!is_canonical("697af213-6a63-4edb-ad9c-1ff588ee434d.json.bak"));
        assert!(!is_canonical("baseline-A-697af213.json"));
        assert!(!is_canonical("697af213-6a63-4edb-ad9c-1ff588ee434d.json.imported-1792468402525"));
    }
}
