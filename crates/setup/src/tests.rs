//! End-to-end setup tests on a temporary home with fixture configs that carry comments.

use crate::plan::apply_with;
use crate::*;
use craft_anything_evidence::ToolError;
use std::path::{Path, PathBuf};

const CLAUDE: &str =
    "// Claude Code state\n{\n  \"numStartups\": 3, /* keep */\n  \"mcpServers\": {\n    \"other\": {\"command\": \"other-server\"}, // theirs\n  },\n}\n";
const CODEX: &str = "# Codex config\nmodel = \"o\" # chosen\n\n[mcp_servers.other]\ncommand = \"other-server\" # theirs\n";
const HERMES: &str = "# Hermes\nmodel: x # inline\nmcp_servers:\n  # friend\n  other:\n    command: other-server\n";

struct Fx {
    _dir: tempfile::TempDir,
    host: Host,
    cli: PathBuf,
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();
    let mut host = Host::with_home(&home);
    host.os = Os::Linux;
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude.json"), CLAUDE).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(home.join(".codex/config.toml"), CODEX).unwrap();
    std::fs::create_dir_all(home.join(".hermes")).unwrap();
    std::fs::write(home.join(".hermes/config.yaml"), HERMES).unwrap();
    std::fs::create_dir_all(home.join(".cursor")).unwrap(); // detected, no config yet
    let cli = home.join("bin").join("craft-anything-cli");
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&cli, "#!/bin/sh\n").unwrap();
    Fx { _dir: dir, host, cli }
}

fn req(fx: &Fx, clients: &[&str]) -> PlanRequest {
    PlanRequest {
        clients: clients.iter().map(|s| s.to_string()).collect(),
        all_detected: clients.is_empty(),
        command: vec![fx.cli.display().to_string(), "mcp".into()],
        include_skill: true,
    }
}

fn read(p: impl AsRef<Path>) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn detects_installed_clients() {
    let fx = fixture();
    let d = detected(&fx.host);
    for id in ["claude_code", "codex", "hermes", "cursor"] {
        assert!(d.contains(&id), "{id}");
    }
    assert!(!d.contains(&"windsurf"));
}

