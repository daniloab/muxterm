//! Per-agent model lists, discovered from the agent CLIs and cached, so a
//! model release doesn't mean editing `agent::AGENTS` and rebuilding.
//!
//! Shape: the registry's `seed_models` are the compiled-in floor - what the
//! pickers show until a CLI-discovered list replaces them, and what
//! everything falls back to when discovery fails (CLI missing, hung, or
//! its output unparseable). Discovered lists live in
//! `~/.muxterm/models.json` (`Cache`), keyed by agent id and stamped with
//! the CLI's `--version` line and a fetch time, and are refreshed on a
//! background thread only when that version moves or the entry is a day
//! old (`needs_refresh`). Both binaries `install` the cache at startup into
//! one process-global `CATALOG` that every reader (`for_agent`,
//! `default_model`, `fast_model`) answers from - the readers include free
//! functions with no App in reach (the popup's default, the settings
//! stepper, the title one-shot) and the `mux` CLI, so threading the lists
//! through would touch every call site for no gain.
//!
//! Sources (`discover`): codex has a real catalog command (`codex debug
//! models`, JSON, filtered to rows codex itself lists and sorted by its
//! priority). claude has none - its `--model` takes floating family aliases
//! that always resolve to the latest release, so the alias set can't go
//! stale by itself - but org-specific extras (the `[1m]` rows of `/model`)
//! sit in `~/.claude.json`, so the claude list is aliases + those. pi and
//! opencode can list (20+ mixed-provider rows) but keep their seeds until
//! the picker can curate that; adding either is one arm in `discover`.

use std::collections::HashMap;
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::agent::{self, Agent};
use crate::state;

/// A discovered list goes stale after this even under an unchanged CLI
/// version (claude's org extras can move without a release).
pub const TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// `<bin> --version` through the login shell; a hung probe leaves the cache
/// as it was.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// The catalog command itself. `codex debug models` returns in well under
/// a second; the budget covers a cold login shell.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(20);

/// claude's floating family aliases, oldest-default first (matches the
/// registry seed): each resolves to the family's latest release inside the
/// CLI, so this list only needs an edit when a new *family* ships.
pub const CLAUDE_ALIASES: &[&str] = &["opus", "fable", "sonnet", "haiku"];

/// One agent's discovered list, as cached on disk.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unix seconds of the fetch.
    pub fetched_at: u64,
    /// First line of `<bin> --version` at fetch time.
    pub version: String,
    pub models: Vec<String>,
}

/// agent id -> entry, the on-disk shape of `models.json`.
pub type Cache = HashMap<String, Entry>;

/// What every reader answers from: agent id -> live list. Empty means
/// "use the seed"; `install`/`refresh` fill it.
static CATALOG: RwLock<Option<HashMap<String, Vec<String>>>> =
    RwLock::new(None);

pub fn path() -> PathBuf {
    state::config_dir().join("models.json")
}

/// The cache file's mtime, for the GUI's live-reload tick (`mux models
/// --refresh` from a shell lands without a relaunch).
pub fn mtime() -> Option<SystemTime> {
    fs::metadata(path()).and_then(|m| m.modified()).ok()
}

pub fn load() -> Cache {
    load_from(&path())
}

