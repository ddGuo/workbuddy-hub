//! 凭据导出 / 导入：给一个可以跨机器搬运的加密文件。
//!
//! 账号库里的令牌是用**本机 at-rest 密钥**封装的，换台电脑解不开。
//! 导出时用用户给的密码重新加密（PBKDF2-HMAC-SHA256 + AES-256-GCM），
//! 导入时再用本机密钥封装回去。
//!
//! 文件结构：
//! ```json
//! {
//!   "format": "workbuddy-hub-credentials",
//!   "version": 1,
//!   "created_at": 0,
//!   "kdf":    { "alg": "pbkdf2-hmac-sha256", "iterations": 150000, "salt": "..." },
//!   "cipher": { "alg": "aes-256-gcm", "nonce": "...", "data": "..." }
//! }
//! ```
//! 明文里是 `{ "accounts": [ { uid, domain, expires_at, refresh_expires_at,
//! access_token, refresh_token } ] }`。

use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, OsRng, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const FORMAT: &str = "workbuddy-hub-credentials";
pub const VERSION: u32 = 1;
const ITERATIONS: u32 = 150_000;
const SALT_LEN: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedAccount {
    pub uid: String,
    #[serde(default)]
    pub domain: String,
    /// unix 秒
    pub expires_at: i64,
    pub refresh_expires_at: i64,
    pub access_token: String,
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ExportPayload {
    pub accounts: Vec<ExportedAccount>,
}

#[derive(Serialize, Deserialize)]
struct Kdf {
    alg: String,
    iterations: u32,
    salt: String,
}

#[derive(Serialize, Deserialize)]
struct Cipher {
    alg: String,
    nonce: String,
    data: String,
}

#[derive(Serialize, Deserialize)]
struct EnvelopeFile {
    format: String,
    version: u32,
    created_at: i64,
    kdf: Kdf,
    cipher: Cipher,
}

/// 用密码把账号列表封装成一段可直接写文件 JSON 文本。
pub fn seal(payload: &ExportPayload, password: &str) -> Result<String> {
    seal_with(payload, password, ITERATIONS)
}

fn seal_with(payload: &ExportPayload, password: &str, iterations: u32) -> Result<String> {
    if password.chars().count() < 6 {
        bail!("密码至少 6 位");
    }

    let mut salt = [0u8; SALT_LEN];
    OsRng.fill_bytes(&mut salt);
    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let key = pbkdf2(password.as_bytes(), &salt, iterations, 32);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let nonce = Nonce::from_slice(&nonce_bytes);
    let aad = format!("{FORMAT}:{VERSION}");
    let plain = serde_json::to_vec(payload)?;
    let sealed = cipher
        .encrypt(
            nonce,
            Payload {
                msg: &plain,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("加密失败"))?;

    let file = EnvelopeFile {
        format: FORMAT.to_string(),
        version: VERSION,
        created_at: crate::now_secs(),
        kdf: Kdf {
            alg: "pbkdf2-hmac-sha256".into(),
            iterations,
            salt: STANDARD.encode(salt),
        },
        cipher: Cipher {
            alg: "aes-256-gcm".into(),
            nonce: STANDARD.encode(nonce_bytes),
            data: STANDARD.encode(sealed),
        },
    };
    Ok(serde_json::to_string_pretty(&file)?)
}

/// 读取并解密导出文件。
pub fn open(text: &str, password: &str) -> Result<ExportPayload> {
    let file: EnvelopeFile = serde_json::from_str(text).context("文件不是有效的导出格式")?;
    if file.format != FORMAT {
        bail!("文件不是 WorkBuddy Hub 导出的凭据");
    }
    if file.version != VERSION {
        bail!("不支持的文件版本 {}", file.version);
    }
    if file.kdf.alg != "pbkdf2-hmac-sha256" || file.cipher.alg != "aes-256-gcm" {
        bail!("不支持的加密算法");
    }

    let salt = STANDARD.decode(&file.kdf.salt).context("盐值损坏")?;
    let nonce = STANDARD.decode(&file.cipher.nonce).context("nonce 损坏")?;
    let data = STANDARD.decode(&file.cipher.data).context("密文损坏")?;
    if nonce.len() != 12 {
        bail!("nonce 长度不对");
    }

    let key = pbkdf2(password.as_bytes(), &salt, file.kdf.iterations, 32);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
    let aad = format!("{FORMAT}:{VERSION}");
    let plain = cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &data,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("密码错误，或文件已损坏"))?;

    let payload: ExportPayload = serde_json::from_slice(&plain).context("内容不是有效 JSON")?;
    Ok(payload)
}

// --- PBKDF2-HMAC-SHA256 ---------------------------------------------------

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        let d = Sha256::digest(key);
        block[..32].copy_from_slice(&d);
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(data);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

fn pbkdf2(password: &[u8], salt: &[u8], iterations: u32, out_len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(out_len);
    let mut block_index: u32 = 1;
    while out.len() < out_len {
        let mut salt_block = salt.to_vec();
        salt_block.extend_from_slice(&block_index.to_be_bytes());
        let mut u = hmac_sha256(password, &salt_block);
        let mut t = u;
        for _ in 1..iterations.max(1) {
            u = hmac_sha256(password, &u);
            for i in 0..32 {
                t[i] ^= u[i];
            }
        }
        out.extend_from_slice(&t);
        block_index += 1;
    }
    out.truncate(out_len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ExportPayload {
        ExportPayload {
            accounts: vec![ExportedAccount {
                uid: "u1".into(),
                domain: "www.workbuddy.cn".into(),
                expires_at: 100,
                refresh_expires_at: 200,
                access_token: "a".into(),
                refresh_token: "r".into(),
            }],
        }
    }

    #[test]
    fn roundtrip() {
        let text = seal_with(&sample(), "hunter2", 1000).unwrap();
        let back = open(&text, "hunter2").unwrap();
        assert_eq!(back.accounts[0].uid, "u1");
        assert_eq!(back.accounts[0].refresh_token, "r");
    }

    #[test]
    fn wrong_password_fails() {
        let text = seal_with(&sample(), "hunter2", 1000).unwrap();
        let err = open(&text, "wrong").unwrap_err().to_string();
        assert!(err.contains("密码错误"), "{err}");
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let text = seal_with(&sample(), "hunter2", 1000).unwrap();
        let mut v: Value = serde_json::from_str(&text).unwrap();
        v["cipher"]["data"] = Value::String("AAAA".into());
        let err = open(&v.to_string(), "hunter2").unwrap_err().to_string();
        assert!(err.contains("密码错误") || err.contains("损坏"), "{err}");
    }

    #[test]
    fn rejects_short_password() {
        assert!(seal_with(&sample(), "123", 1000).is_err());
    }
}
