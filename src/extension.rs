//! Versioned, external capability collectors.
//!
//! The sidebar intentionally knows no provider-specific transcript format.
//! Collectors receive a small request on stdin and return a bounded JSON reply.

use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 750;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtensionsConfig {
    pub version: u32,
    pub hidden_rows: Vec<String>,
    pub row_order: Vec<String>,
    pub slots: Vec<SlotConfig>,
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotConfig {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Executable plus arguments. It is never evaluated by a shell.
    pub argv: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Debug, Clone, Serialize)]
pub struct Request<'a> {
    pub version: u32,
    pub op: &'a str,
    pub provider: &'a str,
    pub pane_id: &'a str,
    /// Canonical current directory reported by tmux for this pane. The
    /// collector must not infer it from its own process working directory.
    pub cwd: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub subagent_id: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Observed,
    Available,
    Inferred,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Fact {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub detail: String,
    pub evidence: Evidence,
    #[serde(default)]
    pub source: String,
    /// Opaque provider-owned token accepted only by the `detail` operation.
    /// It is deliberately not interpreted as a filesystem path by the UI.
    #[serde(default)]
    pub detail_token: Option<String>,
}

/// Provider-neutral identity. `id` is stable within `(provider, session_id)`;
/// UI code must never fall back to a row number or a rendered label.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentNode {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub label: String,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TreeNode {
    pub id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub label: String,
    #[serde(default)]
    pub fact_ids: Vec<String>,
    /// Optional provider-owned token for details attached directly to this node.
    #[serde(default)]
    pub detail_token: Option<String>,
}

/// Complete text returned by a `detail` request. It is rendered inline by the
/// sidebar and never turned into a popup or a second tmux pane.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Detail {
    pub title: String,
    #[serde(default)]
    pub source: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub facts: Vec<Fact>,
    pub builtins: Vec<Fact>,
    pub slots: BTreeMap<String, String>,
    pub error: Option<String>,
    pub agents: Vec<AgentNode>,
    pub tree: Vec<TreeNode>,
    pub detail: Option<Detail>,
}

#[derive(Debug, Clone)]
pub struct Inspection {
    pub reply: Reply,
    pub stale: bool,
}

pub fn default_config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
        .join("tmux-agent-sidebar/extensions.json")
}

pub fn load_config(path: &Path) -> Result<ExtensionsConfig, String> {
    let expanded_path = PathBuf::from(expand_home(&path.to_string_lossy()));
    let text = std::fs::read_to_string(&expanded_path)
        .map_err(|error| format!("cannot read {}: {error}", expanded_path.display()))?;
    let config: ExtensionsConfig = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", expanded_path.display()))?;
    if config.version != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported extensions config version {}",
            config.version
        ));
    }
    for provider in config.providers.values() {
        if provider.argv.is_empty() || provider.argv[0].is_empty() {
            return Err("provider argv must contain an executable".into());
        }
        validate_executable(&provider.argv[0])?;
    }
    let mut slot_ids = std::collections::HashSet::new();
    for slot in &config.slots {
        if !is_safe_text(&slot.id) || !is_safe_text(&slot.title) || !slot_ids.insert(&slot.id) {
            return Err("invalid or duplicate slot id".into());
        }
    }
    Ok(config)
}

pub fn inspect(
    config: &ExtensionsConfig,
    provider: &str,
    pane_id: &str,
    session_id: Option<&str>,
    cache: &mut HashMap<String, Inspection>,
) -> Inspection {
    let key = inspection_cache_key(provider, pane_id, session_id);
    let Some(program) = config.providers.get(provider) else {
        return cache
            .get(&key)
            .cloned()
            .unwrap_or_else(|| unknown("no collector configured"));
    };
    let request = Request {
        version: PROTOCOL_VERSION,
        op: "inspect",
        provider,
        pane_id,
        cwd: None,
        session_id,
        subagent_id: None,
    };
    match call(program, &request) {
        Ok(reply) => {
            let inspection = Inspection {
                reply,
                stale: false,
            };
            cache.insert(key, inspection.clone());
            inspection
        }
        Err(error) => cache.get(&key).cloned().map_or_else(
            || unknown(&error),
            |mut previous| {
                previous.stale = true;
                previous
            },
        ),
    }
}

fn inspection_cache_key(provider: &str, pane_id: &str, session_id: Option<&str>) -> String {
    format!("{provider}:{pane_id}:{}", session_id.unwrap_or("<none>"))
}

