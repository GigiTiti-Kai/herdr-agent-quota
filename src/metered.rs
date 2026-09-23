//! DeepSeek / OpenRouter で動く Claude Code ペインの `bal` / `day · mon` / `ses`。
//!
//! 金額と残高はキーを持つ中継（dotfiles `bin/claude-or-proxy.py`）が
//! `CLAUDE_BILLING_DIR` に書く。ここは読むだけで、通信もキーも持たない。
//! 形式の正本は `fork/specs/2026-09-24-third-party-meters-design.md`。
//!
//! どのペインが第三者モデルかは、statusLine hook がそのセッションの
//! `CLAUDE_BILLING_BACKEND` を `session-backends.json` に書いて結びつける。
//! 判定するのは `refresh::resolved_pane_tokens` の Claude 分岐だけ。

use crate::model::Severity;
use crate::presentation::{
    gauge_cells, meter, provider_model_label, MetadataTokens, SidebarShape, GAUGE_LABEL_WIDTH,
    NARROW_IDENTITY_CONTENT_WIDTH,
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SESSION_FILE: &str = "session-backends.json";
/// 中継の summary と同じ。これより古い結びつきは捨てる
const SESSION_TTL_SECONDS: u64 = 7 * 86_400;
const JST_OFFSET_SECONDS: i64 = 9 * 3_600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    DeepSeek,
    OpenRouter,
}

impl Backend {
    /// launcher が export する `CLAUDE_BILLING_BACKEND` の値
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "deepseek" => Some(Self::DeepSeek),
            "openrouter" => Some(Self::OpenRouter),
            _ => None,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::OpenRouter => "openrouter",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::DeepSeek => "DeepSeek",
            Self::OpenRouter => "OpenRouter",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Binding {
    backend: Backend,
    seen: u64,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// ponytail: ロック無しの read-modify-write。2 つの hook が同時に書くと片方の
/// 結びつきが消えることがあるが、次の statusLine（最長 refreshInterval 60 秒）で戻る。
pub fn record_session(
    state_root: &Path,
    session_id: &str,
    backend: Backend,
    now: u64,
) -> Result<()> {
    let path = state_root.join(SESSION_FILE);
    let mut map: BTreeMap<String, Binding> = read_json(&path).unwrap_or_default();
    map.retain(|_, binding| now.saturating_sub(binding.seen) < SESSION_TTL_SECONDS);
    map.insert(session_id.to_string(), Binding { backend, seen: now });
    std::fs::create_dir_all(state_root).context("create plugin state directory")?;
    write_bindings(&path, &map)
}

pub fn session_backend(state_root: &Path, session_id: &str, now: u64) -> Option<Backend> {
    read_json::<BTreeMap<String, Binding>>(&state_root.join(SESSION_FILE))?
        .remove(session_id)
        .filter(|binding| now.saturating_sub(binding.seen) < SESSION_TTL_SECONDS)
        .map(|binding| binding.backend)
}

/// 同じ session が素の claude で `--resume` された時に、前の backend を外す。
/// statusLine は毎回呼ばれるので、束縛が無ければファイルに触れない。
pub fn forget_session(state_root: &Path, session_id: &str) -> Result<()> {
    let path = state_root.join(SESSION_FILE);
    let Some(mut map) = read_json::<BTreeMap<String, Binding>>(&path) else {
        return Ok(());
    };
    if map.remove(session_id).is_none() {
        return Ok(());
    }
    write_bindings(&path, &map)
}

fn write_bindings(path: &Path, map: &BTreeMap<String, Binding>) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec(map)?).context("write session backends")?;
    std::fs::rename(&temporary, path).context("replace session backends")
}

