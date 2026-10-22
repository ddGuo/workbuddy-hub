//! 每日签到：用账号自己的凭据调用官方签到接口。
//!
//! 客户端里对应的实现是 `POST {endpoint}/v2/billing/meter/checkin-activity-status`
//! 和 `POST {endpoint}/v2/billing/meter/daily-checkin`，鉴权就是账号自己的
//! `Authorization: Bearer <accessToken>` + `X-User-Id` + `X-Domain`。
//!
//! 这里**不引入 HTTP 库**，改用系统自带的 `curl`，并且把请求写进 stdin 的
//! config，而不是命令行参数 —— token 不会出现在进程列表里，也不会落盘。
//! 代价是依赖 Windows 10 1803+ 自带的 curl，缺失时功能直接报错，不做降级。
//!
//! 和切号不同，签到是**账号自己的动作**：token 只发给官方域名，不经过任何
//! 第三方，也不碰客户端的进程和数据库。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// 一次签到要用到的凭据。只在本进程内存里存在。
pub struct Credentials<'a> {
    pub token: &'a str,
    pub uid: &'a str,
    /// 形如 `www.workbuddy.cn`
    pub domain: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CheckinStatus {
    pub active: bool,
    pub today_checked_in: bool,
    pub streak_days: i64,
    pub daily_credit: i64,
    pub today_credit: i64,
    pub total_credits: i64,
    pub checkin_dates: Vec<String>,
    pub activity_name: Option<String>,
    pub claim_button_text: Option<String>,
}

/// 领取结果。`code == 0` 表示领到了；其它值是服务端给的原因（已领过、活动结束等）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaimOutcome {
    pub code: i64,
    pub msg: String,
    pub credit: i64,
    pub streak_days: i64,
}

/// 一个账号当天的签到记录，也是界面展示的数据来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckinRecord {
    pub uid: String,
    /// 本地日期 `YYYY-MM-DD`
    pub date: String,
    /// `claimed` / `already` / `inactive` / `error`
    pub state: String,
    pub credit: i64,
    pub streak_days: i64,
    pub message: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckinStore {
    pub version: u32,
    /// 是否每天自动签到
    pub auto: bool,
    pub records: BTreeMap<String, CheckinRecord>,
    pub last_run: i64,
    pub last_summary: String,
}

impl Default for CheckinStore {
    fn default() -> Self {
        Self {
            version: 1,
            // 用户要的就是「自动签到」，默认开着；界面上可以关掉
            auto: true,
            records: BTreeMap::new(),
            last_run: 0,
            last_summary: String::new(),
        }
    }
}

impl CheckinStore {
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
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, text).with_context(|| format!("写入 {} 失败", path.display()))?;
        Ok(())
    }

    /// 今天已签的账号数 / 有记录的账号数。
    pub fn today_progress(&self, today: &str) -> (usize, usize) {
        let mut done = 0;
        let mut total = 0;
        for r in self.records.values() {
            if r.date != today {
                continue;
            }
            total += 1;
            if r.state == "claimed" || r.state == "already" {
                done += 1;
            }
        }
        (done, total)
    }
}

/// 查活动状态（只读，不会领）。
pub fn status(c: &Credentials) -> Result<CheckinStatus> {
    let v = post(c, "/v2/billing/meter/checkin-activity-status")?;
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    if data.is_null() {
        bail!("服务端没有返回签到状态");
    }
    Ok(CheckinStatus {
        active: data.get("active").and_then(Value::as_bool).unwrap_or(false),
        today_checked_in: data
            .get("today_checked_in")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        streak_days: int(&data, "streak_days"),
        daily_credit: int(&data, "daily_credit"),
        today_credit: int(&data, "today_credit"),
        total_credits: int(&data, "total_credits"),
        checkin_dates: data
            .get("checkin_dates")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        activity_name: str_of(&data, "activity_name"),
        claim_button_text: str_of(&data, "claim_button_text"),
    })
}

/// 领取今天的签到奖励。
pub fn claim(c: &Credentials) -> Result<ClaimOutcome> {
    let v = post(c, "/v2/billing/meter/daily-checkin")?;
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
    let data = v.get("data").cloned().unwrap_or(Value::Null);
    Ok(ClaimOutcome {
        code,
        msg: v
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // 成功响应里积分字段名不完全确定，按常见命名回退
        credit: {
            let c = int(&data, "credit");
            if c > 0 {
                c
            } else {
                int(&data, "daily_credit")
            }
        },
        streak_days: int(&data, "streak_days"),
    })
}