/// One bounded collector request. This is called only from the extension
/// worker; UI refresh/render paths consume its cache instead of spawning.
pub fn inspect_once(
    config: &ExtensionsConfig,
    provider: &str,
    pane_id: &str,
    cwd: Option<&str>,
    session_id: Option<&str>,
) -> Result<Reply, String> {
    let program = config
        .providers
        .get(provider)
        .ok_or_else(|| "no collector configured".to_string())?;
    call(
        program,
        &Request {
            version: PROTOCOL_VERSION,
            op: "inspect",
            provider,
            pane_id,
            cwd,
            session_id,
            subagent_id: None,
        },
    )
}

/// Uniform activation for Claude, Codex and OpenCode.
/// A collector may automate its native viewer; otherwise it must return an
/// error. The sidebar always focuses the original pane before showing it.
pub fn activate(
    config: &ExtensionsConfig,
    provider: &str,
    pane_id: &str,
    session_id: Option<&str>,
    subagent_id: &str,
) -> Result<Reply, String> {
    let program = config
        .providers
        .get(provider)
        .ok_or_else(|| "no collector configured".to_string())?;
    call(
        program,
        &Request {
            version: PROTOCOL_VERSION,
            op: "activate",
            provider,
            pane_id,
            cwd: None,
            session_id,
            subagent_id: Some(subagent_id),
        },
    )
}

/// Load the text for one selected capability. `detail_token` must have come
/// from the collector's own inspect reply, so rendered labels can never cause
/// an arbitrary local file to be read.
pub fn detail(
    config: &ExtensionsConfig,
    provider: &str,
    pane_id: &str,
    session_id: Option<&str>,
    detail_token: &str,
) -> Result<Detail, String> {
    let program = config
        .providers
        .get(provider)
        .ok_or_else(|| "no collector configured".to_string())?;
    let reply = call(
        program,
        &Request {
            version: PROTOCOL_VERSION,
            op: "detail",
            provider,
            pane_id,
            cwd: None,
            session_id,
            subagent_id: Some(detail_token),
        },
    )?;
    reply
        .detail
        .ok_or_else(|| "collector returned no detail".to_string())
}

fn unknown(error: &str) -> Inspection {
    Inspection {
        reply: Reply {
            version: PROTOCOL_VERSION,
            error: Some(error.to_string()),
            ..Reply::default()
        },
        stale: false,
    }
}

fn call(provider: &ProviderConfig, request: &Request<'_>) -> Result<Reply, String> {
    let argv: Vec<String> = provider
        .argv
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            if index == 0 {
                expand_executable(arg)
            } else {
                arg.to_string()
            }
        })
        .collect();
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .env_clear();
    for key in ["HOME", "PATH", "TERM", "XDG_CONFIG_HOME", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // A collector may start descendants. Isolating its process group lets
        // timeout terminate the whole request rather than leaving a child.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start collector: {error}"))?;
    let body = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    child
        .stdin
        .as_mut()
        .ok_or("collector stdin unavailable")?
        .write_all(&body)
        .map_err(|error| error.to_string())?;
    drop(child.stdin.take());
    let stdout = child.stdout.take().ok_or("collector stdout unavailable")?;
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut bytes = Vec::with_capacity(MAX_OUTPUT_BYTES.min(8192));
        stdout
            .take((MAX_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_millis(provider.timeout_ms.max(1));
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            let _ = child.wait();
            let _ = reader.join();
            return Err("collector timed out".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let stdout = reader
        .join()
        .map_err(|_| "collector stdout reader panicked".to_string())?
        .map_err(|error| error.to_string())?;
    if stdout.len() > MAX_OUTPUT_BYTES {
        return Err("collector output exceeds 1 MiB".into());
    }
    if !status.success() {
        return Err(format!("collector exited with {status}"));
    }
    let reply: Reply = serde_json::from_slice(&stdout)
        .map_err(|error| format!("invalid collector reply: {error}"))?;
    if reply.version != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported collector reply version {}",
            reply.version
        ));
    }
    validate_reply(&reply)?;
    Ok(reply)
}

fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn validate_reply(reply: &Reply) -> Result<(), String> {
    const MAX_TEXT: usize = 4096;
    let valid = |value: &str| !value.is_empty() && value.len() <= MAX_TEXT && is_safe_text(value);
    let mut ids = std::collections::HashSet::new();
    let mut fact_ids = std::collections::HashSet::new();
    for fact in reply.facts.iter().chain(&reply.builtins) {
        if !valid(&fact.id)
            || !valid(&fact.label)
            || !fact.detail.is_empty() && (!valid(&fact.detail))
            || !fact.source.is_empty() && (!valid(&fact.source))
            || !fact_ids.insert(fact.id.as_str())
        {
            return Err("invalid or duplicate fact id".into());
        }
        if let Some(token) = &fact.detail_token
            && !valid(token)
        {
            return Err("invalid detail token".into());
        }
    }
    for node in &reply.agents {
        if !valid(&node.id) || !valid(&node.label) || !ids.insert(node.id.as_str()) {
            return Err("invalid or duplicate agent id".into());
        }
    }
    let parents: std::collections::HashMap<&str, Option<&str>> = reply
        .agents
        .iter()
        .map(|node| (node.id.as_str(), node.parent_id.as_deref()))
        .collect();
    for node in &reply.agents {
        let mut seen = std::collections::HashSet::new();
        let mut current = Some(node.id.as_str());
        while let Some(id) = current {
            if !seen.insert(id) {
                return Err("agent tree cycle".into());
            }
            current = parents.get(id).copied().flatten();
        }
    }
    let agent_ids: std::collections::HashSet<&str> =
        reply.agents.iter().map(|node| node.id.as_str()).collect();
    for node in &reply.agents {
        if let Some(parent) = &node.parent_id
            && (!agent_ids.contains(parent.as_str()) || parent == &node.id)
        {
            return Err("invalid agent parent".into());
        }
    }
    for node in &reply.tree {
        if !valid(&node.id) || !valid(&node.label) || !ids.insert(node.id.as_str()) {
            return Err("invalid or duplicate tree id".into());
        }
        if let Some(token) = &node.detail_token
            && !valid(token)
        {
            return Err("invalid detail token".into());
        }
    }
    for (slot_id, value) in &reply.slots {
        if !valid(slot_id) || !valid(value) {
            return Err("invalid slot value".into());
        }
    }
    let tree_parents: std::collections::HashMap<&str, Option<&str>> = reply
        .tree
        .iter()
        .map(|node| (node.id.as_str(), node.parent_id.as_deref()))
        .collect();
    for node in &reply.tree {
        let mut seen = std::collections::HashSet::new();
        let mut current = Some(node.id.as_str());
        while let Some(id) = current {
            if !seen.insert(id) {
                return Err("capability tree cycle".into());
            }
            current = tree_parents.get(id).copied().flatten();
        }
        if let Some(parent) = &node.parent_id
            && !tree_parents.contains_key(parent.as_str())
        {
            return Err("invalid capability tree parent".into());
        }
    }
    if let Some(detail) = &reply.detail
        && (!valid(&detail.title)
            || detail.text.len() > MAX_OUTPUT_BYTES
            || detail
                .text
                .chars()
                .any(|ch| ch != '\n' && ch != '\t' && ch.is_control())
            || (!detail.source.is_empty() && !valid(&detail.source)))
    {
        return Err("invalid detail reply".into());
    }
    Ok(())
}

fn validate_executable(value: &str) -> Result<(), String> {
    if std::path::Path::new(value).is_absolute() || value.starts_with("~/") {
        Ok(())
    } else {
        Err("collector executable must be absolute or start with '~/'".into())
    }
}

fn is_safe_text(value: &str) -> bool {
    value.len() <= 4096 && !value.chars().any(char::is_control)
}

fn expand_executable(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home)
            .join(rest)
            .to_string_lossy()
            .into_owned();
    }
    value.to_string()
}

