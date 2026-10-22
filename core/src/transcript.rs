//! 会话正文预览：把 `projects/<编码 cwd>/<session-id>.jsonl` 解析成可读的消息流。
//!
//! jsonl 里混着七种记录，界面只关心其中四种：用户消息、助手回复、工具调用、
//! 工具结果。`reasoning` 的体积通常占一半以上，只统计字数不展示 —— 否则
//! 预览会被模型的思考过程淹没，反而看不到人真正说了什么。
//!
//! 解析全程只读，单文件上限由 [`MAX_ENTRIES`] 控制：客户端不裁剪历史，
//! 一个跑了几天的会话可能有上万条记录，界面只需要最后几百条。

use crate::normalize_ts;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// 最多返回多少条消息。更早的只计入统计。
const MAX_ENTRIES: usize = 400;
/// 单条消息的展示上限（字符数）。
const MAX_TEXT: usize = 4000;
/// 工具结果的展示上限，比正文更短 —— 多数是日志，展开才看。
const MAX_TOOL_OUTPUT: usize = 600;
/// 工具调用参数的摘要长度。
const MAX_TOOL_ARGS: usize = 160;

/// 一条可展示的记录。
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    /// unix 秒
    pub ts: i64,
    /// `user` / `assistant` / `tool` / `tool_result`
    pub kind: String,
    /// 工具名（仅 tool / tool_result）
    pub name: Option<String>,
    /// 工具执行状态（仅 tool_result）
    pub status: Option<String>,
    pub text: String,
    /// 原始字符数，用于告诉用户「被截断了多少」
    pub chars: usize,
    pub truncated: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Stats {
    pub user_turns: usize,
    pub assistant_turns: usize,
    pub tool_calls: usize,
    pub reasoning_chars: usize,
    pub total_chars: usize,
    /// 因为超出上限没有返回的条数
    pub dropped_entries: usize,
    /// 工具名 → 调用次数，按次数倒序
    pub tools: Vec<(String, usize)>,
}

#[derive(Debug, Serialize)]
pub struct Transcript {
    pub session_id: String,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub started_at: i64,
    pub ended_at: i64,
    pub entries: Vec<Entry>,
    pub stats: Stats,
    pub path: String,
    pub size_bytes: u64,
}

impl Transcript {
    fn new(path: &Path) -> Self {
        let session_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let size_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        Self {
            session_id,
            title: None,
            cwd: None,
            model: None,
            started_at: 0,
            ended_at: 0,
            entries: Vec::new(),
            stats: Stats::default(),
            path: path.display().to_string(),
            size_bytes,
        }
    }
}

/// 解析一个会话文件。
pub fn parse(path: &Path) -> anyhow::Result<Transcript> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut out = Transcript::new(path);
    let mut lines: VecDeque<Entry> = VecDeque::new();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut buf: Vec<u8> = Vec::new();

    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        // 会话里偶有被截断的行（进程被杀时正在写），lossy 解析后跳过即可
        let raw = String::from_utf8_lossy(&buf);
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "ai-title" => {
                if let Some(t) = v.get("aiTitle").and_then(Value::as_str) {
                    if !t.trim().is_empty() {
                        out.title = Some(t.to_string());
                    }
                }
            }
            "message" => {
                let role = v.get("role").and_then(Value::as_str).unwrap_or("user");
                let ts = ts_of(&v);
                if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                    if out.cwd.is_none() {
                        out.cwd = Some(cwd.to_string());
                    }
                }
                if let Some(m) = model_of(&v) {
                    out.model = Some(m);
                }
                let text = content_text(v.get("content"));
                let text = if role == "user" {
                    strip_system_reminders(&text)
                } else {
                    text.trim().to_string()
                };
                // 只有系统注入、没有真人内容的轮次不计入
                if text.is_empty() {
                    continue;
                }
                if role == "assistant" {
                    out.stats.assistant_turns += 1;
                } else {
                    out.stats.user_turns += 1;
                }
                out.stats.total_chars += text.chars().count();
                let (text, chars, truncated) = clip(&text, MAX_TEXT);
                push(&mut lines, &mut out, Entry {
                    ts,
                    kind: role.to_string(),
                    name: None,
                    status: None,
                    text,
                    chars,
                    truncated,
                });
            }
            "reasoning" => {
                let text = content_text(v.get("content"));
                let text = if text.trim().is_empty() {
                    content_text(v.get("rawContent"))
                } else {
                    text
                };
                out.stats.reasoning_chars += text.chars().count();
            }
            "function_call" => {
                let name = v
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                if let Some(m) = model_of(&v) {
                    out.model = Some(m);
                }
                let args = v.get("arguments").cloned().unwrap_or(Value::Null);
                let summary = summarize_args(&args);
                *tools.entry(name.clone()).or_default() += 1;
                out.stats.tool_calls += 1;
                push(&mut lines, &mut out, Entry {
                    ts: ts_of(&v),
                    kind: "tool".into(),
                    name: Some(name),
                    status: None,
                    text: summary,
                    chars: args.to_string().chars().count(),
                    truncated: false,
                });
            }
            "function_call_result" => {
                let text = v
                    .get("output")
                    .and_then(|o| o.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let (text, chars, truncated) = clip(&text, MAX_TOOL_OUTPUT);
                push(&mut lines, &mut out, Entry {
                    ts: ts_of(&v),
                    kind: "tool_result".into(),
                    name: v
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status: v
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    text,
                    chars,
                    truncated,
                });
            }
            _ => {}
        }
    }

    out.entries = lines.into_iter().collect();
    out.stats.tools = sorted_tools(tools);
    Ok(out)
}