fn load_from(path: &Path) -> Cache {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Atomic like `state::save`: a torn cache must never lose the lists (load
/// shrugs it off anyway and the seeds take over).
pub fn save(cache: &Cache) {
    save_to(&path(), cache)
}

fn save_to(path: &Path, cache: &Cache) {
    let Ok(json) = serde_json::to_string_pretty(cache) else {
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, json).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

/// Make a loaded cache the live catalog. Entries with an empty list are
/// ignored (the seed stays), so a bad fetch can never blank a picker.
pub fn install(cache: Cache) {
    let live: HashMap<String, Vec<String>> = cache
        .into_iter()
        .filter(|(_, e)| !e.models.is_empty())
        .map(|(id, e)| (id, e.models))
        .collect();
    if let Ok(mut c) = CATALOG.write() {
        *c = Some(live);
    }
}

fn set(id: &str, models: Vec<String>) {
    if models.is_empty() {
        return;
    }
    if let Ok(mut c) = CATALOG.write() {
        c.get_or_insert_with(HashMap::new)
            .insert(id.to_string(), models);
    }
}

/// The agent's current model list: discovered when one is installed, the
/// registry seed otherwise. First entry is the picker default.
pub fn for_agent(id: &str) -> Vec<String> {
    if let Ok(c) = CATALOG.read() {
        if let Some(found) = c.as_ref().and_then(|m| m.get(id)) {
            return found.clone();
        }
    }
    agent::by_id(id)
        .map(|a| a.seed_models.iter().map(|m| m.to_string()).collect())
        .unwrap_or_default()
}

/// Whether `id`'s list came from a CLI (true) or is the seed (false).
pub fn is_discovered(id: &str) -> bool {
    CATALOG
        .read()
        .ok()
        .and_then(|c| c.as_ref().map(|m| m.contains_key(id)))
        .unwrap_or(false)
}

/// The picker's default selection for an agent: its first listed model.
pub fn default_model(id: &str) -> String {
    for_agent(id).into_iter().next().unwrap_or_default()
}

/// The model for quick one-shots (`mux ask` without `agent_model`, title
/// generation): the registry's preference when the live list still offers
/// it, else the list's default - a retired slug degrades to a working
/// model instead of a CLI error. None stays None (the CLI's own default).
pub fn fast_model(agent: &Agent) -> Option<String> {
    let pref = agent.fast_model?;
    let list = for_agent(agent.id);
    if list.iter().any(|m| m == pref) {
        return Some(pref.to_string());
    }
    Some(list.into_iter().next().unwrap_or_else(|| pref.to_string()))
}

/// Whether a cached entry should be re-fetched: none yet, the CLI moved
/// (a release is when new models appear), or simply old.
pub fn needs_refresh(entry: Option<&Entry>, version: &str, now: u64) -> bool {
    match entry {
        None => true,
        Some(e) => {
            e.version != version
                || now.saturating_sub(e.fetched_at) > TTL.as_secs()
        },
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Which agents `discover` has a source for.
pub fn discoverable(agent: &Agent) -> bool {
    matches!(agent.id, "codex" | "claude")
}

/// Bring one agent's cache entry up to date if it needs it, installing the
/// result into the live catalog. Returns whether the cache changed (the
/// caller saves). Never removes an entry: a failed fetch keeps the last
/// good list. `force` skips the staleness test but not the version probe
/// (the entry is stamped with it).
pub fn refresh(agent: &Agent, cache: &mut Cache, force: bool) -> bool {
    if !discoverable(agent) {
        return false;
    }
    let Some(version) = cli_version(agent.bin) else {
        return false;
    };
    let now = now_secs();
    if !force && !needs_refresh(cache.get(agent.id), &version, now) {
        return false;
    }
    let Some(models) = discover(agent) else {
        return false;
    };
    let entry = Entry {
        fetched_at: now,
        version,
        models: models.clone(),
    };
    let changed = cache.get(agent.id) != Some(&entry);
    cache.insert(agent.id.to_string(), entry);
    set(agent.id, models);
    changed
}

/// Ask the CLI for its models. None when there is no source or the source
/// yields nothing usable; the caller keeps what it had.
pub fn discover(agent: &Agent) -> Option<Vec<String>> {
    let models = match agent.id {
        "codex" => {
            let out = shell_output("codex debug models", DISCOVER_TIMEOUT)?;
            parse_codex_catalog(&out)
        },
        "claude" => {
            let home = std::env::var_os("HOME").map(PathBuf::from)?;
            let text = fs::read_to_string(home.join(".claude.json")).ok();
            claude_models(text.as_deref())
        },
        _ => return None,
    };
    (!models.is_empty()).then_some(models)
}

/// codex's `debug models` JSON: `{"models":[{slug, visibility, priority,
/// upgrade, ...}]}`. Keep what codex itself lists (`visibility == "list"`)
/// and hasn't marked for migration (`upgrade` set = "switch to X to
/// continue"), in codex's own priority order.
pub fn parse_codex_catalog(json: &str) -> Vec<String> {
    #[derive(Deserialize)]
    struct Catalog {
        #[serde(default)]
        models: Vec<Row>,
    }
    #[derive(Deserialize)]
    struct Row {
        slug: String,
        #[serde(default)]
        visibility: String,
        #[serde(default)]
        priority: i64,
        #[serde(default)]
        upgrade: Option<serde_json::Value>,
    }
    let Ok(cat) = serde_json::from_str::<Catalog>(json) else {
        return Vec::new();
    };
    let mut rows: Vec<Row> = cat
        .models
        .into_iter()
        .filter(|r| r.visibility == "list")
        .filter(|r| r.upgrade.as_ref().map_or(true, |u| u.is_null()))
        .filter(|r| !r.slug.is_empty())
        .collect();
    rows.sort_by_key(|r| r.priority);
    rows.into_iter().map(|r| r.slug).collect()
}

/// claude's list: the family aliases, then the org-specific extras claude
/// caches in `~/.claude.json` (`additionalModelOptionsCache[].value`, e.g.
/// `claude-fable-5-1[1m]`), deduped in order. A missing or odd file just
/// yields the aliases.
pub fn claude_models(home_json: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> =
        CLAUDE_ALIASES.iter().map(|s| s.to_string()).collect();
    let extras = home_json
        .and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok())
        .and_then(|v| v.get("additionalModelOptionsCache").cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for row in extras {
        let Some(value) = row.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() && !out.iter().any(|m| m == value) {
            out.push(value.to_string());
        }
    }
    out
}

/// First line of `<bin> --version`, or None when the probe fails or hangs.
pub fn cli_version(bin: &str) -> Option<String> {
    let out = shell_output(&format!("{bin} --version"), VERSION_TIMEOUT)?;
    let line = out.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(line.to_string())
}

/// How long after the process exits to keep collecting stdout. Both CLIs
/// leave a background child (update check, daemon) that inherits the pipe
/// and holds it open for seconds after `--version` has returned, so
/// waiting for EOF would cost that on every probe; by exit the answer is
/// already in the pipe.
const DRAIN_GRACE: Duration = Duration::from_millis(300);

/// Run a command line through the user's interactive login shell (PATH -
/// see `agent::binary_available`) under a deadline, returning its stdout.
/// Unlike `agent::output_with_timeout` the pipe is pumped by a thread, so a
/// catalog larger than the pipe buffer can't stall the child past the
/// deadline; and the pump is *not* joined - reading stops `DRAIN_GRACE`
/// after the process ends, whether or not an orphan still holds the write
/// end (the thread finishes on its own when that closes). Stderr is
/// dropped: a chatty CLI's progress isn't the answer.
fn shell_output(line: &str, timeout: Duration) -> Option<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(shell)
        .args(["-ilc", line])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                },
            }
        }
    });
    let mut out: Vec<u8> = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    let ok = loop {
        while let Ok(chunk) = rx.try_recv() {
            out.extend_from_slice(&chunk);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            },
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            },
        }
    };
    // The process is gone; take what its pipe still holds, then stop.
    let until = std::time::Instant::now() + DRAIN_GRACE;
    loop {
        let left = until.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(chunk) => out.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    ok.then(|| String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `CATALOG` is process-global and cargo runs tests in parallel: the
    /// tests that `install` take this so they can't reset each other
    /// mid-assertion (read-only tests elsewhere only see seeds or a codex/pi
    /// override, never a blanked list).
    static INSTALL_LOCK: Mutex<()> = Mutex::new(());

    const CODEX_FIXTURE: &str = r#"{"models":[
        {"slug":"gpt-reserve","display_name":"GPT-Reserve","visibility":"hide","priority":3},
        {"slug":"gpt-5.6-terra","visibility":"list","priority":7,"upgrade":null},
        {"slug":"gpt-6-astra","visibility":"list","priority":1},
        {"slug":"gpt-5.4-mini","visibility":"list","priority":23,
         "upgrade":{"model":"gpt-5.6-luna","retirement_at":"2026-08-31T19:00:00Z"}},
        {"slug":"gpt-5.6-sol","visibility":"list","priority":6}
    ]}"#;

    #[test]
    fn codex_catalog_keeps_listed_rows_in_priority_order() {
        assert_eq!(
            parse_codex_catalog(CODEX_FIXTURE),
            vec!["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra"]
        );
        assert!(parse_codex_catalog("not json").is_empty());
        assert!(parse_codex_catalog(r#"{"models":[]}"#).is_empty());
    }

    #[test]
    fn claude_models_are_aliases_plus_org_extras() {
        let home = r#"{"additionalModelOptionsCache":[
            {"value":"claude-fable-5-1[1m]","label":"Fable"},
            {"value":"opus","label":"dupe"},
            {"value":"  ","label":"blank"},
            {"label":"no value"}
        ]}"#;
        let mut want: Vec<String> =
            CLAUDE_ALIASES.iter().map(|s| s.to_string()).collect();
        want.push("claude-fable-5-1[1m]".into());
        assert_eq!(claude_models(Some(home)), want);
        // No file / garbage / no key: just the aliases.
        let aliases: Vec<String> =
            CLAUDE_ALIASES.iter().map(|s| s.to_string()).collect();
        assert_eq!(claude_models(None), aliases);
        assert_eq!(claude_models(Some("{{")), aliases);
        assert_eq!(claude_models(Some("{}")), aliases);
    }

    #[test]
    fn staleness_is_missing_or_version_moved_or_old() {
        let e = Entry {
            fetched_at: 1_000_000,
            version: "codex-cli 0.153.4".into(),
            models: vec!["x".into()],
        };
        let now = e.fetched_at + 60;
        assert!(needs_refresh(None, "codex-cli 0.153.4", now));
        assert!(!needs_refresh(Some(&e), "codex-cli 0.153.4", now));
        assert!(needs_refresh(Some(&e), "codex-cli 0.154.0", now));
        let old = e.fetched_at + TTL.as_secs() + 1;
        assert!(needs_refresh(Some(&e), "codex-cli 0.153.4", old));
        // A clock that went backwards is not "old".
        assert!(!needs_refresh(Some(&e), "codex-cli 0.153.4", 0));
    }

    #[test]
    fn fast_model_prefers_registry_pick_then_default() {
        let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        install(Cache::new());
        // The registry seeds all contain their preference.
        for a in agent::AGENTS {
            assert_eq!(
                fast_model(a),
                a.fast_model.map(|m| m.to_string()),
                "{}",
                a.id
            );
        }
        // Under a discovered list that dropped the preference, the first
        // listed model stands in.
        let codex = agent::by_id("codex").unwrap();
        let mut cache = Cache::new();
        cache.insert(
            "codex".into(),
            Entry {
                fetched_at: 1,
                version: "v".into(),
                models: vec!["gpt-9".into(), "gpt-8".into()],
            },
        );
        install(cache);
        assert_eq!(for_agent("codex"), vec!["gpt-9", "gpt-8"]);
        assert_eq!(default_model("codex"), "gpt-9");
        assert_eq!(fast_model(codex).as_deref(), Some("gpt-9"));
        assert!(is_discovered("codex"));
        // Other agents still answer from their seeds.
        assert!(!is_discovered("claude"));
        let claude = agent::by_id("claude").unwrap();
        assert_eq!(for_agent("claude").len(), claude.seed_models.len());
        // Back to seeds for the other tests in this process.
        install(Cache::new());
        assert_eq!(fast_model(codex).as_deref(), codex.fast_model);
    }

    #[test]
    fn empty_lists_never_replace_a_seed() {
        let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut cache = Cache::new();
        cache.insert("pi".into(), Entry::default());
        install(cache);
        assert!(!is_discovered("pi"));
        assert!(!for_agent("pi").is_empty());
        install(Cache::new());
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "muxterm-models-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models.json");
        let mut cache = Cache::new();
        cache.insert(
            "codex".into(),
            Entry {
                fetched_at: 42,
                version: "codex-cli 1".into(),
                models: vec!["a".into(), "b".into()],
            },
        );
        save_to(&path, &cache);
        assert_eq!(load_from(&path), cache);
        fs::write(&path, "garbage").unwrap();
        assert!(load_from(&path).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }
}
