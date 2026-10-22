//! 限流识别：从会话 jsonl 里读出上游返回的错误原文，判断是否该换号。
//!
//! 客户端不会把错误写进数据库，只把任务的 `status` 置成 `error`；
//! 真正的错误文本藏在会话正文最后一条 assistant 消息里：
//!
//! ```text
//! 429 您的使用量已超出频率限制，将在 2026-10-19 09:56:23 UTC+8 重置，
//!     您也可以切换其他模型继续使用。
//! ```
//!
//! 这句话里同时包含「状态码」「是否账号级」「何时恢复」，比只看 429 有用得多：
//! 有重置时间就精确冷却到那一刻，没有就按退避策略。

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// 限流的影响范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    /// 账号额度用尽：换号才有用
    Account,
    /// 单模型被限流：同账号换模型仍可用
    Model,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitSignal {
    pub session_id: String,
    pub uid: String,
    /// 命中的原文（截断到 200 字符）
    pub message: String,
    pub kind: LimitKind,
    /// 上游给出的恢复时间（unix 秒）；没有则为 None
    pub reset_at: Option<i64>,
    /// 这条错误发生在什么时候（unix 秒）。优先取消息自身的 timestamp，
    /// 没有时由调用方用任务更新时间补齐。
    #[serde(default)]
    pub detected_at: i64,
    /// 触发限流的模型（`providerData.model`）
    #[serde(default)]
    pub model: Option<String>,
}

impl LimitSignal {
    /// 冷却到什么时候。有重置时间就用它，否则按档位退避。
    pub fn cooldown_until(&self, now: i64, strikes: u32) -> i64 {
        match self.reset_at {
            Some(t) if t > now => t,
            _ => {
                let backoff = match strikes {
                    0 | 1 => 10 * 60,
                    2 => 30 * 60,
                    _ => 24 * 3600,
                };
                now + backoff
            }
        }
    }
}

/// 从一段文本里识别限流。
///
/// 只认**上游错误原文**，不做关键词泛匹配：助手正常聊天时也可能出现
/// 「限流」二字（例如解释这个功能），把那种句子当限流会误报。
pub fn classify(text: &str) -> Option<(LimitKind, Option<i64>)> {
    // 上游错误都是短提示；长回复里可能引用错误原文，不能整段当成限流
    if text.chars().count() > 300 {
        return None;
    }

    let lower = text.to_lowercase();
    let head = text.trim_start();

    // 必须像一条错误开头：`429 您的…` / `429：…`；
    // 代码里的行号（`429→`）或长数字不能算
    let is_429 = starts_with_status(head, "429")
        || starts_with_status(head, "402")
        || lower.starts_with("http 429")
        || lower.starts_with("http 402");
    let credit = text.contains("使用量已超出")
        || text.contains("积分不足")
        || text.contains("额度不足")
        || lower.contains("quota exceeded")
        || lower.contains("insufficient credit");
    let rate = text.contains("频率限制")
        || text.contains("请求过于频繁")
        || text.contains("模型已限流")
        || text.contains("模型限流")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("overloaded_error");
    // 「可以切换其他模型」是模型级的强特征：正常解释功能时不会整句这么写
    let model_hint = text.contains("切换其他模型")
        || text.contains("切换至其他模型")
        || text.contains("切换模型继续")
        || text.contains("您可先切换");

    if !(is_429 || credit || rate || model_hint) {
        return None;
    }

    let kind = if model_hint {
        LimitKind::Model
    } else if credit {
        LimitKind::Account
    } else {
        LimitKind::Model
    };

    Some((kind, parse_reset_time(text)))
}

