use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::SPAWN_BUTTON;
use super::row;
use crate::state::{AppState, Focus, TreeTarget};
use crate::ui::text::display_width;

#[derive(Debug, Default)]
pub(super) struct CollectedRows {
    pub lines: Vec<Line<'static>>,
    pub line_to_row: Vec<Option<usize>>,
    pub pending_spawn: Vec<(usize, String, String)>,
    pub pending_remove: Vec<(usize, u16, String)>,
    pub pending_tree: Vec<(usize, TreeTarget)>,
}

const INACTIVE_AGENTS_DISCLOSURE: &str = "__inactive_agents__";

#[cfg(test)]
fn agent_label(agent: &crate::extension::AgentNode) -> &str {
    &agent.label
}

#[cfg(test)]
fn scoped_selected_agent_id(
    pane_id: &str,
    provider_id: &str,
    session_id: Option<&str>,
    agents: &[crate::extension::AgentNode],
    subagent: Option<&crate::state::SubagentTarget>,
    tree: Option<&TreeTarget>,
) -> Option<String> {
    let candidate = subagent
        .filter(|target| {
            target.parent_pane_id == pane_id
                && target.provider_id == provider_id
                && target.session_id.as_deref() == session_id
        })
        .map(|target| target.agent_id.as_str())
        .or_else(|| {
            tree.filter(|target| {
                target.parent_pane_id == pane_id
                    && target.provider_id == provider_id
                    && target.session_id.as_deref() == session_id
            })
            .map(|target| target.agent_id.as_str())
        })?;
    agents
        .iter()
        .any(|agent| agent.id == candidate)
        .then(|| candidate.to_string())
}

fn agent_line(
    marker: &str,
    agent: &crate::extension::AgentNode,
    color: ratatui::style::Color,
) -> Line<'static> {
    let model = agent
        .model
        .as_deref()
        .map(|model| format!("  {model}"))
        .unwrap_or_default();
    Line::from(Span::styled(
        format!("  {marker} {}{model}", agent.label),
        Style::default().fg(color),
    ))
}

fn child_is_active(
    inspection: &crate::extension::Inspection,
    agent: &crate::extension::AgentNode,
) -> bool {
    !inspection.stale && matches!(agent.lifecycle, crate::extension::AgentLifecycle::Running)
}

