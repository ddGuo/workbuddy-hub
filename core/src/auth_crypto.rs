//! 与 WorkBuddy 桌面客户端保持一致的凭据加解密。
//!
//! 客户端把 accessToken / refreshToken / nickname 等字段包成
//! `{"$wbEncrypted":1,"envelope":"<base64>"}`，envelope 解出来是：
//! `{suite,keyId,nonce,authTag,ciphertext}`（除 suite 外均为 base64）。
//!
//! 密钥来自客户端自己：`atRestSecretKey` → `SHA256` → AES-256-GCM key，
//! `keyId = hex(SHA256(key))[..16]`，AAD 由固定前缀 + 长度前缀字段拼装。
//!
//! 这里逐字节复刻 `dsh-workbuddy-switcher` 中已验证的实现，任何顺序或
//! 长度前缀差异都会导致 GCM 校验失败，所以拼装函数单独抽出来便于对拍。

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    Aes256Gcm, Key, Nonce,
};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const AAD_MAGIC: &[u8] = b"WB-AAD\0";
const ENVELOPE_VERSION: &[u8] = b"WBEV1";
const SUITE_LABEL: &[u8] = b"sym-v1";
const DEFAULT_SUITE: u32 = 1;

/// 客户端 at-rest 密钥派生结果。
pub struct AtRestKey {
    key: [u8; 32],
    key_id: String,
}

impl AtRestKey {
    pub fn from_secret(secret: &str) -> Self {
        let key: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        let digest = Sha256::digest(key);
        let key_id = hex::encode(digest)[..16].to_string();
        Self { key, key_id }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

#[derive(Debug, Deserialize)]
struct Envelope {
    suite: u32,
    #[serde(rename = "keyId")]
    key_id: String,
    nonce: String,
    #[serde(rename = "authTag")]
    auth_tag: String,
    ciphertext: String,
}

#[derive(Debug, Serialize)]
struct EnvelopeOut<'a> {
    suite: u32,
    #[serde(rename = "keyId")]
    key_id: &'a str,
    nonce: String,
    #[serde(rename = "authTag")]
    auth_tag: String,
    ciphertext: String,
}

/// `u32be(len) || bytes`
fn len_prefixed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// 构造 GCM 的附加认证数据。顺序与客户端完全一致，不要调整。
fn build_aad(suite: u32, key_id: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(64);
    aad.extend_from_slice(AAD_MAGIC);
    aad.push(1);
    aad.extend_from_slice(&len_prefixed(ENVELOPE_VERSION));
    aad.extend_from_slice(&len_prefixed(SUITE_LABEL));
    aad.extend_from_slice(&suite.to_be_bytes());
    aad.extend_from_slice(&len_prefixed(key_id.as_bytes()));
    aad.push(2);
    aad.push(0);
    aad.push(0);
    aad
}

/// 判断某个字段是否是加密信封。
pub fn is_encrypted(value: &serde_json::Value) -> bool {
    value
        .get("$wbEncrypted")
        .and_then(|v| v.as_i64())
        .map(|v| v == 1)
        .unwrap_or(false)
}

/// 解密一个 `{"$wbEncrypted":1,"envelope":"..."}` 字段。
pub fn decrypt_field(envelope_b64: &str, at_rest: &AtRestKey) -> Result<String> {
    let raw = STANDARD
        .decode(envelope_b64.trim())
        .context("envelope 不是合法 base64")?;
    let env: Envelope = serde_json::from_slice(&raw).context("envelope 内部不是预期 JSON")?;

    if env.key_id != at_rest.key_id {
        bail!(
            "keyId 不匹配：信封为 {}，本地派生为 {}（说明该凭据是用另一把 at-rest 密钥加密的）",
            env.key_id,
            at_rest.key_id
        );
    }

    let aad = build_aad(env.suite, &env.key_id);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&at_rest.key));

    let nonce_bytes = STANDARD.decode(&env.nonce).context("nonce 解码失败")?;
    if nonce_bytes.len() != 12 {
        bail!("nonce 长度应为 12，实际 {}", nonce_bytes.len());
    }
    let nonce = Nonce::from_slice(&nonce_bytes);

    let mut msg = STANDARD.decode(&env.ciphertext).context("ciphertext 解码失败")?;
    let tag = STANDARD.decode(&env.auth_tag).context("authTag 解码失败")?;
    if tag.len() != 16 {
        bail!("authTag 长度应为 16，实际 {}", tag.len());
    }
    msg.extend_from_slice(&tag);

    let plain = cipher
        .decrypt(nonce, Payload { msg: &msg, aad: &aad })
        .map_err(|_| anyhow!("AES-GCM 校验失败（密钥、AAD 或密文不匹配）"))?;

    String::from_utf8(plain).context("解密结果不是 UTF-8")
}

/// 加密一个字段，返回可直接放进 `{"$wbEncrypted":1,"envelope":...}` 的 base64。
pub fn encrypt_field(plain: &str, at_rest: &AtRestKey, suite: Option<u32>) -> Result<String> {
    let suite = suite.unwrap_or(DEFAULT_SUITE);
    let aad = build_aad(suite, &at_rest.key_id);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&at_rest.key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

    let sealed = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plain.as_bytes(),
                aad: &aad,
            },
        )
        .map_err(|_| anyhow!("AES-GCM 加密失败"))?;

    if sealed.len() < 16 {
        bail!("密文长度异常：{}", sealed.len());
    }
    let (ct, tag) = sealed.split_at(sealed.len() - 16);

    let env = EnvelopeOut {
        suite,
        key_id: &at_rest.key_id,
        nonce: STANDARD.encode(nonce.as_slice()),
        auth_tag: STANDARD.encode(tag),
        ciphertext: STANDARD.encode(ct),
    };
    let json = serde_json::to_vec(&env)?;
    Ok(STANDARD.encode(json))
}

/// 方便测试：暴露 AAD 结构供对拍使用。
pub fn debug_aad_hex(at_rest: &AtRestKey, suite: u32) -> String {
    hex::encode(build_aad(suite, &at_rest.key_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aad_layout_matches_expected_length() {
        let k = AtRestKey::from_secret("test-secret");
        // 7 + 1 + (4+5) + (4+6) + 4 + (4+16) + 3 = 54
        assert_eq!(build_aad(1, k.key_id()).len(), 54);
    }

    #[test]
    fn key_id_is_16_hex_chars() {
        let k = AtRestKey::from_secret("test-secret");
        assert_eq!(k.key_id().len(), 16);
        assert!(k.key_id().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn roundtrip() {
        let k = AtRestKey::from_secret("roundtrip-secret");
        let env = encrypt_field("hello-世界", &k, None).unwrap();
        assert_eq!(decrypt_field(&env, &k).unwrap(), "hello-世界");
    }

    #[test]
    fn wrong_key_fails() {
        let a = AtRestKey::from_secret("secret-a");
        let b = AtRestKey::from_secret("secret-b");
        let env = encrypt_field("x", &a, None).unwrap();
        assert!(decrypt_field(&env, &b).is_err());
    }
}