fn expand_home(value: &str) -> String {
    std::env::var_os("HOME").map_or_else(
        || value.to_string(),
        |home| {
            let home = home.to_string_lossy();
            value.replace("$HOME", &home).replace("${HOME}", &home)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn config_requires_current_protocol_version() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("extensions.json");
        std::fs::write(&path, r#"{"version":2}"#).unwrap();
        assert!(load_config(&path).unwrap_err().contains("unsupported"));
    }

    #[test]
    fn config_rejects_shell_like_empty_argv() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("extensions.json");
        std::fs::write(&path, r#"{"version":1,"providers":{"claude":{"argv":[]}}}"#).unwrap();
        assert!(load_config(&path).unwrap_err().contains("argv"));
    }

    #[test]
    fn config_rejects_path_search_for_collector_executable() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("extensions.json");
        std::fs::write(
            &path,
            r#"{"version":1,"providers":{"claude":{"argv":["collector"]}}}"#,
        )
        .unwrap();
        assert!(load_config(&path).unwrap_err().contains("absolute"));
    }

    #[cfg(unix)]
    #[test]
    fn fake_provider_reply_is_bounded_and_validated() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let script = dir.path().join("collector");
        std::fs::write(
            &script,
            "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in *'\"cwd\":\"/tmp/project\"'*) ;; *) exit 7 ;; esac\nprintf '%s' '{\"version\":1,\"facts\":[{\"id\":\"model\",\"label\":\"model\",\"evidence\":\"observed\"}]}'\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        let config = ExtensionsConfig {
            version: 1,
            providers: BTreeMap::from([(
                "claude".into(),
                ProviderConfig {
                    argv: vec![script.to_string_lossy().into_owned()],
                    timeout_ms: 100,
                },
            )]),
            ..ExtensionsConfig::default()
        };
        let reply = inspect_once(
            &config,
            "claude",
            "%1",
            Some("/tmp/project"),
            Some("session"),
        )
        .unwrap();
        assert_eq!(reply.facts[0].id, "model");
    }

    #[test]
    fn stale_cache_is_preserved_when_collector_fails() {
        let config = ExtensionsConfig {
            version: 1,
            providers: BTreeMap::from([(
                "claude".into(),
                ProviderConfig {
                    argv: vec!["definitely-not-a-command".into()],
                    timeout_ms: 1,
                },
            )]),
            ..ExtensionsConfig::default()
        };
        let mut cache = HashMap::from([(
            inspection_cache_key("claude", "%1", None),
            Inspection {
                reply: Reply {
                    version: 1,
                    facts: vec![Fact {
                        id: "model".into(),
                        label: "model: test".into(),
                        detail: String::new(),
                        evidence: Evidence::Observed,
                        source: String::new(),
                        detail_token: None,
                    }],
                    ..Reply::default()
                },
                stale: false,
            },
        )]);
        let result = inspect(&config, "claude", "%1", None, &mut cache);
        assert!(result.stale);
        assert_eq!(result.reply.facts[0].label, "model: test");
    }

    #[test]
    fn stale_cache_is_not_reused_after_session_change() {
        let config = ExtensionsConfig {
            version: 1,
            providers: BTreeMap::from([(
                "claude".into(),
                ProviderConfig {
                    argv: vec!["/definitely-not-a-command".into()],
                    timeout_ms: 1,
                },
            )]),
            ..ExtensionsConfig::default()
        };
        let mut cache = HashMap::from([(
            inspection_cache_key("claude", "%1", Some("old")),
            Inspection {
                reply: Reply {
                    version: 1,
                    facts: vec![Fact {
                        id: "old-model".into(),
                        label: "old".into(),
                        detail: String::new(),
                        evidence: Evidence::Observed,
                        source: String::new(),
                        detail_token: None,
                    }],
                    ..Reply::default()
                },
                stale: false,
            },
        )]);
        let result = inspect(&config, "claude", "%1", Some("new"), &mut cache);
        assert!(!result.stale);
        assert!(result.reply.facts.is_empty());
    }

    #[test]
    fn argv_expands_home_without_a_shell() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_home("$HOME/bin/collector"),
            format!("{home}/bin/collector")
        );
    }

    #[test]
    fn reply_rejects_duplicate_stable_node_ids() {
        let mut reply = Reply {
            version: 1,
            ..Reply::default()
        };
        reply.agents = vec![
            AgentNode {
                id: "a".into(),
                parent_id: None,
                label: "a".into(),
                model: None,
            },
            AgentNode {
                id: "a".into(),
                parent_id: None,
                label: "b".into(),
                model: None,
            },
        ];
        assert!(validate_reply(&reply).unwrap_err().contains("duplicate"));
    }

    #[test]
    fn reply_rejects_capability_tree_cycle() {
        let reply = Reply {
            version: 1,
            tree: vec![
                TreeNode {
                    id: "a".into(),
                    parent_id: Some("b".into()),
                    label: "A".into(),
                    fact_ids: vec![],
                    detail_token: None,
                },
                TreeNode {
                    id: "b".into(),
                    parent_id: Some("a".into()),
                    label: "B".into(),
                    fact_ids: vec![],
                    detail_token: None,
                },
            ],
            ..Reply::default()
        };
        assert!(validate_reply(&reply).unwrap_err().contains("cycle"));
    }
}
