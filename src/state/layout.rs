use super::{AppState, RepoFilter, StatusFilter, TreeTarget};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct RowTarget {
    pub pane_id: String,
}

/// A child displayed under a parent pane. Every identifier comes from the
/// provider reply; no activation is resolved from text or a row index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentTarget {
    pub parent_pane_id: String,
    pub provider_id: String,
    pub session_id: Option<String>,
    pub agent_id: String,
    pub node_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigationTarget {
    Pane { row: usize },
    Subagent(SubagentTarget),
    Tree(TreeTarget),
}

/// Click target for the `+` button rendered at the right edge of each
/// repo-group header in the agents panel. Clicking it opens the spawn
/// modal prefilled for that repo.
#[derive(Debug, Clone)]
pub struct RepoSpawnTarget {
    pub rect: ratatui::layout::Rect,
    pub repo_name: String,
    pub repo_root: String,
}

/// Click target for the red `×` rendered next to the branch of a
/// sidebar-spawned pane. Clicking it opens the close-pane confirmation
/// for that specific pane.
#[derive(Debug, Clone)]
pub struct SpawnRemoveTarget {
    pub rect: ratatui::layout::Rect,
    pub pane_id: String,
}

#[derive(Debug, Clone)]
pub struct VcsBranchTarget {
    pub rect: ratatui::layout::Rect,
    pub kind: crate::git::VcsKind,
    pub root: String,
}

/// Screen-positioned hyperlink overlay for OSC 8 terminal hyperlinks.
#[derive(Debug, Clone)]
pub struct HyperlinkOverlay {
    pub x: u16,
    pub y: u16,
    pub text: String,
    pub url: String,
}

/// Ephemeral render output cached for click hit-testing.
///
/// Every field here is **rewritten on every frame** by the UI layer and
/// only read by event handlers (mouse/keyboard) before the next render.
/// Bundling them under `state.layout` makes the "frame-scoped vs
/// persistent state" boundary visible at a glance, since the rest of
/// `AppState` only holds data that survives across frames.
#[derive(Debug, Clone, Default)]
pub struct FrameLayout {
    /// Filtered pane list, in the order the UI rendered them. Index
    /// matches `GlobalState::selected_pane_row`.
    pub pane_row_targets: Vec<RowTarget>,
    /// Maps each rendered text line in the agents panel back to a row in
    /// `pane_row_targets`. `None` for header/blank lines that should not
    /// route clicks to a pane.
    pub line_to_row: Vec<Option<usize>>,
    /// Agent-panel source line → exact subagent in its original tmux pane.
    pub subagent_line_targets: HashMap<usize, SubagentTarget>,
    /// Agent and capability tree lines rendered for the selected pane.
    /// Stable `node_id` makes duplicate labels safe to select and disclose.
    pub tree_line_targets: HashMap<usize, TreeTarget>,
    pub navigation_targets: Vec<NavigationTarget>,
    /// X column of the repo filter button in the secondary header. `None`
    /// when the button is hidden. Used for click hit-testing.
    pub repo_button_col: Option<u16>,
    /// Click regions for the `[+]` spawn button rendered at the right
    /// edge of each repo-group header. One entry per visible repo group.
    pub repo_spawn_targets: Vec<RepoSpawnTarget>,
    /// Click regions for the red `×` remove marker rendered next to the
    /// branch of each sidebar-spawned pane. One entry per visible row.
    pub spawn_remove_targets: Vec<SpawnRemoveTarget>,
    /// Branch rows rendered in the Git/Arc tab.
    pub vcs_branch_targets: Vec<VcsBranchTarget>,
    /// OSC 8 hyperlink overlays the main loop writes after each frame so
    /// terminals can recognise PR numbers as clickable links.
    pub hyperlink_overlays: Vec<HyperlinkOverlay>,
    /// Full-sidebar transcript back affordance. Rewritten each transcript
    /// frame and consumed before any normal pane click handling.
    pub transcript_back_rect: Option<ratatui::layout::Rect>,
}

pub(super) fn point_in_rect(row: u16, col: u16, rect: ratatui::layout::Rect) -> bool {
    rect.contains(ratatui::layout::Position { x: col, y: row })
}

