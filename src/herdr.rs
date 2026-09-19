use crate::model::{ContextUsage, Harness, Provider};
use crate::presentation::{MetadataTokens, RowStyle, SidebarShape};
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

const METADATA_TTL_MS: &str = "86400000";
const MAX_METADATA_TOKENS: usize = 16;
/// Every name [`desired_tokens`] can produce, and nothing else.
///
/// This list is the comparison set for [`metadata_matches`] and the report set
/// for [`metadata_report_names`]. A name that is listed but never produced is
/// not free: it is compared on every refresh and it competes for Herdr's
/// 16-token report budget. Add a name here only together with the field that
/// fills it.
const METADATA_TOKEN_NAMES: [&str; 38] = [
    "quota_group",
    "quota_pad",
    "quota_icon",
    "quota_icon_working",
    "quota_icon_done",
    "quota_provider",
    "quota_model",
    "quota_provider_model",
    "quota_context",
    "quota_context_normal",
    "quota_context_warning",
    "quota_context_danger",
    "quota_cache",
    "quota_cache_ttl",
    "quota_cache_state",
    "quota_5h_normal",
    "quota_5h_warning",
    "quota_5h_danger",
    "quota_5h_unknown",
    "quota_week_normal",
    "quota_week_warning",
    "quota_week_danger",
    "quota_week_unknown",
    "quota_week_inline_normal",
    "quota_week_inline_warning",
    "quota_week_inline_danger",
    "quota_week_inline_unknown",
    "quota_week_scoped_normal",
    "quota_week_scoped_warning",
    "quota_week_scoped_danger",
    "quota_week_scoped_unknown",
    "quota_month_normal",
    "quota_month_warning",
    "quota_month_danger",
    "quota_month_unknown",
    "quota_topic",
    "quota_error",
    HEADROOM_TOKEN,
];
/// Sort key for Herdr's Agent view: remaining quota as a zero-padded percent
/// (`007`), so Herdr's ordering of the token values is also their numeric
/// ordering.
///
/// Published for every pane whose quota is known, whether or not the user
/// chose `--agent-order quota`, because it never renders: no sidebar row
/// references it. Publishing it unconditionally is what makes changing the
/// order a Herdr-side toggle instead of a metadata write to every pane, and it
/// costs no extra writes — the value only moves when a quota token beside it
/// moves anyway.
pub(crate) const HEADROOM_TOKEN: &str = "quota_headroom";
/// The subset of [`METADATA_TOKEN_NAMES`] whose value comes from the cached
/// quota windows and nothing else. [`quota_rows_have_drifted`] compares these,
/// so a name added here must be one a snapshot alone can render.
const QUOTA_WINDOW_TOKEN_NAMES: [&str; 21] = [
    "quota_5h_normal",
    "quota_5h_warning",
    "quota_5h_danger",
    "quota_5h_unknown",
    "quota_week_normal",
    "quota_week_warning",
    "quota_week_danger",
    "quota_week_unknown",
    "quota_week_inline_normal",
    "quota_week_inline_warning",
    "quota_week_inline_danger",
    "quota_week_inline_unknown",
    "quota_week_scoped_normal",
    "quota_week_scoped_warning",
    "quota_week_scoped_danger",
    "quota_week_scoped_unknown",
    "quota_month_normal",
    "quota_month_warning",
    "quota_month_danger",
    "quota_month_unknown",
    HEADROOM_TOKEN,
];
/// Names a pane may still carry from an older build of this plugin. They are
/// never produced again, so a report clears them until the pane is clean.
const OBSOLETE_METADATA_TOKEN_NAMES: [&str; 14] = [
    "quota_state",
    "quota_status",
    "quota_summary",
    "quota_5h",
    "quota_5h_label",
    "quota_5h_percent",
    "quota_5h_eta",
    "quota_5h_caution",
    "quota_week",
    "quota_week_label",
    "quota_week_percent",
    "quota_week_eta",
    "quota_week_caution",
    "quota_week_inline_caution",
];
const LEGACY_METADATA_TOKEN_NAMES: [&str; 4] = [
    "quota_badge",
    "quota_session",
    "quota_week_inline_label",
    "quota_week_inline_eta",
];
/// The names the context row can be published into, in the order a report
/// clears them. Herdr fixes a token's colour by name, so the only way to
/// colour the context row is to publish it into a name whose row template
/// already carries that colour — which is why `gauges` needs the three
/// severity variants and `packed`/`stacked` keep the plain one.
///
/// Exactly one is ever filled. Publishing a second would draw two context
/// rows in the same pane.
const CONTEXT_TOKEN_NAMES: [&str; 4] = [
    "quota_context",
    "quota_context_normal",
    "quota_context_warning",
    "quota_context_danger",
];
/// Values that must reach the pane in the *same* report that changed them,
/// even when the budget is tight: the identity, the live diagnostics, and the
/// inline week variants, whose styling flips as soon as a 5h window appears.
const ROWS_THAT_MUST_NOT_LAG: [&str; 17] = [
    "quota_group",
    "quota_icon",
    "quota_provider",
    "quota_model",
    "quota_provider_model",
    "quota_topic",
    "quota_context",
    "quota_context_normal",
    "quota_context_warning",
    "quota_context_danger",
    "quota_cache",
    "quota_cache_ttl",
    "quota_cache_state",
    "quota_week_inline_normal",
    "quota_week_inline_warning",
    "quota_week_inline_danger",
    "quota_week_inline_unknown",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub kind: Option<String>,
    pub value: String,
}

impl AgentSession {
    /// Existing Herdr integrations historically omitted `kind`; keep treating
    /// those values as opaque ids. A path is never exposed through this seam.
    pub fn id(&self) -> Option<&str> {
        self.kind
            .as_deref()
            .is_none_or(|kind| kind == "id")
            .then_some(self.value.as_str())
    }

    pub fn path(&self) -> Option<&str> {
        self.kind
            .as_deref()
            .is_some_and(|kind| kind == "path")
            .then_some(self.value.as_str())
    }
}

/// Herdr's effective pane status from `agent_status`.
///
/// The CLI list maps idle+unseen to `done`, but same-tab completions are
/// often already `idle` on the server while the TUI ring is still teal.
/// Brand-icon colour therefore uses this enum after `refresh` has applied
/// the plugin's own unseen set — never a second `state_icon` ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentStatus {
    #[default]
    Idle,
    Working,
    Done,
    Blocked,
    Unknown,
}

impl AgentStatus {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "working" => Self::Working,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "unknown" => Self::Unknown,
            _ => Self::Idle,
        }
    }

    pub fn is_working(self) -> bool {
        matches!(self, Self::Working)
    }

    /// Metadata token that carries the brand glyph for this status.
    fn icon_token(self) -> &'static str {
        match self {
            Self::Working => "quota_icon_working",
            Self::Done => "quota_icon_done",
            Self::Idle | Self::Blocked | Self::Unknown => "quota_icon",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub harness: Harness,
    pub session: Option<AgentSession>,
    pub session_summary: String,
    pub topic: String,
    pub tokens: BTreeMap<String, String>,
    /// From Herdr `agent_status`. Drives brand-icon colour and the watch pulse.
    pub status: AgentStatus,
    /// Herdr `focused` describes the current pane, not whether a completion
    /// was acknowledged by a later focus event.
    pub focused: bool,
}

impl AgentPane {
    pub fn working(&self) -> bool {
        self.status.is_working()
    }

    /// Status the brand icon should mirror.
    ///
    /// Working is yellow, unseen completion is teal, acknowledged is white.
    /// Callers fold the plugin unseen-set into `status` before publish. Do
    /// not treat a leftover `$quota_icon_done` token as unseen: a concurrent
    /// inventory read after mark-seen still carries that token and would
    /// paint teal back on.
    pub fn icon_status(&self) -> AgentStatus {
        if self.status.is_working() {
            return AgentStatus::Working;
        }
        if self.status == AgentStatus::Done {
            return AgentStatus::Done;
        }
        self.status
    }

    pub fn icon_needs_update(&self) -> bool {
        let active = self.icon_status().icon_token();
        ICON_TOKEN_NAMES
            .into_iter()
            .any(|name| self.tokens.contains_key(name) != (name == active))
    }
}

#[derive(Debug, Clone, Default)]
pub struct AgentState {
    pub panes: Vec<AgentPane>,
    pub working_providers: Vec<Provider>,
    pub working_pane_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PaneIdentity {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub enum PaneQuotaUpdate {
    Replace(Box<MetadataTokens>),
    Clear,
    Preserve,
}

#[derive(Debug, Clone)]
pub struct PaneTokens {
    pub pane_id: String,
    pub quota: PaneQuotaUpdate,
    pub identity: Option<PaneIdentity>,
    pub context: Option<ContextUsage>,
}

/// Show one Herdr notification.
///
/// Failure is reported to the caller but is never worth aborting a publish
/// for: a missed toast costs the user nothing that the sidebar does not
/// already show.
pub fn notify(title: &str, body: &str) -> Result<()> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(&executable)
        .args(["notification", "show", title, "--body", body])
        .args(["--sound", "request"])
        .output()
        .context("show Herdr notification")?;
    if !output.status.success() {
        anyhow::bail!("Herdr notification failed with {}", output.status);
    }
    Ok(())
}

/// Source that owns this plugin's Herdr Agent view. Herdr requires the
/// `plugin:<id>` form and rejects a set whose plugin is missing or disabled.
const AGENT_VIEW_SOURCE: &str = "plugin:herdr-agent-quota";
/// A socket request must not outlive the event hook that sent it. Herdr
/// answers these in microseconds; anything near this is a hung server, and a
/// sidebar sort is never worth blocking a turn for.
const SOCKET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Ask Herdr to order its Agent panel by space, then least quota left.
///
/// Herdr keeps one Agent view and this replaces it. The default agent order
/// is `quota`, so configure and startup both call this unless the user
/// chose `default`. The view does not survive a server restart, which is
/// why the startup hook re-applies it.
///
/// `workspace_order` keeps each Space contiguous — the same grouping Herdr's
/// own spaces sort uses — so quota ranking never scatters one project's
/// agents across the panel. Inside a space, `quota_headroom` ranks tightest
/// first.
pub fn set_quota_agent_view() -> Result<()> {
    socket_request(&serde_json::json!({
        "id": "agent-quota:view-set",
        "method": "agent.view.set",
        "params": {
            "source": AGENT_VIEW_SOURCE,
            "label": crate::cli::AgentOrder::LABEL,
            "sort": [
                {"field": "workspace_order", "order": "asc"},
                {"field": {"token": HEADROOM_TOKEN}, "order": "asc"},
            ],
        },
    }))
    .map(|_| ())
}

/// Give the Agent panel back to Herdr's own ordering.
///
/// Scoped to this plugin's source: a view someone else owns must survive.
pub fn clear_quota_agent_view() -> Result<()> {
    socket_request(&serde_json::json!({
        "id": "agent-quota:view-clear",
        "method": "agent.view.clear",
        "params": {"source": AGENT_VIEW_SOURCE},
    }))
    .map(|_| ())
}

/// One request, one reply, one connection.
///
/// `agent.view.*` has no CLI subcommand in Herdr 0.8, so this is the only
/// place the plugin speaks the raw socket protocol. Nothing here subscribes,
/// so no stream is ever held open — the replay and focus-storm problems that
/// come with `events.subscribe` do not apply.
///
/// Outside Herdr there is no socket and this is a no-op, exactly like
/// [`crate::prefs::write`], so a direct CLI run still works.
fn socket_request(payload: &Value) -> Result<Option<Value>> {
    use std::io::{BufRead, BufReader, Write};

    let Some(path) = std::env::var_os("HERDR_SOCKET_PATH") else {
        return Ok(None);
    };
    let stream = std::os::unix::net::UnixStream::connect(&path)
        .with_context(|| format!("connect to Herdr at {}", path.to_string_lossy()))?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    let mut writer = &stream;
    writeln!(writer, "{payload}").context("send Herdr socket request")?;
    writer.flush().context("flush Herdr socket request")?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .context("read Herdr socket reply")?;
    let reply: Value = serde_json::from_str(&line).context("parse Herdr socket reply")?;
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Herdr rejected the request");
        anyhow::bail!("{message}");
    }
    Ok(Some(reply))
}

