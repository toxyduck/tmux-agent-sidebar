use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime};

use crate::extension::{self, Detail, ExtensionsConfig, Inspection, Reply};

/// A frame target is identified by collector data, never by a rendered label
/// or a row number. Duplicate agent/capability labels are therefore harmless.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeTarget {
    pub parent_pane_id: String,
    pub provider_id: String,
    /// Session identity comes from the inspected tmux pane. It scopes
    /// disclosure state so a recycled pane id cannot inherit old UI state.
    pub session_id: Option<String>,
    pub agent_id: String,
    pub node_id: String,
    pub detail_token: Option<String>,
    /// Collector text already present in the inspect reply. It opens inline
    /// without another provider RPC.
    pub inline_detail: Option<InlineDetail>,
    pub is_disclosure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineDetail {
    pub title: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct DetailView {
    pub title: String,
    pub source: String,
    pub text: String,
    pub scroll: usize,
    pub previous_selection: Option<TreeTarget>,
    pub previous_pane_scroll: usize,
}

#[derive(Debug, Default)]
pub struct CapabilityUiState {
    expanded: HashSet<DisclosureKey>,
    pub selected: Option<TreeTarget>,
    pub detail: Option<DetailView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DisclosureKey {
    provider_id: String,
    parent_pane_id: String,
    session_id: Option<String>,
    agent_id: String,
    node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PaneScopeKey {
    provider_id: String,
    parent_pane_id: String,
    session_id: Option<String>,
}

impl CapabilityUiState {
    fn key(target: &TreeTarget) -> DisclosureKey {
        DisclosureKey {
            provider_id: target.provider_id.clone(),
            parent_pane_id: target.parent_pane_id.clone(),
            session_id: target.session_id.clone(),
            agent_id: target.agent_id.clone(),
            node_id: target.node_id.clone(),
        }
    }

    pub fn is_expanded(&self, target: &TreeTarget) -> bool {
        self.expanded.contains(&Self::key(target))
    }

    pub fn toggle(&mut self, target: &TreeTarget) {
        let key = Self::key(target);
        if !self.expanded.insert(key.clone()) {
            self.expanded.remove(&key);
        }
        self.selected = Some(target.clone());
    }

    pub(crate) fn is_expanded_scope(
        &self,
        provider_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        agent_id: &str,
        node_id: &str,
    ) -> bool {
        self.expanded.contains(&DisclosureKey {
            provider_id: provider_id.to_string(),
            parent_pane_id: pane_id.to_string(),
            session_id: session_id.map(str::to_string),
            agent_id: agent_id.to_string(),
            node_id: node_id.to_string(),
        })
    }

    fn reconcile_scope(&mut self, key: &CacheKey, reply: &Reply) {
        let available: HashSet<(&str, &str)> = reply
            .tree
            .iter()
            .filter(|node| {
                !node.fact_ids.is_empty()
                    || reply.tree.iter().any(|candidate| {
                        candidate.agent_id == node.agent_id
                            && candidate.parent_id.as_deref() == Some(node.id.as_str())
                    })
            })
            .map(|node| (node.agent_id.as_str(), node.id.as_str()))
            .chain(
                reply
                    .builtins
                    .iter()
                    .map(|summary| (summary.agent_id.as_str(), "__builtins__")),
            )
            .collect();
        self.expanded.retain(|expanded| {
            expanded.provider_id != key.provider
                || expanded.parent_pane_id != key.pane_id
                || expanded.session_id != key.session_id
                || available.contains(&(expanded.agent_id.as_str(), expanded.node_id.as_str()))
        });
        self.invalidate_missing_transient_targets(key, reply);
    }

    fn prune_dead_scopes(&mut self, live_scopes: &HashSet<PaneScopeKey>) {
        self.expanded.retain(|expanded| {
            live_scopes.contains(&PaneScopeKey {
                provider_id: expanded.provider_id.clone(),
                parent_pane_id: expanded.parent_pane_id.clone(),
                session_id: expanded.session_id.clone(),
            })
        });
        if self
            .selected
            .as_ref()
            .is_some_and(|target| !live_scopes.contains(&PaneScopeKey::from_tree_target(target)))
        {
            self.selected = None;
        }
        if self.detail.as_ref().is_some_and(|detail| {
            detail.previous_selection.as_ref().is_some_and(|target| {
                !live_scopes.contains(&PaneScopeKey::from_tree_target(target))
            })
        }) {
            self.detail = None;
        }
    }

    fn prune_unconfigured_providers(&mut self, configured_providers: &HashSet<String>) {
        self.expanded
            .retain(|expanded| configured_providers.contains(&expanded.provider_id));
        if self
            .selected
            .as_ref()
            .is_some_and(|target| !configured_providers.contains(&target.provider_id))
        {
            self.selected = None;
        }
        if self.detail.as_ref().is_some_and(|detail| {
            detail
                .previous_selection
                .as_ref()
                .is_some_and(|target| !configured_providers.contains(&target.provider_id))
        }) {
            self.detail = None;
        }
    }

    fn invalidate_missing_transient_targets(&mut self, key: &CacheKey, reply: &Reply) {
        if self
            .selected
            .as_ref()
            .is_some_and(|target| target.matches_cache_key(key) && !target.exists_in(reply))
        {
            self.selected = None;
        }
        if self.detail.as_ref().is_some_and(|detail| {
            detail
                .previous_selection
                .as_ref()
                .is_some_and(|target| target.matches_cache_key(key) && !target.exists_in(reply))
        }) {
            self.detail = None;
        }
    }
}

impl PaneScopeKey {
    fn from_tree_target(target: &TreeTarget) -> Self {
        Self {
            provider_id: target.provider_id.clone(),
            parent_pane_id: target.parent_pane_id.clone(),
            session_id: target.session_id.clone(),
        }
    }
}

impl TreeTarget {
    fn matches_cache_key(&self, key: &CacheKey) -> bool {
        self.provider_id == key.provider
            && self.parent_pane_id == key.pane_id
            && self.session_id == key.session_id
    }

    fn exists_in(&self, reply: &Reply) -> bool {
        let agent_exists = reply.agents.iter().any(|agent| agent.id == self.agent_id);
        if !agent_exists {
            return false;
        }
        if self.node_id == "__builtins__" {
            return reply
                .builtins
                .iter()
                .any(|summary| summary.agent_id == self.agent_id);
        }
        if let Some(fact_id) = self.node_id.strip_prefix("fact:") {
            return reply
                .facts
                .iter()
                .any(|fact| fact.agent_id == self.agent_id && fact.id == fact_id);
        }
        if self.node_id.strip_prefix("slot:").is_some() {
            return reply.slots.contains_key(&self.node_id[5..]);
        }
        if self.node_id.starts_with("builtin:") {
            return reply.builtins.iter().any(|summary| {
                summary.agent_id == self.agent_id
                    && summary.items.iter().any(|item| {
                        self.node_id == format!("builtin:{}:{}", summary.agent_id, item.id)
                    })
            });
        }
        reply
            .tree
            .iter()
            .any(|node| node.agent_id == self.agent_id && node.id == self.node_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey {
    provider: String,
    pane_id: String,
    session_id: Option<String>,
}

impl CacheKey {
    fn new(provider: &str, pane_id: &str, session_id: Option<&str>) -> Self {
        Self {
            provider: provider.into(),
            pane_id: pane_id.into(),
            session_id: session_id.map(str::to_string),
        }
    }
}

enum WorkerRequest {
    Inspect {
        key: CacheKey,
        cwd: Option<String>,
        pane_pid: Option<u32>,
        current_command: Option<String>,
        config: ExtensionsConfig,
    },
    Detail {
        target: TreeTarget,
        session_id: Option<String>,
        cwd: Option<String>,
        previous_pane_scroll: usize,
        config: ExtensionsConfig,
    },
    Activate {
        target: crate::state::SubagentTarget,
        cwd: Option<String>,
        pane_pid: Option<u32>,
        current_command: Option<String>,
        config: ExtensionsConfig,
    },
}

pub(crate) enum WorkerResult {
    Inspect {
        key: CacheKey,
        result: Result<Reply, String>,
    },
    Detail {
        target: TreeTarget,
        previous_pane_scroll: usize,
        result: Result<Detail, String>,
    },
    Activate {
        result: Result<Reply, String>,
    },
}

pub struct ExtensionsState {
    pub config: Option<ExtensionsConfig>,
    pub config_error: Option<String>,
    config_path: PathBuf,
    config_mtime: Option<SystemTime>,
    cache: HashMap<CacheKey, Inspection>,
    in_flight: HashSet<CacheKey>,
    inspect_tx: SyncSender<WorkerRequest>,
    control_tx: SyncSender<WorkerRequest>,
    rx: Receiver<WorkerResult>,
    last_refresh: Instant,
    pub ui: CapabilityUiState,
}

impl ExtensionsState {
    pub fn load() -> Self {
        let config_path = crate::tmux::get_option(crate::tmux::SIDEBAR_EXTENSIONS_CONFIG)
            .map(PathBuf::from)
            .unwrap_or_else(extension::default_config_path);
        let (inspect_tx, inspect_rx) = mpsc::sync_channel(32);
        let (control_tx, control_rx) = mpsc::sync_channel(8);
        let (worker_tx, rx) = mpsc::channel();
        std::thread::spawn({
            let worker_tx = worker_tx.clone();
            move || worker_loop(inspect_rx, worker_tx)
        });
        std::thread::spawn(move || worker_loop(control_rx, worker_tx));
        let mut state = Self {
            config: None,
            config_error: None,
            config_path,
            config_mtime: None,
            cache: HashMap::new(),
            in_flight: HashSet::new(),
            inspect_tx,
            control_tx,
            rx,
            last_refresh: Instant::now() - Duration::from_secs(2),
            ui: CapabilityUiState::default(),
        };
        state.reload_config(true);
        state
    }

    pub fn inspection(
        &self,
        pane_id: &str,
        provider: &str,
        session_id: Option<&str>,
    ) -> Option<&Inspection> {
        self.cache
            .get(&CacheKey::new(provider, pane_id, session_id))
    }

    pub fn refresh(
        &mut self,
        panes: impl Iterator<
            Item = (
                String,
                String,
                Option<String>,
                Option<String>,
                Option<u32>,
                Option<String>,
            ),
        >,
    ) {
        self.reload_config(false);
        let panes: Vec<_> = panes.collect();
        let live_scopes: HashSet<_> = panes
            .iter()
            .map(|(pane_id, provider, session_id, ..)| PaneScopeKey {
                provider_id: provider.clone(),
                parent_pane_id: pane_id.clone(),
                session_id: session_id.clone(),
            })
            .collect();
        self.ui.prune_dead_scopes(&live_scopes);
        if self.last_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_refresh = Instant::now();
        let Some(config) = self.config.clone() else {
            return;
        };
        for (pane_id, provider, session_id, cwd, pane_pid, current_command) in panes {
            let key = CacheKey::new(&provider, &pane_id, session_id.as_deref());
            if self.in_flight.contains(&key) {
                continue;
            }
            match self.inspect_tx.try_send(WorkerRequest::Inspect {
                key: key.clone(),
                cwd,
                pane_pid,
                current_command,
                config: config.clone(),
            }) {
                Ok(()) => {
                    self.in_flight.insert(key);
                }
                Err(TrySendError::Full(_)) => self.set_notice("collector queue is full"),
                Err(TrySendError::Disconnected(_)) => self.set_notice("collector worker stopped"),
            }
        }
    }

    pub fn queue_detail(
        &mut self,
        target: TreeTarget,
        session_id: Option<String>,
        cwd: Option<String>,
        previous_pane_scroll: usize,
    ) -> Result<(), String> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| "no provider collector configured".to_string())?;
        self.control_tx
            .try_send(WorkerRequest::Detail {
                target,
                session_id,
                cwd,
                previous_pane_scroll,
                config,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "collector queue is full".to_string(),
                TrySendError::Disconnected(_) => "collector worker stopped".to_string(),
            })
    }

    pub fn queue_activate(
        &mut self,
        target: crate::state::SubagentTarget,
        cwd: Option<String>,
        pane_pid: Option<u32>,
        current_command: Option<String>,
    ) -> Result<(), String> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| "no provider collector configured".to_string())?;
        self.control_tx
            .try_send(WorkerRequest::Activate {
                target,
                cwd,
                pane_pid,
                current_command,
                config,
            })
            .map_err(|error| match error {
                TrySendError::Full(_) => "collector queue is full".to_string(),
                TrySendError::Disconnected(_) => "collector worker stopped".to_string(),
            })
    }

    pub(crate) fn take_results(&mut self) -> Vec<WorkerResult> {
        let mut results = Vec::new();
        while let Ok(result) = self.rx.try_recv() {
            if let WorkerResult::Inspect { key, result } = &result {
                self.in_flight.remove(key);
                match result {
                    Ok(reply) => {
                        self.ui.reconcile_scope(key, reply);
                        self.cache.insert(
                            key.clone(),
                            Inspection {
                                reply: reply.clone(),
                                stale: false,
                            },
                        );
                    }
                    Err(error) => {
                        if let Some(previous) = self.cache.get_mut(key) {
                            previous.stale = true;
                        } else {
                            self.cache.insert(
                                key.clone(),
                                Inspection {
                                    reply: Reply {
                                        version: extension::PROTOCOL_VERSION,
                                        error: Some(error.clone()),
                                        ..Reply::default()
                                    },
                                    stale: false,
                                },
                            );
                        }
                    }
                }
            }
            results.push(result);
        }
        results
    }

    pub(crate) fn accepts_detail(&self, target: &TreeTarget) -> bool {
        self.ui.selected.as_ref() == Some(target)
            && self
                .inspection(
                    &target.parent_pane_id,
                    &target.provider_id,
                    target.session_id.as_deref(),
                )
                .is_some_and(|inspection| !inspection.stale && target.exists_in(&inspection.reply))
    }

    fn reload_config(&mut self, force: bool) {
        let mtime = std::fs::metadata(&self.config_path)
            .and_then(|meta| meta.modified())
            .ok();
        if !force && mtime == self.config_mtime {
            return;
        }
        self.config_mtime = mtime;
        match extension::load_config(&self.config_path) {
            Ok(config) => {
                self.ui
                    .prune_unconfigured_providers(&config.providers.keys().cloned().collect());
                self.config = Some(config);
                self.config_error = None;
                self.cache.clear();
                self.in_flight.clear();
            }
            Err(error) => {
                self.config = None;
                self.cache.clear();
                self.in_flight.clear();
                if self.config_path.exists() {
                    self.set_notice(error);
                } else {
                    self.config_error = None;
                }
            }
        }
    }

    fn set_notice(&mut self, error: impl Into<String>) {
        self.config_error = Some(format!("extensions: {}", error.into()));
    }
}

fn worker_loop(rx: Receiver<WorkerRequest>, tx: mpsc::Sender<WorkerResult>) {
    while let Ok(request) = rx.recv() {
        let result = match request {
            WorkerRequest::Inspect {
                key,
                cwd,
                pane_pid,
                current_command,
                config,
            } => WorkerResult::Inspect {
                result: extension::inspect_once(
                    &config,
                    &key.provider,
                    &key.pane_id,
                    cwd.as_deref(),
                    key.session_id.as_deref(),
                    pane_pid,
                    current_command.as_deref(),
                ),
                key,
            },
            WorkerRequest::Detail {
                target,
                session_id,
                cwd,
                previous_pane_scroll,
                config,
            } => WorkerResult::Detail {
                result: target.detail_token.as_deref().map_or_else(
                    || Err("selected capability has no detail".to_string()),
                    |token| {
                        extension::detail(
                            &config,
                            &target.provider_id,
                            &target.parent_pane_id,
                            cwd.as_deref(),
                            session_id.as_deref(),
                            token,
                        )
                    },
                ),
                target,
                previous_pane_scroll,
            },
            WorkerRequest::Activate {
                target,
                cwd,
                pane_pid,
                current_command,
                config,
            } => WorkerResult::Activate {
                result: extension::activate(
                    &config,
                    extension::ActivationRequest {
                        provider: &target.provider_id,
                        pane_id: &target.parent_pane_id,
                        session_id: target.session_id.as_deref(),
                        subagent_id: &target.agent_id,
                        cwd: cwd.as_deref(),
                        pane_pid,
                        current_command: current_command.as_deref(),
                    },
                ),
            },
        };
        if tx.send(result).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_extensions_state(
        config_path: PathBuf,
    ) -> (ExtensionsState, mpsc::Sender<WorkerResult>) {
        let (inspect_tx, _inspect_rx) = mpsc::sync_channel(1);
        let (control_tx, _control_rx) = mpsc::sync_channel(1);
        let (result_tx, rx) = mpsc::channel();
        (
            ExtensionsState {
                config: None,
                config_error: None,
                config_path,
                config_mtime: None,
                cache: HashMap::new(),
                in_flight: HashSet::new(),
                inspect_tx,
                control_tx,
                rx,
                last_refresh: Instant::now(),
                ui: CapabilityUiState::default(),
            },
            result_tx,
        )
    }

    fn config_for(provider: &str) -> String {
        format!(r#"{{"version":1,"providers":{{"{provider}":{{"argv":["/bin/true"]}}}}}}"#)
    }

    fn disclosure_target(
        provider_id: &str,
        pane_id: &str,
        session_id: Option<&str>,
        agent_id: &str,
        node_id: &str,
    ) -> TreeTarget {
        TreeTarget {
            parent_pane_id: pane_id.into(),
            provider_id: provider_id.into(),
            session_id: session_id.map(str::to_string),
            agent_id: agent_id.into(),
            node_id: node_id.into(),
            detail_token: None,
            inline_detail: None,
            is_disclosure: true,
        }
    }

    fn reply_with_target(target: &TreeTarget) -> Reply {
        Reply {
            version: 1,
            agents: vec![crate::extension::AgentNode {
                id: target.agent_id.clone(),
                parent_id: None,
                label: "Agent".into(),
                role: crate::extension::AgentRole::Main,
                model: None,
            }],
            tree: vec![crate::extension::TreeNode {
                id: target.node_id.clone(),
                agent_id: target.agent_id.clone(),
                parent_id: None,
                label: "Skills".into(),
                fact_ids: vec!["skill".into()],
                detail_token: None,
                category: "skills".into(),
            }],
            ..Reply::default()
        }
    }

    #[test]
    fn cache_key_keeps_session_snapshots_separate() {
        let first = CacheKey::new("claude", "%1", Some("one"));
        let second = CacheKey::new("claude", "%1", Some("two"));
        assert_ne!(first, second);
    }

    #[test]
    fn disclosure_survives_switching_between_live_panes() {
        let first = disclosure_target("claude", "%1", Some("one"), "main", "skills");
        let second = disclosure_target("claude", "%2", Some("two"), "main", "skills");
        let mut ui = CapabilityUiState::default();

        ui.toggle(&first);
        assert!(ui.is_expanded(&first));
        ui.toggle(&second);
        assert!(ui.is_expanded(&second));
        assert!(ui.is_expanded(&first));
    }

    #[test]
    fn disclosure_key_separates_provider_pane_session_and_agent() {
        let targets = [
            disclosure_target("claude", "%1", None, "main", "skills"),
            disclosure_target("codex", "%1", None, "main", "skills"),
            disclosure_target("claude", "%2", None, "main", "skills"),
            disclosure_target("claude", "%1", Some("one"), "main", "skills"),
            disclosure_target("claude", "%1", None, "subagent", "skills"),
        ];
        let mut ui = CapabilityUiState::default();
        for target in &targets {
            ui.toggle(target);
        }
        assert!(targets.iter().all(|target| ui.is_expanded(target)));

        ui.toggle(&targets[0]);
        assert!(!ui.is_expanded(&targets[0]));
        assert!(targets[1..].iter().all(|target| ui.is_expanded(target)));
    }

    #[test]
    fn replaced_session_drops_old_disclosure_from_live_snapshot() {
        let old = disclosure_target("claude", "%1", Some("old"), "main", "skills");
        let new = disclosure_target("claude", "%1", Some("new"), "main", "skills");
        let mut ui = CapabilityUiState::default();
        ui.toggle(&old);
        ui.prune_dead_scopes(&HashSet::from([PaneScopeKey {
            provider_id: "claude".into(),
            parent_pane_id: "%1".into(),
            session_id: Some("new".into()),
        }]));

        assert!(!ui.is_expanded(&old));
        assert!(!ui.is_expanded(&new));
    }

    #[test]
    fn none_session_scope_survives_live_snapshot_pruning() {
        let target = disclosure_target("claude", "%1", None, "main", "skills");
        let mut ui = CapabilityUiState::default();
        ui.toggle(&target);
        ui.prune_dead_scopes(&HashSet::from([PaneScopeKey {
            provider_id: "claude".into(),
            parent_pane_id: "%1".into(),
            session_id: None,
        }]));

        assert!(ui.is_expanded(&target));
    }

    #[test]
    fn successful_inspect_prunes_only_missing_nodes_in_its_exact_scope() {
        let retained = disclosure_target("claude", "%1", Some("one"), "main", "skills");
        let removed = disclosure_target("claude", "%1", Some("one"), "main", "tools");
        let other_pane = disclosure_target("claude", "%2", Some("one"), "main", "tools");
        let mut ui = CapabilityUiState::default();
        ui.toggle(&retained);
        ui.toggle(&removed);
        ui.toggle(&other_pane);
        ui.reconcile_scope(
            &CacheKey::new("claude", "%1", Some("one")),
            &Reply {
                version: 1,
                tree: vec![crate::extension::TreeNode {
                    id: "skills".into(),
                    agent_id: "main".into(),
                    parent_id: None,
                    label: "Skills".into(),
                    fact_ids: vec!["skill".into()],
                    detail_token: None,
                    category: "skills".into(),
                }],
                ..Reply::default()
            },
        );

        assert!(ui.is_expanded(&retained));
        assert!(!ui.is_expanded(&removed));
        assert!(ui.is_expanded(&other_pane));
    }

    #[test]
    fn disclosure_becoming_a_leaf_is_pruned() {
        let target = disclosure_target("claude", "%1", None, "main", "skills");
        let mut ui = CapabilityUiState::default();
        ui.toggle(&target);
        ui.reconcile_scope(
            &CacheKey::new("claude", "%1", None),
            &Reply {
                version: 1,
                tree: vec![crate::extension::TreeNode {
                    id: "skills".into(),
                    agent_id: "main".into(),
                    parent_id: None,
                    label: "Skills".into(),
                    fact_ids: vec![],
                    detail_token: None,
                    category: "skills".into(),
                }],
                ..Reply::default()
            },
        );

        assert!(!ui.is_expanded(&target));
    }

    #[test]
    fn successful_inspect_invalidates_missing_selection_and_closes_detail() {
        let missing = disclosure_target("claude", "%1", Some("one"), "main", "skills");
        let mut ui = CapabilityUiState {
            selected: Some(missing.clone()),
            detail: Some(DetailView {
                title: "Skill".into(),
                source: String::new(),
                text: String::new(),
                scroll: 0,
                previous_selection: Some(missing),
                previous_pane_scroll: 0,
            }),
            ..CapabilityUiState::default()
        };
        ui.reconcile_scope(
            &CacheKey::new("claude", "%1", Some("one")),
            &Reply {
                version: 1,
                agents: vec![crate::extension::AgentNode {
                    id: "main".into(),
                    parent_id: None,
                    label: "Ada".into(),
                    role: crate::extension::AgentRole::Main,
                    model: None,
                }],
                ..Reply::default()
            },
        );

        assert!(ui.selected.is_none());
        assert!(ui.detail.is_none());
    }

    #[test]
    fn detail_from_reused_pane_session_is_rejected() {
        let (mut state, _result_tx) = test_extensions_state(PathBuf::new());
        let old = disclosure_target("claude", "%1", Some("old"), "main", "skills");
        let new = disclosure_target("claude", "%1", Some("new"), "main", "skills");
        state.ui.selected = Some(old.clone());
        state.cache.insert(
            CacheKey::new("claude", "%1", Some("new")),
            Inspection {
                reply: reply_with_target(&new),
                stale: false,
            },
        );
        state.ui.prune_dead_scopes(&HashSet::from([PaneScopeKey {
            provider_id: "claude".into(),
            parent_pane_id: "%1".into(),
            session_id: Some("new".into()),
        }]));

        assert!(state.ui.selected.is_none());
        assert!(!state.accepts_detail(&old));
    }

    #[test]
    fn detail_is_rejected_after_newer_inspect_removes_target() {
        let (mut state, _result_tx) = test_extensions_state(PathBuf::new());
        let target = disclosure_target("claude", "%1", Some("session"), "main", "skills");
        state.ui.selected = Some(target.clone());
        state.cache.insert(
            CacheKey::new("claude", "%1", Some("session")),
            Inspection {
                reply: Reply {
                    version: 1,
                    agents: vec![crate::extension::AgentNode {
                        id: "main".into(),
                        parent_id: None,
                        label: "Agent".into(),
                        role: crate::extension::AgentRole::Main,
                        model: None,
                    }],
                    ..Reply::default()
                },
                stale: false,
            },
        );

        assert!(!state.accepts_detail(&target));
    }

    #[test]
    fn detail_requires_exact_selected_target_and_fresh_inspection() {
        let (mut state, _result_tx) = test_extensions_state(PathBuf::new());
        let target = disclosure_target("claude", "%1", Some("session"), "main", "skills");
        let key = CacheKey::new("claude", "%1", Some("session"));
        state.ui.selected = Some(target.clone());
        state.cache.insert(
            key.clone(),
            Inspection {
                reply: reply_with_target(&target),
                stale: false,
            },
        );
        assert!(state.accepts_detail(&target));

        state.cache.get_mut(&key).unwrap().stale = true;
        assert!(!state.accepts_detail(&target));
    }

    #[test]
    fn inspect_error_keeps_stale_scope_disclosure_and_selection() {
        let (mut state, result_tx) = test_extensions_state(PathBuf::new());
        let target = disclosure_target("claude", "%1", None, "main", "skills");
        let key = CacheKey::new("claude", "%1", None);
        state.ui.toggle(&target);
        state.cache.insert(
            key.clone(),
            Inspection {
                reply: Reply {
                    version: 1,
                    ..Reply::default()
                },
                stale: false,
            },
        );
        result_tx
            .send(WorkerResult::Inspect {
                key: key.clone(),
                result: Err("collector unavailable".into()),
            })
            .unwrap();

        state.take_results();
        assert!(state.cache.get(&key).unwrap().stale);
        assert!(state.ui.is_expanded(&target));
        assert_eq!(state.ui.selected, Some(target));
    }

    #[test]
    fn successful_config_provider_removal_prunes_only_removed_provider_scope() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extensions.json");
        let (mut state, _result_tx) = test_extensions_state(path.clone());
        let claude = disclosure_target("claude", "%1", None, "main", "skills");
        let codex = disclosure_target("codex", "%1", None, "main", "skills");
        state.ui.toggle(&claude);
        state.ui.toggle(&codex);

        std::fs::write(&path, config_for("claude")).unwrap();
        state.reload_config(true);
        assert!(state.ui.is_expanded(&claude));
        assert!(!state.ui.is_expanded(&codex));

        std::fs::write(&path, config_for("codex")).unwrap();
        state.reload_config(true);
        assert!(!state.ui.is_expanded(&claude));
    }

    #[test]
    fn unchanged_or_invalid_config_preserves_existing_scope_disclosure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("extensions.json");
        let (mut state, _result_tx) = test_extensions_state(path.clone());
        let target = disclosure_target("claude", "%1", None, "main", "skills");

        std::fs::write(&path, config_for("claude")).unwrap();
        state.reload_config(true);
        state.ui.toggle(&target);
        state.reload_config(false);
        assert!(state.ui.is_expanded(&target));

        std::fs::write(&path, "not json").unwrap();
        state.reload_config(true);
        assert!(state.ui.is_expanded(&target));
    }
}