/// 中継と statusline.js と同じ置き場
pub fn billing_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_BILLING_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/state/claude-billing"))
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Summary {
    day: String,
    day_usd: f64,
    month: String,
    month_usd: f64,
    sessions: BTreeMap<String, SessionCost>,
    unpriced: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SessionCost {
    usd: f64,
    unpriced: u64,
}

#[derive(Debug, Deserialize)]
struct Balance {
    status: String,
    balance: Option<f64>,
    full: Option<f64>,
    fetched_at: Option<f64>,
    last_ok_at: Option<f64>,
}

/// 日本時間の `YYYY-MM-DD`
fn jst_day(now: u64) -> String {
    let local = now as i64 + JST_OFFSET_SECONDS;
    time::OffsetDateTime::from_unix_timestamp(local)
        .map(|t| {
            let d = t.date();
            format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
        })
        .unwrap_or_default()
}

/// 1 セント未満の使用が多いので、0.1 ドル未満は 3 桁まで出す（statusline.js と同じ）
fn usd(value: f64) -> String {
    if value >= 0.1 {
        format!("${value:.2}")
    } else {
        format!("${value:.3}")
    }
}

fn ago(seconds: f64) -> String {
    let seconds = seconds.max(0.0);
    if seconds < 3_600.0 {
        format!("{}m前", ((seconds / 60.0).round() as u64).max(1))
    } else if seconds < 86_400.0 {
        format!("{}h前", (seconds / 3_600.0).round() as u64)
    } else {
        format!("{}d前", (seconds / 86_400.0).round() as u64)
    }
}

fn flag(unpriced: u64) -> &'static str {
    if unpriced > 0 {
        "+?"
    } else {
        ""
    }
}

/// 燃料計の向き: 満タン = 最後にチャージした直後の残高。残り 20% 未満で warning、10% 未満で danger
fn balance_row(
    balance: Option<&Balance>,
    now: u64,
    shape: SidebarShape,
) -> (String, Option<Severity>) {
    let Some(balance) = balance else {
        return ("bal ?".to_string(), None);
    };
    if balance.status == "key_expired" {
        return ("bal key期限切れ".to_string(), Some(Severity::Danger));
    }
    let Some(value) = balance.balance else {
        return ("bal ?".to_string(), None);
    };
    if balance.status != "ok" {
        let since = balance
            .last_ok_at
            .or(balance.fetched_at)
            .unwrap_or(now as f64);
        return (
            format!("bal {} ({})", usd(value), ago(now as f64 - since)),
            None,
        );
    }
    let left = match balance.full {
        Some(full) if full > 0.0 => (value / full * 100.0).round().clamp(0.0, 100.0) as u32,
        _ => 0,
    };
    let severity = if left < 10 {
        Severity::Danger
    } else if left < 20 {
        Severity::Warning
    } else {
        Severity::Normal
    };
    // 割合は推測の満タン（初回観測・チャージ時の残高）が分母なので文字では出さない。
    // 実額だけ見せ、バーと色は減り具合の目安に使う
    let text = match gauge_cells(shape, "bal") {
        Some(cells) => format!(
            "{:<width$} {} {}",
            "bal",
            meter(left, cells),
            usd(value),
            width = GAUGE_LABEL_WIDTH
        ),
        None => format!("bal {}", usd(value)),
    };
    (text, Some(severity))
}