pub fn list_agent_panes() -> Result<Vec<AgentPane>> {
    Ok(list_agent_state()?.panes)
}

/// Read Herdr's agent inventory once and derive both panes and working
/// providers from that same response. The active-turn watcher uses this
/// combined view so one poll does not fan out into one `agent list` call per
/// provider.
pub fn list_agent_state() -> Result<AgentState> {
    let value = list_agent_value()?;
    let mut panes = Vec::new();
    collect_agent_panes(&value, &mut panes);
    // Keep Herdr's order: it is the Agent panel's draw order, and group head
    // election depends on it. A `pane_id` sort would put `w1:p10` before
    // `w1:p7`.
    let mut seen = BTreeSet::new();
    panes.retain(|pane| seen.insert(pane.pane_id.clone()));
    attach_muse_sessions(&mut panes);
    let mut working_pane_ids = Vec::new();
    collect_working_providers(&value, &mut Vec::new(), &mut working_pane_ids);
    working_pane_ids.sort();
    working_pane_ids.dedup();
    Ok(AgentState {
        panes,
        working_providers: working_providers_from(&value),
        working_pane_ids,
    })
}

/// One pane from a single inventory read, for the event and focus paths.
/// Only that pane's Muse session is resolved, so an event on another agent
/// never walks Muse's process and session state.
pub fn find_agent_pane(pane_id: &str) -> Result<Option<AgentPane>> {
    let value = list_agent_value()?;
    let mut panes = Vec::new();
    collect_agent_panes(&value, &mut panes);
    let Some(pane) = panes.into_iter().find(|pane| pane.pane_id == pane_id) else {
        return Ok(None);
    };
    let mut panes = [pane];
    attach_muse_sessions(&mut panes);
    let [pane] = panes;
    Ok(Some(pane))
}

/// Return only the named agents from one inventory read. A focus change must
/// never acknowledge an unrelated green pane in the same tab.
pub fn find_agent_icon_panes(pane_ids: &[&str]) -> Result<Vec<AgentPane>> {
    let value = list_agent_value()?;
    let mut panes = Vec::new();
    collect_agent_panes(&value, &mut panes);
    panes.retain(|pane| pane_ids.contains(&pane.pane_id.as_str()));
    panes.sort_by(|left, right| left.pane_id.cmp(&right.pane_id));
    panes.dedup_by(|left, right| left.pane_id == right.pane_id);
    Ok(panes)
}

/// Herdr has no Muse session integration, so a Muse pane arrives without a
/// session. Resolve it from Muse's own session lock; a session Herdr does
/// report is always kept as-is.
fn attach_muse_sessions(panes: &mut [AgentPane]) {
    attach_muse_sessions_with(panes, crate::providers::muse::session_ids_for_panes);
}

fn attach_muse_sessions_with(
    panes: &mut [AgentPane],
    resolve: impl FnOnce(&[String]) -> BTreeMap<String, String>,
) {
    let missing = panes
        .iter()
        .filter(|pane| pane.harness == Harness::Muse && pane.session.is_none())
        .map(|pane| pane.pane_id.clone())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return;
    }
    let resolved = resolve(&missing);
    for pane in panes {
        if pane.harness != Harness::Muse || pane.session.is_some() {
            continue;
        }
        if let Some(session_id) = resolved.get(&pane.pane_id) {
            pane.session = Some(AgentSession {
                kind: Some("id".to_string()),
                value: session_id.clone(),
            });
        }
    }
}

fn list_agent_value() -> Result<Value> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(&executable)
        .args(["agent", "list"])
        .output()
        .context("list Herdr agents")?;
    if !output.status.success() {
        anyhow::bail!("Herdr agent list failed with {}", output.status);
    }
    serde_json::from_slice(&output.stdout).context("parse Herdr agent list")
}

/// Pane id and harness of the focused pane.
///
/// `pane.focused` carries no agent in its payload, so this is the only way to
/// learn which pane the user moved to. Herdr answers with the pane id, which
/// is what keeps `focus` scoped to exactly one pane.
pub fn current_focused_pane() -> Result<Option<(String, Harness)>> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(executable)
        .args(["pane", "current"])
        .output()
        .context("read focused Herdr pane")?;
    if !output.status.success() {
        anyhow::bail!("Herdr pane current failed with {}", output.status);
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).context("parse focused Herdr pane")?;
    let pane = value.pointer("/result/pane").unwrap_or(&value);
    let Some(pane_id) = pane.get("pane_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    Ok(pane
        .get("agent")
        .and_then(Value::as_str)
        .and_then(Harness::from_agent_name)
        .map(|harness| (pane_id.to_string(), harness)))
}

/// Resolve a workspace/tab focus event to that tab's focused pane. Ignore a
/// delayed event once the session has focused somewhere else.
pub fn focused_pane_in_snapshot(
    workspace_id: Option<&str>,
    tab_id: Option<&str>,
) -> Result<Option<String>> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(executable)
        .args(["api", "snapshot"])
        .output()
        .context("read Herdr focus snapshot")?;
    if !output.status.success() {
        anyhow::bail!("Herdr api snapshot failed with {}", output.status);
    }
    let value: Value =
        serde_json::from_slice(&output.stdout).context("parse Herdr focus snapshot")?;
    let snapshot = value.pointer("/result/snapshot").unwrap_or(&value);
    if workspace_id
        .is_some_and(|id| snapshot.get("focused_workspace_id").and_then(Value::as_str) != Some(id))
        || tab_id
            .is_some_and(|id| snapshot.get("focused_tab_id").and_then(Value::as_str) != Some(id))
    {
        return Ok(None);
    }
    if workspace_id.is_none() && tab_id.is_none() {
        return Ok(snapshot
            .get("focused_pane_id")
            .and_then(Value::as_str)
            .map(str::to_owned));
    }
    let tab_id = tab_id.or_else(|| {
        snapshot
            .get("workspaces")?
            .as_array()?
            .iter()
            .find(|workspace| {
                workspace.get("workspace_id").and_then(Value::as_str) == workspace_id
            })?
            .get("active_tab_id")?
            .as_str()
    });
    let Some(tab_id) = tab_id else {
        return Ok(None);
    };
    Ok(snapshot
        .get("layouts")
        .and_then(Value::as_array)
        .and_then(|layouts| {
            layouts
                .iter()
                .find(|layout| layout.get("tab_id").and_then(Value::as_str) == Some(tab_id))
        })
        .and_then(|layout| layout.get("focused_pane_id"))
        .and_then(Value::as_str)
        .map(str::to_owned))
}

// Reading a pane makes Herdr repaint it, which visibly scrolls the agent's
// terminal. Only the pane that fired the event is worth that cost; every other
// pane keeps the topic it last published.
pub fn refresh_pane_topic(pane: &mut AgentPane) {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    if let Some(topic) = read_pane_topic(&executable, pane) {
        pane.topic = topic;
    }
}

fn collect_agent_panes(value: &Value, panes: &mut Vec<AgentPane>) {
    match value {
        Value::Object(map) => {
            let pane_id = map
                .get("pane_id")
                .or_else(|| map.get("paneId"))
                .and_then(Value::as_str);
            let kind = map
                .get("agent")
                .and_then(Value::as_str)
                .or_else(|| map.get("kind").and_then(Value::as_str))
                .or_else(|| {
                    map.get("agent_session")
                        .and_then(Value::as_object)
                        .and_then(|session| session.get("agent"))
                        .and_then(Value::as_str)
                });
            if let (Some(pane_id), Some(kind)) = (pane_id, kind) {
                if let Some(harness) = Harness::from_agent_name(kind) {
                    let tokens: BTreeMap<String, String> = map
                        .get("tokens")
                        .and_then(Value::as_object)
                        .into_iter()
                        .flat_map(|tokens| tokens.iter())
                        .filter_map(|(name, value)| {
                            value
                                .as_str()
                                .map(|value| (name.clone(), value.to_string()))
                        })
                        .collect();
                    let topic = tokens.get("quota_topic").cloned().unwrap_or_default();
                    let session_summary = tokens.get("quota_session").cloned().unwrap_or_default();
                    let session =
                        map.get("agent_session")
                            .and_then(Value::as_object)
                            .and_then(|session| {
                                session.get("value").and_then(Value::as_str).map(|value| {
                                    AgentSession {
                                        kind: session
                                            .get("kind")
                                            .and_then(Value::as_str)
                                            .map(str::to_string),
                                        value: value.to_string(),
                                    }
                                })
                            });
                    let workspace_id = map
                        .get("workspace_id")
                        .or_else(|| map.get("workspaceId"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| workspace_id_from_pane_id(pane_id))
                        .unwrap_or_default();
                    let status = map
                        .get("agent_status")
                        .or_else(|| map.get("agentStatus"))
                        .or_else(|| map.get("status"))
                        .or_else(|| map.get("state"))
                        .and_then(Value::as_str)
                        .map(AgentStatus::parse)
                        .unwrap_or_default();
                    let focused = map.get("focused").and_then(Value::as_bool).unwrap_or(false);
                    panes.push(AgentPane {
                        pane_id: pane_id.to_string(),
                        workspace_id,
                        harness,
                        session,
                        session_summary,
                        // Preserve the last published topic during quota-only
                        // refreshes. Agent events refresh it from pane output.
                        topic,
                        tokens,
                        status,
                        focused,
                    });
                }
            }
            for child in map.values() {
                collect_agent_panes(child, panes);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_agent_panes(child, panes);
            }
        }
        _ => {}
    }
}

fn working_providers_from(value: &Value) -> Vec<Provider> {
    let mut providers = Vec::new();
    collect_working_providers(value, &mut providers, &mut Vec::new());
    providers.sort_by_key(|provider| {
        Provider::ALL
            .iter()
            .position(|candidate| candidate == provider)
    });
    providers.dedup();
    providers
}

fn collect_working_providers(
    value: &Value,
    providers: &mut Vec<Provider>,
    pane_ids: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => {
            let kind = map
                .get("agent")
                .and_then(Value::as_str)
                .or_else(|| map.get("kind").and_then(Value::as_str))
                .or_else(|| {
                    map.get("agent_session")
                        .and_then(Value::as_object)
                        .and_then(|session| session.get("agent"))
                        .and_then(Value::as_str)
                });
            let status = map
                .get("agent_status")
                .or_else(|| map.get("agentStatus"))
                .or_else(|| map.get("status"))
                .or_else(|| map.get("state"))
                .and_then(Value::as_str);
            if let (Some(kind), Some(status)) = (kind, status) {
                if status.eq_ignore_ascii_case("working") {
                    if Harness::from_agent_name(kind).is_some() {
                        if let Some(pane_id) = map
                            .get("pane_id")
                            .or_else(|| map.get("paneId"))
                            .and_then(Value::as_str)
                        {
                            pane_ids.push(pane_id.to_string());
                        }
                    }
                    if let Some(provider) = Harness::billing_for_agent(kind) {
                        providers.push(provider);
                    }
                }
            }
            for child in map.values() {
                collect_working_providers(child, providers, pane_ids);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_working_providers(child, providers, pane_ids);
            }
        }
        _ => {}
    }
}

pub fn publish_pane_tokens(
    panes: &[AgentPane],
    tokens: &[PaneTokens],
    sequence: u64,
    row: RowStyle,
) -> Result<()> {
    publish_pane_tokens_inner(panes, tokens, sequence, row, false)
}

/// Watcher refreshes may need to clear a stale icon while its pane is scrolled.
/// The inner publisher still sends only the icon twins in that case; quota and
/// topic writes remain deferred until the pane is visible.
pub fn publish_pane_tokens_with_scrolled_icons(
    panes: &[AgentPane],
    tokens: &[PaneTokens],
    sequence: u64,
    row: RowStyle,
) -> Result<()> {
    publish_pane_tokens_inner(panes, tokens, sequence, row, true)
}

