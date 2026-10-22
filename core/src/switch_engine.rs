//! 切号引擎：备份 → 写凭据 → 重启 → 验证 / 回滚。
//!
//! 这里的每一步都真的会动客户端，所以默认只做「计划」（[`plan`]）和
//! 「演练」（[`apply`] 且 `dry_run = true`）。真正执行必须显式开启，
//! 因为退出客户端会中断正在跑的任务 —— 包括你正在和它对话的那个会话。

use crate::account_store::Account;
use crate::auth_crypto::{encrypt_field, AtRestKey};
use crate::task_index::TaskRow;
use crate::{format_ts, now_secs};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// 切号前的现状分析。全部只读。
#[derive(Debug, Serialize)]
pub struct SwitchPlan {
    pub target_uid: String,
    pub target_short: String,
    pub current_uid: String,
    pub current_short: String,
    pub client_running: bool,
    /// 客户端里状态为 working 的任务数（>0 时切号会打断它们）
    pub working_tasks: Vec<(String, String)>,
    /// 目标账号在本地有多少任务
    pub target_task_count: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApplyReport {
    pub dry_run: bool,
    pub backup_path: Option<String>,
    pub written_fields: Vec<String>,
    pub steps: Vec<String>,
    pub rollback_performed: bool,
}

/// 生成切号计划，不做任何写入。
pub fn plan(
    target: &Account,
    current_uid: &str,
    tasks: &[TaskRow],
) -> SwitchPlan {
    let working: Vec<(String, String)> = tasks
        .iter()
        .filter(|t| t.status == "working")
        .map(|t| (t.short_uid.clone(), t.title.clone()))
        .collect();

    let target_task_count = tasks.iter().filter(|t| t.uid == target.uid).count();
    let client_running = is_client_running();

    let mut warnings = Vec::new();
    if !working.is_empty() {
        warnings.push(format!(
            "客户端有 {} 个任务处于 running 状态，切号需要重启客户端，会中断它们",
            working.len()
        ));
    }
    if target_task_count == 0 {
        warnings.push("目标账号在本地没有任务记录，切过去会看到空列表".into());
    }
    if target.uid == current_uid {
        warnings.push("目标账号就是当前账号，无需切换".into());
    }

    SwitchPlan {
        target_uid: target.uid.clone(),
        target_short: target.summary().short_uid,
        current_uid: current_uid.to_string(),
        current_short: current_uid.chars().take(8).collect(),
        client_running,
        working_tasks: working,
        target_task_count,
        warnings,
    }
}

/// 执行（或演练）切号。
///
/// `dry_run = true` 时只走完备份之后的构造过程，把「将会写入什么」报出来，
/// 不碰客户端进程、不改 auth 文件。
pub fn apply(
    target: &Account,
    key: &AtRestKey,
    auth_path: &Path,
    backup_dir: &Path,
    dry_run: bool,
) -> Result<ApplyReport> {
    let mut steps = Vec::new();
    let mut report = ApplyReport {
        dry_run,
        backup_path: None,
        written_fields: Vec::new(),
        steps: vec![],
        rollback_performed: false,
    };

    // 1. 读当前凭据文件
    let text = fs::read_to_string(auth_path)
        .with_context(|| format!("读取凭据文件失败：{}", auth_path.display()))?;
    let current: Value = serde_json::from_str(&text).context("凭据文件不是合法 JSON")?;
    steps.push("读取当前凭据文件".into());

    // 2. 取目标账号的明文令牌
    let access = target.access_token(key)?;
    let refresh = target.refresh_token(key)?;
    steps.push("解密目标账号令牌".into());

    // 3. 用客户端 at-rest 密钥重新封装
    let access_env = encrypt_field(&access, key, None)?;
    let refresh_env = encrypt_field(&refresh, key, None)?;
    steps.push("用 at-rest 密钥重新封装令牌".into());

    // 4. 组装新文档：替换 auth 与 account.uid，保留其余结构
    let mut next = current.clone();
    let auth = next
        .get_mut("auth")
        .and_then(|v| v.as_object_mut())
        .context("凭据文件缺少 auth 对象")?;
    auth.insert(
        "accessToken".into(),
        json!({ "$wbEncrypted": 1, "envelope": access_env }),
    );
    auth.insert(
        "refreshToken".into(),
        json!({ "$wbEncrypted": 1, "envelope": refresh_env }),
    );
    auth.insert("expiresAt".into(), json!(target.expires_at));
    auth.insert("refreshExpiresAt".into(), json!(target.refresh_expires_at));
    if !target.domain.is_empty() {
        auth.insert("domain".into(), json!(target.domain));
    }
    report.written_fields = vec![
        "auth.accessToken".into(),
        "auth.refreshToken".into(),
        "auth.expiresAt".into(),
        "auth.refreshExpiresAt".into(),
        "auth.domain".into(),
        "account.uid".into(),
    ];
    steps.push("组装新凭据文档".into());

    if let Some(account) = next.get_mut("account").and_then(|v| v.as_object_mut()) {
        account.insert("uid".into(), json!(target.uid));
        // nickname / phoneNumber 属于上一个账号，必须清掉，
        // 否则客户端会显示错误的身份信息（它会在登录后自己补回来）。
        account.remove("nickname");
        account.remove("phoneNumber");
        account.remove("uin");
        steps.push("替换 account.uid 并清除上一账号的身份字段".into());
    }

    if dry_run {
        steps.push("演练模式：不写文件、不动进程".into());
        report.steps = steps;
        return Ok(report);
    }

    // 5. 备份
    fs::create_dir_all(backup_dir)
        .with_context(|| format!("创建备份目录失败：{}", backup_dir.display()))?;
    let stamp = format_ts(now_secs()).replace(['-', ' ', ':'], "");
    let backup = backup_dir.join(format!("workbuddy-desktop.{stamp}.info"));
    fs::copy(auth_path, &backup).context("备份凭据文件失败")?;
    cleanup_old_backups(backup_dir, 10);
    report.backup_path = Some(backup.display().to_string());
    steps.push(format!("备份到 {}", backup.display()));

    // 6. 原子写入
    let serialized = serde_json::to_string_pretty(&next)?;
    if let Err(e) = atomic_write(auth_path, serialized.as_bytes()) {
        // 写入失败时原文件未被替换，直接返回
        report.steps = steps;
        report.rollback_performed = true;
        return Err(e.context("写入凭据失败，原文件未改动"));
    }
    steps.push("原子写入凭据文件".into());

    // 7. 校验回读：能解出新 uid 才算成功
    let verify_text = fs::read_to_string(auth_path)?;
    let verify: Value = serde_json::from_str(&verify_text)?;
    let got_uid = verify
        .get("account")
        .and_then(|a| a.get("uid"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if got_uid != target.uid {
        // 回滚
        if let Err(re) = fs::copy(&backup, auth_path) {
            steps.push(format!("回滚失败：{re}"));
        } else {
            report.rollback_performed = true;
            steps.push("回读校验失败，已回滚".into());
        }
        report.steps = steps;
        bail!("回读校验失败：期望 {}，实际 {}", target.uid, got_uid);
    }
    steps.push("回读校验通过".into());

    report.steps = steps;
    Ok(report)
}

/// 从备份恢复。
pub fn rollback(backup: &Path, auth_path: &Path) -> Result<()> {
    if !backup.is_file() {
        bail!("备份不存在：{}", backup.display());
    }
    fs::copy(backup, auth_path)
        .with_context(|| format!("回滚失败，无法把 {} 写回 {}", backup.display(), auth_path.display()))?;
    Ok(())
}

/// 临时文件 + rename，避免写一半留下坏文件。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("凭据文件没有父目录")?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        now_secs()
    ));
    fs::write(&tmp, bytes).context("写临时文件失败")?;
    fs::rename(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// 只保留最近 N 份备份。
fn cleanup_old_backups(dir: &Path, keep: usize) {
    let mut entries: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("workbuddy-desktop.") && n.ends_with(".info"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return,
    };
    if entries.len() <= keep {
        return;
    }
    entries.sort();
    let remove_count = entries.len() - keep;
    for p in entries.into_iter().take(remove_count) {
        let _ = fs::remove_file(p);
    }
}

/// 客户端是否在运行（直接复用进程控制模块，避免重复探测逻辑）。
pub fn is_client_running() -> bool {
    crate::client_control::is_running()
}

/// 读取凭据文件里当前登录的 uid。
pub fn current_uid(auth_path: &Path) -> Result<String> {
    let text = fs::read_to_string(auth_path)
        .with_context(|| format!("读取凭据文件失败：{}", auth_path.display()))?;
    let doc: Value = serde_json::from_str(&text)?;
    Ok(doc
        .get("account")
        .and_then(|a| a.get("uid"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let dir = std::env::temp_dir().join("hub-test-atomic");
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("x.info");
        fs::write(&f, b"old").unwrap();
        atomic_write(&f, b"new").unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_keeps_newest() {
        let dir = std::env::temp_dir().join("hub-test-cleanup");
        let _ = fs::create_dir_all(&dir);
        for i in 0..15 {
            fs::write(dir.join(format!("workbuddy-desktop.20260101{i:04}.info")), b"x").unwrap();
        }
        cleanup_old_backups(&dir, 10);
        let n = fs::read_dir(&dir).unwrap().count();
        assert_eq!(n, 10);
        let _ = fs::remove_dir_all(&dir);
    }
}