#[test]
fn full_setup_preserves_comments_backs_up_and_is_idempotent() {
    let fx = fixture();
    let h = &fx.host.home;
    let p = plan(&fx.host, &req(&fx, &[])).unwrap();
    assert!(!p.blocked);
    let kinds: Vec<(String, ActionKind)> = p.actions.iter().map(|a| (a.client.clone(), a.action)).collect();
    assert!(kinds.contains(&("cursor".into(), ActionKind::Create)));
    assert!(kinds.contains(&("codex".into(), ActionKind::Update)));
    assert!(p.actions.iter().find(|a| a.client == "codex").unwrap().preview.contains("+ "));
    // planning writes nothing
    assert_eq!(read(h.join(".codex/config.toml")), CODEX);

    let r = apply(&p);
    assert_eq!(r.status, "configured", "{r:?}");
    let claude = read(h.join(".claude.json"));
    assert!(claude.contains("// Claude Code state") && claude.contains("/* keep */") && claude.contains("// theirs"));
    let codex = read(h.join(".codex/config.toml"));
    assert!(codex.contains("# chosen") && codex.contains("# theirs") && codex.contains("startup_timeout_sec = 30"));
    let hermes = read(h.join(".hermes/config.yaml"));
    assert!(hermes.contains("# friend") && hermes.contains("enabled: true"));
    assert!(h.join(".cursor/mcp.json").exists());
    // backups hold the originals; created files get none
    assert_eq!(read(h.join(".codex/config.toml.craft-anything.backup")), CODEX);
    assert!(!h.join(".cursor/mcp.json.craft-anything.backup").exists());
    // skill landed in the shared folder and Claude Code's own
    assert!(h.join(".agents/skills/craft-anything/SKILL.md").exists());
    assert!(h.join(".claude/skills/craft-anything/SKILL.md").exists());
    assert!(h.join(".hermes/skills/craft-anything/SKILL.md").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(h.join(".codex/config.toml")).unwrap().permissions().mode() & 0o777, 0o600);
    }

    // Second run: nothing to do.
    let p2 = plan(&fx.host, &req(&fx, &[])).unwrap();
    assert_eq!(p2.change_count(), 0, "{:?}", p2.actions.iter().map(|a| (&a.client, a.action)).collect::<Vec<_>>());
    assert_eq!(apply(&p2).status, "unchanged");
    // With no scope, setup refreshes exactly the apps we already own.
    let refresh = plan(&fx.host, &PlanRequest { clients: vec![], all_detected: false, ..req(&fx, &[]) }).unwrap();
    assert_eq!(refresh.actions.len(), 4);

    // A new command updates; the first backup is never overwritten.
    let mut moved = req(&fx, &["codex"]);
    moved.command = vec!["/opt/new/craft-anything-cli".into(), "mcp".into()];
    let r = apply(&plan(&fx.host, &moved).unwrap());
    assert_eq!(r.status, "configured");
    assert_eq!(read(h.join(".codex/config.toml.craft-anything.backup")), CODEX);

    // Doctor sees the aligned registrations and the stale one.
    let rep = doctor(&fx.host, &DoctorRequest { cli_path: Some(fx.cli.clone()), self_test: false, engines: false, ..Default::default() });
    let get = |n: &str| rep.checks.iter().find(|c| c.name == n).unwrap().status;
    assert_eq!(get("registration:claude_code"), CheckStatus::Ok);
    assert_eq!(get("registration:codex"), CheckStatus::Fail); // points at a missing binary
    assert!(!rep.healthy);

    // Uninstall removes only ours and keeps comments and the other servers.
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "removed", "{u:?}");
    let claude = read(h.join(".claude.json"));
    assert!(claude.contains("// theirs") && claude.contains("other-server") && !claude.contains("craft-anything-cli"));
    let codex = read(h.join(".codex/config.toml"));
    assert!(codex.contains("# theirs") && !codex.contains("craft-anything-cli"));
    let hermes = read(h.join(".hermes/config.yaml"));
    assert_eq!(hermes, HERMES);
    assert!(!h.join(".agents/skills/craft-anything/SKILL.md").exists());
    assert_eq!(uninstall(&fx.host).status, "nothing_to_remove");
}

#[test]
fn malformed_config_blocks_plan_and_uninstall() {
    let fx = fixture();
    let h = &fx.host.home;
    assert_eq!(apply(&plan(&fx.host, &req(&fx, &["cursor"])).unwrap()).status, "configured");
    std::fs::write(h.join(".codex/config.toml"), "model = [broken").unwrap();
    let p = plan(&fx.host, &req(&fx, &["codex", "claude_code"])).unwrap();
    assert!(p.blocked);
    assert_eq!(p.actions.iter().find(|a| a.client == "codex").unwrap().action, ActionKind::Invalid);
    let r = apply(&p);
    assert_eq!(r.status, "blocked");
    assert_eq!(read(h.join(".claude.json")), CLAUDE);
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "blocked");
    assert!(read(h.join(".cursor/mcp.json")).contains("craft-anything-cli"));
}

/// #238: Pi's folder exists but its mcp.json doesn't yet; setup creates it. An empty file is
/// still refused.
#[test]
fn pi_config_is_created_when_missing() {
    let fx = fixture();
    let pi = fx.host.home.join(".pi/agent/mcp.json");
    std::fs::create_dir_all(pi.parent().unwrap()).unwrap();
    let p = plan(&fx.host, &req(&fx, &["pi"])).unwrap();
    assert!(!p.blocked, "{p:?}");
    assert_eq!(apply(&p).status, "configured");
    let v: serde_json::Value = serde_json::from_str(&read(&pi)).unwrap();
    assert_eq!(v["mcpServers"]["craft-anything"]["args"][0], "mcp");
    std::fs::write(&pi, "").unwrap();
    assert!(plan(&fx.host, &req(&fx, &["pi"])).unwrap().blocked);
}