/// Sidebar icon colour must update on focus even if the terminal is scrolled —
/// the scroll guard exists to protect reading scrollback during quota refreshes,
/// not to leave a stale teal glyph after mark-seen.
pub fn publish_status_icons(
    panes: &[AgentPane],
    tokens: &[PaneTokens],
    sequence: u64,
    row: RowStyle,
) -> Result<()> {
    publish_pane_tokens_inner(panes, tokens, sequence, row, true)
}

/// Focus and watcher reconciliation change only the three icon colour tokens.
/// They must not turn an icon acknowledgement into a quota or group refresh.
pub fn publish_icon_tokens(panes: &[AgentPane], sequence: u64) -> Result<()> {
    if panes.is_empty() {
        return Ok(());
    }
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    // `report_icon_metadata` writes only `ICON_TOKEN_NAMES`, so head election
    // here would be dead work (one `agent list` per focus event). Pass no
    // heads; the `quota_group` it removes from `desired` is never reported.
    let group_heads = BTreeMap::new();
    let mut reported = 0;
    let mut failed = Vec::new();
    for pane in panes {
        let mut desired = pane.tokens.clone();
        apply_group_and_icon(&mut desired, pane, &group_heads, &BTreeMap::new());
        if icon_tokens_match(&pane.tokens, &desired) {
            continue;
        }
        reported += 1;
        if !report_icon_metadata(&executable, pane, &desired, sequence)? {
            failed.push(pane.pane_id.clone());
        }
    }
    if reported > 0 && failed.len() == reported {
        anyhow::bail!(
            "Herdr icon report failed for every pane: {}",
            failed.join(", ")
        );
    }
    Ok(())
}

fn publish_pane_tokens_inner(
    panes: &[AgentPane],
    tokens: &[PaneTokens],
    sequence: u64,
    row: RowStyle,
    allow_while_scrolled: bool,
) -> Result<()> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let workspace_labels = list_workspace_labels().unwrap_or_default();
    // Event/focus publish one pane. Head selection and sibling clears need the
    // full Space membership — otherwise a lone pane preserves a stale
    // `$quota_group` and the Space name prints twice (radar writes `group:null`
    // on every non-head every frame).
    let inventory = match list_agent_panes() {
        Ok(all) if !all.is_empty() => all,
        _ => panes.to_vec(),
    };
    let group_heads =
        group_head_pane_ids(&inventory, panes, tokens, group_head_ranks_by_headroom());
    let mut reported = 0usize;
    let mut failed = Vec::new();
    for pane in panes {
        let Some(pane_tokens) = tokens.iter().find(|tokens| tokens.pane_id == pane.pane_id) else {
            continue;
        };
        let topic = display_topic(pane);
        let mut desired = match &pane_tokens.quota {
            PaneQuotaUpdate::Replace(values) => desired_tokens(values, &topic, row.shape),
            PaneQuotaUpdate::Clear => desired_cleared_quota(pane),
            PaneQuotaUpdate::Preserve => pane.tokens.clone(),
        };
        if let Some(identity) = &pane_tokens.identity {
            apply_identity(&mut desired, identity, row.shape.content_width);
        }
        if let Some(context) = &pane_tokens.context {
            apply_context(&mut desired, context, sequence / 1_000, row);
        }
        fold_cache_row(&mut desired, row);
        apply_group_and_icon(&mut desired, pane, &group_heads, &workspace_labels);
        if metadata_matches(&pane.tokens, &desired) {
            continue;
        }
        // Herdr versions that repaint metadata can snap a terminal viewport
        // back to the bottom. Never mutate pane metadata while the user is
        // reading scrollback; the next refresh after they return catches up.
        // Icon-only sync may proceed while scrolled — see publish_status_icons.
        if pane_is_scrolled(&executable, &pane.pane_id) {
            if !allow_while_scrolled || icon_tokens_match(&pane.tokens, &desired) {
                continue;
            }
            reported += 1;
            if !report_icon_metadata(&executable, pane, &desired, sequence)? {
                failed.push(pane.pane_id.clone());
            }
            continue;
        }
        reported += 1;
        if !report_pane_metadata(&executable, pane, &desired, sequence)? {
            failed.push(pane.pane_id.clone());
        }
    }
    reported += sync_sibling_group_headers(
        &executable,
        &inventory,
        panes,
        &group_heads,
        &workspace_labels,
        sequence,
        &mut failed,
    )?;
    // A pane can exit between `agent list` and this report, and the exit event
    // itself triggers a publish. One stale pane id must not stop the panes
    // that are still alive from being updated.
    if reported > 0 && failed.len() == reported {
        anyhow::bail!(
            "Herdr metadata report failed for every pane: {}",
            failed.join(", ")
        );
    }
    Ok(())
}

fn icon_tokens_match(
    current: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> bool {
    ICON_TOKEN_NAMES
        .into_iter()
        .all(|name| current.get(name) == desired.get(name))
}

fn report_icon_metadata(
    executable: &std::ffi::OsStr,
    pane: &AgentPane,
    desired: &BTreeMap<String, String>,
    sequence: u64,
) -> Result<bool> {
    let mut command = Command::new(executable);
    command
        .args([
            "pane",
            "report-metadata",
            &pane.pane_id,
            "--source",
            "herdr-agent-quota",
        ])
        .args(["--seq", &sequence.to_string(), "--ttl-ms", METADATA_TTL_MS]);
    for name in ICON_TOKEN_NAMES {
        if let Some(value) = desired.get(name) {
            command.args(["--token", &format!("{name}={value}")]);
        } else {
            command.args(["--clear-token", name]);
        }
    }
    let output = command.output().context("report icon metadata to Herdr")?;
    Ok(output.status.success())
}

fn report_pane_metadata(
    executable: &std::ffi::OsStr,
    pane: &AgentPane,
    desired: &BTreeMap<String, String>,
    sequence: u64,
) -> Result<bool> {
    let mut command = Command::new(executable);
    command
        .args([
            "pane",
            "report-metadata",
            &pane.pane_id,
            "--source",
            "herdr-agent-quota",
        ])
        .args(["--seq", &sequence.to_string()])
        .args(["--ttl-ms", METADATA_TTL_MS]);
    for name in metadata_report_names(pane, desired) {
        if let Some(value) = desired.get(name) {
            command.args(["--token", &format!("{name}={value}")]);
        } else {
            command.args(["--clear-token", name]);
        }
    }
    let output = command.output().context("report quota metadata to Herdr")?;
    Ok(output.status.success())
}

/// Clear or set `$quota_group` on siblings in the same Space that this pass
/// did not otherwise touch. Without this, a one-pane event leaves the old
/// head's header in place after headroom moves the title to another pane.
fn sync_sibling_group_headers(
    executable: &std::ffi::OsStr,
    inventory: &[AgentPane],
    published: &[AgentPane],
    group_heads: &BTreeMap<String, String>,
    workspace_labels: &BTreeMap<String, String>,
    sequence: u64,
    failed: &mut Vec<String>,
) -> Result<usize> {
    let published_ids = published
        .iter()
        .map(|pane| pane.pane_id.as_str())
        .collect::<BTreeSet<_>>();
    let touched = published
        .iter()
        .map(|pane| pane.workspace_id.as_str())
        .filter(|workspace| !workspace.is_empty())
        .collect::<BTreeSet<_>>();
    let mut reported = 0usize;
    for sibling in inventory {
        if published_ids.contains(sibling.pane_id.as_str()) {
            continue;
        }
        if !touched.contains(sibling.workspace_id.as_str()) {
            continue;
        }
        let want = group_label_for(sibling, group_heads, workspace_labels);
        let have = sibling
            .tokens
            .get("quota_group")
            .filter(|value| !value.is_empty())
            .cloned();
        if want == have {
            continue;
        }
        if pane_is_scrolled(executable, &sibling.pane_id) {
            continue;
        }
        reported += 1;
        let mut command = Command::new(executable);
        command
            .args([
                "pane",
                "report-metadata",
                &sibling.pane_id,
                "--source",
                "herdr-agent-quota",
            ])
            .args(["--seq", &sequence.to_string()])
            .args(["--ttl-ms", METADATA_TTL_MS]);
        if let Some(label) = &want {
            command.args(["--token", &format!("quota_group={label}")]);
        } else {
            command.args(["--clear-token", "quota_group"]);
        }
        let output = command.output().context("report group header to Herdr")?;
        if !output.status.success() {
            failed.push(sibling.pane_id.clone());
        }
    }
    Ok(reported)
}

fn pane_is_scrolled(executable: &std::ffi::OsStr, pane_id: &str) -> bool {
    let Ok(output) = Command::new(executable)
        .args(["pane", "get", pane_id])
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    serde_json::from_slice::<Value>(&output.stdout)
        .ok()
        .and_then(|value| {
            value
                .pointer("/result/pane/scroll/offset_from_bottom")
                .and_then(Value::as_u64)
        })
        .is_some_and(|offset| offset > 0)
}

/// `w1:p9` → `w1`. Used when an older inventory omits `workspace_id`.
fn workspace_id_from_pane_id(pane_id: &str) -> Option<String> {
    let (workspace, _) = pane_id.split_once(':')?;
    (!workspace.is_empty()).then(|| workspace.to_string())
}

/// Workspace id → label from one `herdr workspace list` call.
fn list_workspace_labels() -> Result<BTreeMap<String, String>> {
    let executable = std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| "herdr".into());
    let output = Command::new(&executable)
        .args(["workspace", "list"])
        .output()
        .context("list Herdr workspaces")?;
    if !output.status.success() {
        anyhow::bail!("Herdr workspace list failed with {}", output.status);
    }
    let value: Value = serde_json::from_slice(&output.stdout).context("parse workspace list")?;
    let mut labels = BTreeMap::new();
    collect_workspace_labels(&value, &mut labels);
    Ok(labels)
}

fn collect_workspace_labels(value: &Value, labels: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            let id = map
                .get("workspace_id")
                .or_else(|| map.get("workspaceId"))
                .and_then(Value::as_str);
            let label = map.get("label").and_then(Value::as_str);
            if let (Some(id), Some(label)) = (id, label) {
                labels.insert(id.to_string(), label.to_string());
            }
            for child in map.values() {
                collect_workspace_labels(child, labels);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_workspace_labels(child, labels);
            }
        }
        _ => {}
    }
}