/// 找到某个会话的 jsonl。先按 `cwd` 编码推路径，推不中再全盘扫一遍
/// （`cwd` 为空或后来被改过的会话只能走兜底）。
pub fn find_session_file(projects: &Path, id: &str, cwd: &str) -> Option<PathBuf> {
    if !cwd.trim().is_empty() {
        let direct = projects
            .join(crate::daemon::encode_cwd(cwd))
            .join(format!("{id}.jsonl"));
        if direct.is_file() {
            return Some(direct);
        }
    }
    for entry in fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn push(lines: &mut VecDeque<Entry>, out: &mut Transcript, e: Entry) {
    if out.started_at == 0 || (e.ts > 0 && e.ts < out.started_at) {
        out.started_at = e.ts;
    }
    if e.ts > out.ended_at {
        out.ended_at = e.ts;
    }
    if lines.len() >= MAX_ENTRIES {
        lines.pop_front();
        out.stats.dropped_entries += 1;
    }
    lines.push_back(e);
}

fn ts_of(v: &Value) -> i64 {
    v.get("timestamp")
        .and_then(Value::as_i64)
        .map(normalize_ts)
        .unwrap_or(0)
}

fn model_of(v: &Value) -> Option<String> {
    let pd = v.get("providerData")?;
    for key in ["requestModelName", "requestModelId", "model"] {
        if let Some(s) = pd.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// content 可能是数组（多段文本 / 图片引用），也可能是纯字符串。
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut parts = Vec::new();
            for it in items {
                match it.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") | Some("reasoning_text") => {
                        if let Some(t) = it.get("text").and_then(Value::as_str) {
                            parts.push(t);
                        }
                    }
                    Some("image_blob_ref") => parts.push("［图片］"),
                    _ => {}
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// 去掉客户端自动注入的 `<system-reminder>` 块。
///
/// 每轮用户消息前面都可能挂一段几百行的环境上下文（工作目录、身份文件、技能列表），
/// 预览里留着它会把真正的一句话挤到屏幕外。
fn strip_system_reminders(s: &str) -> String {
    const OPEN: &str = "<system-reminder";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// 把工具参数压成一行可读摘要。
fn summarize_args(args: &Value) -> String {
    // arguments 有的是对象，有的是 JSON 字符串
    let v = match args {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::String(s.clone())),
        other => other.clone(),
    };
    const KEYS: [&str; 9] = [
        "command",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "prompt",
        "description",
        "task",
    ];
    if let Value::Object(map) = &v {
        for key in KEYS {
            if let Some(Value::String(s)) = map.get(key) {
                if !s.trim().is_empty() {
                    return one_line(s, MAX_TOOL_ARGS);
                }
            }
        }
        for val in map.values() {
            if let Value::String(s) = val {
                if !s.trim().is_empty() {
                    return one_line(s, MAX_TOOL_ARGS);
                }
            }
        }
        return String::new();
    }
    match &v {
        Value::String(s) if !s.trim().is_empty() => one_line(s, MAX_TOOL_ARGS),
        _ => String::new(),
    }
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (flat, _, _) = clip(&flat, max);
    flat
}

fn clip(s: &str, max: usize) -> (String, usize, bool) {
    let n = s.chars().count();
    if n <= max {
        (s.to_string(), n, false)
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('…');
        (t, n, true)
    }
}

fn sorted_tools(map: BTreeMap<String, usize>) -> Vec<(String, usize)> {
    let mut v: Vec<_> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn sample(tag: &str, lines: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wb-hub-transcript-{}-{tag}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}.jsonl"));
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn parses_messages_tools_and_strips_reminders() {
        let path = sample("basic", &[
            r#"{"type":"session-meta","timestamp":1792570303988}"#,
            r#"{"type":"message","role":"user","timestamp":1792570295244,"content":[{"type":"input_text","text":"<system-reminder data-role=\"x\">环境\n上下文</system-reminder>\n怎么切换账号"}]}"#,
            r#"{"type":"reasoning","timestamp":1792570328577,"content":[{"type":"reasoning_text","text":"先想一下……"}]}"#,
            r#"{"type":"message","role":"assistant","timestamp":1792570330000,"providerData":{"requestModelName":"Hy4 preview"},"content":[{"type":"output_text","text":"可以这样切。"}]}"#,
            r#"{"type":"function_call","timestamp":1792570331000,"name":"Bash","arguments":"{\"command\":\"git status\"}"}"#,
            r#"{"type":"function_call_result","timestamp":1792570332000,"name":"Bash","status":"completed","output":{"type":"text","text":"clean"}}"#,
            r#"{"type":"ai-title","timestamp":1792570340000,"aiTitle":"切换账号保留任务"}"#,
            "not-json",
        ]);
        let t = parse(&path).unwrap();
        assert_eq!(t.title.as_deref(), Some("切换账号保留任务"));
        assert_eq!(t.model.as_deref(), Some("Hy4 preview"));
        assert_eq!(t.entries.len(), 4);
        assert_eq!(t.entries[0].kind, "user");
        assert!(!t.entries[0].text.contains("system-reminder"));
        assert_eq!(t.entries[0].text, "怎么切换账号");
        assert_eq!(t.entries[2].kind, "tool");
        assert_eq!(t.entries[2].name.as_deref(), Some("Bash"));
        assert_eq!(t.entries[2].text, "git status");
        assert_eq!(t.stats.user_turns, 1);
        assert_eq!(t.stats.assistant_turns, 1);
        assert_eq!(t.stats.tool_calls, 1);
        assert_eq!(t.stats.reasoning_chars, "先想一下……".chars().count());
        assert_eq!(t.stats.tools[0], ("Bash".to_string(), 1));
        assert!(t.started_at > 0 && t.ended_at >= t.started_at);
        fs::remove_file(&path).ok();
    }

    #[test]
    fn keeps_only_the_tail() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..(MAX_ENTRIES + 25) {
            lines.push(format!(
                r#"{{"type":"message","role":"user","timestamp":{},"content":[{{"type":"input_text","text":"第 {i} 条"}}]}}"#,
                1_792_570_000 + i as i64
            ));
        }
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let path = sample("tail", &refs);
        let t = parse(&path).unwrap();
        assert_eq!(t.entries.len(), MAX_ENTRIES);
        assert_eq!(t.stats.dropped_entries, 25);
        assert!(t.entries.last().unwrap().text.contains("第 424 条"));
        fs::remove_file(&path).ok();
    }

    #[test]
    fn encodes_cwd_like_client() {
        assert_eq!(
            crate::daemon::encode_cwd(r"C:\Users\Administrator\WorkBuddy\2026-10-21-16-11-34"),
            "c-Users-Administrator-WorkBuddy-2026-10-21-16-11-34"
        );
    }
}