impl AppState {
    pub fn rebuild_row_targets(&mut self) {
        // Reset stale repo filter if the repo no longer exists, and
        // persist the reset back to tmux so fresh sidebar instances do
        // not reload the dead repo name on startup.
        if let RepoFilter::Repo(ref name) = self.global.repo_filter
            && !self.repo_groups.iter().any(|g| g.name == *name)
        {
            self.global.repo_filter = RepoFilter::All;
            self.global.save_repo_filter();
        }

        self.layout.pane_row_targets.clear();
        for group in &self.repo_groups {
            if !self.global.repo_filter.matches_group(&group.name) {
                continue;
            }
            for (pane, _) in &group.panes {
                if self.global.status_filter.matches(&pane.status) {
                    self.layout.pane_row_targets.push(RowTarget {
                        pane_id: pane.pane_id.clone(),
                    });
                }
            }
        }
        if self.global.selected_pane_row >= self.layout.pane_row_targets.len()
            && !self.layout.pane_row_targets.is_empty()
        {
            self.global.selected_pane_row = self.layout.pane_row_targets.len() - 1;
        }
    }

    /// Handle mouse scroll event, routing to agents or bottom panel based on Y position.
    pub fn handle_mouse_scroll(
        &mut self,
        row: u16,
        term_height: u16,
        bottom_panel_height: u16,
        delta: isize,
    ) {
        let bottom_start = term_height.saturating_sub(bottom_panel_height);
        if self.extensions.ui.transcript.view.is_some()
            || self.extensions.ui.transcript.loading.is_some()
        {
            self.scroll_transcript(delta);
        } else if self.extensions.ui.detail.is_some() {
            self.scroll_detail(delta);
        } else if row >= bottom_start {
            self.scroll_bottom(delta);
        } else {
            self.scrolls.panes.scroll(delta);
        }
    }

    /// Handle mouse click on the filter bar (row 0).
    /// Determines which filter was clicked based on x coordinate.
    /// Debounces rapid clicks to ignore phantom mouse events from tmux
    /// pane resize/layout changes.
    pub fn handle_filter_click(&mut self, col: u16) {
        const DEBOUNCE_MS: u128 = 150;
        let now = std::time::Instant::now();
        if now
            .duration_since(self.timers.last_filter_click)
            .as_millis()
            < DEBOUNCE_MS
        {
            return;
        }
        self.timers.last_filter_click = now;

        let (all, running, background, waiting, idle, error) = self.status_counts();
        // Layout: " ∑N  ●N  ◎N  ◐N  ○N  ✕N"
        // Each filter item renders as `icon(1) + count`, so the clickable
        // width is `1 + digits(count)`.
        let mut x = 1usize; // leading space
        let items: Vec<(StatusFilter, usize)> = vec![
            (StatusFilter::All, 1 + format!("{all}").len()),
            (StatusFilter::Running, 1 + format!("{running}").len()),
            (StatusFilter::Background, 1 + format!("{background}").len()),
            (StatusFilter::Waiting, 1 + format!("{waiting}").len()),
            (StatusFilter::Idle, 1 + format!("{idle}").len()),
            (StatusFilter::Error, 1 + format!("{error}").len()),
        ];
        let col = col as usize;
        for (i, (filter, width)) in items.iter().enumerate() {
            if i > 0 {
                x += 2; // "  " separator
            }
            if col >= x && col < x + width {
                self.global.status_filter = *filter;
                self.global.save_filter();
                self.rebuild_row_targets();
                return;
            }
            x += width;
        }
    }

    /// Handle mouse click on the secondary header row (row 1).
    /// The repo filter button lives on the far right of this row.
    pub fn handle_secondary_header_click(&mut self, col: u16) {
        if self
            .notices
            .button_col
            .is_some_and(|notices_col| col == notices_col)
        {
            self.toggle_notices_popup();
            return;
        }
        if self
            .layout
            .repo_button_col
            .is_some_and(|repo_button_col| col >= repo_button_col)
        {
            self.toggle_repo_popup();
        }
    }