/// Claude の窓の代わりに `bal` / `day · mon` / `ses` を既存の 3 枠へ入れる。
/// herdr 側のトークン名とテンプレートは変えない。cache / ttl / context はそのまま
pub fn overlay(
    values: &mut MetadataTokens,
    backend: Backend,
    session_id: &str,
    dir: Option<&Path>,
    now: u64,
    shape: SidebarShape,
) {
    let summary: Summary = dir
        .and_then(|dir| read_json(&dir.join(format!("summary-{}.json", backend.key()))))
        .unwrap_or_default();
    let balance: Option<Balance> =
        dir.and_then(|dir| read_json(&dir.join(format!("balance-{}.json", backend.key()))));
    let today = jst_day(now);
    let this_month = today.get(..7).unwrap_or_default();
    let (day, month, month_unpriced) = (
        if summary.day == today {
            summary.day_usd
        } else {
            0.0
        },
        if summary.month == this_month {
            summary.month_usd
        } else {
            0.0
        },
        if summary.month == this_month {
            summary.unpriced
        } else {
            0
        },
    );
    let session = summary.sessions.get(session_id);

    let narrow = shape.content_width > 0 && shape.content_width < NARROW_IDENTITY_CONTENT_WIDTH;
    values.quota_provider = if narrow && !values.quota_model.is_empty() {
        String::new()
    } else {
        backend.display_name().to_string()
    };
    values.quota_provider_model = provider_model_label(
        backend.display_name(),
        &values.quota_model,
        shape.content_width,
    );
    (values.quota_5h, values.quota_5h_severity) = balance_row(balance.as_ref(), now, shape);
    values.quota_week = format!(
        "day {}{} · mon {}{}",
        usd(day),
        flag(month_unpriced),
        usd(month),
        flag(month_unpriced)
    );
    values.quota_week_severity = Some(Severity::Normal);
    values.quota_week_scoped = format!(
        "ses {}{}",
        usd(session.map_or(0.0, |s| s.usd)),
        flag(session.map_or(0, |s| s.unpriced))
    );
    values.quota_week_scoped_severity = Some(Severity::Normal);
    values.quota_month.clear();
    values.quota_month_severity = None;
    // Claude の低残量通知と headroom 並びに第三者モデルの残高を混ぜない
    values.quota_headroom = None;
    // Claude の usage API の失敗は、このペインの数字とは関係ない
    values.quota_error = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SidebarLayout;
    use crate::model::{Provider, Severity};
    use crate::presentation::{MetadataTokens, SidebarShape};
    use std::fs;
    use tempfile::tempdir;

    /// 2026-09-24 03:00 UTC = 日本時間 12:00。
    const NOW: u64 = 1_790_218_800;

    fn base() -> MetadataTokens {
        let mut values = MetadataTokens::unavailable(Provider::Claude, "x");
        values.quota_error = None;
        values.quota_model = "deepseek-flash[1m]".to_string();
        values.quota_5h = "5h 10% 2h".to_string();
        values.quota_week = "7d 18% 3d".to_string();
        values.quota_week_scoped = "Fab 22% 3d".to_string();
        values.quota_month = "30d 1%".to_string();
        values.quota_headroom = Some(78);
        values
    }

    fn packed() -> SidebarShape {
        SidebarShape::default()
    }

    fn gauges() -> SidebarShape {
        SidebarShape {
            layout: SidebarLayout::Gauges,
            meter_cells: Some(6),
            content_width: 40,
        }
    }

    fn write(dir: &std::path::Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    fn summary(dir: &std::path::Path, day: &str) {
        write(
            dir,
            "summary-deepseek.json",
            &format!(
                r#"{{"backend":"deepseek","day":"{day}","day_usd":0.21,"month":"2026-09","month_usd":3.4,
                "sessions":{{"sX":{{"usd":0.05,"unpriced":0,"last":1790218000}}}},"unpriced":0}}"#
            ),
        );
    }

    fn balance(dir: &std::path::Path, status: &str, balance: f64, full: f64) {
        write(
            dir,
            "balance-deepseek.json",
            &format!(
                r#"{{"status":"{status}","balance":{balance},"full":{full},"currency":"USD",
                "fetched_at":{NOW}.0,"last_ok_at":{}.0}}"#,
                NOW - 720
            ),
        );
    }

    fn overlaid(dir: &std::path::Path, shape: SidebarShape) -> MetadataTokens {
        let mut values = base();
        overlay(&mut values, Backend::DeepSeek, "sX", Some(dir), NOW, shape);
        values
    }

    #[test]
    fn a_metered_pane_shows_balance_spend_and_session_instead_of_claude_windows() {
        let dir = tempdir().unwrap();
        summary(dir.path(), "2026-09-24");
        balance(dir.path(), "ok", 7.2, 10.0);
        let values = overlaid(dir.path(), packed());
        assert_eq!(values.quota_5h, "bal $7.20");
        assert_eq!(values.quota_week, "day $0.21 · mon $3.40");
        assert_eq!(values.quota_week_scoped, "ses $0.050");
        assert_eq!(values.quota_month, "");
        assert_eq!(values.quota_month_severity, None);
        assert_eq!(values.quota_headroom, None);
        assert_eq!(values.quota_provider, "DeepSeek");
        assert_eq!(values.quota_provider_model, "DeepSeek/deepseek-flash[1m]");
        assert_eq!(values.quota_5h_severity, Some(Severity::Normal));
        assert_eq!(values.quota_week_severity, Some(Severity::Normal));
    }

    #[test]
    fn the_gauge_layout_draws_a_fuel_meter_of_what_is_left() {
        let dir = tempdir().unwrap();
        balance(dir.path(), "ok", 7.2, 10.0);
        let values = overlaid(dir.path(), gauges());
        assert!(
            values.quota_5h.starts_with("bal ▰▰▰▰"),
            "{}",
            values.quota_5h
        );
        assert!(
            values.quota_5h.ends_with(" $7.20") && !values.quota_5h.contains('%'),
            "{}",
            values.quota_5h
        );
    }

    #[test]
    fn yesterdays_summary_reads_as_nothing_spent_today() {
        let dir = tempdir().unwrap();
        summary(dir.path(), "2026-09-23");
        let values = overlaid(dir.path(), packed());
        assert_eq!(values.quota_week, "day $0.000 · mon $3.40");
    }

    #[test]
    fn no_files_yet_still_prints_every_row() {
        let dir = tempdir().unwrap();
        let values = overlaid(dir.path(), packed());
        assert_eq!(values.quota_5h, "bal ?");
        assert_eq!(values.quota_week, "day $0.000 · mon $0.000");
        assert_eq!(values.quota_week_scoped, "ses $0.000");
    }

    #[test]
    fn an_expired_management_key_and_a_failed_fetch_are_told_apart() {
        let dir = tempdir().unwrap();
        balance(dir.path(), "key_expired", 7.2, 10.0);
        let values = overlaid(dir.path(), packed());
        assert_eq!(values.quota_5h, "bal key期限切れ");
        assert_eq!(values.quota_5h_severity, Some(Severity::Danger));
        balance(dir.path(), "error", 7.2, 10.0);
        assert_eq!(overlaid(dir.path(), packed()).quota_5h, "bal $7.20 (12m前)");
    }

    #[test]
    fn little_left_turns_the_balance_row_warning_then_danger() {
        let dir = tempdir().unwrap();
        balance(dir.path(), "ok", 1.5, 10.0);
        assert_eq!(
            overlaid(dir.path(), packed()).quota_5h_severity,
            Some(Severity::Warning)
        );
        balance(dir.path(), "ok", 0.5, 10.0);
        assert_eq!(
            overlaid(dir.path(), packed()).quota_5h_severity,
            Some(Severity::Danger)
        );
    }

    #[test]
    fn unpriced_responses_are_flagged_on_the_spend_rows() {
        let dir = tempdir().unwrap();
        write(
            dir.path(),
            "summary-deepseek.json",
            r#"{"day":"2026-09-24","day_usd":0.21,"month":"2026-09","month_usd":3.4,
            "sessions":{"sX":{"usd":0.05,"unpriced":1,"last":1}},"unpriced":2}"#,
        );
        let values = overlaid(dir.path(), packed());
        assert_eq!(values.quota_week, "day $0.21+? · mon $3.40+?");
        assert_eq!(values.quota_week_scoped, "ses $0.050+?");
    }

    #[test]
    fn a_session_binding_round_trips_and_week_old_ones_are_dropped() {
        let dir = tempdir().unwrap();
        record_session(dir.path(), "old", Backend::OpenRouter, NOW - 8 * 86_400).unwrap();
        record_session(dir.path(), "sX", Backend::DeepSeek, NOW).unwrap();
        assert_eq!(
            session_backend(dir.path(), "sX", NOW),
            Some(Backend::DeepSeek)
        );
        assert_eq!(session_backend(dir.path(), "old", NOW), None);
        assert_eq!(session_backend(dir.path(), "never", NOW), None);
    }

    #[test]
    fn a_binding_expires_on_read_and_can_be_forgotten() {
        let dir = tempdir().unwrap();
        record_session(dir.path(), "sX", Backend::DeepSeek, NOW).unwrap();
        // 書き込みが止まったまま 7 日経ったものは、ファイルに残っていても効かない
        assert_eq!(session_backend(dir.path(), "sX", NOW + 8 * 86_400), None);
        forget_session(dir.path(), "sX").unwrap();
        assert_eq!(session_backend(dir.path(), "sX", NOW), None);
        // 無いものを忘れても失敗しない（ファイルも作らない）
        let empty = tempdir().unwrap();
        forget_session(empty.path(), "sX").unwrap();
        assert!(!empty.path().join(SESSION_FILE).exists());
    }

    #[test]
    fn a_claude_error_does_not_survive_on_a_metered_row() {
        let dir = tempdir().unwrap();
        let mut values = base();
        values.quota_error = Some("usage 429".to_string());
        overlay(
            &mut values,
            Backend::DeepSeek,
            "sX",
            Some(dir.path()),
            NOW,
            packed(),
        );
        assert_eq!(values.quota_error, None);
    }

    #[test]
    fn backend_names_are_the_ones_the_launchers_export() {
        assert_eq!(Backend::parse("deepseek"), Some(Backend::DeepSeek));
        assert_eq!(Backend::parse("openrouter"), Some(Backend::OpenRouter));
        assert_eq!(Backend::parse("claude"), None);
    }
}
