//! Plan what setup would change, apply it transactionally, and uninstall what we own.

use crate::clients::{Client, Format, Host, SERVER_NAME, clients, is_owned_entry};
use crate::config;
use crate::skill;
use craft_anything_engine::settings::SETTINGS_VERSION;
use craft_anything_evidence::{ErrorCode, ToolError};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// What to set up.
#[derive(Debug, Clone, Default)]
pub struct PlanRequest {
    /// Client ids. Empty (and not `all_detected`) means "refresh the clients that already have our entry".
    pub clients: Vec<String>,
    pub all_detected: bool,
    /// Program and args to register, e.g. `["/usr/local/bin/craft-anything-cli", "mcp"]`.
    pub command: Vec<String>,
    /// Also install the skill into each selected client's skills folder.
    pub include_skill: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Create,
    Update,
    AlreadyCurrent,
    Invalid,
    Manual,
}

/// One config file and what will happen to it.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub client: String,
    pub display: String,
    pub path: PathBuf,
    pub action: ActionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
    /// The lines that change (`-` old, `+` new), short.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub preview: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// File text when planned (to detect changes before apply) and the text to write.
    #[serde(skip)]
    pub(crate) old_text: Option<String>,
    #[serde(skip)]
    pub(crate) new_text: Option<String>,
    #[serde(skip)]
    pub(crate) client_def: Option<Client>,
}

/// Skill files for one skills folder.
#[derive(Debug, Clone, Serialize)]
pub struct SkillPlan {
    pub dir: PathBuf,
    pub for_clients: Vec<String>,
    pub files: Vec<skill::SkillFileAction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub command: Vec<String>,
    pub actions: Vec<Action>,
    pub skills: Vec<SkillPlan>,
    /// True when any selected client's config is invalid: nothing will be written.
    pub blocked: bool,
    pub notes: Vec<String>,
}

impl Plan {
    /// Number of file writes this plan would make.
    pub fn change_count(&self) -> usize {
        self.actions.iter().filter(|a| matches!(a.action, ActionKind::Create | ActionKind::Update)).count()
            + self.skills.iter().flat_map(|s| &s.files).filter(|f| f.action != "already_current").count()
    }
}

pub fn backup_path(p: &Path) -> PathBuf {
    skill::backup_path(p)
}