#[test]
fn strict_json_client_rejects_comments() {
    let fx = fixture();
    let pi = fx.host.home.join(".pi/agent/mcp.json");
    std::fs::create_dir_all(pi.parent().unwrap()).unwrap();
    std::fs::write(&pi, "{ // nope\n}").unwrap();
    let p = plan(&fx.host, &req(&fx, &["pi"])).unwrap();
    assert!(p.blocked);
    std::fs::write(&pi, "{\"mcpServers\": {}}").unwrap();
    let p = plan(&fx.host, &req(&fx, &["pi"])).unwrap();
    assert!(!p.blocked);
    assert_eq!(apply(&p).status, "configured");
    let v: serde_json::Value = serde_json::from_str(&read(&pi)).unwrap();
    assert_eq!(v["mcpServers"]["craft-anything"]["type"], "stdio");
}

#[test]
fn failed_readback_rolls_everything_back() {
    let fx = fixture();
    let h = &fx.host.home;
    let p = plan(&fx.host, &req(&fx, &["claude_code", "cursor", "codex"])).unwrap();
    // Writes "succeed" but the third file's content is wrong, so its read-back fails.
    let calls = std::cell::Cell::new(0);
    let bad = |path: &Path, b: &[u8]| -> Result<(), ToolError> {
        calls.set(calls.get() + 1);
        if calls.get() == 3 { craft_anything_platform::write_atomic(path, b"[mcp_servers]\n") } else { craft_anything_platform::write_atomic(path, b) }
    };
    let r = apply_with(&p, &bad);
    assert_eq!(r.status, "failed");
    assert_eq!(read(h.join(".claude.json")), CLAUDE);
    assert_eq!(read(h.join(".codex/config.toml")), CODEX);
    assert!(!h.join(".cursor/mcp.json").exists(), "created file must be removed");
    assert!(r.results.iter().any(|x| x.outcome == Outcome::RolledBack));
    assert!(r.results.iter().any(|x| x.outcome == Outcome::Failed));
}

#[test]
fn file_changed_after_plan_is_refused() {
    let fx = fixture();
    let p = plan(&fx.host, &req(&fx, &["codex"])).unwrap();
    std::fs::write(fx.host.home.join(".codex/config.toml"), "# edited meanwhile\n").unwrap();
    let r = apply(&p);
    assert_eq!(r.status, "failed");
    assert_eq!(read(fx.host.home.join(".codex/config.toml")), "# edited meanwhile\n");
}

#[test]
fn unknown_client_and_manual_client() {
    let fx = fixture();
    let e = plan(&fx.host, &req(&fx, &["nope"])).unwrap_err();
    assert!(e.remediation.unwrap().contains("claude_code"));
    let p = plan(&fx.host, &req(&fx, &["dsh"])).unwrap();
    assert_eq!(p.actions[0].action, ActionKind::Manual);
    assert!(p.actions[0].remediation.is_some());
    assert!(!p.blocked);
    let p = plan(&fx.host, &req(&fx, &["claude-code"])).unwrap();
    assert_eq!(p.actions[0].client, "claude_code");
}

#[test]
fn foreign_entry_with_our_name_is_replaced_but_not_uninstalled() {
    let fx = fixture();
    let cursor = fx.host.home.join(".cursor/mcp.json");
    std::fs::write(&cursor, "{\"mcpServers\": {\"craft-anything\": {\"command\": \"npx\", \"args\": [\"something\"]}}}").unwrap();
    assert_eq!(uninstall(&fx.host).status, "nothing_to_remove");
    let p = plan(&fx.host, &req(&fx, &["cursor"])).unwrap();
    assert!(p.actions[0].reason.as_deref().unwrap_or_default().contains("replaced"));
}