/// 从错误原文里提取模型名，如「当前您在Deepseek-V4.1-Flash模型的使用量…」。
pub fn extract_model(text: &str) -> Option<String> {
    for marker in ["您在", "在"] {
        if let Some(i) = text.find(marker) {
            let rest = &text[i + marker.len()..];
            if let Some(j) = rest.find("模型") {
                let name = rest[..j].trim();
                if !name.is_empty() && name.chars().count() <= 40 && !name.contains(' ') {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

fn starts_with_status(text: &str, code: &str) -> bool {
    let Some(rest) = text.strip_prefix(code) else {
        return false;
    };
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_whitespace() || matches!(c, '：' | ':' | '，' | ',') || (c as u32) > 0x2E80,
    }
}

/// 解析「将在 2026-10-19 09:56:23 UTC+8 重置」里的时间。
pub fn parse_reset_time(text: &str) -> Option<i64> {
    // 两种措辞：「将在 2026-10-19 09:56:23 …重置」/「可在 2026-10-23 09:51:21 重置后使用」
    for marker in ["将在", "可在"] {
        if let Some(pos) = text.find(marker) {
            let rest = text[pos + marker.len()..].trim_start();
            let candidate: String = rest.chars().take(19).collect();
            if let Some(ts) = parse_utc8_datetime(&candidate) {
                return Some(ts);
            }
        }
    }
    None
}

/// `YYYY-MM-DD HH:MM:SS` 按 UTC+8 解释，返回 unix 秒。
pub fn parse_utc8_datetime(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse::<i64>().ok() };
    let y = num(0..4)?;
    let mo = num(5..7)? as u32;
    let d = num(8..10)? as u32;
    let h = num(11..13)?;
    let mi = num(14..16)?;
    let se = num(17..19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    Some(days * 86_400 + h * 3600 + mi * 60 + se - 8 * 3600)
}

/// Howard Hinnant 的 (y, m, d) → days 算法。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 读文件尾部，避免为了看最后一条消息而加载整个会话。
fn read_tail(path: &Path, max_bytes: u64) -> Result<String> {
    let mut f = File::open(path).with_context(|| format!("无法打开 {}", path.display()))?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(max_bytes).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).to_string();
    // 从中间开始读时首行可能不完整，丢掉
    if start > 0 {
        if let Some(idx) = text.find('\n') {
            return Ok(text[idx + 1..].to_string());
        }
    }
    Ok(text)
}

/// 扫描一个会话的 jsonl，找出最后一条 assistant 消息里的限流信息。
pub fn scan_session(jsonl: &Path, session_id: &str, uid: &str) -> Result<Option<LimitSignal>> {
    if !jsonl.is_file() {
        return Ok(None);
    }
    let text = read_tail(jsonl, 256 * 1024)?;

    // 从后往前找**最近一条**带限流特征的 assistant 消息。
    // 不能只看最后一条：客户端被限流后常会换模型重试，最后一条是成功回复，
    // 限流提示被夹在中间，只看最后一条就会漏记。
    for line in text.lines().rev() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = v.get("content") else {
            continue;
        };
        let mut collected = String::new();
        if let Some(parts) = content.as_array() {
            for p in parts {
                if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                    collected.push_str(t);
                }
            }
        } else if let Some(t) = content.as_str() {
            collected.push_str(t);
        }
        if collected.is_empty() {
            continue;
        }
        if let Some((kind, reset_at)) = classify(&collected) {
            let detected_at = v
                .get("timestamp")
                .and_then(serde_json::Value::as_i64)
                .map(crate::normalize_ts)
                .unwrap_or(0);
            let model = v
                .get("providerData")
                .and_then(|p| p.get("model"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| extract_model(&collected));
            return Ok(Some(LimitSignal {
                session_id: session_id.to_string(),
                uid: uid.to_string(),
                message: truncate(&collected, 200),
                kind,
                reset_at,
                detected_at,
                model,
            }));
        }
    }
    Ok(None)
}

/// 扫描缓存：jsonl 没变化就不重复解析。
///
/// 后台每 5 秒轮询一次，22 个会话文件全量读太浪费；按 mtime 命中缓存后，
/// 一轮只剩 stat 的开销。uid 参与 key：切号后归属变了要重新生成 signal。
type CacheKey = (PathBuf, String);
static SCAN_CACHE: OnceLock<Mutex<HashMap<CacheKey, (u128, Option<LimitSignal>)>>> = OnceLock::new();

pub fn scan_session_cached(
    jsonl: &Path,
    session_id: &str,
    uid: &str,
) -> Result<Option<LimitSignal>> {
    let Some(stamp) = std::fs::metadata(jsonl)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
    else {
        return Ok(None);
    };

    let key = (jsonl.to_path_buf(), uid.to_string());
    let cache = SCAN_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock() {
        if let Some((cached_stamp, sig)) = guard.get(&key) {
            if *cached_stamp == stamp {
                return Ok(sig.clone());
            }
        }
    }

    let sig = scan_session(jsonl, session_id, uid)?;
    if let Ok(mut guard) = cache.lock() {
        guard.insert(key, (stamp, sig.clone()));
    }
    Ok(sig)
}

fn truncate(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= n {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "429 您的使用量已超出频率限制，将在 2026-10-19 09:56:23 UTC+8 重置，您也可以切换其他模型继续使用。 (01a14d687dd77777bfa4dc6315232db5/9bb9bbc1)";

    #[test]
    fn parses_real_message() {
        let (kind, reset) = classify(REAL).expect("应识别为限流");
        assert_eq!(kind, LimitKind::Model, "带了「切换其他模型」应判为模型级");
        let ts = reset.expect("应解析出重置时间");
        // 2026-10-19 09:56:23 UTC+8 == 2026-10-19 01:56:23 UTC
        assert_eq!(ts, 1792374983);
    }

    #[test]
    fn model_level_with_shorter_wording() {
        // 客户端实际返回的另一版措辞
        let text = "抱歉，发生未知错误暂无响应。您可以稍后再试，或尝试新建任务、切换模型\n\
                    当前您在Deepseek-V4.1-Flash模型的使用量已超出频率限制，\
                    可在2026-10-23 09:51:21重置后使用。您可先切换至其他模型。";
        let (kind, reset) = classify(text).expect("应识别为限流");
        assert_eq!(kind, LimitKind::Model);
        assert_eq!(reset, parse_utc8_datetime("2026-10-23 09:51:21"));
    }

    #[test]
    fn account_level_when_credit_wording() {
        let (kind, _) = classify("402 积分不足，请充值后重试").expect("应识别");
        assert_eq!(kind, LimitKind::Account);
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(classify("工具调用失败：ENOENT").is_none());
        assert!(classify("网络连接超时").is_none());
    }

    #[test]
    fn ignores_chatter_and_code_mentioning_limits() {
        // 助手解释这个功能时的正常句子，不能当成限流
        assert!(classify("积分与限流展示都对了，用真实数据再验一次").is_none());
        assert!(classify("刚才被限流了，换了个模型").is_none());
        // 代码里出现的行号 429
        assert!(classify("429→    const isMoreAccurate = accuracyDelta < 0").is_none());
        // 真正的错误原文仍然识别
        assert!(classify(REAL).is_some());
    }

    #[test]
    fn finds_limit_before_final_success() {
        let dir = std::env::temp_dir().join(format!("hub-rl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("s.jsonl");
        let lines = [
            serde_json::json!({
                "role": "assistant",
                "timestamp": 1_792_374_000_000_i64,
                "content": [{ "type": "output_text", "text": REAL }],
                "providerData": { "model": "glm-5.2" }
            })
            .to_string(),
            serde_json::json!({
                "role": "assistant",
                "timestamp": 1_792_374_100_000_i64,
                "content": [{ "type": "output_text", "text": "已换模型完成" }]
            })
            .to_string(),
        ];
        std::fs::write(&file, lines.join("\n")).unwrap();

        let sig = scan_session(&file, "s", "u").unwrap().expect("应找到限流");
        assert_eq!(sig.kind, LimitKind::Model);
        assert_eq!(sig.model.as_deref(), Some("glm-5.2"));
        assert_eq!(sig.detected_at, 1_792_374_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extracts_model_from_message() {
        let text = "当前您在Deepseek-V4.1-Flash模型的使用量已超出频率限制";
        assert_eq!(extract_model(text).as_deref(), Some("Deepseek-V4.1-Flash"));
    }

    #[test]
    fn datetime_roundtrip() {
        let ts = parse_utc8_datetime("2026-10-19 09:56:23").unwrap();
        assert_eq!(ts, 1792374983);
        // 与 format_ts 的结果对得上
        assert_eq!(crate::format_ts(ts), "2026-10-19 09:56");
    }

    #[test]
    fn cooldown_uses_reset_time() {
        let s = LimitSignal {
            session_id: "x".into(),
            uid: "u".into(),
            message: REAL.into(),
            kind: LimitKind::Model,
            reset_at: Some(2_000_000_000),
            detected_at: 0,
            model: Some("glm-5.2".into()),
        };
        assert_eq!(s.cooldown_until(1_900_000_000, 0), 2_000_000_000);
        // 重置时间已过 → 退避
        assert_eq!(s.cooldown_until(2_100_000_000, 0), 2_100_000_600);
    }
}
