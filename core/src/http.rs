//! 极简 HTTP：调用系统自带的 `curl`，请求配置通过 stdin 传入。
//!
//! 为什么不用 reqwest / ureq：
//! - 只需要 GET / POST 两个动作，没必要为它拉几十个 crate；
//! - `curl` 在 Windows 10 1803+ 自带，走 Schannel，复用系统信任链；
//! - 配置写 stdin（`curl -K -`），token 不进 argv，也不会落在临时文件里。
//!
//! 代价是依赖 `curl.exe`；找不到时直接报错，不做降级。

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::io::Write;
use std::process::Stdio;

/// 发一个请求，返回 (HTTP 状态码, 解析后的 JSON)。
///
/// HTTP >= 400 直接报错；业务层错误码（`code != 0`）交给调用方判断 ——
/// 签到「今天已领」这类返回是正常的业务状态，不该在这里被当成失败。
pub fn send(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<(u16, Value)> {
    let mut config = format!(
        "url = \"{}\"\nrequest = \"{}\"\nsilent\nshow-error\nmax-time = 20\n",
        esc(url),
        esc(method)
    );
    for (k, v) in headers {
        config.push_str(&format!("header = \"{}: {}\"\n", esc(k), esc(v)));
    }
    if let Some(b) = body {
        config.push_str(&format!("data = \"{}\"\n", esc(b)));
    }
    config.push_str("write-out = \"\\n%{http_code}\"\n");

    // 必须走 proc::hidden：否则每发一个请求都会闪一次 cmd 窗口
    let mut child = crate::proc::hidden("curl")
        .args(["-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("启动 curl 失败（Windows 10 1803+ 自带 curl，请确认 PATH 里有 curl.exe）")?;
    child
        .stdin
        .as_mut()
        .context("curl stdin 不可用")?
        .write_all(config.as_bytes())
        .context("写入 curl 配置失败")?;

    let out = child.wait_with_output().context("等待 curl 失败")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        bail!("请求失败：{}", first_line(&stderr));
    }

    let (body, code) = match stdout.rsplit_once('\n') {
        Some((b, c)) => (b, c.trim().to_string()),
        None => (stdout.as_ref(), String::new()),
    };
    let code: u16 = code.parse().unwrap_or(0);
    if code == 0 {
        bail!("没有拿到 HTTP 状态码");
    }

    let value: Value = serde_json::from_str(body)
        .with_context(|| format!("响应不是 JSON（HTTP {code}）：{}", first_line(body)))?;

    if code >= 400 {
        let msg = value
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("请求被拒绝");
        bail!("HTTP {code} · {msg}");
    }
    Ok((code, value))
}

/// 账号接口的标准请求头。`X-Device-Token` 是客户端 Turing SDK 的反爬头，
/// hub 不伪造；实测签到、账号资料都不需要它。
pub fn account_headers(token: &str, uid: &str, domain: &str) -> Vec<(String, String)> {
    vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-User-Id".into(), uid.to_string()),
        ("X-Domain".into(), domain.to_string()),
        ("Accept".into(), "application/json".into()),
        ("Content-Type".into(), "application/json".into()),
    ]
}

/// curl config 里的双引号字符串需要转义，换行要压掉（配置按行解析）。
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ")
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .take(160)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_config_values() {
        assert_eq!(esc("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(esc("a\nb"), "a b");
    }

    #[test]
    fn builds_account_headers() {
        let h = account_headers("tok", "uid-1", "www.workbuddy.cn");
        assert!(h.iter().any(|(k, v)| k == "Authorization" && v == "Bearer tok"));
        assert!(h.iter().any(|(k, v)| k == "X-User-Id" && v == "uid-1"));
    }
}