/// #1149: the app's own config.toml, as the reporter found it after uninstall.
const APP_CONFIG: &str = "# kept comment\nversion = 3\ndeep_mode = true\nask_before_running = true\nauto_mode = true\ntheme = \"studio-light\"\n\n[[ai.backends]]\nenabled = true\nkind = \"agent_cli\"\nmodel = \"opencode\"\nname = \"OpenCode\"\n\n[[ai.backends]]\nkind = \"agent_cli\"\nmodel = \"codex\"\nname = \"Codex\"\n";

fn app_config_path(fx: &Fx) -> PathBuf {
    fx.host.settings_path()
}

fn write_app_config(fx: &Fx, text: &str) -> PathBuf {
    let p = app_config_path(fx);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, text).unwrap();
    p
}

#[test]
fn uninstall_makes_app_config_inert() {
    let fx = fixture();
    let p = write_app_config(&fx, APP_CONFIG);
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "removed", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(p.as_path()));
    let after = read(&p);
    assert!(after.contains("auto_mode = false") && !after.contains("auto_mode = true"), "{after}");
    assert_eq!(after.matches("enabled = false").count(), 2, "{after}");
    assert!(!after.contains("enabled = true"), "{after}");
    // The real loader agrees the result is inert.
    let s = craft_anything_engine::settings::read_settings(&after).unwrap();
    assert!(!s.auto_mode, "{s:?}");
    // Everything else is preserved, including the comment.
    assert!(after.contains("# kept comment") && after.contains("deep_mode = true") && after.contains("theme = \"studio-light\""), "{after}");
    // Idempotent: a second uninstall changes nothing.
    let u2 = uninstall(&fx.host);
    assert_eq!(u2.status, "nothing_to_remove", "{u2:?}");
    assert_eq!(u2.neutralized_config, None);
    assert_eq!(read(&p), after);
}

#[test]
fn uninstall_leaves_already_inert_app_config_untouched() {
    let fx = fixture();
    let inert = APP_CONFIG.replace("auto_mode = true", "auto_mode = false").replace("enabled = true", "enabled = false");
    let inert = inert.replacen("kind = \"agent_cli\"\nmodel = \"codex\"", "enabled = false\nkind = \"agent_cli\"\nmodel = \"codex\"", 1);
    let p = write_app_config(&fx, &inert);
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "nothing_to_remove", "{u:?}");
    assert_eq!(u.neutralized_config, None);
    assert_eq!(read(&p), inert);
}

/// Missing keys count as on (`Settings::default` turns auto mode on, and a
/// backend without `enabled` is enabled), so uninstall writes them off.
#[test]
fn uninstall_writes_off_missing_auto_mode_and_enabled_keys() {
    let fx = fixture();
    let minimal = "version = 3\n\n[[ai.backends]]\nkind = \"agent_cli\"\nmodel = \"opencode\"\nname = \"OpenCode\"\n";
    let p = write_app_config(&fx, minimal);
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "removed", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(p.as_path()));
    let after = read(&p);
    assert!(after.contains("auto_mode = false"), "{after}");
    assert!(after.contains("enabled = false"), "{after}");
    let s = craft_anything_engine::settings::read_settings(&after).unwrap();
    assert!(!s.auto_mode, "{s:?}");
}

#[test]
fn uninstall_leaves_malformed_app_config_alone() {
    let fx = fixture();
    let broken = "auto_mode = [broken\n";
    let p = write_app_config(&fx, broken);
    let u = uninstall(&fx.host);
    // Still live, so the uninstall is honestly partial, not a success:
    // exit code 1, and the note tells the user the config was left alone.
    assert_eq!(u.status, "partial", "{u:?}");
    assert_eq!(u.neutralized_config, None);
    assert!(u.app_config_error.is_some(), "{u:?}");
    assert_eq!(read(&p), broken);
}

#[test]
fn uninstall_skips_newer_version_app_config() {
    let fx = fixture();
    let newer = APP_CONFIG.replace("version = 3", "version = 99");
    let p = write_app_config(&fx, &newer);
    let u = uninstall(&fx.host);
    // The config is live and was skipped, so this is partial too.
    assert_eq!(u.status, "partial", "{u:?}");
    assert_eq!(u.neutralized_config, None);
    assert!(u.app_config_error.is_some(), "{u:?}");
    assert_eq!(read(&p), newer);
}