    /// Handle mouse click in agents panel. Maps screen row to agent row
    /// via line_to_row (adjusted for scroll offset) and activates that pane.
    /// Row 0 is the fixed filter bar, row 1+ maps to the scrollable agent list.
    pub fn handle_mouse_click(&mut self, row: u16, col: u16) {
        if self.extensions.ui.transcript.view.is_some()
            || self.extensions.ui.transcript.loading.is_some()
        {
            if self
                .layout
                .transcript_back_rect
                .is_some_and(|rect| point_in_rect(row, col, rect))
            {
                self.close_transcript();
            }
            return;
        }
        if self.is_notices_popup_open() {
            if let Some(area) = self.notices_popup_area()
                && point_in_rect(row, col, area)
            {
                if let Some(agent) = self.notices_copy_target_at(row, col).map(str::to_string) {
                    self.copy_notices_prompt(&agent);
                }
                return;
            }
            self.close_notices_popup();
            return;
        }
        if self.is_repo_popup_open() {
            if let Some(area) = self.repo_popup_area()
                && point_in_rect(row, col, area)
            {
                // Skip clicks on the popup chrome (top border / title row).
                // Without this guard `saturating_sub(1)` collapses a click on
                // the title row into `item_index == 0`, switching the filter
                // to the first repo the moment the user reaches for the
                // popup.
                if row > area.y {
                    let item_index = (row - area.y - 1) as usize;
                    if item_index < self.repo_names().len() {
                        self.set_repo_popup_selected(item_index);
                        self.confirm_repo_popup();
                    }
                }
                return;
            }
            self.close_repo_popup();
            return;
        }
        if self.is_spawn_input_open() {
            if let Some(area) = self.spawn_input_popup_area()
                && point_in_rect(row, col, area)
            {
                return;
            }
            self.close_spawn_input();
            return;
        }
        if self.is_remove_confirm_open() {
            if let Some(area) = self.remove_confirm_popup_area()
                && point_in_rect(row, col, area)
            {
                return;
            }
            self.close_remove_confirm();
            return;
        }

        if row == 0 {
            self.handle_filter_click(col);
            return;
        }
        if row == 1 {
            self.handle_secondary_header_click(col);
            return;
        }

        // Check the `+` spawn buttons before the pane-row fallback so a
        // click on the button doesn't also shift the pane selection.
        if let Some((repo_name, repo_root, anchor_y)) = self
            .layout
            .repo_spawn_targets
            .iter()
            .find(|t| point_in_rect(row, col, t.rect))
            .map(|t| (t.repo_name.clone(), t.repo_root.clone(), t.rect.y))
        {
            self.open_spawn_input_for_repo(repo_name, repo_root, Some(anchor_y));
            return;
        }

        // Check the red `×` remove markers next to spawn-created branches.
        if let Some(pane_id) = self
            .layout
            .spawn_remove_targets
            .iter()
            .find(|t| point_in_rect(row, col, t.rect))
            .map(|t| t.pane_id.clone())
        {
            self.open_remove_confirm_for_pane(pane_id);
            return;
        }

        let line_index = (row as usize - 2) + self.scrolls.panes.offset;
        if let Some(target) = self.layout.tree_line_targets.get(&line_index).cloned() {
            self.selected_navigation_target = Some(NavigationTarget::Tree(target.clone()));
            if !self
                .selected_subagent_target
                .as_ref()
                .is_some_and(|subagent| {
                    subagent.parent_pane_id == target.parent_pane_id
                        && subagent.provider_id == target.provider_id
                        && subagent.session_id == target.session_id
                        && subagent.agent_id == target.agent_id
                })
            {
                self.selected_subagent_target = None;
            }
            if target.is_disclosure {
                self.extensions.ui.toggle(&target);
            } else {
                self.extensions.ui.selected = Some(target);
            }
            return;
        }
        if let Some(target) = self.layout.subagent_line_targets.get(&line_index).cloned() {
            self.selected_subagent_target = Some(target.clone());
            self.selected_navigation_target = Some(NavigationTarget::Subagent(target.clone()));
            self.activate_subagent(target);
            return;
        }
        if let Some(Some(agent_row)) = self.layout.line_to_row.get(line_index) {
            self.global.selected_pane_row = *agent_row;
            self.extensions.ui.selected = None;
            self.selected_subagent_target = None;
            self.selected_navigation_target = Some(NavigationTarget::Pane { row: *agent_row });
            self.global.queue_cursor_save();
        }
    }
}