pub(super) fn collect(state: &AppState, width: u16) -> CollectedRows {
    let width = width as usize;
    let theme = &state.theme;

    let mut collected = CollectedRows::default();
    let filter = state.global.status_filter;
    let mut first_group = true;
    let mut row_index: usize = 0;

    for group in &state.repo_groups {
        if !state.global.repo_filter.matches_group(&group.name) {
            continue;
        }
        let filtered_panes: Vec<_> = group
            .panes
            .iter()
            .filter(|(pane, _)| filter.matches(&pane.status))
            .collect();
        if filtered_panes.is_empty() {
            continue;
        }

        if !first_group {
            // Separate repo groups, but do not add a leading blank before
            // the first repo so the list starts immediately below the header.
            collected.lines.push(Line::from(""));
            collected.line_to_row.push(None);
        }
        first_group = false;

        let group_has_focused_pane = state
            .focus_state
            .focused_pane_id
            .as_ref()
            .is_some_and(|fid| group.panes.iter().any(|(p, _)| p.pane_id == *fid));

        // Plain repo header at column 0, with a `[+]` spawn button
        // right-aligned on the same row. Only rendered when the group
        // has a resolved repo_root — panes outside a git repo get a
        // plain title.
        let title = &group.name;
        let title_color = if group_has_focused_pane {
            theme.accent
        } else {
            theme.text_active
        };
        let repo_root = group
            .panes
            .iter()
            .find_map(|(_, git)| git.repo_root.clone());
        let spans: Vec<Span<'static>> = if let Some(ref root) = repo_root {
            let title_w = display_width(title);
            let pad_width = width
                .saturating_sub(title_w)
                .saturating_sub(SPAWN_BUTTON.len());
            collected
                .pending_spawn
                .push((collected.lines.len(), group.name.clone(), root.clone()));
            let button_color = if group_has_focused_pane {
                theme.accent
            } else {
                theme.text_active
            };
            vec![
                Span::styled(title.clone(), Style::default().fg(title_color)),
                Span::raw(" ".repeat(pad_width)),
                Span::styled(SPAWN_BUTTON, Style::default().fg(button_color)),
            ]
        } else {
            vec![Span::styled(
                title.clone(),
                Style::default().fg(title_color),
            )]
        };
        collected.lines.push(Line::from(spans));
        collected.line_to_row.push(None);

        for (pane, git_info) in filtered_panes.iter() {
            let cursor_selected = row_index == state.global.selected_pane_row;
            let is_selected = state.focus_state.sidebar_focused
                && state.focus_state.focus == Focus::Panes
                && cursor_selected;

            let is_active = state.focus_state.focused_pane_id.as_ref() == Some(&pane.pane_id);

            let pane_state = state.pane_state(&pane.pane_id);
            let ports = state
                .show_ports
                .then(|| pane_state.map(|s| s.ports.as_slice()))
                .flatten();
            let task_progress = pane_state.and_then(|s| s.task_progress.as_ref());
            let status_line_idx = collected.lines.len();
            let pane_lines = row::render_pane_lines_with_ports(
                pane,
                git_info,
                ports,
                task_progress,
                is_selected,
                is_active,
                width,
                &state.icons,
                theme,
                state.spinner_frame,
                state.now,
            );
            let mut pane_lines = pane_lines;
            // Activity is not a selection detail: every rendered pane keeps
            // fresh, running children visible. Only the synthetic inactive
            // disclosure receives a tree target; child rows never do.
            if let Some(inspection) = state.extensions.inspection(
                &pane.pane_id,
                pane.agent.as_str(),
                pane.session_id.as_deref(),
            ) {
                let root_id = inspection
                    .reply
                    .agents
                    .iter()
                    .find(|agent| agent.parent_id.is_none())
                    .map(|agent| agent.id.clone())
                    .unwrap_or_else(|| pane.pane_id.clone());
                let mut children: Vec<_> = inspection
                    .reply
                    .agents
                    .iter()
                    .filter(|agent| agent.parent_id.is_some())
                    .collect();
                children.sort_by(|left, right| {
                    let left_rank = child_is_active(inspection, left) as u8;
                    let right_rank = child_is_active(inspection, right) as u8;
                    right_rank
                        .cmp(&left_rank)
                        .then_with(|| left.label.cmp(&right.label))
                        .then_with(|| left.id.cmp(&right.id))
                });
                for agent in children
                    .iter()
                    .copied()
                    .filter(|agent| child_is_active(inspection, agent))
                {
                    pane_lines.push(agent_line("●", agent, theme.accent));
                }
                let inactive: Vec<_> = children
                    .into_iter()
                    .filter(|agent| !child_is_active(inspection, agent))
                    .collect();
                if !inactive.is_empty() {
                    let target = TreeTarget {
                        parent_pane_id: pane.pane_id.clone(),
                        provider_id: pane.agent.as_str().to_string(),
                        session_id: pane.session_id.clone(),
                        agent_id: root_id,
                        node_id: INACTIVE_AGENTS_DISCLOSURE.to_string(),
                        detail_token: None,
                        inline_detail: None,
                        is_disclosure: true,
                    };
                    let expanded = state.extensions.ui.is_expanded(&target);
                    let disclosure = if expanded { "▼" } else { "▶" };
                    let line = collected.lines.len() + pane_lines.len();
                    pane_lines.push(Line::from(Span::styled(
                        format!("  {disclosure} Inactive ({})", inactive.len()),
                        Style::default().fg(theme.text_muted),
                    )));
                    collected.pending_tree.push((line, target));
                    if expanded {
                        for agent in inactive {
                            pane_lines.push(agent_line("○", agent, theme.text_muted));
                        }
                    }
                }
            }
            // Capabilities remain a root-only detail of the cursor-selected
            // pane. Child facts deliberately do not become selectable rows.
            if cursor_selected {
                // Capabilities are intentionally rendered only for the selected
                // parent AgentNode. Their click identity uses TreeNode.id, never
                // a duplicate label or current line offset.
                let tree_start = collected.lines.len() + pane_lines.len();
                let inspection = state.extensions.inspection(
                    &pane.pane_id,
                    pane.agent.as_str(),
                    pane.session_id.as_deref(),
                );
                for (offset, (line, target)) in row::render_extension_tree(
                    inspection,
                    state.extensions.config.as_ref(),
                    &pane.pane_id,
                    pane.agent.as_str(),
                    pane.session_id.as_deref(),
                    None,
                    &state.extensions.ui,
                    width,
                    theme,
                )
                .into_iter()
                .enumerate()
                {
                    pane_lines.push(line);
                    collected.pending_tree.push((tree_start + offset, target));
                }
                if let Some(error) = &state.extensions.config_error {
                    pane_lines.push(Line::from(Span::styled(
                        format!("  × {error}"),
                        Style::default().fg(theme.status_error),
                    )));
                }
            }
            let pane_line_count = pane_lines.len();
            collected.lines.extend(pane_lines);
            for _ in 0..pane_line_count {
                collected.line_to_row.push(Some(row_index));
            }

            // The branch row is always `status_line_idx + 1` when
            // `branch_ports_row` emits a line (which requires a
            // non-empty branch). Look up the exact column of the
            // trailing `×` from the row helper so the click target
            // lines up with the rendered glyph even when the branch
            // name truncates.
            if pane.sidebar_spawned
                && git_info.is_worktree
                && pane_line_count >= 2
                && let Some(x) =
                    row::sidebar_remove_marker_col(git_info, ports, true, width.saturating_sub(2))
            {
                collected
                    .pending_remove
                    .push((status_line_idx + 1, x, pane.pane_id.clone()));
            }

            row_index += 1;
        }
    }

    collected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::{
        AgentLifecycle, AgentNode, AgentRole, Evidence, ExtensionsConfig, Fact, ProviderConfig,
        Reply, TreeNode,
    };
    use crate::group::{PaneGitInfo, RepoGroup};
    use crate::state::{AppState, ExtensionsState, StatusFilter, SubagentTarget};
    use crate::tmux::{AgentType, PaneInfo, PaneStatus, PermissionMode, WorktreeMetadata};
    use std::collections::BTreeMap;

    fn make_pane(id: &str, status: PaneStatus) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            pane_active: false,
            status,
            attention: false,
            agent: AgentType::Claude,
            path: "/tmp/repo".into(),
            current_command: String::new(),
            prompt: String::new(),
            prompt_is_response: false,
            started_at: None,
            wait_reason: String::new(),
            permission_mode: PermissionMode::Default,
            subagents: vec![],
            pane_pid: None,
            worktree: WorktreeMetadata::default(),
            session_id: None,
            session_name: String::new(),
            sidebar_spawned: false,
            bg_shell_cmd: None,
        }
    }

    fn reply_with_children<'a>(
        main_id: &str,
        children: impl IntoIterator<Item = (&'a str, &'a str, AgentLifecycle)>,
    ) -> Reply {
        let mut agents = vec![AgentNode {
            id: main_id.into(),
            parent_id: None,
            label: format!("Main {main_id}"),
            role: AgentRole::Main,
            model: None,
            lifecycle: AgentLifecycle::Running,
            transcript_available: false,
        }];
        agents.extend(
            children
                .into_iter()
                .map(|(id, label, lifecycle)| AgentNode {
                    id: id.into(),
                    parent_id: Some(main_id.into()),
                    label: label.into(),
                    role: AgentRole::Subagent,
                    model: Some("model".into()),
                    lifecycle,
                    transcript_available: false,
                }),
        );
        Reply {
            version: 1,
            agents,
            ..Reply::default()
        }
    }

    #[test]
    fn agent_label_renders_semantic_label_verbatim() {
        let main = AgentNode {
            id: "main".into(),
            parent_id: None,
            label: "Ada".into(),
            role: AgentRole::Main,
            model: Some("model-a".into()),
            lifecycle: Default::default(),
            transcript_available: false,
        };
        let subagent = AgentNode {
            id: "subagent".into(),
            parent_id: Some("main".into()),
            label: "Ada".into(),
            role: AgentRole::Subagent,
            model: None,
            lifecycle: Default::default(),
            transcript_available: false,
        };
        let unknown = AgentNode {
            id: "unknown".into(),
            parent_id: None,
            label: "Ada".into(),
            role: AgentRole::Unknown,
            model: None,
            lifecycle: Default::default(),
            transcript_available: false,
        };

        assert_eq!(agent_label(&main), "Ada");
        assert_eq!(agent_label(&subagent), "Ada");
        assert_eq!(agent_label(&unknown), "Ada");
    }

    #[test]
    fn hidden_ports_reserve_no_row_or_width() {
        let mut state = AppState::new("%sidebar".into());
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: true,
            panes: vec![(
                make_pane("%1", PaneStatus::Running),
                PaneGitInfo {
                    repo_root: Some("/tmp/repo".into()),
                    branch: Some("feature/very-long-branch-name-that-wraps".into()),
                    is_worktree: false,
                    worktree_name: None,
                },
            )],
        }];
        state.rebuild_row_targets();
        state.set_pane_ports("%1", vec![3000, 5173]);

        let hidden = collect(&state, 14);
        let hidden_text = hidden
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        assert!(!hidden_text.iter().any(|line| line.contains(":3000")));
        assert!(hidden_text.iter().any(|line| line.contains("feature")));

        state.show_ports = true;
        let visible = collect(&state, 14);
        let visible_text = visible
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>();
        assert!(visible_text.iter().any(|line| line.contains(":3000")));
        assert!(!visible_text.iter().any(|line| line.contains("feature")));
        assert_ne!(hidden_text, visible_text);
        assert_eq!(hidden.lines.len(), visible.lines.len());
    }

    #[test]
    fn focus_round_trip_preserves_tree_text_targets_and_disclosure() {
        let mut pane = make_pane("%1", PaneStatus::Running);
        pane.session_id = Some("session".into());
        let mut state = AppState::new("%sidebar".into());
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: true,
            panes: vec![(pane, PaneGitInfo::default())],
        }];
        state.extensions.config = Some(ExtensionsConfig {
            providers: BTreeMap::from([(
                "claude".into(),
                ProviderConfig {
                    argv: vec!["/bin/true".into()],
                    timeout_ms: 100,
                    env: BTreeMap::new(),
                },
            )]),
            ..ExtensionsConfig::default()
        });
        state.extensions.seed_test_inspection(
            "%1",
            "claude",
            Some("session"),
            Reply {
                version: 1,
                agents: vec![AgentNode {
                    id: "main".into(),
                    parent_id: None,
                    label: "Main agent".into(),
                    role: AgentRole::Main,
                    model: None,
                    lifecycle: Default::default(),
                    transcript_available: false,
                }],
                tree: vec![TreeNode {
                    id: "skills".into(),
                    agent_id: "main".into(),
                    parent_id: None,
                    label: "Skills".into(),
                    fact_ids: vec!["skill-a".into()],
                    detail_token: None,
                    category: "skills".into(),
                }],
                facts: vec![Fact {
                    id: "skill-a".into(),
                    agent_id: "main".into(),
                    category: "skills".into(),
                    label: "Project skill".into(),
                    detail: String::new(),
                    evidence: Evidence::Available,
                    source: String::new(),
                    detail_token: None,
                }],
                ..Reply::default()
            },
        );
        let disclosure = TreeTarget {
            parent_pane_id: "%1".into(),
            provider_id: "claude".into(),
            session_id: Some("session".into()),
            agent_id: "main".into(),
            node_id: "skills".into(),
            detail_token: None,
            inline_detail: None,
            is_disclosure: true,
        };
        state.extensions.ui.toggle(&disclosure);
        state.focus_state.sidebar_focused = true;

        let focused = collect(&state, 28);
        state.focus_state.sidebar_focused = false;
        let unfocused = collect(&state, 28);
        state.focus_state.sidebar_focused = true;
        let refocused = collect(&state, 28);

        let line_text = |rows: &CollectedRows| {
            rows.lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };
        assert_eq!(line_text(&focused), line_text(&unfocused));
        assert_eq!(line_text(&focused), line_text(&refocused));
        assert_eq!(focused.pending_tree, unfocused.pending_tree);
        assert_eq!(focused.pending_tree, refocused.pending_tree);
        assert!(state.extensions.ui.is_expanded(&disclosure));
    }

    #[test]
    fn two_live_scopes_keep_running_children_accented_across_selection_and_focus_round_trip() {
        let mut first = make_pane("%1", PaneStatus::Running);
        first.session_id = Some("one".into());
        let mut second = make_pane("%2", PaneStatus::Running);
        second.session_id = Some("two".into());
        let mut state = AppState::new("%sidebar".into());
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: true,
            panes: vec![
                (first, PaneGitInfo::default()),
                (second, PaneGitInfo::default()),
            ],
        }];
        state.extensions.seed_test_inspection(
            "%1",
            "claude",
            Some("one"),
            reply_with_children(
                "main-one",
                [("child-one", "Running child A", AgentLifecycle::Running)],
            ),
        );
        state.extensions.seed_test_inspection(
            "%2",
            "claude",
            Some("two"),
            reply_with_children(
                "main-two",
                [("child-two", "Running child B", AgentLifecycle::Running)],
            ),
        );
        state.focus_state.sidebar_focused = true;
        state.focus_state.focus = Focus::Panes;

        let child_color = |rows: &CollectedRows, label: &str| {
            rows.lines
                .iter()
                .find(|line| line.to_string().contains(label))
                .and_then(|line| line.spans.first().and_then(|span| span.style.fg))
        };
        let assert_both_accented = |rows: &CollectedRows| {
            assert_eq!(
                child_color(rows, "Running child A"),
                Some(state.theme.accent)
            );
            assert_eq!(
                child_color(rows, "Running child B"),
                Some(state.theme.accent)
            );
        };

        state.global.selected_pane_row = 0;
        assert_both_accented(&collect(&state, 80));
        state.global.selected_pane_row = 1;
        assert_both_accented(&collect(&state, 80));
        state.focus_state.sidebar_focused = false;
        assert_both_accented(&collect(&state, 80));
        state.focus_state.sidebar_focused = true;
        state.global.selected_pane_row = 0;
        assert_both_accented(&collect(&state, 80));
    }

    #[test]
    fn inactive_disclosure_persists_per_scope_until_successful_inspect_removes_children() {
        let mut first = make_pane("%1", PaneStatus::Running);
        first.session_id = Some("one".into());
        let mut second = make_pane("%2", PaneStatus::Running);
        second.session_id = Some("two".into());
        let mut state = AppState::new("%sidebar".into());
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: true,
            panes: vec![
                (first, PaneGitInfo::default()),
                (second, PaneGitInfo::default()),
            ],
        }];
        let config = ExtensionsConfig {
            providers: BTreeMap::from([(
                "claude".into(),
                ProviderConfig {
                    argv: vec!["/bin/true".into()],
                    timeout_ms: 100,
                    env: BTreeMap::new(),
                },
            )]),
            ..ExtensionsConfig::default()
        };
        let (extensions, queue) = ExtensionsState::with_test_control_queue(config);
        state.extensions = extensions;
        let first_reply = reply_with_children(
            "main-one",
            [("inactive-one", "Unknown child A", AgentLifecycle::Unknown)],
        );
        queue.complete_inspect("%1", "claude", Some("one"), Ok(first_reply.clone()));
        queue.complete_inspect(
            "%2",
            "claude",
            Some("two"),
            Ok(reply_with_children(
                "main-two",
                [(
                    "inactive-two",
                    "Completed child B",
                    AgentLifecycle::Completed,
                )],
            )),
        );
        state.extensions.take_results();
        let disclosure = TreeTarget {
            parent_pane_id: "%1".into(),
            provider_id: "claude".into(),
            session_id: Some("one".into()),
            agent_id: "main-one".into(),
            node_id: INACTIVE_AGENTS_DISCLOSURE.into(),
            detail_token: None,
            inline_detail: None,
            is_disclosure: true,
        };
        state.extensions.ui.toggle(&disclosure);
        assert!(state.extensions.ui.is_expanded(&disclosure));

        queue.complete_inspect("%1", "claude", Some("one"), Ok(first_reply));
        state.extensions.take_results();
        assert!(state.extensions.ui.is_expanded(&disclosure));

        state.focus_state.sidebar_focused = true;
        state.focus_state.focus = Focus::Panes;
        for selected_pane_row in [0, 1, 0] {
            state.global.selected_pane_row = selected_pane_row;
            let lines = collect(&state, 80)
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            assert!(lines.iter().any(|line| line.contains("Unknown child A")));
            assert!(lines.iter().any(|line| line.contains("○ Unknown child A")));
            assert!(lines.iter().any(|line| line.contains("Inactive (1)")));
            assert!(!lines.iter().any(|line| line.contains("? Unknown child A")));
            assert!(state.extensions.ui.is_expanded(&disclosure));
        }

        queue.complete_inspect(
            "%1",
            "claude",
            Some("one"),
            Ok(reply_with_children("main-one", [])),
        );
        state.extensions.take_results();
        assert!(!state.extensions.ui.is_expanded(&disclosure));
        let lines = collect(&state, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(!lines.iter().any(|line| line.contains("Unknown child A")));
    }

    #[test]
    fn stale_inspection_demotes_running_children_into_inactive_disclosure() {
        let mut pane = make_pane("%1", PaneStatus::Running);
        pane.session_id = Some("session".into());
        let mut state = AppState::new("%sidebar".into());
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: true,
            panes: vec![(pane, PaneGitInfo::default())],
        }];
        let config = ExtensionsConfig {
            providers: BTreeMap::from([(
                "claude".into(),
                ProviderConfig {
                    argv: vec!["/bin/true".into()],
                    timeout_ms: 100,
                    env: BTreeMap::new(),
                },
            )]),
            ..ExtensionsConfig::default()
        };
        let (extensions, queue) = ExtensionsState::with_test_control_queue(config);
        state.extensions = extensions;
        let reply = reply_with_children(
            "main",
            [("running", "Running child", AgentLifecycle::Running)],
        );
        queue.complete_inspect("%1", "claude", Some("session"), Ok(reply));
        state.extensions.take_results();
        let fresh_lines = collect(&state, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(
            fresh_lines
                .iter()
                .any(|line| line.contains("● Running child"))
        );
        assert!(!fresh_lines.iter().any(|line| line.contains("Inactive (1)")));

        queue.complete_inspect("%1", "claude", Some("session"), Err("timeout".into()));
        state.extensions.take_results();
        let disclosure = TreeTarget {
            parent_pane_id: "%1".into(),
            provider_id: "claude".into(),
            session_id: Some("session".into()),
            agent_id: "main".into(),
            node_id: INACTIVE_AGENTS_DISCLOSURE.into(),
            detail_token: None,
            inline_detail: None,
            is_disclosure: true,
        };
        let stale_lines = collect(&state, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(
            !stale_lines
                .iter()
                .any(|line| line.contains("● Running child"))
        );
        assert!(stale_lines.iter().any(|line| line.contains("Inactive (1)")));

        state.extensions.ui.toggle(&disclosure);
        let expanded_lines = collect(&state, 80)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(
            expanded_lines
                .iter()
                .any(|line| line.contains("○ Running child"))
        );
        assert!(
            !expanded_lines
                .iter()
                .any(|line| line.contains("? Running child"))
        );
    }

    #[test]
    fn selected_agent_requires_matching_provider_session_and_known_agent() {
        let agents = vec![
            AgentNode {
                id: "main".into(),
                parent_id: None,
                label: "Main".into(),
                role: AgentRole::Main,
                model: None,
                lifecycle: Default::default(),
                transcript_available: false,
            },
            AgentNode {
                id: "child".into(),
                parent_id: Some("main".into()),
                label: "Child".into(),
                role: AgentRole::Subagent,
                model: None,
                lifecycle: Default::default(),
                transcript_available: false,
            },
        ];
        let subagent = SubagentTarget {
            parent_pane_id: "%1".into(),
            provider_id: "claude".into(),
            session_id: Some("session".into()),
            agent_id: "child".into(),
            node_id: "child".into(),
        };
        let tree = TreeTarget {
            parent_pane_id: "%1".into(),
            provider_id: "claude".into(),
            session_id: Some("session".into()),
            agent_id: "child".into(),
            node_id: "skills".into(),
            detail_token: None,
            inline_detail: None,
            is_disclosure: true,
        };

        assert_eq!(
            scoped_selected_agent_id(
                "%1",
                "claude",
                Some("session"),
                &agents,
                Some(&subagent),
                None,
            ),
            Some("child".into())
        );
        assert!(
            scoped_selected_agent_id(
                "%1",
                "codex",
                Some("session"),
                &agents,
                Some(&subagent),
                None,
            )
            .is_none()
        );
        assert!(
            scoped_selected_agent_id(
                "%1",
                "claude",
                Some("other"),
                &agents,
                Some(&subagent),
                None,
            )
            .is_none()
        );
        let unknown = SubagentTarget {
            agent_id: "missing".into(),
            ..subagent
        };
        assert!(
            scoped_selected_agent_id(
                "%1",
                "claude",
                Some("session"),
                &agents,
                Some(&unknown),
                None,
            )
            .is_none()
        );
        assert_eq!(
            scoped_selected_agent_id("%1", "claude", Some("session"), &agents, None, Some(&tree),),
            Some("child".into())
        );
    }

    #[test]
    fn collect_empty_repo_groups_produces_no_lines() {
        let state = AppState::new("%0".into());
        let collected = collect(&state, 40);
        assert!(collected.lines.is_empty());
        assert!(collected.line_to_row.is_empty());
        assert!(collected.pending_spawn.is_empty());
        assert!(collected.pending_remove.is_empty());
    }

    #[test]
    fn collect_skips_group_when_status_filter_excludes_all_panes() {
        let mut state = AppState::new("%0".into());
        // The group has only Running panes, so filter to Waiting to drop them all.
        state.global.status_filter = StatusFilter::Waiting;
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: false,
            panes: vec![(make_pane("%1", PaneStatus::Running), PaneGitInfo::default())],
        }];
        let collected = collect(&state, 40);
        assert!(collected.lines.is_empty());
        assert!(collected.pending_spawn.is_empty());
    }

    #[test]
    fn collect_records_pending_spawn_when_repo_root_present() {
        let mut state = AppState::new("%0".into());
        let git_info = PaneGitInfo {
            repo_root: Some("/tmp/repo".into()),
            branch: None,
            is_worktree: false,
            worktree_name: None,
        };
        state.repo_groups = vec![RepoGroup {
            name: "repo".into(),
            has_focus: false,
            panes: vec![(make_pane("%1", PaneStatus::Running), git_info)],
        }];
        let collected = collect(&state, 40);
        assert_eq!(
            collected.pending_spawn.len(),
            1,
            "groups with a repo_root should emit a spawn target"
        );
        assert_eq!(collected.pending_spawn[0].1, "repo");
        assert_eq!(collected.pending_spawn[0].2, "/tmp/repo");
        // At least the header plus one pane row should have been pushed.
        assert!(!collected.lines.is_empty());
    }

    #[test]
    fn collect_no_pending_spawn_without_repo_root() {
        let mut state = AppState::new("%0".into());
        state.repo_groups = vec![RepoGroup {
            name: "raw-path".into(),
            has_focus: false,
            panes: vec![(make_pane("%1", PaneStatus::Running), PaneGitInfo::default())],
        }];
        let collected = collect(&state, 40);
        assert!(
            collected.pending_spawn.is_empty(),
            "groups without repo_root must not produce spawn targets"
        );
    }

    #[test]
    fn collect_pending_spawn_grows_with_repo_root_bearing_groups() {
        let mut state = AppState::new("%0".into());
        let with_root = |root: &str, name: &str, pane_id: &str| RepoGroup {
            name: name.into(),
            has_focus: false,
            panes: vec![(
                make_pane(pane_id, PaneStatus::Running),
                PaneGitInfo {
                    repo_root: Some(root.into()),
                    branch: None,
                    is_worktree: false,
                    worktree_name: None,
                },
            )],
        };
        state.repo_groups = vec![
            with_root("/repo/a", "a", "%1"),
            with_root("/repo/b", "b", "%2"),
            with_root("/repo/c", "c", "%3"),
        ];
        let collected = collect(&state, 40);
        assert_eq!(collected.pending_spawn.len(), 3);
    }
}