#[test]
fn uninstall_makes_pending_sidecar_inert_too() {
    let fx = fixture();
    let p = write_app_config(&fx, APP_CONFIG);
    // A busy shutdown deferred non-AI edits (auto_mode on, language changed)
    // to the sidecar; the next load would merge them back into config.toml.
    let pending = "version = 3\nlanguage = \"de\"\nauto_mode = true\n";
    let pp = p.with_extension("pending.toml");
    std::fs::write(&pp, pending).unwrap();
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "removed", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(p.as_path()));
    assert_eq!(u.neutralized_pending_config.as_deref(), Some(pp.as_path()));
    // The sidecar is inert but its unrelated edit survives.
    let after = read(&pp);
    assert!(after.contains("auto_mode = false") && after.contains("language = \"de\""), "{after}");
    let s = craft_anything_engine::settings::read_settings(&after).unwrap();
    assert!(!s.auto_mode, "{s:?}");
}

#[test]
fn uninstall_reports_partial_when_pending_sidecar_cannot_be_made_inert() {
    let fx = fixture();
    let p = write_app_config(&fx, APP_CONFIG);
    let broken = "auto_mode = [broken\n";
    let pp = p.with_extension("pending.toml");
    std::fs::write(&pp, broken).unwrap();
    let u = uninstall(&fx.host);
    // The main config is inert, but the sidecar cannot be made inert safely,
    // so the uninstall is honestly partial, not a success.
    assert_eq!(u.status, "partial", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(p.as_path()));
    assert_eq!(u.neutralized_pending_config, None);
    assert!(u.app_config_error.as_deref().unwrap_or_default().contains("pending.toml"), "{u:?}");
    assert_eq!(read(&pp), broken);
}

#[test]
fn uninstall_neutralizes_config_dir_env_var_location() {
    let fx = fixture();
    let custom = fx.host.home.join("elsewhere");
    std::fs::create_dir_all(&custom).unwrap();
    let mut host = fx.host.clone();
    host.config_dir_override = Some(custom.clone());
    // A live config in the platform location must be left alone: the app
    // loads the env-var location instead.
    let platform = write_app_config(&fx, APP_CONFIG);
    let real = custom.join("config.toml");
    std::fs::write(&real, APP_CONFIG).unwrap();
    let u = uninstall(&host);
    assert_eq!(u.status, "removed", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(real.as_path()));
    assert!(read(&real).contains("auto_mode = false"), "{}", read(&real));
    assert_eq!(read(&platform), APP_CONFIG);
}

#[test]
fn uninstall_rejects_negative_version_app_config() {
    let fx = fixture();
    let negative = APP_CONFIG.replace("version = 3", "version = -1");
    // The loader rejects a negative version, so uninstall must not edit a
    // file the app cannot load.
    assert!(craft_anything_engine::settings::read_settings(&negative).is_err());
    let p = write_app_config(&fx, &negative);
    let u = uninstall(&fx.host);
    assert_eq!(u.status, "partial", "{u:?}");
    assert_eq!(u.neutralized_config, None);
    assert!(u.app_config_error.is_some(), "{u:?}");
    assert_eq!(read(&p), negative);
}

/// Switch the process working directory for the duration of a test and
/// restore it on drop, even on panic. No other test in this crate reads the
/// working directory, so the switch cannot disturb them.
struct ChangeDir(PathBuf);

fn change_dir(dir: &Path) -> ChangeDir {
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir).unwrap();
    ChangeDir(prev)
}

