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
    pub agent_id: String,
    pub node_id: String,
    pub detail_token: Option<String>,
    pub is_disclosure: bool,
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
    pub expanded: HashSet<String>,
    pub selected: Option<TreeTarget>,
    pub detail: Option<DetailView>,
}

impl CapabilityUiState {
    pub fn key(pane_id: &str, node_id: &str) -> String {
        format!("{pane_id}:{node_id}")
    }

    pub fn is_expanded(&self, pane_id: &str, node_id: &str) -> bool {
        self.expanded.contains(&Self::key(pane_id, node_id))
    }

    pub fn toggle(&mut self, target: &TreeTarget) {
        let key = Self::key(&target.parent_pane_id, &target.node_id);
        if !self.expanded.insert(key.clone()) {
            self.expanded.remove(&key);
        }
        self.selected = Some(target.clone());
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
        session_id: Option<String>,
        cwd: Option<String>,
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
        panes: impl Iterator<Item = (String, String, Option<String>, Option<String>)>,
    ) {
        self.reload_config(false);
        if self.last_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_refresh = Instant::now();
        let Some(config) = self.config.clone() else {
            return;
        };
        for (pane_id, provider, session_id, cwd) in panes {
            let key = CacheKey::new(&provider, &pane_id, session_id.as_deref());
            if self.in_flight.contains(&key) {
                continue;
            }
            match self.inspect_tx.try_send(WorkerRequest::Inspect {
                key: key.clone(),
                cwd,
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
        session_id: Option<String>,
        cwd: Option<String>,
    ) -> Result<(), String> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| "no provider collector configured".to_string())?;
        self.control_tx
            .try_send(WorkerRequest::Activate {
                target,
                session_id,
                cwd,
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
            WorkerRequest::Inspect { key, cwd, config } => WorkerResult::Inspect {
                result: extension::inspect_once(
                    &config,
                    &key.provider,
                    &key.pane_id,
                    cwd.as_deref(),
                    key.session_id.as_deref(),
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
                session_id,
                cwd,
                config,
            } => WorkerResult::Activate {
                result: extension::activate(
                    &config,
                    &target.provider_id,
                    &target.parent_pane_id,
                    cwd.as_deref(),
                    session_id.as_deref(),
                    &target.agent_id,
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

    #[test]
    fn cache_key_keeps_session_snapshots_separate() {
        let first = CacheKey::new("claude", "%1", Some("one"));
        let second = CacheKey::new("claude", "%1", Some("two"));
        assert_ne!(first, second);
    }
}
