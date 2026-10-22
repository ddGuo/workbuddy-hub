//! 账号资料：昵称、手机号、邮箱、账号类型。
//!
//! 本地只有**当前登录账号**的资料（客户端凭据文件里的 `account.phoneNumber` 是加密的），
//! 其余账号只有 uid。要拿到全部账号的手机号，用各账号自己的 token 打
//! `GET /console/accounts` —— 客户端自己也是这么取账号列表的。
//!
//! 手机号一律脱敏后保存（`199****9283`）：界面上只需要「能认出是哪个号」，
//! 完整号码没有必要落盘，也没有必要显示。

use crate::http;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    pub uid: String,
    pub nickname: String,
    /// 已脱敏，形如 `199****9283`；没有手机号时为空
    pub phone: String,
    pub email: String,
    pub uin: i64,
    pub account_type: String,
    pub fetched_at: i64,
}

impl Profile {
    /// 列表里优先显示的内容：手机号 → 昵称 → 空（调用方回退到 uid 前缀）。
    pub fn label(&self) -> &str {
        if !self.phone.is_empty() {
            &self.phone
        } else {
            &self.nickname
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileStore {
    pub version: u32,
    pub profiles: BTreeMap<String, Profile>,
    pub last_run: i64,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: BTreeMap::new(),
            last_run: 0,
        }
    }
}

impl ProfileStore {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Self>(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

/// 读一个账号的资料。
pub fn fetch(domain: &str, token: &str, uid: &str) -> Result<Profile> {
    let d = if domain.trim().is_empty() {
        "www.workbuddy.cn"
    } else {
        domain.trim()
    };
    let url = format!("https://{d}/console/accounts");
    let headers = http::account_headers(token, uid, d);
    let (_, v) = http::send("GET", &url, &headers, None)?;

    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code != 0 {
        bail!(
            "读取账号资料失败：{}",
            v.get("msg").and_then(|m| m.as_str()).unwrap_or("未知错误")
        );
    }

    let empty = Vec::new();
    let arr = v
        .pointer("/data/accounts")
        .and_then(|a| a.as_array())
        .unwrap_or(&empty);
    let hit = arr
        .iter()
        .find(|a| a.get("uid").and_then(|u| u.as_str()) == Some(uid))
        .or_else(|| arr.first());
    let Some(a) = hit else {
        bail!("接口没有返回账号列表");
    };

    Ok(Profile {
        uid: uid.to_string(),
        nickname: a
            .get("nickname")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        phone: mask_phone(
            a.get("phoneNumber")
                .or_else(|| a.get("phone_number"))
                .and_then(|s| s.as_str())
                .unwrap_or(""),
        ),
        email: a
            .get("email")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        uin: a.get("uin").and_then(|u| u.as_i64()).unwrap_or(0),
        account_type: a
            .get("type")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        fetched_at: crate::now_secs(),
    })
}

/// `19912349283` → `199****9283`。位数不足时返回空串，调用方回退到昵称 / uid。
///
/// 超过 11 位时取后 11 位：`+86 199-1234-9283` 这种带国际区号的写法，
/// 直接取前 3 位会变成 `861****9283`。
pub fn mask_phone(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.len();
    let local: &str = if n > 11 { &digits[n - 11..] } else { &digits };
    if local.len() < 7 {
        return String::new();
    }
    let head: String = local.chars().take(3).collect();
    let tail: String = local
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{head}****{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_phone_numbers() {
        assert_eq!(mask_phone("19912349283"), "199****9283");
        assert_eq!(mask_phone("+86 199-1234-9283"), "199****9283");
        assert_eq!(mask_phone("123"), "");
        assert_eq!(mask_phone(""), "");
    }

    #[test]
    fn label_prefers_phone_then_nickname() {
        let mut p = Profile::default();
        p.nickname = "阿郭".into();
        assert_eq!(p.label(), "阿郭");
        p.phone = "199****9283".into();
        assert_eq!(p.label(), "199****9283");
    }

    #[test]
    fn store_roundtrip_is_stable() {
        let dir = std::env::temp_dir().join(format!("wb-hub-profile-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("profiles.json");
        let mut s = ProfileStore::default();
        s.profiles.insert(
            "u1".into(),
            Profile {
                uid: "u1".into(),
                nickname: "n".into(),
                phone: "199****9283".into(),
                ..Default::default()
            },
        );
        s.save(&path).unwrap();
        let back = ProfileStore::load(&path);
        assert_eq!(back.profiles.get("u1").unwrap().phone, "199****9283");
        fs::remove_file(&path).ok();
    }
}
