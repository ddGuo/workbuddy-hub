//! 剩余积分：客户端「资源包」接口。
//!
//! 签到接口的 `total_credits` 是**累计签到积分**（天数 × 每日额度），不是余额。
//! 真正的剩余额度来自：
//!
//! ```text
//! POST {domain}/v2/billing/meter/get-user-resource-summary   {}
//! → data.Packages[]: { CycleTotalCapacity, CycleRemainCapacity, CycleUsedCapacity }
//! ```
//!
//! 剩余 = 各资源包 `CycleRemainCapacity` 之和；企业账号走
//! `get-enterprise-user-usage`（limit_num - used_num），这里先只处理个人账号。

use crate::checkin::Credentials;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CreditSummary {
    /// 剩余积分（可能带小数）
    pub remaining: f64,
    /// 周期总额度
    pub total: f64,
    /// 资源包个数
    pub packages: usize,
    /// 拉取时间（unix 秒）
    pub updated_at: i64,
    /// 服务端是否仍认这个账号的令牌；401 时为 false
    #[serde(default = "default_valid")]
    pub valid: bool,
}

fn default_valid() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditStore {
    pub version: u32,
    pub credits: BTreeMap<String, CreditSummary>,
}

impl Default for CreditStore {
    fn default() -> Self {
        Self {
            version: 1,
            credits: BTreeMap::new(),
        }
    }
}

impl CreditStore {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
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

/// 令牌在服务端是否仍然有效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenState {
    Valid,
    /// 服务端明确返回 401：登录态已失效，切换过去客户端会要求重新登录
    Invalid,
    /// 网络异常 / 5xx，无法判断，按可用处理
    Unknown,
}

pub fn check_token(c: &Credentials) -> TokenState {
    match fetch_raw(c) {
        Ok(_) => TokenState::Valid,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("401") {
                TokenState::Invalid
            } else {
                TokenState::Unknown
            }
        }
    }
}

/// 原始响应，排查字段时用（CLI `--raw`）。
pub fn fetch_raw(c: &Credentials) -> Result<Value> {
    let domain = if c.domain.trim().is_empty() {
        "www.workbuddy.cn"
    } else {
        c.domain.trim()
    };
    // 客户端里 resourcePrefix 为空、由 http 层补 /v2；这里允许用环境变量核对路径
    let path = std::env::var("HUB_CREDIT_PATH")
        .unwrap_or_else(|_| "/billing/meter/get-user-resource-summary".to_string());
    let url = format!("https://{domain}{path}");
    let headers = crate::http::account_headers(c.token, c.uid, domain);
    let (code, v) = crate::http::send("POST", &url, &headers, Some("{}"))?;
    if v.get("data").is_none() {
        let msg = v
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("请求被拒绝");
        bail!("HTTP {code} · {msg}");
    }
    Ok(v)
}

pub fn fetch(c: &Credentials) -> Result<CreditSummary> {
    let v = fetch_raw(c)?;
    parse_summary(&v).ok_or_else(|| anyhow::anyhow!("响应里没有资源包信息"))
}

/// 兼容不同版本：优先用 `data.TotalCredits`，否则累加资源包。
fn parse_summary(v: &Value) -> Option<CreditSummary> {
    let data = v.get("data")?;

    // 容量字段在服务端是字符串（"10.90000042"），统一按 f64 解析
    let mut total = 0f64;
    let mut pack_remaining = 0f64;
    let mut packages = 0usize;

    if let Some(list) = data
        .get("Packages")
        .or_else(|| data.get("packages"))
        .and_then(Value::as_array)
    {
        packages = list.len();
        for p in list {
            pack_remaining += num(p.get("CycleRemainCapacity").or_else(|| p.get("cycleRemain")));
            total += num(p.get("CycleTotalCapacity").or_else(|| p.get("cycleTotal")));
        }
    }

    // 服务端给的合计最准（含未列在 Packages 里的奖励）
    let remaining = data
        .get("TotalCredits")
        .map(|v| num(Some(v)))
        .filter(|v| *v > 0.0)
        .unwrap_or(pack_remaining);

    if remaining == 0.0 && total == 0.0 && packages == 0 {
        return None;
    }
    Some(CreditSummary {
        remaining,
        total,
        packages,
        updated_at: crate::now_secs(),
        valid: true,
    })
}

fn num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_remaining_capacity() {
        let v = serde_json::json!({
            "code": 0,
            "data": {
                "Packages": [
                    { "PackageCode": "a", "CycleTotalCapacity": 1000, "CycleRemainCapacity": 620.4 },
                    { "PackageCode": "b", "CycleTotalCapacity": 500, "CycleRemainCapacity": 80 }
                ]
            }
        });
        let s = parse_summary(&v).unwrap();
        assert_eq!(s.remaining, 700.4);
        assert_eq!(s.total, 1500.0);
        assert_eq!(s.packages, 2);
    }

    #[test]
    fn handles_empty_packages() {
        let v = serde_json::json!({ "data": { "Packages": [] } });
        assert!(parse_summary(&v).is_none());
    }
}