impl Drop for ChangeDir {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[test]
fn host_env_drops_empty_values() {
    let env = Host::capture_env([("XDG_CONFIG_HOME", String::new()), ("APPDATA", String::new()), ("LOCALAPPDATA", "/x".to_string())].into_iter());
    assert!(!env.contains_key("XDG_CONFIG_HOME"));
    assert!(!env.contains_key("APPDATA"));
    assert_eq!(env.get("LOCALAPPDATA"), Some(&"/x".to_string()));
}

#[test]
fn settings_path_matches_engine_config_dir_without_override() {
    // No override and no injected platform dir: the fallback must be the
    // engine's own `config_dir()`, exactly the file the app loads. A
    // relative XDG_CONFIG_HOME in the host env must not steer it — the
    // engine ignores relative values, and so must we.
    let mut host = Host::with_home("/home/u");
    host.config_dir_override = None;
    host.platform_config_dir = None;
    host.env.insert("XDG_CONFIG_HOME".to_string(), "rel/xdg".to_string());
    assert_eq!(host.settings_path(), craft_anything_engine::settings::config_dir().join("config.toml"));
}

#[test]
fn settings_path_uses_injected_platform_dir() {
    let host = Host::with_home("/home/u");
    assert_eq!(host.settings_path(), PathBuf::from("/home/u/.config/craft-anything/config.toml"));
}

#[test]
fn uninstall_neutralizes_config_in_working_directory_when_config_dir_is_empty() {
    let fx = fixture();
    // With CRAFT_ANYTHING_CONFIG_DIR empty the app loads ./config.toml, so
    // uninstall must neutralize that file, not the platform config.
    let cwd = fx.host.home.join("appcwd");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("config.toml"), APP_CONFIG).unwrap();
    let pending = "version = 3\nlanguage = \"de\"\nauto_mode = true\n";
    std::fs::write(cwd.join("config.pending.toml"), pending).unwrap();
    // A live platform config must be left alone: the app is not using it.
    let platform = write_app_config(&fx, APP_CONFIG);
    let mut host = fx.host.clone();
    host.config_dir_override = Some(PathBuf::from(""));
    let u = {
        let _guard = change_dir(&cwd);
        uninstall(&host)
    };
    assert_eq!(u.status, "removed", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(Path::new("config.toml")));
    assert_eq!(u.neutralized_pending_config.as_deref(), Some(Path::new("config.pending.toml")));
    assert!(read(cwd.join("config.toml")).contains("auto_mode = false"), "{}", read(cwd.join("config.toml")));
    let after_pending = read(cwd.join("config.pending.toml"));
    assert!(after_pending.contains("auto_mode = false") && after_pending.contains("language = \"de\""), "{after_pending}");
    assert_eq!(read(&platform), APP_CONFIG);
}

#[test]
#[cfg(unix)]
fn uninstall_neutralizes_non_utf8_config_dir() {
    use std::os::unix::ffi::OsStringExt;
    let fx = fixture();
    // A config dir whose bytes are not valid UTF-8: the engine still reads it
    // (`var_os`), so uninstall must neutralize it, not the platform config.
    let non_utf8 = fx.host.home.join(std::ffi::OsString::from_vec(b"cfg\xffdir".to_vec()));
    std::fs::create_dir_all(&non_utf8).unwrap();
    let real = non_utf8.join("config.toml");
    std::fs::write(&real, APP_CONFIG).unwrap();
    let real_pending = non_utf8.join("config.pending.toml");
    let pending = "version = 3\nlanguage = \"de\"\nauto_mode = true\n";
    std::fs::write(&real_pending, pending).unwrap();
    // A live platform config must be left alone: the app is not using it.
    let platform = write_app_config(&fx, APP_CONFIG);
    let mut host = fx.host.clone();
    host.config_dir_override = Some(non_utf8.clone());
    assert_eq!(host.settings_path(), real);
    let u = uninstall(&host);
    assert_eq!(u.status, "removed", "{u:?}");
    assert_eq!(u.neutralized_config.as_deref(), Some(real.as_path()));
    assert_eq!(u.neutralized_pending_config.as_deref(), Some(real_pending.as_path()));
    let after = read(&real);
    assert!(after.contains("auto_mode = false"), "{after}");
    let after_pending = read(&real_pending);
    assert!(after_pending.contains("auto_mode = false") && after_pending.contains("language = \"de\""), "{after_pending}");
    assert_eq!(read(&platform), APP_CONFIG);
}