/// 查一次、必要时领一次，最后落成一条可展示的记录。
///
/// 不抛错：签到失败不该让整轮调度中断，原因写进 `message` 让界面显示。
pub fn check_and_claim(c: &Credentials, today: &str) -> CheckinRecord {
    let mut rec = CheckinRecord {
        uid: c.uid.to_string(),
        date: today.to_string(),
        state: "error".into(),
        credit: 0,
        streak_days: 0,
        message: String::new(),
        updated_at: crate::now_secs(),
    };

    let st = match status(c) {
        Ok(s) => s,
        Err(e) => {
            rec.message = format!("{e:#}");
            return rec;
        }
    };
    rec.streak_days = st.streak_days;
    if !st.active {
        rec.state = "inactive".into();
        rec.message = "签到活动未开启".into();
        return rec;
    }
    if st.today_checked_in {
        rec.state = "already".into();
        rec.credit = st.today_credit;
        rec.message = format!("今天已签到 · 连签 {} 天", st.streak_days);
        return rec;
    }
    match claim(c) {
        Ok(o) if o.code == 0 => {
            rec.state = "claimed".into();
            rec.credit = o.credit;
            rec.streak_days = o.streak_days;
            rec.message = if o.credit > 0 {
                format!("签到成功 · +{} 积分", o.credit)
            } else {
                "签到成功".into()
            };
            rec
        }
        Ok(o) => {
            rec.state = "error".into();
            rec.message = if o.msg.is_empty() {
                format!("签到失败（code {}）", o.code)
            } else {
                format!("签到失败：{}", o.msg)
            };
            rec
        }
        Err(e) => {
            let msg = format!("{e:#}");
            // 服务端把「今天已领过」做成 HTTP 400，和真正的失败要分开
            if is_already_claimed(&msg) {
                rec.state = "already".into();
                rec.credit = st.today_credit;
                rec.message = format!("今天已签到 · 连签 {} 天", st.streak_days);
            } else {
                rec.message = msg;
            }
            rec
        }
    }
}

/// 服务端用它表示「今天已经领过」，属于正常状态而不是错误。
fn is_already_claimed(msg: &str) -> bool {
    ["已签到", "明天再来", "已经签到", "already"]
        .iter()
        .any(|k| msg.contains(k))
}

fn int(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

/// 发一个 POST，返回解析后的 JSON。
///
/// 请求细节（curl、stdin 配置、HTTP 错误处理）在 [`crate::http`]，
/// 这里只负责把「业务错误码」翻译成错误。
fn post(c: &Credentials, path: &str) -> Result<Value> {
    let domain = if c.domain.trim().is_empty() {
        "www.workbuddy.cn"
    } else {
        c.domain.trim()
    };
    let url = format!("https://{domain}{path}");
    let headers = crate::http::account_headers(c.token, c.uid, domain);
    let (code, v) = crate::http::send("POST", &url, &headers, Some("{}"))?;

    // 签到的「今天已领」等状态走 code != 0，属于正常业务返回，不在这里拦
    let api_code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    if api_code != 0 && v.get("data").is_none() {
        let msg = v
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("请求被拒绝");
        bail!("HTTP {code} · {msg}");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_defaults_to_auto_and_counts_today() {
        let mut s = CheckinStore::default();
        assert!(s.auto);
        s.records.insert(
            "a".into(),
            CheckinRecord {
                uid: "a".into(),
                date: "2026-10-22".into(),
                state: "claimed".into(),
                credit: 10,
                streak_days: 3,
                message: String::new(),
                updated_at: 0,
            },
        );
        s.records.insert(
            "b".into(),
            CheckinRecord {
                uid: "b".into(),
                date: "2026-10-22".into(),
                state: "error".into(),
                credit: 0,
                streak_days: 0,
                message: "x".into(),
                updated_at: 0,
            },
        );
        s.records.insert(
            "c".into(),
            CheckinRecord {
                uid: "c".into(),
                date: "2026-10-21".into(),
                state: "claimed".into(),
                credit: 10,
                streak_days: 1,
                message: String::new(),
                updated_at: 0,
            },
        );
        assert_eq!(s.today_progress("2026-10-22"), (1, 2));
    }

    #[test]
    fn detects_already_claimed() {
        assert!(is_already_claimed("HTTP 400 · 今天已签到，请明天再来"));
        assert!(is_already_claimed("already checked in"));
        assert!(!is_already_claimed("HTTP 401 · 登录已失效"));
        assert!(!is_already_claimed("HTTP 500 · internal error"));
    }

    #[test]
    fn parses_status_payload() {
        let v: Value = serde_json::from_str(
            r#"{"code":0,"data":{"active":true,"today_checked_in":false,"streak_days":4,
                "daily_credit":10,"today_credit":0,"total_credits":120,
                "checkin_dates":["2026-10-20","2026-10-21"],"activity_name":"每日签到"}}"#,
        )
        .unwrap();
        let data = v.get("data").unwrap();
        assert!(data.get("active").unwrap().as_bool().unwrap());
        assert_eq!(int(data, "streak_days"), 4);
        assert_eq!(data.get("checkin_dates").unwrap().as_array().unwrap().len(), 2);
    }
}
