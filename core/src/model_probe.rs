//! 主动探测：用账号自己的凭据，拿指定模型发一次最小请求，判断是否被限流。
//!
//! 相比翻会话日志，这条路径更直接：服务端当场返回 429 / 额度文案，
//! 不需要等客户端把错误写进 jsonl。代价是会产生一次真实请求（max_tokens = 1），
//! 所以只在需要时调用，不做后台高频轮询。

use crate::checkin::Credentials;
use crate::rate_limit;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    pub model: String,
    pub limited: bool,
    pub http_status: u16,
    pub message: String,
    pub reset_at: Option<i64>,
}

/// 发一次最小对话请求。`model` 用客户端里的模型 id（如 `deepseek-v4.1-flash`）。
pub fn probe(c: &Credentials, model: &str) -> Result<ProbeResult> {
    let domain = if c.domain.trim().is_empty() {
        "www.workbuddy.cn"
    } else {
        c.domain.trim()
    };
    let url = format!("https://{domain}/v2/chat/completions");
    let headers = crate::http::account_headers(c.token, c.uid, domain);
    let body = json!({
        "model": model,
        "messages": [{ "role": "user", "content": "hi" }],
        "max_tokens": 1,
        "stream": true
    })
    .to_string();

    let (code, text) = crate::http::send_raw("POST", &url, &headers, Some(&body))?;
    let message = extract_message(&text);
    let (kind, reset_at) = rate_limit::classify(&message)
        .map(|(k, r)| (Some(k), r))
        .unwrap_or((None, None));

    Ok(ProbeResult {
        model: model.to_string(),
        limited: code == 429 || kind.is_some(),
        http_status: code,
        message,
        reset_at,
    })
}

/// 从 JSON 或 SSE 文本里取出可读的错误信息。
fn extract_message(text: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if let Some(s) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .or_else(|| v.get("msg"))
            .or_else(|| v.get("message"))
            .and_then(Value::as_str)
        {
            return s.to_string();
        }
    }
    text.lines()
        .find(|l| l.contains("429") || l.contains("频率") || l.contains("额度"))
        .unwrap_or_else(|| text.lines().next().unwrap_or(""))
        .trim()
        .chars()
        .take(300)
        .collect()
}