/// The default registration: this machine's `craft-anything-cli` plus `mcp`.
pub fn default_command() -> Result<Vec<String>, ToolError> {
    let p = craft_anything_platform::self_cli_path().ok_or_else(|| {
        ToolError::new(ErrorCode::EngineMissing, "Couldn't find craft-anything-cli.")
            .with_remediation("Install Craft Anything (the CLI ships next to the app), or pass its path explicitly.")
    })?;
    Ok(vec![p.display().to_string(), "mcp".into()])
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read(path) {
        Ok(b) => String::from_utf8(b).map(Some).map_err(|_| "the file is not valid UTF-8".to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn preview(old: &str, new: &str) -> String {
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let prefix = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let suffix = o.iter().rev().zip(n.iter().rev()).take_while(|(a, b)| a == b).count().min(o.len().saturating_sub(prefix)).min(n.len().saturating_sub(prefix));
    let mut out = String::new();
    for l in o.iter().skip(prefix).take(o.len().saturating_sub(prefix + suffix)).take(40) {
        out.push_str(&format!("- {l}\n"));
    }
    for l in n.iter().skip(prefix).take(n.len().saturating_sub(prefix + suffix)).take(40) {
        out.push_str(&format!("+ {l}\n"));
    }
    out
}

/// Inspect one client and decide what to do.
pub fn inspect_client(c: &Client, command: &[String]) -> Action {
    let mut a = Action {
        client: c.id.into(),
        display: c.display.into(),
        path: c.config.clone(),
        action: ActionKind::AlreadyCurrent,
        backup: None,
        preview: String::new(),
        reason: None,
        remediation: None,
        old_text: None,
        new_text: None,
        client_def: Some(c.clone()),
    };
    if c.format == Format::Manual {
        a.action = ActionKind::Manual;
        a.reason = Some("This app's configuration isn't edited automatically.".into());
        a.remediation = c.manual.map(str::to_string);
        return a;
    }
    let desired = c.desired_entry(command);
    let text = match read_text(&c.config) {
        Ok(t) => t,
        Err(e) => {
            a.action = ActionKind::Invalid;
            a.reason = Some(format!("Can't read {}: {e}", c.config.display()));
            a.remediation = Some("Check the file's permissions.".into());
            return a;
        }
    };
    // A missing file is created; only an existing empty one is invalid where empty is (#238).
    let base = text.clone().unwrap_or_else(|| if c.empty_is_invalid { "{}".into() } else { String::new() });
    let existing = match config::read_entry(c, &base) {
        Ok(e) => e,
        Err(e) => {
            a.action = ActionKind::Invalid;
            a.reason = Some(format!("{} can't be read as {:?}: {e}", c.config.display(), c.format));
            a.remediation = Some(format!("Fix or move {} (it was left untouched), then run setup again.", c.config.display()));
            return a;
        }
    };
    if existing.as_ref().is_some_and(|e| c.entry_is_current(e, &desired)) && !config::is_disabled(c, &base) {
        a.old_text = text;
        return a;
    }
    if let Some(e) = &existing
        && !is_owned_entry(e)
    {
        a.reason = Some(format!("A different \"{SERVER_NAME}\" entry exists and will be replaced."));
    }
    match config::set_entry(c, &base, &desired) {
        Ok(new) => {
            a.preview = preview(text.as_deref().unwrap_or_default(), &new);
            a.action = if text.is_some() { ActionKind::Update } else { ActionKind::Create };
            a.backup = text.as_ref().map(|_| backup_path(&c.config));
            a.old_text = text;
            a.new_text = Some(new);
        }
        Err(e) => {
            a.action = ActionKind::Invalid;
            a.reason = Some(format!("Can't add our entry to {}: {e}", c.config.display()));
            a.remediation = Some("Add it by hand, or fix the file and run setup again.".into());
        }
    }
    a
}

/// Work out what setup would change. Nothing is written.
pub fn plan(host: &Host, req: &PlanRequest) -> Result<Plan, ToolError> {
    if req.command.is_empty() {
        return Err(ToolError::invalid("No command to register."));
    }
    let all = clients(host);
    let mut notes = Vec::new();
    let selected: Vec<&Client> = if !req.clients.is_empty() {
        let mut v = Vec::new();
        for id in &req.clients {
            match all.iter().find(|c| c.is(id)) {
                Some(c) => v.push(c),
                None => {
                    let ids: Vec<&str> = all.iter().map(|c| c.id).collect();
                    return Err(ToolError::invalid(format!("Unknown client {id}")).with_remediation(format!("Use one of: {}", ids.join(", "))));
                }
            }
        }
        v
    } else if req.all_detected {
        all.iter().filter(|c| c.detected()).collect()
    } else {
        let owned: Vec<&Client> = all
            .iter()
            .filter(|c| {
                c.format != Format::Manual
                    && read_text(&c.config).ok().flatten().and_then(|t| config::read_entry(c, &t).ok().flatten()).is_some_and(|e| is_owned_entry(&e))
            })
            .collect();
        if owned.is_empty() {
            notes.push("No app is connected yet. Choose apps with --client <id> or --all-detected.".into());
        }
        owned
    };
    let actions: Vec<Action> = selected.iter().map(|c| inspect_client(c, &req.command)).collect();
    let blocked = actions.iter().any(|a| a.action == ActionKind::Invalid);
    if blocked {
        notes.push("Some configuration files can't be read safely, so nothing will be changed. Fix them and run setup again.".into());
    }
    let mut skills: Vec<SkillPlan> = Vec::new();
    if req.include_skill {
        for c in selected.iter().filter(|c| c.format != Format::Manual) {
            match skills.iter_mut().find(|s| s.dir == c.skills_dir) {
                Some(s) => s.for_clients.push(c.id.into()),
                None => skills.push(SkillPlan { dir: c.skills_dir.clone(), for_clients: vec![c.id.into()], files: skill::plan(&c.skills_dir) }),
            }
        }
    }
    Ok(Plan { command: req.command.clone(), actions, skills, blocked, notes })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Configured,
    Removed,
    Unchanged,
    Skipped,
    Failed,
    RolledBack,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileResult {
    pub client: String,
    pub path: PathBuf,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    /// `configured`, `unchanged`, `blocked` or `failed`.
    pub status: &'static str,
    pub results: Vec<FileResult>,
    pub skill_files: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_error: Option<String>,
    /// Apps to restart so they pick up the change.
    pub restart: Vec<String>,
}

type Writer<'a> = &'a dyn Fn(&Path, &[u8]) -> Result<(), ToolError>;

fn real_write(p: &Path, b: &[u8]) -> Result<(), ToolError> {
    craft_anything_platform::write_atomic_private(p, b)
}

/// Apply a plan. Config files are written one by one with a first-time backup, an atomic write and
/// a read-back check; if any step fails, every file changed by this call is put back.
pub fn apply(plan: &Plan) -> ApplyReport {
    apply_with(plan, &real_write)
}

pub(crate) fn apply_with(plan: &Plan, write: Writer<'_>) -> ApplyReport {
    let mut results = Vec::new();
    if plan.blocked {
        for a in &plan.actions {
            results.push(FileResult { client: a.client.clone(), path: a.path.clone(), outcome: Outcome::Skipped, backup: None, error: a.reason.clone() });
        }
        return ApplyReport { status: "blocked", results, skill_files: Vec::new(), skill_error: None, restart: Vec::new() };
    }
    let mut done: Vec<(usize, PathBuf, Option<String>)> = Vec::new();
    let mut failed = false;
    for a in &plan.actions {
        let (Some(new), Some(c)) = (&a.new_text, &a.client_def) else {
            let outcome = if a.action == ActionKind::AlreadyCurrent { Outcome::Unchanged } else { Outcome::Skipped };
            results.push(FileResult { client: a.client.clone(), path: a.path.clone(), outcome, backup: None, error: a.remediation.clone() });
            continue;
        };
        let r = write_one(a, c, new, write);
        if r.as_ref().err().is_none_or(|(_, written)| *written) {
            done.push((results.len(), a.path.clone(), a.old_text.clone()));
        }
        match r.map_err(|(e, _)| e) {
            Ok(()) => {
                results.push(FileResult { client: a.client.clone(), path: a.path.clone(), outcome: Outcome::Configured, backup: a.backup.clone(), error: None })
            }
            Err(e) => {
                results.push(FileResult {
                    client: a.client.clone(),
                    path: a.path.clone(),
                    outcome: Outcome::Failed,
                    backup: a.backup.clone(),
                    error: Some(e.message),
                });
                failed = true;
                break;
            }
        }
    }
    if failed {
        for (idx, path, old) in done.iter().rev() {
            let r = match old {
                Some(t) => craft_anything_platform::write_atomic(path, t.as_bytes()).map_err(|e| e.message),
                None => match std::fs::remove_file(path) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                    _ => Ok(()),
                },
            };
            if let Some(res) = results.get_mut(*idx) {
                match r {
                    Ok(()) => {
                        if res.outcome == Outcome::Configured {
                            res.outcome = Outcome::RolledBack;
                        }
                    }
                    Err(e) => {
                        res.error = Some(format!(
                            "{} Restoring it failed ({e}); restore it from {} by hand.",
                            res.error.clone().unwrap_or_default(),
                            backup_path(path).display()
                        ));
                    }
                }
            }
        }
        return ApplyReport { status: "failed", results, skill_files: Vec::new(), skill_error: None, restart: Vec::new() };
    }
    let mut skill_files = Vec::new();
    let mut skill_error = None;
    for s in &plan.skills {
        match skill::install(&s.dir) {
            Ok(mut w) => skill_files.append(&mut w),
            Err(e) => {
                skill_error = Some(e.message);
                break;
            }
        }
    }
    let restart: Vec<String> = results
        .iter()
        .filter(|r| r.outcome == Outcome::Configured)
        .map(|r| plan.actions.iter().find(|a| a.client == r.client).map(|a| a.display.clone()).unwrap_or_default())
        .collect();
    let status = if skill_error.is_some() {
        "failed"
    } else if restart.is_empty() && skill_files.is_empty() {
        "unchanged"
    } else {
        "configured"
    };
    ApplyReport { status, results, skill_files, skill_error, restart }
}

/// Write one file. On error, the flag says whether the file may have been changed.
fn write_one(a: &Action, c: &Client, new: &str, write: Writer<'_>) -> Result<(), (ToolError, bool)> {
    // The file must not have changed since the plan was made.
    let now = read_text(&a.path).map_err(|e| (ToolError::new(ErrorCode::FileError, e), false))?;
    if now != a.old_text {
        return Err((
            ToolError::new(ErrorCode::FileChanged, format!("{} changed after the plan was made.", a.path.display())).with_remediation("Run setup again."),
            false,
        ));
    }
    if now.is_some() {
        craft_anything_platform::backup_once(&a.path, &backup_path(&a.path)).map_err(|e| (e, false))?;
    }
    write(&a.path, new.as_bytes()).map_err(|e| (e, true))?;
    verify(c, &a.path, &a.client_desired()).map_err(|e| (e, true))
}

impl Action {
    fn client_desired(&self) -> Value {
        match (&self.client_def, &self.new_text) {
            (Some(c), Some(t)) => config::read_entry(c, t).ok().flatten().unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }
}

/// Re-read `path` and check our entry equals `desired` and isn't disabled.
pub fn verify(c: &Client, path: &Path, desired: &Value) -> Result<(), ToolError> {
    let text = std::fs::read_to_string(path).map_err(|e| ToolError::io(path.display(), e))?;
    let got = config::read_entry(c, &text).map_err(|e| ToolError::new(ErrorCode::FileError, format!("{} no longer parses: {e}", path.display())))?;
    if got.as_ref() != Some(desired) || desired.is_null() || config::is_disabled(c, &text) {
        return Err(ToolError::new(ErrorCode::FileError, format!("{} did not read back with the expected entry.", path.display())));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct UninstallReport {
    /// `removed`, `nothing_to_remove`, `blocked` or `partial`. `removed`
    /// also covers making the app config inert; `nothing_to_remove` means
    /// nothing was removed and the app config was already inert or absent;
    /// `partial` means something was left live — a client entry that failed
    /// to remove, or an app config (or its pending sidecar) that could not
    /// be made inert safely.
    pub status: &'static str,
    pub removed_entries: Vec<FileResult>,
    pub removed_skill_files: Vec<PathBuf>,
    /// The app's own `config.toml` when uninstall made it inert.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neutralized_config: Option<PathBuf>,
    /// The deferred `config.pending.toml` sidecar when uninstall made it
    /// inert. A shutdown may defer non-AI edits there; the next
    /// `Settings::load()` merges them back into `config.toml`, so the sidecar
    /// is neutralized in place too, keeping its other edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub neutralized_pending_config: Option<PathBuf>,
    /// Why the app's `config.toml` (or its pending sidecar) is still live:
    /// it was left alone because it could not be made inert safely
    /// (unreadable, malformed or newer), or the write/read-back failed.
    /// Absent when the file is missing or already inert.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_config_error: Option<String>,
    /// Configs that stopped the uninstall (malformed or unreadable).
    pub blocking: Vec<Action>,
    pub kept: Vec<String>,
}

/// Remove our registrations and skill files, and make the app's own config inert so the
/// app stops executing tools on its own. If any client config can't be read safely,
/// nothing at all is removed. A malformed or newer app config is left alone and reported.
/// The settings path is the one the app actually loads (`Host::settings_path`),
/// and a deferred `config.pending.toml` sidecar is neutralized in place too,
/// keeping its unrelated edits.
pub fn uninstall(host: &Host) -> UninstallReport {
    let all = clients(host);
    let mut blocking = Vec::new();
    let mut targets: Vec<(&Client, String, String)> = Vec::new();
    for c in all.iter().filter(|c| c.format != Format::Manual) {
        let text = match read_text(&c.config) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                let mut a = inspect_client(c, &[]);
                a.action = ActionKind::Invalid;
                a.reason = Some(e);
                blocking.push(a);
                continue;
            }
        };
        match config::read_entry(c, &text) {
            Err(_) => blocking.push(inspect_client(c, &[])),
            Ok(Some(e)) if is_owned_entry(&e) => match config::remove_entry(c, &text) {
                Ok(Some(new)) => targets.push((c, text, new)),
                _ => blocking.push(inspect_client(c, &[])),
            },
            Ok(_) => {}
        }
    }
    let kept = vec!["Projects, evidence and settings".into(), "Other MCP servers and unrelated skills".into(), "Analysis engines".into()];
    if !blocking.is_empty() {
        return UninstallReport {
            status: "blocked",
            removed_entries: Vec::new(),
            removed_skill_files: Vec::new(),
            neutralized_config: None,
            neutralized_pending_config: None,
            app_config_error: None,
            blocking,
            kept,
        };
    }
    let mut removed_entries = Vec::new();
    let mut partial = false;
    for (c, old, new) in targets {
        let backup = backup_path(&c.config);
        let r = craft_anything_platform::backup_once(&c.config, &backup)
            .and_then(|_| craft_anything_platform::write_atomic_private(&c.config, new.as_bytes()))
            .and_then(|_| {
                let t = std::fs::read_to_string(&c.config).map_err(|e| ToolError::io(c.config.display(), e))?;
                match config::read_entry(c, &t) {
                    Ok(None) => Ok(()),
                    _ => Err(ToolError::new(ErrorCode::FileError, "the entry is still there after removal")),
                }
            });
        let (outcome, error) = match r {
            Ok(()) => (Outcome::Removed, None),
            Err(e) => {
                partial = true;
                let restored = craft_anything_platform::write_atomic(&c.config, old.as_bytes()).is_ok();
                (Outcome::Failed, Some(if restored { e.message } else { format!("{} Restore {} by hand.", e.message, backup.display()) }))
            }
        };
        removed_entries.push(FileResult { client: c.id.into(), path: c.config.clone(), outcome, backup: Some(backup), error });
    }
    let mut dirs: Vec<PathBuf> = all.iter().map(|c| c.skills_dir.clone()).collect();
    dirs.push(host.shared_skills());
    dirs.sort();
    dirs.dedup();
    let removed_skill_files: Vec<PathBuf> = dirs.iter().flat_map(|d| skill::uninstall(d)).collect();
    let app_config = host.settings_path();
    let (neutralized_config, app_config_error, app_config_live) = neutralize_app_config(&app_config);
    // A busy shutdown defers non-AI edits to `config.pending.toml`, and the
    // next `Settings::load()` merges them back into `config.toml` — so a
    // sidecar with auto_mode on would undo the neutralization above. Make it
    // inert in place too, keeping its unrelated edits; if it cannot be made
    // safe, the uninstall is partial rather than a success.
    let pending_path = app_config.with_extension("pending.toml");
    let (neutralized_pending_config, pending_error, pending_live) = neutralize_app_config(&pending_path);
    let app_config_error = match (app_config_error, pending_error) {
        (Some(a), Some(b)) => Some(format!("{a}; {b}")),
        (a, b) => a.or(b),
    };
    let status = if partial || app_config_live || pending_live {
        "partial"
    } else if removed_entries.is_empty() && removed_skill_files.is_empty() && neutralized_config.is_none() && neutralized_pending_config.is_none() {
        "nothing_to_remove"
    } else {
        "removed"
    };
    UninstallReport { status, removed_entries, removed_skill_files, neutralized_config, neutralized_pending_config, app_config_error, blocking, kept }
}

/// Make a `config.toml` text inert: `auto_mode = false` and every
/// `[[ai.backends]]` entry `enabled = false`. A backend without an `enabled`
/// key counts as on — `BackendConfig::enabled` defaults to true, as does
/// `Settings::default` for auto mode — so the key is written explicitly.
/// Returns the new text when anything changed, `None` when it is already
/// inert. `deep_mode` is left alone: the issue only asks for automatic
/// execution to stop, and `deep_mode` on its own does not run anything.
fn inert_config_text(text: &str) -> Result<Option<String>, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("not valid TOML: {e}"))?;
    // Same version rules as `Settings::read_settings`: missing means 0, a
    // non-integer or negative version is broken, a newer version is left alone.
    let version = match doc.get("version") {
        None => 0,
        Some(v) => v.as_integer().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| format!("its version ({v}) isn't a whole number"))?,
    };
    if version > SETTINGS_VERSION {
        return Err(format!("version {version} is newer than this build understands (max {SETTINGS_VERSION}); left alone"));
    }
    let mut changed = false;
    match doc.get_mut("auto_mode") {
        None => {
            doc["auto_mode"] = toml_edit::value(false);
            changed = true;
        }
        Some(item) => match item.as_bool() {
            Some(true) => {
                *item = toml_edit::value(false);
                changed = true;
            }
            Some(false) => {}
            None => return Err("auto_mode isn't true or false".to_string()),
        },
    }
    if let Some(ai) = doc.get_mut("ai") {
        let Some(table) = ai.as_table_mut() else {
            return Err("ai isn't a table".to_string());
        };
        if let Some(backends) = table.get_mut("backends") {
            changed |= disable_backends(backends)?;
        }
    }
    Ok(if changed { Some(doc.to_string()) } else { None })
}

/// Set `enabled = false` on every backend entry. `[[ai.backends]]` parses
/// as an array of tables; an inline `ai.backends = [...]` array is accepted
/// too. Returns whether anything changed.
fn disable_backends(backends: &mut toml_edit::Item) -> Result<bool, String> {
    let mut changed = false;
    if let Some(tables) = backends.as_array_of_tables_mut() {
        for table in tables.iter_mut() {
            changed |= disable_backend(table)?;
        }
        return Ok(changed);
    }
    let Some(array) = backends.as_array_mut() else {
        return Err("ai.backends isn't an array of tables".to_string());
    };
    for entry in array.iter_mut() {
        let Some(table) = entry.as_inline_table_mut() else {
            return Err("an ai.backends entry isn't a table".to_string());
        };
        // Inline tables hold `Value`s, not `Item`s, so the same disable
        // logic as `disable_backend` with `Value::from(false)`.
        match table.get_mut("enabled") {
            None => {
                table["enabled"] = toml_edit::Value::from(false);
                changed = true;
            }
            Some(item) => match item.as_bool() {
                Some(true) => {
                    *item = toml_edit::Value::from(false);
                    changed = true;
                }
                Some(false) => {}
                None => return Err("an ai.backends enabled isn't true or false".to_string()),
            },
        }
    }
    Ok(changed)
}

/// Set `enabled = false` on one backend table, writing the key when it is
/// missing (a backend without `enabled` counts as on). Returns whether
/// anything changed.
fn disable_backend(table: &mut toml_edit::Table) -> Result<bool, String> {
    match table.get_mut("enabled") {
        None => {
            table["enabled"] = toml_edit::value(false);
            Ok(true)
        }
        Some(item) => match item.as_bool() {
            Some(true) => {
                *item = toml_edit::value(false);
                Ok(true)
            }
            Some(false) => Ok(false),
            None => Err("an ai.backends enabled isn't true or false".to_string()),
        },
    }
}

/// Make the app's own `config.toml` inert in place, keeping every other
/// setting. Returns `(neutralized path, note, still_live)`: at most one of
/// the first two is `Some`. A missing or already-inert file is `(None, None,
/// false)`; a file left alone while possibly live (unreadable, malformed or
/// newer) carries the reason as a note with `still_live` true, as does a
/// failed write or read-back — either way the caller must not report
/// success, since automatic execution may still be on.
fn neutralize_app_config(path: &Path) -> (Option<PathBuf>, Option<String>, bool) {
    let old = match read_text(path) {
        Ok(Some(text)) => text,
        Ok(None) => return (None, None, false),
        Err(e) => return (None, Some(format!("{} can't be read: {e}", path.display())), true),
    };
    let new = match inert_config_text(&old) {
        Ok(Some(text)) => text,
        Ok(None) => return (None, None, false),
        Err(e) => return (None, Some(format!("{} left alone: {e}", path.display())), true),
    };
    let backup = backup_path(path);
    let result = craft_anything_platform::backup_once(path, &backup)
        .and_then(|_| craft_anything_platform::write_atomic_private(path, new.as_bytes()))
        .and_then(|_| match std::fs::read_to_string(path).map_err(|e| ToolError::io(path.display(), e)) {
            Ok(text) => match inert_config_text(&text) {
                Ok(None) => Ok(()),
                _ => Err(ToolError::new(ErrorCode::FileError, "the config is still live after neutralizing")),
            },
            Err(e) => Err(e),
        });
    match result {
        Ok(()) => (Some(path.to_path_buf()), None, false),
        Err(e) => {
            let restored = craft_anything_platform::write_atomic(path, old.as_bytes()).is_ok();
            let message = if restored { e.message } else { format!("{} Restore {} by hand.", e.message, backup.display()) };
            (None, Some(message), true)
        }
    }
}