/// Which pane carries the group header for each workspace.
///
/// The header must sit on whichever pane Herdr draws first in the Space.
/// `inventory` is Herdr's `agent list`, which walks workspaces → tabs → layout
/// in the same order the Agent panel draws under Herdr's default `grouped`
/// sort, so with `rank_by_headroom` off the head is simply the first pane
/// listed. A client toggled to `priority` draws another order the plugin
/// cannot observe (no API exposes `agent_panel_sort`), and panes whose agent
/// maps to no known harness are drawn by Herdr but absent here; both put the
/// header one row down and are accepted.
/// Under the plugin's quota view Herdr sorts each Space by headroom (stable,
/// so ties keep layout order), and the head is the tightest pane with the
/// same tie-break. `publishing` / `tokens` overlay headroom for panes this
/// pass is about to write so a forced refresh that moves the title does not
/// leave the old head labelled.
fn group_head_pane_ids(
    inventory: &[AgentPane],
    publishing: &[AgentPane],
    tokens: &[PaneTokens],
    rank_by_headroom: bool,
) -> BTreeMap<String, String> {
    let publishing_ids = publishing
        .iter()
        .map(|pane| pane.pane_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut heads: BTreeMap<String, (u8, usize, &str)> = BTreeMap::new();
    for (index, pane) in inventory.iter().enumerate() {
        if pane.workspace_id.is_empty() {
            continue;
        }
        let headroom = if !rank_by_headroom {
            0
        } else if publishing_ids.contains(pane.pane_id.as_str()) {
            published_headroom(pane, tokens)
        } else {
            pane.tokens
                .get(HEADROOM_TOKEN)
                .and_then(|value| value.parse().ok())
                .unwrap_or(u8::MAX)
        };
        let candidate = (headroom, index, pane.pane_id.as_str());
        match heads.get(&pane.workspace_id) {
            Some(current) if *current <= candidate => {}
            _ => {
                heads.insert(pane.workspace_id.clone(), candidate);
            }
        }
    }
    // A pane present only in this pass (inventory read failed) still needs a
    // head entry so its Space is not left without a label.
    for (index, pane) in publishing.iter().enumerate() {
        if pane.workspace_id.is_empty() || heads.contains_key(&pane.workspace_id) {
            continue;
        }
        let headroom = if rank_by_headroom {
            published_headroom(pane, tokens)
        } else {
            0
        };
        heads.insert(
            pane.workspace_id.clone(),
            (headroom, inventory.len() + index, pane.pane_id.as_str()),
        );
    }
    heads
        .into_iter()
        .map(|(workspace, (_, _, pane_id))| (workspace, pane_id.to_string()))
        .collect()
}

/// Whether Herdr's Agent panel is under this plugin's headroom-ranked view.
///
/// Off (`agent-order default`) Herdr draws layout order, so the group header
/// must follow that order, not the tightest pane. This reads the plugin's own
/// preference, not the server: the two diverge after `configure` runs outside
/// Herdr (no socket, view left in place) or until the startup hook re-applies
/// the view after a server restart. The cost is a header one row off.
fn group_head_ranks_by_headroom() -> bool {
    let cache = crate::cache::CacheStore::from_env().ok();
    crate::configure::resolved_agent_order(None, cache.as_ref()).is_quota()
}

fn published_headroom(pane: &AgentPane, tokens: &[PaneTokens]) -> u8 {
    tokens
        .iter()
        .find(|tokens| tokens.pane_id == pane.pane_id)
        .and_then(|tokens| match &tokens.quota {
            PaneQuotaUpdate::Replace(values) => values.quota_headroom,
            _ => None,
        })
        .or_else(|| {
            pane.tokens
                .get(HEADROOM_TOKEN)
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(u8::MAX)
}

fn group_label_for(
    pane: &AgentPane,
    group_heads: &BTreeMap<String, String>,
    workspace_labels: &BTreeMap<String, String>,
) -> Option<String> {
    if pane.workspace_id.is_empty() {
        return None;
    }
    if group_heads
        .get(&pane.workspace_id)
        .is_none_or(|head| head != &pane.pane_id)
    {
        return None;
    }
    workspace_labels
        .get(&pane.workspace_id)
        .cloned()
        .filter(|label| !label.is_empty())
        .or_else(|| {
            let id = pane.workspace_id.clone();
            (!id.is_empty()).then_some(id)
        })
}

/// Always reported together so a lagging inventory cannot leave a stale
/// colour twin on screen after working→done or done→idle.
const ICON_TOKEN_NAMES: [&str; 3] = ["quota_icon", "quota_icon_working", "quota_icon_done"];

/// Vendor mark always; group header only on the Space head pane.
///
/// Exactly one of `$quota_icon` / `_working` / `_done` is published so the
/// brand glyph itself carries Herdr's status colour (no `state_icon` ring).
/// Every pane publishes the bare glyph. Stale `$quota_pad` and the member
/// indent older builds baked into the logo are cleared.
///
/// No member indent: Herdr indents by row *index* (row 0 by 1 column, every
/// later row by 3), not by group membership. A member collapses its empty
/// `$quota_group`, which pulls its whole row list up by one — so the logo row
/// is a continuation row on heads and members alike and already shares the
/// 3-column offset. Padding it moved members' logos 2 columns right of
/// everyone else's, and only on the logo row, whenever a user row sat between
/// the group header and the logo (`build_managed_rows` preserves those).
fn apply_group_and_icon(
    desired: &mut BTreeMap<String, String>,
    pane: &AgentPane,
    group_heads: &BTreeMap<String, String>,
    workspace_labels: &BTreeMap<String, String>,
) {
    let mark = crate::icons::for_harness(pane.harness);
    let active = pane.icon_status().icon_token();
    for token in ICON_TOKEN_NAMES {
        if token == active {
            desired.insert(token.to_string(), mark.to_string());
        } else {
            desired.remove(token);
        }
    }
    desired.remove("quota_pad");
    // Never preserve a previous header: non-heads must omit the token so the
    // report clears it. Blind preserve is what left `ifs` on two panes.
    if let Some(label) = group_label_for(pane, group_heads, workspace_labels) {
        desired.insert("quota_group".to_string(), label);
    } else {
        desired.remove("quota_group");
    }
}

fn desired_tokens(
    values: &MetadataTokens,
    topic: &str,
    shape: SidebarShape,
) -> BTreeMap<String, String> {
    let mut tokens = BTreeMap::new();
    insert_optional_token(&mut tokens, "quota_provider", &values.quota_provider);
    tokens.insert(
        "quota_provider_model".to_string(),
        values.quota_provider_model.clone(),
    );
    insert_optional_token(&mut tokens, "quota_model", &values.quota_model);
    insert_context_token(
        &mut tokens,
        &values.quota_context,
        values.quota_context_severity,
        shape,
    );
    insert_optional_token(&mut tokens, "quota_cache", &values.quota_cache);
    insert_optional_token(&mut tokens, "quota_cache_ttl", &values.quota_cache_ttl);
    insert_optional_token(&mut tokens, "quota_cache_state", &values.quota_cache_state);
    let week_base = week_style_base(&values.quota_5h);
    insert_severity_token(
        &mut tokens,
        "quota_5h",
        &values.quota_5h,
        values.quota_5h_severity,
    );
    insert_severity_token(
        &mut tokens,
        week_base,
        &values.quota_week,
        values.quota_week_severity,
    );
    insert_severity_token(
        &mut tokens,
        "quota_week_scoped",
        &values.quota_week_scoped,
        values.quota_week_scoped_severity,
    );
    insert_severity_token(
        &mut tokens,
        "quota_month",
        &values.quota_month,
        values.quota_month_severity,
    );
    insert_optional_token(&mut tokens, "quota_topic", topic);
    if let Some(error) = &values.quota_error {
        tokens.insert("quota_error".to_string(), error.clone());
    }
    if let Some(headroom) = values.quota_headroom {
        tokens.insert(HEADROOM_TOKEN.to_string(), format!("{headroom:03}"));
    }
    tokens
}

fn display_topic(pane: &AgentPane) -> String {
    let topic = pane.topic.trim();
    if topic.is_empty() || is_status_line(topic) {
        return truncate_topic(&pane.session_summary);
    }
    truncate_topic(topic)
}

pub(crate) fn plugin_quota_present(tokens: &BTreeMap<String, String>) -> bool {
    METADATA_TOKEN_NAMES
        .into_iter()
        .chain(OBSOLETE_METADATA_TOKEN_NAMES)
        .chain(LEGACY_METADATA_TOKEN_NAMES)
        .filter(|name| *name != "quota_topic")
        .any(|name| tokens.contains_key(name))
}

fn desired_cleared_quota(pane: &AgentPane) -> BTreeMap<String, String> {
    let mut tokens = BTreeMap::new();
    let topic = display_topic(pane);
    if !topic.is_empty() {
        tokens.insert("quota_topic".to_string(), topic);
    }
    tokens
}

fn apply_identity(
    tokens: &mut BTreeMap<String, String>,
    identity: &PaneIdentity,
    content_width: usize,
) {
    let narrow = content_width > 0 && content_width < 22;
    if identity.model.is_empty() {
        tokens.remove("quota_model");
        tokens.insert("quota_provider".to_string(), identity.provider.clone());
        tokens.insert(
            "quota_provider_model".to_string(),
            identity.provider.clone(),
        );
        return;
    }
    tokens.insert("quota_model".to_string(), identity.model.clone());
    if narrow {
        // Logo already says who; keep the model only and collapse a stacked
        // provider row so it does not re-introduce the prefix.
        tokens.remove("quota_provider");
        tokens.insert("quota_provider_model".to_string(), identity.model.clone());
    } else {
        tokens.insert("quota_provider".to_string(), identity.provider.clone());
        tokens.insert(
            "quota_provider_model".to_string(),
            format!("{}/{}", identity.provider, identity.model),
        );
    }
}

fn apply_context(
    tokens: &mut BTreeMap<String, String>,
    context: &ContextUsage,
    now_unix: u64,
    row: RowStyle,
) {
    insert_context_token(
        tokens,
        &crate::presentation::sidebar_context(Some(context), row.percent, row.shape),
        Some(crate::presentation::context_severity(context, row.percent)),
        row.shape,
    );
    let cache = crate::presentation::sidebar_cache(Some(context));
    if cache.is_empty() {
        tokens.remove("quota_cache");
    } else {
        tokens.insert("quota_cache".to_string(), cache);
    }
    for (name, value) in [
        (
            "quota_cache_ttl",
            crate::presentation::sidebar_cache_ttl(Some(context), now_unix),
        ),
        (
            "quota_cache_state",
            crate::presentation::sidebar_cache_state(Some(context), now_unix),
        ),
    ] {
        if value.is_empty() {
            tokens.remove(name);
        } else {
            tokens.insert(name.to_string(), value);
        }
    }
}

/// Keep the configured rows fixed. An empty TTL token collapses its row when
/// both visible fields fit inside the cache token; the next refresh can split
/// them again from the session evidence without rewriting Herdr's config.
///
/// `no cached` is not folded here: it keeps `$quota_cache_state` so the amber
/// warning colour survives. Gauges puts that token on the cache row.
fn fold_cache_row(tokens: &mut BTreeMap<String, String>, row: RowStyle) {
    use crate::cli::{SidebarField, SidebarLayout};
    if row.shape.layout != SidebarLayout::Gauges
        || !row.fields.contains(SidebarField::Cache)
        || !row.fields.contains(SidebarField::Ttl)
    {
        return;
    }
    let (Some(cache), Some(ttl)) = (tokens.get("quota_cache"), tokens.get("quota_cache_ttl"))
    else {
        return;
    };
    let joined = format!("{cache} · {ttl}");
    // These are plugin-generated numeric labels; · and ≈ each occupy one cell.
    if joined.chars().count() <= row.shape.content_width {
        tokens.insert("quota_cache".to_string(), joined);
        tokens.remove("quota_cache_ttl");
    }
}

/// True when the quota rows this pane is carrying are not the ones `values`
/// would render.
///
/// Only the window rows are comparable against a snapshot alone: publishing
/// rewrites provider, model, context and cache rows from pane-local evidence
/// this caller does not have, so including them would report drift that no
/// republish can settle.
///
/// A pane that has never been published to is not drift: waking those would
/// pull every quota-less pane into every pass. A pane that already carries
/// plugin tokens but no window row is drift once the snapshot has windows —
/// Claude/Agy statusLine only writes the mailbox, so an idle pane has no
/// other way to pick up a session that just reported quota.
pub(crate) fn quota_rows_have_drifted(
    current: &BTreeMap<String, String>,
    values: &MetadataTokens,
    shape: SidebarShape,
) -> bool {
    let desired = desired_tokens(values, "", shape);
    let current_has_window = QUOTA_WINDOW_TOKEN_NAMES
        .into_iter()
        .any(|name| current.contains_key(name));
    let desired_has_window = QUOTA_WINDOW_TOKEN_NAMES
        .into_iter()
        .any(|name| desired.contains_key(name));
    if !current_has_window {
        return desired_has_window && plugin_quota_present(current);
    }
    QUOTA_WINDOW_TOKEN_NAMES
        .into_iter()
        .any(|name| current.get(name) != desired.get(name))
}

fn metadata_matches(
    current: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> bool {
    METADATA_TOKEN_NAMES
        .into_iter()
        .all(|name| current.get(name) == desired.get(name))
        && OBSOLETE_METADATA_TOKEN_NAMES
            .into_iter()
            .all(|name| !current.contains_key(name))
        && LEGACY_METADATA_TOKEN_NAMES
            .into_iter()
            .all(|name| !current.contains_key(name))
}

fn metadata_report_names(
    pane: &AgentPane,
    desired: &BTreeMap<String, String>,
) -> Vec<&'static str> {
    let mut names = METADATA_TOKEN_NAMES
        .into_iter()
        .filter(|name| desired.contains_key(*name) || pane.tokens.contains_key(*name))
        .collect::<Vec<_>>();
    // Icon twins must always be named so an inactive colour is cleared even
    // when `agent list` omitted the stale token from `pane.tokens`.
    for name in ICON_TOKEN_NAMES {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let cleanup_names = OBSOLETE_METADATA_TOKEN_NAMES
        .into_iter()
        .filter(|name| pane.tokens.contains_key(*name))
        .chain(
            LEGACY_METADATA_TOKEN_NAMES
                .into_iter()
                .filter(|name| pane.tokens.contains_key(*name)),
        )
        .collect::<Vec<_>>();
    if names.len() + cleanup_names.len() <= MAX_METADATA_TOKENS {
        names.extend(cleanup_names);
        return names;
    }

    // Herdr accepts at most sixteen token arguments. Reserve room for stale
    // names first so an upgraded pane can actually clear them; an unchanged
    // value is re-sent on the next bounded report instead.
    let active_capacity = MAX_METADATA_TOKENS.saturating_sub(cleanup_names.len());
    while names.len() > active_capacity {
        let Some(index) = names.iter().position(|name| {
            // Dropping a name the pane still carries but no longer wants would
            // leave that row on screen forever, so those are never given up.
            // Icon twins are never dropped either — a stale colour is worse
            // than a briefly lagged quota digit.
            let must_clear = pane.tokens.contains_key(*name) && !desired.contains_key(*name);
            let is_icon = ICON_TOKEN_NAMES.contains(name);
            !must_clear && !is_icon && !ROWS_THAT_MUST_NOT_LAG.contains(name)
        }) else {
            break;
        };
        names.remove(index);
    }
    names.truncate(active_capacity);
    names.extend(cleanup_names);
    names
}

fn week_style_base(quota_5h: &str) -> &'static str {
    // Empty 5h publishes week beside context (`context · 7d`). A present 5h
    // keeps week on the limits row so 5h never shares a line with context.
    if quota_5h.trim().is_empty() {
        "quota_week_inline"
    } else {
        "quota_week"
    }
}

/// Publish the context row into the one name its layout and severity choose,
/// and clear the other three.
///
/// The clear is the point: a severity change or a layout switch moves the
/// value to a different name, and a pane that kept the old one would show two
/// context rows at once.
fn insert_context_token(
    tokens: &mut BTreeMap<String, String>,
    value: &str,
    severity: Option<crate::model::Severity>,
    shape: SidebarShape,
) {
    for name in CONTEXT_TOKEN_NAMES {
        tokens.remove(name);
    }
    if value.trim().is_empty() {
        return;
    }
    tokens.insert(
        context_token_name(shape, severity).to_string(),
        value.to_string(),
    );
}

fn context_token_name(
    shape: SidebarShape,
    severity: Option<crate::model::Severity>,
) -> &'static str {
    if shape.layout != crate::cli::SidebarLayout::Gauges {
        return "quota_context";
    }
    // `Severity::for_context_remaining` never returns `Unknown`, and a caller
    // with no severity has no coloured band to claim, so both read as normal.
    match severity {
        Some(crate::model::Severity::Warning) => "quota_context_warning",
        Some(crate::model::Severity::Danger) => "quota_context_danger",
        _ => "quota_context_normal",
    }
}

fn insert_severity_token(
    tokens: &mut BTreeMap<String, String>,
    base: &str,
    value: &str,
    severity: Option<crate::model::Severity>,
) {
    if value.trim().is_empty() {
        return;
    }
    let variant = severity_variant(severity);
    tokens.insert(format!("{base}_{variant}"), value.to_string());
}

fn severity_variant(severity: Option<crate::model::Severity>) -> &'static str {
    match severity.unwrap_or(crate::model::Severity::Unknown) {
        crate::model::Severity::Normal => "normal",
        crate::model::Severity::Warning => "warning",
        crate::model::Severity::Danger => "danger",
        crate::model::Severity::Unknown => "unknown",
    }
}

fn insert_optional_token(tokens: &mut BTreeMap<String, String>, name: &str, value: &str) {
    if !value.trim().is_empty() {
        tokens.insert(name.to_string(), value.to_string());
    }
}

// `recent` rebuilds the pane's wrapped scrollback, which takes seconds and
// repaints the pane: the agent's terminal visibly scrolls, once per read.
// `visible` is the current screen only, costs microseconds, and repaints
// nothing. The prompt is on screen at the moment idle->working fires, which is
// exactly when the topic changes; later in the turn it may have scrolled off,
// and then the caller keeps the topic it already published.
fn topic_read_args(pane_id: &str) -> [&str; 7] {
    [
        "pane", "read", pane_id, "--source", "visible", "--format", "text",
    ]
}

fn read_pane_topic(executable: &std::ffi::OsStr, pane: &AgentPane) -> Option<String> {
    let output = Command::new(executable)
        .args(topic_read_args(&pane.pane_id))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    extract_topic(&text, pane.harness)
}

fn extract_topic(text: &str, harness: Harness) -> Option<String> {
    text.lines().rev().find_map(|line| {
        let cleaned_line = strip_control_chars(line);
        let line = cleaned_line.trim();
        let candidate = prompt_candidate(line, harness)?;
        if candidate.is_empty() || is_status_line(candidate) {
            return None;
        }
        Some(truncate_topic(candidate))
    })
}

fn prompt_candidate(line: &str, harness: Harness) -> Option<&str> {
    let marker = match harness {
        Harness::Claude if line.starts_with('❯') => '❯',
        Harness::Codex if line.starts_with('›') => '›',
        Harness::Grok if line.starts_with('❯') => '❯',
        Harness::Grok | Harness::Agy if line.starts_with('>') => '>',
        _ => return None,
    };
    Some(line.trim_start_matches(marker).trim())
}

fn truncate_topic(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    if characters.len() <= 80 {
        return value.to_string();
    }
    let mut topic: String = characters.into_iter().take(77).collect();
    topic.push('…');
    topic
}

fn strip_control_chars(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .collect()
}

fn is_status_line(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.starts_with("accept-edits mode:")
        || lower.starts_with("context ")
        || lower.starts_with("session ")
        || lower.starts_with("auto mode")
        || lower.starts_with("shift+tab")
        || lower == "ask codex to do anything"
        || matches!(
            lower.as_str(),
            "/clear" | "/compact" | "/help" | "/status" | "/usage" | "/model" | "/config"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{FieldSet, PercentStyle, SidebarLayout};
    use crate::model::{
        CacheUsage, ContextUsage, ProviderSnapshot, ResetAt, UsageWindow, WindowKind,
    };
    use serde_json::json;

    #[test]
    fn muse_sessions_fill_only_session_less_muse_panes() {
        let pane = |id: &str, harness: Harness, session: Option<&str>| AgentPane {
            pane_id: id.to_string(),
            workspace_id: "w1".to_string(),
            harness,
            session: session.map(|value| AgentSession {
                kind: Some("id".to_string()),
                value: value.to_string(),
            }),
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::new(),
            status: AgentStatus::Idle,
            focused: false,
        };
        let mut panes = vec![
            pane("w1:p1", Harness::Muse, None),
            pane("w1:p2", Harness::Muse, Some("herdr-session")),
            pane("w1:p3", Harness::Claude, None),
        ];
        let mut asked = Vec::new();
        attach_muse_sessions_with(&mut panes, |pane_ids| {
            asked = pane_ids.to_vec();
            ["w1:p1", "w1:p2", "w1:p3"]
                .into_iter()
                .map(|id| (id.to_string(), format!("lock-{id}")))
                .collect()
        });
        assert_eq!(asked, vec!["w1:p1".to_string()]);
        assert_eq!(
            panes[0].session.as_ref().and_then(AgentSession::id),
            Some("lock-w1:p1")
        );
        assert_eq!(
            panes[1].session.as_ref().and_then(AgentSession::id),
            Some("herdr-session")
        );
        assert_eq!(panes[2].session, None);

        let mut without_muse = vec![pane("w1:p3", Harness::Claude, None)];
        attach_muse_sessions_with(&mut without_muse, |_| {
            panic!("a pane list without a session-less Muse pane never resolves")
        });
    }

    /// Herdr orders an Agent view by the token's own value, so the padding is
    /// the whole contract: `007` must sort before `042`, and `100` last.
    #[test]
    fn the_headroom_token_is_padded_so_its_text_order_is_its_numeric_order() {
        let token = |headroom: Option<u8>| {
            let mut values = MetadataTokens::unavailable(Provider::Claude, "test");
            values.quota_headroom = headroom;
            desired_tokens(&values, "", SidebarShape::default())
                .get(HEADROOM_TOKEN)
                .cloned()
        };
        assert_eq!(token(Some(7)).as_deref(), Some("007"));
        assert_eq!(token(Some(42)).as_deref(), Some("042"));
        assert_eq!(token(Some(100)).as_deref(), Some("100"));
        assert_eq!(token(None), None);

        let mut sorted = ["100", "007", "042", "000"];
        sorted.sort_unstable();
        assert_eq!(sorted, ["000", "007", "042", "100"]);
    }

    /// The comparison set and the report set are the same list, so a token
    /// that is published but not listed silently stops being compared and
    /// every refresh becomes a write.
    #[test]
    fn the_headroom_token_is_listed_among_the_names_that_are_compared() {
        assert!(METADATA_TOKEN_NAMES.contains(&HEADROOM_TOKEN));
        assert!(!OBSOLETE_METADATA_TOKEN_NAMES.contains(&HEADROOM_TOKEN));
    }

    /// A one-pane publish still knows the Space head from inventory, and the
    /// non-head drops `$quota_group` instead of preserving a stale label.
    #[test]
    fn group_header_lands_on_the_tightest_pane_and_clears_siblings() {
        let head = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Codex,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::from([
                (HEADROOM_TOKEN.to_string(), "000".to_string()),
                ("quota_group".to_string(), "ifs".to_string()),
            ]),
            status: AgentStatus::Idle,
            focused: false,
        };
        let sibling = AgentPane {
            pane_id: "w1:p2".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Grok,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::from([
                (HEADROOM_TOKEN.to_string(), "016".to_string()),
                ("quota_group".to_string(), "ifs".to_string()),
            ]),
            status: AgentStatus::Idle,
            focused: false,
        };
        let inventory = vec![head.clone(), sibling.clone()];
        let heads = group_head_pane_ids(&inventory, std::slice::from_ref(&sibling), &[], true);
        assert_eq!(heads.get("w1").map(String::as_str), Some("w1:p1"));

        let labels = BTreeMap::from([("w1".to_string(), "ifs".to_string())]);
        let mut head_desired = BTreeMap::new();
        apply_group_and_icon(&mut head_desired, &head, &heads, &labels);
        assert_eq!(
            head_desired.get("quota_group").map(String::as_str),
            Some("ifs")
        );

        let mut sibling_desired = sibling.tokens.clone();
        apply_group_and_icon(&mut sibling_desired, &sibling, &heads, &labels);
        assert!(!sibling_desired.contains_key("quota_group"));
        assert_eq!(
            sibling_desired.get("quota_icon").map(String::as_str),
            Some(crate::icons::for_harness(sibling.harness)),
            "member logo stays bare: Herdr indents by row index, so the logo \
             row is already hang-indented on heads and members alike"
        );
        assert!(
            !sibling_desired.contains_key("quota_pad"),
            "stand-alone pad draws a leading · separator"
        );
        assert_eq!(
            group_label_for(&sibling, &heads, &labels),
            None,
            "stale sibling header must clear"
        );

        // Under Herdr's own order the first listed pane is drawn first, so the
        // header must land there even when a later sibling is tighter.
        let inventory = vec![sibling.clone(), head.clone()];
        let heads = group_head_pane_ids(&inventory, &[], &[], false);
        assert_eq!(heads.get("w1").map(String::as_str), Some("w1:p2"));
        let heads = group_head_pane_ids(&inventory, &[], &[], true);
        assert_eq!(heads.get("w1").map(String::as_str), Some("w1:p1"));

        // Equal headroom keeps Herdr's stable order, not `pane_id` text order.
        let mut late = head.clone();
        late.pane_id = "w1:p10".to_string();
        late.tokens
            .insert(HEADROOM_TOKEN.to_string(), "016".to_string());
        let inventory = vec![sibling.clone(), late];
        let heads = group_head_pane_ids(&inventory, &[], &[], true);
        assert_eq!(heads.get("w1").map(String::as_str), Some("w1:p2"));
        assert_eq!(
            head_desired.get("quota_icon").map(String::as_str),
            Some(crate::icons::for_harness(head.harness)),
            "head logo stays bare; Herdr hang-indents the row"
        );

        // Brand icon colour follows agent_status: working publishes the
        // yellow twin and clears idle/done so only one glyph shows.
        let mut working = sibling.clone();
        working.status = AgentStatus::Working;
        let mut working_desired = BTreeMap::from([
            ("quota_icon".to_string(), "stale".to_string()),
            ("quota_icon_done".to_string(), "stale".to_string()),
        ]);
        apply_group_and_icon(&mut working_desired, &working, &heads, &labels);
        assert_eq!(
            working_desired
                .get("quota_icon_working")
                .map(String::as_str),
            Some(crate::icons::for_harness(working.harness)),
            "working panes publish the yellow brand icon"
        );
        assert!(!working_desired.contains_key("quota_icon"));
        assert!(!working_desired.contains_key("quota_icon_done"));

        let mut done = sibling.clone();
        done.status = AgentStatus::Done;
        let mut done_desired = BTreeMap::new();
        apply_group_and_icon(&mut done_desired, &done, &heads, &labels);
        assert!(done_desired.contains_key("quota_icon_done"));
        assert!(!done_desired.contains_key("quota_icon"));
        assert!(!done_desired.contains_key("quota_icon_working"));

        // Merely being focused when a turn finishes does not acknowledge it.
        let mut seen = done.clone();
        seen.focused = true;
        let mut seen_desired = BTreeMap::new();
        apply_group_and_icon(&mut seen_desired, &seen, &heads, &labels);
        assert!(
            seen_desired.contains_key("quota_icon_done"),
            "focused completion stays teal until the focus hook acknowledges it"
        );
        assert!(!seen_desired.contains_key("quota_icon"));

        // Unfocused sibling finishing must keep teal — not follow the focused
        // pane's yellow→white shortcut.
        let mut other = sibling.clone();
        other.pane_id = "w1:p3".into();
        other.status = AgentStatus::Done;
        other.focused = false;
        let mut other_desired = BTreeMap::new();
        apply_group_and_icon(&mut other_desired, &other, &heads, &labels);
        assert!(
            other_desired.contains_key("quota_icon_done"),
            "unfocused completion keeps teal until that pane is focused"
        );
        assert!(!other_desired.contains_key("quota_icon"));

        // Same-tab: refresh folds unseen into status=Done before publish.
        // A leftover done token on idle must not paint teal by itself —
        // that is how a stale agent-list read restored teal after focus.
        let mut stale_token = other.clone();
        stale_token.status = AgentStatus::Idle;
        stale_token
            .tokens
            .insert("quota_icon_done".to_string(), "teal".to_string());
        let mut stale_desired = BTreeMap::new();
        apply_group_and_icon(&mut stale_desired, &stale_token, &heads, &labels);
        assert!(
            stale_desired.contains_key("quota_icon"),
            "idle + leftover done token must not restore teal"
        );
        assert!(!stale_desired.contains_key("quota_icon_done"));
    }

    #[test]
    fn discovers_canonical_agent_panes_from_nested_json() {
        let value = json!({"result": {"agents": [
            {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "codex"},
            {"pane_id": "w1:p2", "tab_id": "w1:t2", "agent_session": {"agent": "claude"}},
            {"pane_id": "w1:p3", "agent": "unknown"},
            {"pane_id": "w1:p4", "agent": "opencode"}
        ], "tabs": [
            {"tab_id": "w1:t1", "label": "Owner"},
            {"tab_id": "w1:t2", "label": "Executor"}
        ]}});
        let mut panes = Vec::new();
        collect_agent_panes(&value, &mut panes);
        panes.sort_by(|left, right| left.pane_id.cmp(&right.pane_id));
        assert_eq!(
            panes,
            vec![
                AgentPane {
                    pane_id: "w1:p1".to_string(),
                    workspace_id: "w1".to_string(),
                    harness: Harness::Codex,
                    session: None,
                    session_summary: String::new(),
                    topic: String::new(),
                    tokens: BTreeMap::new(),
                    status: AgentStatus::Idle,
                    focused: false,
                },
                AgentPane {
                    pane_id: "w1:p2".to_string(),
                    workspace_id: "w1".to_string(),
                    harness: Harness::Claude,
                    session: None,
                    session_summary: String::new(),
                    topic: String::new(),
                    tokens: BTreeMap::new(),
                    status: AgentStatus::Idle,
                    focused: false,
                },
                AgentPane {
                    pane_id: "w1:p4".to_string(),
                    workspace_id: "w1".to_string(),
                    harness: Harness::OpenCode,
                    session: None,
                    session_summary: String::new(),
                    topic: String::new(),
                    tokens: BTreeMap::new(),
                    status: AgentStatus::Idle,
                    focused: false,
                },
            ]
        );
    }

    /// Herdr reports at most 16 metadata tokens per pane. A snapshot that
    /// fills every optional slot must still fit, or the tail is silently
    /// dropped and the sidebar loses whichever rows land last.
    #[test]
    fn a_fully_populated_pane_stays_within_herdrs_sixteen_token_report_cap() {
        const HERDR_TOKEN_REPORT_CAP: usize = 16;
        for provider in [
            Provider::Codex,
            Provider::Grok,
            Provider::Claude,
            Provider::Agy,
            Provider::OpenCodeGo,
        ] {
            let snapshot = ProviderSnapshot::new(
                provider,
                vec![
                    UsageWindow::new(
                        WindowKind::FiveHour,
                        85.0,
                        Some(ResetAt::from_unix_seconds(9_000)),
                    )
                    .unwrap(),
                    UsageWindow::new(
                        WindowKind::Weekly,
                        42.0,
                        Some(ResetAt::from_unix_seconds(600_000)),
                    )
                    .unwrap(),
                    UsageWindow::new(
                        WindowKind::Monthly,
                        10.0,
                        Some(ResetAt::from_unix_seconds(2_000_000)),
                    )
                    .unwrap(),
                ],
                0,
            )
            .with_model(Some("A Very Long Model Name".to_string()))
            .with_context(Some(ContextUsage {
                used_percent: 61.0,
                cache: Some(CacheUsage {
                    fresh_input_tokens: 1_000,
                    read_tokens: 50_000,
                    creation_tokens: 2_000,
                    hit_percent: 96.4,
                    ttl_seconds: Some(3_540),
                    last_activity_unix: None,
                    expires_at_unix: None,
                    session_totals: None,
                    session_id: None,
                    transcript_offset: 0,
                }),
            }));
            let desired = desired_tokens(
                &MetadataTokens::from_snapshot(&snapshot, 0),
                "a topic that is present",
                SidebarShape::default(),
            );
            assert!(
                desired.len() <= HERDR_TOKEN_REPORT_CAP,
                "{provider:?} would report {} tokens: {:?}",
                desired.len(),
                desired.keys().collect::<Vec<_>>()
            );
            // A monthly window has its own token; it must not ride a weekly one.
            for (name, value) in &desired {
                if name.starts_with("quota_month") {
                    continue;
                }
                assert!(
                    !value.contains("30d"),
                    "{provider:?} put a monthly value in {name}"
                );
            }
        }
    }

    #[test]
    fn plugin_quota_presence_ignores_topic_only_tokens() {
        let mut tokens = BTreeMap::new();
        tokens.insert("quota_topic".to_string(), "keep me".to_string());
        assert!(!plugin_quota_present(&tokens));
        tokens.insert("quota_5h".to_string(), "5h 10%".to_string());
        assert!(plugin_quota_present(&tokens));
    }

    #[test]
    fn retains_opencode_pane_session_id() {
        let value = json!({"result": {"agents": [{
            "pane_id": "w1:p9",
            "agent": "opencode",
            "agent_session": {"agent": "opencode", "value": "ses_go"}
        }]}});
        let mut panes = Vec::new();
        collect_agent_panes(&value, &mut panes);
        assert_eq!(panes[0].harness, Harness::OpenCode);
        assert_eq!(
            panes[0].session.as_ref().and_then(AgentSession::id),
            Some("ses_go")
        );
    }

    #[test]
    fn carries_path_kind_without_exposing_it_as_an_id() {
        let value = json!({"result": {"agents": [{
            "pane_id": "w1:p9",
            "agent": "pi",
            "agent_session": {
                "agent": "pi",
                "kind": "path",
                "source": "herdr:pi",
                "value": "/tmp/pi/sessions/project/session-pi.jsonl"
            }
        }]}});
        let mut panes = Vec::new();
        collect_agent_panes(&value, &mut panes);
        let session = panes[0].session.as_ref().unwrap();
        assert_eq!(panes[0].harness, Harness::Pi);
        assert_eq!(session.kind.as_deref(), Some("path"));
        assert_eq!(session.id(), None);
        assert_eq!(
            session.path(),
            Some("/tmp/pi/sessions/project/session-pi.jsonl")
        );
    }

    #[test]
    fn id_kind_preserves_every_existing_harness_session() {
        for agent in ["claude", "codex", "grok", "agy", "opencode", "devin"] {
            let value = json!({"result": {"agents": [{
                "pane_id": "w1:p1",
                "agent": agent,
                "agent_session": {"kind": "id", "value": "session-id"}
            }]}});
            let mut panes = Vec::new();
            collect_agent_panes(&value, &mut panes);
            assert_eq!(
                panes[0].session.as_ref().and_then(AgentSession::id),
                Some("session-id"),
                "{agent}"
            );
            assert_eq!(
                panes[0].session.as_ref().and_then(AgentSession::path),
                None,
                "{agent}"
            );
        }
    }

    #[test]
    fn unknown_session_kinds_are_not_reinterpreted() {
        for kind in ["PATH", "ID", "uri"] {
            let session = AgentSession {
                kind: Some(kind.to_string()),
                value: "session-value".to_string(),
            };
            assert_eq!(session.id(), None, "{kind}");
            assert_eq!(session.path(), None, "{kind}");
        }
    }

    #[test]
    fn quota_only_discovery_preserves_the_last_published_topic() {
        let value = json!({"result": {"agents": [{
            "pane_id": "w1:p1",
            "agent": "grok",
            "tokens": {"quota_topic": "latest task"}
        }]}});
        let mut panes = Vec::new();
        collect_agent_panes(&value, &mut panes);
        assert_eq!(panes[0].topic, "latest task");
    }

    #[test]
    fn discovers_codex_session_id_and_preserves_session_summary() {
        let value = json!({"result": {"agents": [{
            "pane_id": "w1:p1",
            "agent": "codex",
            "agent_session": {"agent": "codex", "value": "thread-1"},
            "tokens": {"quota_session": "previous summary"}
        }]}});
        let mut panes = Vec::new();
        collect_agent_panes(&value, &mut panes);
        assert_eq!(
            panes[0].session.as_ref().and_then(AgentSession::id),
            Some("thread-1")
        );
        assert_eq!(panes[0].session_summary, "previous summary");
    }

    #[test]
    fn legacy_metadata_tokens_force_one_bounded_cleanup_report() {
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Claude,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::from([(String::from("quota_badge"), String::from("[A]"))]),
            status: AgentStatus::Idle,
            focused: false,
        };
        let desired = BTreeMap::from([(String::from("quota_state"), String::from("?"))]);
        assert!(!metadata_matches(&pane.tokens, &desired));
        let names = metadata_report_names(&pane, &desired);
        assert!(names.contains(&"quota_badge"));
        assert!(names.len() <= MAX_METADATA_TOKENS);
    }

    #[test]
    fn weekly_only_inline_week_stays_inside_herdr_metadata_cap() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Grok,
            vec![crate::model::UsageWindow::new(
                crate::model::WindowKind::Weekly,
                30.0,
                Some(crate::model::ResetAt::from_unix_seconds(183_600)),
            )
            .unwrap()],
            0,
        )
        .with_context(Some(
            crate::model::ContextUsage::new(42.0)
                .unwrap()
                .with_cache(Some(
                    crate::model::CacheUsage::from_token_counts(100, 800, 100)
                        .unwrap()
                        .with_ttl_estimate(3_600, 0)
                        .with_session_totals(
                            crate::model::CacheTotals::from_token_counts(100, 800, 100),
                            "session-1",
                            1,
                        ),
                )),
        ));
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Grok,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::new(),
            status: AgentStatus::Idle,
            focused: false,
        };
        let names = metadata_report_names(&pane, &desired);
        assert!(names.len() <= MAX_METADATA_TOKENS);
        assert!(names.contains(&"quota_week_inline_normal"));
        assert!(!names.contains(&"quota_week_normal"));
        assert!(!names.contains(&"quota_5h"));
    }

    #[test]
    fn cache_diagnostics_stay_inside_herdr_metadata_cap() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Claude,
            vec![
                crate::model::UsageWindow::new(crate::model::WindowKind::FiveHour, 20.0, None)
                    .unwrap(),
                crate::model::UsageWindow::new(crate::model::WindowKind::Weekly, 30.0, None)
                    .unwrap(),
            ],
            0,
        )
        .with_context(Some(
            crate::model::ContextUsage::new(42.0)
                .unwrap()
                .with_cache(Some(
                    crate::model::CacheUsage::from_token_counts(100, 800, 100)
                        .unwrap()
                        .with_ttl_estimate(3_600, 0)
                        .with_session_totals(
                            crate::model::CacheTotals::from_token_counts(100, 800, 100),
                            "session-1",
                            1,
                        ),
                )),
        ));
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Claude,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::new(),
            status: AgentStatus::Idle,
            focused: false,
        };
        let names = metadata_report_names(&pane, &desired);
        assert!(names.len() <= MAX_METADATA_TOKENS);
        assert!(names.contains(&"quota_cache"));
        assert!(names.contains(&"quota_cache_ttl"));
    }

    /// The context row is coloured by the context *left* under `gauges`, on
    /// the windows' own bands, whichever side of the ledger it prints.
    #[test]
    fn gauges_publishes_context_into_the_severity_name_its_headroom_earns() {
        let gauges = SidebarShape::from(crate::cli::SidebarLayout::Gauges);
        for (used, expected) in [
            (31.0, "quota_context_normal"),
            (49.0, "quota_context_normal"),
            (50.0, "quota_context_normal"),
            (51.0, "quota_context_warning"),
            (53.0, "quota_context_warning"),
            (79.0, "quota_context_warning"),
            (80.0, "quota_context_warning"),
            (81.0, "quota_context_danger"),
            (85.0, "quota_context_danger"),
        ] {
            for percent in [PercentStyle::Remaining, PercentStyle::Used] {
                let mut tokens = BTreeMap::new();
                apply_context(
                    &mut tokens,
                    &ContextUsage::new(used).unwrap(),
                    0,
                    RowStyle::new(percent, gauges),
                );
                let published = CONTEXT_TOKEN_NAMES
                    .into_iter()
                    .filter(|name| tokens.contains_key(*name))
                    .collect::<Vec<_>>();
                assert_eq!(
                    published,
                    vec![expected],
                    "context {used} used, {percent:?}"
                );
            }
        }
    }

    /// Only one context name is ever filled, so a severity change or a layout
    /// switch can never leave a pane showing two context rows.
    #[test]
    fn a_context_severity_change_clears_the_name_it_moved_away_from() {
        let gauges = SidebarShape::from(crate::cli::SidebarLayout::Gauges);
        let mut tokens = BTreeMap::new();
        let row = RowStyle::new(PercentStyle::Used, gauges);
        apply_context(&mut tokens, &ContextUsage::new(31.0).unwrap(), 0, row);
        apply_context(&mut tokens, &ContextUsage::new(85.0).unwrap(), 0, row);
        assert!(!tokens.contains_key("quota_context_normal"));
        assert_eq!(
            tokens.get("quota_context_danger").map(String::as_str),
            Some("cx 85%")
        );
        apply_context(
            &mut tokens,
            &ContextUsage::new(85.0).unwrap(),
            0,
            RowStyle::default(),
        );
        assert_eq!(
            tokens.get("quota_context").map(String::as_str),
            Some("context 85%")
        );
        for name in ["quota_context_normal", "quota_context_danger"] {
            assert!(!tokens.contains_key(name), "{name}");
        }
    }

    /// Related cache details share a line when both fields are visible and
    /// the joined text fits the content width; they split again when it does
    /// not. Packed and stacked never fold, and hiding either field keeps the
    /// two tokens apart so the empty Herdr row can still collapse.
    #[test]
    fn gauges_join_cache_and_ttl_when_they_fit_the_content_width() {
        let cache_ttl = || {
            BTreeMap::from([
                ("quota_cache".to_string(), "cache 95.2%".to_string()),
                ("quota_cache_ttl".to_string(), "ttl≈29m".to_string()),
            ])
        };
        let mut wide = cache_ttl();
        fold_cache_row(
            &mut wide,
            RowStyle {
                fields: FieldSet::all(),
                percent: PercentStyle::Remaining,
                shape: SidebarShape::new(SidebarLayout::Gauges, 26),
            },
        );
        assert_eq!(
            wide.get("quota_cache").map(String::as_str),
            Some("cache 95.2% · ttl≈29m")
        );
        assert!(!wide.contains_key("quota_cache_ttl"));

        let mut narrow = cache_ttl();
        fold_cache_row(
            &mut narrow,
            RowStyle {
                fields: FieldSet::all(),
                percent: PercentStyle::Remaining,
                shape: SidebarShape::new(SidebarLayout::Gauges, 18),
            },
        );
        assert_eq!(
            narrow.get("quota_cache").map(String::as_str),
            Some("cache 95.2%")
        );
        assert_eq!(
            narrow.get("quota_cache_ttl").map(String::as_str),
            Some("ttl≈29m")
        );

        let mut cache_only = cache_ttl();
        fold_cache_row(
            &mut cache_only,
            RowStyle {
                fields: FieldSet::parse("cache").unwrap(),
                percent: PercentStyle::Remaining,
                shape: SidebarShape::new(SidebarLayout::Gauges, 26),
            },
        );
        assert_eq!(
            cache_only.get("quota_cache").map(String::as_str),
            Some("cache 95.2%")
        );
        assert!(cache_only.contains_key("quota_cache_ttl"));

        let mut packed = cache_ttl();
        fold_cache_row(
            &mut packed,
            RowStyle::new(
                PercentStyle::Remaining,
                SidebarShape::from(SidebarLayout::Packed),
            ),
        );
        assert_eq!(
            packed.get("quota_cache").map(String::as_str),
            Some("cache 95.2%")
        );
        assert!(packed.contains_key("quota_cache_ttl"));
    }

    /// `no cached` stays on `$quota_cache_state` even when the joined text
    /// would fit. Concatenating it into `$quota_cache` would share the line
    /// and lose the amber warning; gauges puts the two tokens on one row.
    #[test]
    fn gauges_do_not_fold_no_cached_into_the_cache_token() {
        let mut tokens = BTreeMap::from([
            ("quota_cache".to_string(), "cache 70.7%".to_string()),
            ("quota_cache_state".to_string(), "no cached".to_string()),
        ]);
        fold_cache_row(
            &mut tokens,
            RowStyle {
                fields: FieldSet::all(),
                percent: PercentStyle::Remaining,
                shape: SidebarShape::new(SidebarLayout::Gauges, 36),
            },
        );
        assert_eq!(
            tokens.get("quota_cache").map(String::as_str),
            Some("cache 70.7%")
        );
        assert_eq!(
            tokens.get("quota_cache_state").map(String::as_str),
            Some("no cached")
        );
    }

    /// `packed` and `stacked` keep the plain uncoloured name they have always
    /// published, and the used percent they have always printed, whatever the
    /// context value and whichever percent style the windows are drawn with.
    #[test]
    fn packed_and_stacked_keep_publishing_the_plain_context_token() {
        for layout in [
            crate::cli::SidebarLayout::Packed,
            crate::cli::SidebarLayout::Stacked,
        ] {
            for percent in [PercentStyle::Remaining, PercentStyle::Used] {
                let mut tokens = BTreeMap::new();
                apply_context(
                    &mut tokens,
                    &ContextUsage::new(85.0).unwrap(),
                    0,
                    RowStyle::new(percent, SidebarShape::from(layout)),
                );
                assert_eq!(
                    tokens.get("quota_context").map(String::as_str),
                    Some("context 85%"),
                    "{layout:?} {percent:?}"
                );
                for name in [
                    "quota_context_normal",
                    "quota_context_warning",
                    "quota_context_danger",
                ] {
                    assert!(!tokens.contains_key(name), "{layout:?} wrote {name}");
                }
            }
        }
    }

    /// The three context names are three more slots against Herdr's sixteen,
    /// even though at most one of them is ever filled.
    #[test]
    fn a_full_gauges_pane_stays_inside_herdr_metadata_cap() {
        let gauges = SidebarShape::from(crate::cli::SidebarLayout::Gauges);
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Claude,
            vec![
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::FiveHour,
                    20.0,
                    Some(crate::model::ResetAt::from_unix_seconds(18_000)),
                )
                .unwrap(),
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::Weekly,
                    30.0,
                    Some(crate::model::ResetAt::from_unix_seconds(183_600)),
                )
                .unwrap(),
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::WeeklyScoped,
                    40.0,
                    Some(crate::model::ResetAt::from_unix_seconds(183_600)),
                )
                .unwrap()
                .with_source_window("Fab", None),
            ],
            0,
        )
        .with_context(Some(
            crate::model::ContextUsage::new(85.0)
                .unwrap()
                .with_cache(Some(
                    crate::model::CacheUsage::from_token_counts(100, 800, 100)
                        .unwrap()
                        .with_ttl_estimate(3_600, 0)
                        .with_session_totals(
                            crate::model::CacheTotals::from_token_counts(100, 800, 100),
                            "session-1",
                            1,
                        ),
                )),
        ));
        let values = MetadataTokens::from_snapshot_for_session(
            &snapshot,
            0,
            None,
            crate::cli::PercentStyle::default(),
            gauges,
        );
        let desired = desired_tokens(&values, "prompt", gauges);
        assert!(desired.contains_key("quota_context_danger"));
        // The scoped row is part of the worst case, not an optional extra.
        assert!(
            desired.contains_key("quota_week_scoped_normal"),
            "{desired:?}"
        );
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Claude,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens: BTreeMap::new(),
            status: AgentStatus::Idle,
            focused: false,
        };
        let names = metadata_report_names(&pane, &desired);
        assert!(names.len() <= MAX_METADATA_TOKENS, "{names:?}");
    }

    #[test]
    fn exact_context_without_cache_clears_stale_cache_diagnostics() {
        let mut tokens = BTreeMap::from([
            ("quota_context".to_string(), "context 99%".to_string()),
            ("quota_cache".to_string(), "cache 95.0%".to_string()),
            ("quota_cache_ttl".to_string(), "ttl≈1h".to_string()),
        ]);
        apply_context(
            &mut tokens,
            &ContextUsage::new(12.0).unwrap(),
            0,
            RowStyle::default(),
        );
        assert_eq!(
            tokens.get("quota_context").map(String::as_str),
            Some("context 12%")
        );
        assert!(!tokens.contains_key("quota_cache"));
        assert!(!tokens.contains_key("quota_cache_ttl"));
    }

    #[test]
    fn stale_metadata_tokens_are_reported_for_cleanup_with_new_cache_rows() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Claude,
            vec![
                crate::model::UsageWindow::new(crate::model::WindowKind::FiveHour, 20.0, None)
                    .unwrap(),
                crate::model::UsageWindow::new(crate::model::WindowKind::Weekly, 30.0, None)
                    .unwrap(),
            ],
            0,
        )
        .with_context(Some(
            crate::model::ContextUsage::new(42.0)
                .unwrap()
                .with_cache(Some(
                    crate::model::CacheUsage::from_token_counts(100, 800, 100)
                        .unwrap()
                        .with_ttl_estimate(3_600, 0)
                        .with_session_totals(
                            crate::model::CacheTotals::from_token_counts(100, 800, 100),
                            "session-1",
                            1,
                        ),
                )),
        ));
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        let mut tokens = desired.clone();
        tokens.insert("quota_summary".to_string(), "old".to_string());
        tokens.insert("quota_status".to_string(), "OK".to_string());
        tokens.insert("quota_badge".to_string(), "[C]".to_string());
        tokens.insert("quota_session".to_string(), "old".to_string());
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Claude,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens,
            status: AgentStatus::Idle,
            focused: false,
        };
        let names = metadata_report_names(&pane, &desired);
        assert!(names.len() <= MAX_METADATA_TOKENS);
        assert!(names.contains(&"quota_cache"));
        assert!(names.contains(&"quota_cache_ttl"));
        assert!(names.contains(&"quota_summary"));
        assert!(names.contains(&"quota_status"));
        assert!(names.contains(&"quota_badge"));
        assert!(names.contains(&"quota_session"));
    }

    #[test]
    fn working_agent_detection_handles_herdr_agent_list_shape() {
        let value = json!({"result": {"agents": [
            {"agent": "claude", "agent_status": "working"},
            {"agent": "codex", "agent_status": "idle"},
            {"agent": "opencode", "agent_status": "working"}
        ]}});
        assert_eq!(working_providers_from(&value), vec![Provider::Claude]);
    }

    #[test]
    fn one_agent_inventory_deduplicates_working_providers() {
        let value = json!({"result": {"agents": [
            {"agent": "codex", "agent_status": "working"},
            {"agent_session": {"agent": "codex"}, "status": "working"},
            {"agent": "claude", "agent_status": "idle"}
        ]}});
        assert_eq!(working_providers_from(&value), vec![Provider::Codex]);
    }

    #[test]
    fn extracts_latest_agy_prompt_instead_of_status_line() {
        let text = "> older\nHello\n> hi\nHello!\n> Accept-edits mode: file edits auto-approved\n";
        assert_eq!(extract_topic(text, Harness::Agy).as_deref(), Some("hi"));
    }

    #[test]
    fn extracts_latest_claude_prompt_and_skips_clear_command() {
        let text = "❯ /clear\n❯ hi\n⏺ Hi! What can I help with?\n❯\n";
        assert_eq!(extract_topic(text, Harness::Claude).as_deref(), Some("hi"));
    }

    #[test]
    fn ignores_codex_default_prompt_placeholder() {
        assert_eq!(
            extract_topic("› Ask Codex to do anything\n", Harness::Codex),
            None
        );
    }

    #[test]
    fn ignores_ai_status_title_as_a_topic() {
        let value = json!({
            "pane_id": "w1:p1",
            "agent": "grok",
            "terminal_title_stripped": "Thinking - L7 Learning Reset"
        });
        let mut panes = Vec::new();
        collect_agent_panes(&value, &mut panes);
        assert_eq!(panes[0].topic, "");
    }

    #[test]
    fn a_missing_five_hour_window_folds_week_onto_context() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Claude,
            vec![crate::model::UsageWindow::new(
                crate::model::WindowKind::Weekly,
                31.0,
                Some(crate::model::ResetAt::from_unix_seconds(183_600)),
            )
            .unwrap()],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        assert!(!desired.contains_key("quota_5h_unknown"));
        assert!(!desired.contains_key("quota_5h_normal"));
        assert!(!desired.contains_key("quota_5h_label"));
        assert!(desired.contains_key("quota_week_inline_normal"));
        assert!(!desired.contains_key("quota_week_normal"));
    }

    /// A scoped weekly cap is legitimately absent most of the time — not
    /// every account or model has one, and it never carries over from a
    /// prior observation the way 5h/7d do. Absence must publish nothing, not
    /// an empty or stale row.
    #[test]
    fn an_absent_scoped_weekly_window_publishes_no_row() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Claude,
            vec![crate::model::UsageWindow::new(
                crate::model::WindowKind::Weekly,
                31.0,
                Some(crate::model::ResetAt::from_unix_seconds(183_600)),
            )
            .unwrap()],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        assert!(
            !desired
                .keys()
                .any(|name| name.starts_with("quota_week_scoped")),
            "{desired:?}"
        );
    }

    #[test]
    fn a_scoped_weekly_window_publishes_its_own_row_named_by_model() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Claude,
            vec![
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::FiveHour,
                    10.0,
                    Some(crate::model::ResetAt::from_unix_seconds(3_600)),
                )
                .unwrap(),
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::Weekly,
                    31.0,
                    Some(crate::model::ResetAt::from_unix_seconds(183_600)),
                )
                .unwrap(),
                crate::model::UsageWindow::new(crate::model::WindowKind::WeeklyScoped, 92.0, None)
                    .unwrap()
                    .with_source_window("Fab", None),
            ],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        assert!(
            desired
                .get("quota_week_scoped_danger")
                .is_some_and(|value| value.contains("Fab")),
            "{desired:?}"
        );
        // Untouched by the account-wide weekly row beside it.
        assert!(desired.contains_key("quota_week_normal"));
    }

    #[test]
    fn empty_five_hour_publishes_week_beside_context() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Codex,
            vec![crate::model::UsageWindow::new(
                crate::model::WindowKind::Weekly,
                31.0,
                Some(crate::model::ResetAt::from_unix_seconds(183_600)),
            )
            .unwrap()],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        assert!(!desired.contains_key("quota_5h_normal"));
        assert!(!desired.contains_key("quota_5h_label"));
        assert!(desired.contains_key("quota_week_inline_normal"));
        assert!(!desired.contains_key("quota_week_inline_label"));
        assert!(!desired.contains_key("quota_week_normal"));
    }

    #[test]
    fn present_five_hour_keeps_week_off_the_context_row() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Codex,
            vec![
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::FiveHour,
                    5.0,
                    Some(crate::model::ResetAt::from_unix_seconds(14_820)),
                )
                .unwrap(),
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::Weekly,
                    1.0,
                    Some(crate::model::ResetAt::from_unix_seconds(183_600)),
                )
                .unwrap(),
            ],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        assert_eq!(
            desired.get("quota_5h_normal").map(String::as_str),
            Some("5h 95% 4h07m")
        );
        assert_eq!(
            desired.get("quota_week_normal").map(String::as_str),
            Some("7d 99% 2d3h")
        );
        assert!(!desired.contains_key("quota_5h"));
        assert!(!desired.contains_key("quota_5h_label"));
        assert!(!desired.contains_key("quota_5h_eta"));
        assert!(!desired.contains_key("quota_week"));
        assert!(!desired.contains_key("quota_week_inline_normal"));
        assert!(!desired.contains_key("quota_week_inline_warning"));
        assert!(!desired.contains_key("quota_week_inline_danger"));
    }

    #[test]
    fn folding_week_onto_context_clears_limits_week_styles() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Grok,
            vec![crate::model::UsageWindow::new(
                crate::model::WindowKind::Weekly,
                25.0,
                Some(crate::model::ResetAt::from_unix_seconds(183_600)),
            )
            .unwrap()],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        let mut tokens = desired.clone();
        tokens.remove("quota_week_inline_normal");
        tokens.insert("quota_week_normal".to_string(), "7d 75% 5d0h".to_string());
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Grok,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens,
            status: AgentStatus::Idle,
            focused: false,
        };
        assert!(!metadata_matches(&pane.tokens, &desired));
        let names = metadata_report_names(&pane, &desired);
        assert!(names.contains(&"quota_week_normal"));
        assert!(names.contains(&"quota_week_inline_normal"));
        assert!(!desired.contains_key("quota_week_normal"));
        assert!(names.len() <= MAX_METADATA_TOKENS);
    }

    #[test]
    fn switching_into_a_five_hour_window_clears_inline_week() {
        let snapshot = crate::model::ProviderSnapshot::new(
            Provider::Codex,
            vec![
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::FiveHour,
                    5.0,
                    Some(crate::model::ResetAt::from_unix_seconds(14_820)),
                )
                .unwrap(),
                crate::model::UsageWindow::new(
                    crate::model::WindowKind::Weekly,
                    1.0,
                    Some(crate::model::ResetAt::from_unix_seconds(183_600)),
                )
                .unwrap(),
            ],
            0,
        );
        let desired = desired_tokens(
            &MetadataTokens::from_snapshot(&snapshot, 0),
            "prompt",
            SidebarShape::default(),
        );
        let mut tokens = desired.clone();
        tokens.insert("quota_week_inline_normal".to_string(), "7d 99%".to_string());
        let pane = AgentPane {
            pane_id: "w1:p1".to_string(),
            workspace_id: "w1".to_string(),
            harness: Harness::Codex,
            session: None,
            session_summary: String::new(),
            topic: String::new(),
            tokens,
            status: AgentStatus::Idle,
            focused: false,
        };
        assert!(!metadata_matches(&pane.tokens, &desired));
        let names = metadata_report_names(&pane, &desired);
        assert!(names.contains(&"quota_week_inline_normal"));
        assert!(names.contains(&"quota_week_normal"));
        assert!(names.len() <= MAX_METADATA_TOKENS);
    }

    #[test]
    fn publishes_exactly_one_styled_variant_for_each_window() {
        let mut tokens = BTreeMap::new();
        insert_severity_token(
            &mut tokens,
            "quota_week",
            "25%",
            Some(crate::model::Severity::Warning),
        );
        assert_eq!(
            tokens.get("quota_week_warning").map(String::as_str),
            Some("25%")
        );
        assert!(!tokens.contains_key("quota_week_normal"));
        assert!(!tokens.contains_key("quota_week_caution"));
        assert!(!tokens.contains_key("quota_week_danger"));
    }

    #[test]
    fn extracts_latest_grok_user_prompt_instead_of_ai_output() {
        let text = "❯ /goal 你在 ti 工作区接手 L7\n先读计划与权威文档，再按七步做 L7 盘点与设计。\n◇ Ran 1 subagent\n计划已读。先冻结坐标并读材料。\n";
        assert_eq!(
            extract_topic(text, Harness::Grok).as_deref(),
            Some("/goal 你在 ti 工作区接手 L7")
        );
    }

    // `recent` and `recent-unwrapped` rebuild the pane's wrapped scrollback,
    // which repaints it: one read, one visible scroll for the user.
    #[test]
    fn topic_reads_never_rebuild_a_pane_scrollback() {
        let args = topic_read_args("w1:p1");
        assert!(args.contains(&"visible"));
        assert!(!args.contains(&"recent"));
        assert!(!args.contains(&"recent-unwrapped"));
    }

    #[test]
    fn truncates_topics_without_splitting_utf8() {
        let topic = truncate_topic(&"你好".repeat(50));
        assert!(topic.ends_with('…'));
        assert!(topic.chars().count() <= 78);
    }
}
