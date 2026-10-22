//! workbuddy-hub 核心库。
//!
//! 四块能力，互不耦合：
//! - [`client_compat`] 客户端探测与 at-rest 密钥获取
//! - [`auth_crypto`]   凭据字段的 AES-256-GCM 解密 / 封装
//! - [`account_store`] 账号库读取（兼容 DSH 现有格式）
//! - [`task_index`]    只读聚合所有账号的任务

pub mod account_store;
pub mod auth_crypto;
pub mod checkin;
pub mod client_compat;
pub mod client_control;
pub mod daemon;
pub mod proc;
pub mod rate_limit;
pub mod switch_engine;
pub mod task_index;
pub mod task_unify;
pub mod transcript;

/// 时间戳归一化：大于 1e11 的视为毫秒。
///
/// 实测账号库里单位并不统一（9 个毫秒、1 个秒），不归一化会把有效期
/// 算成五万多年，进而误判「永不过期」。
pub fn normalize_ts(v: i64) -> i64 {
    if v > 100_000_000_000 {
        v / 1000
    } else {
        v
    }
}

/// 秒级时间戳格式化为本地时间（UTC+8），零依赖实现。
pub fn format_ts(secs: i64) -> String {
    if secs <= 0 {
        return "-".into();
    }
    let local = secs + 8 * 3600;
    let days = local.div_euclid(86_400);
    let rem = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// 只取日期部分。
pub fn format_date(secs: i64) -> String {
    if secs <= 0 {
        return "-".into();
    }
    let (y, m, d) = civil_from_days((secs + 8 * 3600).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant 的 days → (y, m, d) 算法。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 当前 unix 秒。
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod ts_tests {
    use super::*;

    #[test]
    fn normalizes_millis() {
        assert_eq!(normalize_ts(1_794_734_516_571), 1_794_734_516);
        assert_eq!(normalize_ts(1_794_728_770), 1_794_728_770);
    }

    #[test]
    fn formats_known_date() {
        // 2026-10-21 16:36 (UTC+8) = 1792571760
        assert_eq!(format_date(1_792_571_760), "2026-10-21");
    }
}
