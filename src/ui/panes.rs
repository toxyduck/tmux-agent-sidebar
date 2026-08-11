mod click_targets;
mod filter_bar;
mod popups;
mod row;
mod row_collector;

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

use crate::state::{AppState, Focus, PopupState, RepoFilter, SpawnField};

pub(super) const SPAWN_BUTTON: &str = "+";

/// Width of the clickable region around the `×` marker. One column of
/// slack on either side makes it comfortable to hit without stealing
/// clicks from adjacent branch text.
pub(super) const REMOVE_MARKER_HIT_WIDTH: u16 = 3;

use super::text::{display_width, truncate_to_width};

/// Compute a popup Rect centered inside `area`, clamped so it never
/// exceeds the parent (a narrow sidebar can't end up with a popup wider
/// than its own pane, which used to crash ratatui).
fn center_popup(area: Rect, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width);
    let height = desired_height.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// Place a popup directly below screen row `anchor_y`, left-aligned to
/// `area`, and shift upward when it would overflow the bottom edge.
fn anchor_below(area: Rect, anchor_y: u16, desired_width: u16, desired_height: u16) -> Rect {
    let width = desired_width.min(area.width);
    let height = desired_height.min(area.height);
    let below = anchor_y.saturating_add(1);
    let bottom = area.y.saturating_add(area.height);
    let y = if below + height <= bottom {
        below
    } else {
        bottom.saturating_sub(height).max(area.y)
    };
    Rect::new(area.x, y, width, height)
}

struct PaneLayout {
    filter_area: Rect,
    secondary_area: Rect,
    list_area: Rect,
}

impl PaneLayout {
    fn compute(area: Rect) -> Self {
        let filter_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1.min(area.height),
        };
        let secondary_area = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1.min(area.height.saturating_sub(1)),
        };
        let list_area = Rect {
            x: area.x,
            y: area.y + 2,
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        Self {
            filter_area,
            secondary_area,
            list_area,
        }
    }
}

/// Minimum agents-panel height the expanded Vercel-style spawn modal
/// needs. Below this the popup falls back to a compact label-less
/// layout to avoid clipping rows (the default 20-row bottom panel can
/// leave only ~10 rows for the agents panel on short terminals).
const SPAWN_MODAL_EXPANDED_MIN_HEIGHT: u16 = 12;

/// Border rows contributed to the total popup height (top + bottom).
const POPUP_BORDER_ROWS: u16 = 2;

// Row offsets inside the inner area of the compact popup.
const COMPACT_TASK_Y: u16 = 0;
const COMPACT_AGENT_Y: u16 = 1;
const COMPACT_MODE_Y: u16 = 2;
const COMPACT_ERROR_Y: u16 = 3;

// Row offsets inside the inner area of the expanded Vercel popup.
// Each section is label → value with a blank spacer between them.
const EXP_TASK_LABEL_Y: u16 = 1;
const EXP_TASK_VALUE_Y: u16 = 2;
const EXP_AGENT_LABEL_Y: u16 = 4;
const EXP_AGENT_VALUE_Y: u16 = 5;
const EXP_MODE_LABEL_Y: u16 = 7;
const EXP_MODE_VALUE_Y: u16 = 8;
const EXP_ERROR_Y: u16 = 10;

pub(super) fn render_spawn_input_popup(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let PopupState::SpawnInput {
        input,
        agent_idx,
        mode_idx,
        field,
        anchor_y,
        error,
        ..
    } = &state.popup
    else {
        return;
    };
    let input = input.clone();
    let field = *field;
    let anchor_y = *anchor_y;
    let error = error.clone();
    let agent = crate::worktree::AGENTS
        .get(*agent_idx)
        .copied()
        .unwrap_or("");
    let mode = crate::worktree::modes_for(agent)
        .get(*mode_idx)
        .copied()
        .unwrap_or("");
    let theme = &state.theme;

    let popup_width = area.width.min(32).max(area.width.min(14));
    let compact = area.height < SPAWN_MODAL_EXPANDED_MIN_HEIGHT;
    let content_rows: u16 = if compact { 4 } else { 10 };
    let error_rows: u16 = if error.is_some() { 1 } else { 0 };
    let popup_height = content_rows + error_rows + POPUP_BORDER_ROWS;
    let popup_rect = match anchor_y {
        Some(y) => anchor_below(area, y, popup_width, popup_height),
        None => center_popup(area, popup_width, popup_height),
    };
    state.popup.set_spawn_input_area(Some(popup_rect));

    frame.render_widget(Clear, popup_rect);
    let title_trunc = truncate_to_width(
        " Spawn worktree ",
        popup_rect.width.saturating_sub(2) as usize,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            title_trunc,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    // Row 0 is left blank as a top gutter in expanded mode. Content
    // rows get one column of left padding so they don't hug the border.
    let render_at = |frame: &mut Frame, y_offset: u16, spans: Vec<Span<'_>>| {
        if y_offset < inner.height {
            let row = Rect::new(
                inner.x + 1,
                inner.y + y_offset,
                inner.width.saturating_sub(2),
                1,
            );
            frame.render_widget(Paragraph::new(Line::from(spans)), row);
        }
    };

    let label_style = |target: SpawnField| {
        let base = Style::default().add_modifier(Modifier::BOLD);
        if field == target {
            base.fg(theme.accent)
        } else {
            base.fg(theme.text_muted)
        }
    };
    let value_style = |target: SpawnField| {
        if field == target {
            Style::default().fg(theme.text_active)
        } else {
            Style::default().fg(theme.text_muted)
        }
    };

    let content_width = inner.width.saturating_sub(2) as usize;
    let visible_input = tail_fit(&input, content_width.saturating_sub(1));
    let mut task_spans: Vec<Span<'_>> =
        vec![Span::styled(visible_input, value_style(SpawnField::Task))];
    if field == SpawnField::Task {
        task_spans.push(Span::styled("█", Style::default().fg(theme.accent)));
    }
    let agent_value = truncate_to_width(agent, content_width);
    let mode_value = truncate_to_width(mode, content_width);
    let error_spans = error.as_ref().map(|err| {
        vec![Span::styled(
            truncate_to_width(err, content_width),
            Style::default().fg(theme.status_error),
        )]
    });

    if compact {
        render_at(frame, COMPACT_TASK_Y, task_spans);
        render_at(
            frame,
            COMPACT_AGENT_Y,
            vec![Span::styled(agent_value, value_style(SpawnField::Agent))],
        );
        render_at(
            frame,
            COMPACT_MODE_Y,
            vec![Span::styled(mode_value, value_style(SpawnField::Mode))],
        );
        if let Some(err) = error_spans {
            render_at(frame, COMPACT_ERROR_Y, err);
        }
    } else {
        render_at(
            frame,
            EXP_TASK_LABEL_Y,
            vec![Span::styled("NAME", label_style(SpawnField::Task))],
        );
        render_at(frame, EXP_TASK_VALUE_Y, task_spans);
        render_at(
            frame,
            EXP_AGENT_LABEL_Y,
            vec![Span::styled("AGENT", label_style(SpawnField::Agent))],
        );
        render_at(
            frame,
            EXP_AGENT_VALUE_Y,
            vec![Span::styled(agent_value, value_style(SpawnField::Agent))],
        );
        render_at(
            frame,
            EXP_MODE_LABEL_Y,
            vec![Span::styled("MODE", label_style(SpawnField::Mode))],
        );
        render_at(
            frame,
            EXP_MODE_VALUE_Y,
            vec![Span::styled(mode_value, value_style(SpawnField::Mode))],
        );
        if let Some(err) = error_spans {
            render_at(frame, EXP_ERROR_Y, err);
        }
    }
}

/// Keep only the trailing `max_width` display cells of `text` so the
/// cursor at the end stays visible in a narrow input box. Prepends `…`
/// when truncation is applied.
fn tail_fit(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if max_width == 0 {
        return String::new();
    }
    if display_width(text) <= max_width {
        return text.to_string();
    }
    let budget = max_width.saturating_sub(1);
    let mut taken = 0usize;
    let mut byte_start = text.len();
    for (i, ch) in text.char_indices().rev() {
        let w = ch.width().unwrap_or(0);
        if taken + w > budget {
            break;
        }
        taken += w;
        byte_start = i;
    }
    let mut out = String::with_capacity(3 + (text.len() - byte_start));
    out.push('…');
    out.push_str(&text[byte_start..]);
    out
}

pub(super) fn render_remove_confirm_popup(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let (branch, error) = match &state.popup {
        PopupState::RemoveConfirm { branch, error, .. } => (branch.clone(), error.clone()),
        _ => return,
    };
    let theme = &state.theme;

    // Narrow-friendly: put the branch in the title, keep option rows
    // short enough to fit in ~16 columns. Reserve an extra row when
    // an inline error is present.
    let popup_height: u16 = if error.is_some() { 7 } else { 6 };
    let popup_rect = center_popup(area, area.width.min(28), popup_height);
    state.popup.set_remove_confirm_area(Some(popup_rect));

    frame.render_widget(Clear, popup_rect);
    let title_text = format!(" {branch} ");
    let title = truncate_to_width(&title_text, popup_rect.width.saturating_sub(2) as usize);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.status_error))
        .title(Span::styled(title, Style::default().fg(theme.status_error)));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    let render_row = |frame: &mut Frame, y_offset: u16, text: &str, style: Style| {
        if y_offset < inner.height {
            let row = Rect::new(inner.x, inner.y + y_offset, inner.width, 1);
            let truncated = truncate_to_width(text, row.width as usize);
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(truncated, style))),
                row,
            );
        }
    };

    render_row(
        frame,
        0,
        "[y] remove worktree",
        Style::default().fg(theme.status_error),
    );
    render_row(
        frame,
        1,
        "[c] close window only",
        Style::default().fg(theme.text_active),
    );
    render_row(
        frame,
        2,
        "[n] cancel",
        Style::default().fg(theme.text_muted),
    );
    if let Some(err) = error {
        render_row(frame, 4, &err, Style::default().fg(theme.status_error));
    }
}

pub(super) fn render_repo_popup(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let theme = &state.theme;
    let repos = state.repo_names();
    if repos.is_empty() {
        return;
    }

    let max_name_len = repos.iter().map(|r| display_width(r)).max().unwrap_or(3);
    // Width: padding(1 left + 1 right) + name + borders(2)
    let popup_width = (max_name_len + 4).min(area.width as usize).max(10) as u16;
    let popup_height = (repos.len() as u16 + 2).min(area.height.saturating_sub(2)); // +2 for borders

    // Right-aligned, below the 2-row header
    let popup_x = area.x + area.width.saturating_sub(popup_width);
    let popup_y = area.y + 2;

    let popup_rect = Rect::new(popup_x, popup_y, popup_width, popup_height);
    state.popup.set_repo_area(Some(popup_rect));

    frame.render_widget(Clear, popup_rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent));
    let inner = block.inner(popup_rect);
    frame.render_widget(block, popup_rect);

    let inner_width = inner.width as usize;
    for (i, name) in repos.iter().enumerate() {
        if i >= inner.height as usize {
            break;
        }

        let is_highlighted = i == state.repo_popup_selected();
        let is_current = match &state.global.repo_filter {
            RepoFilter::All => i == 0,
            RepoFilter::Repo(n) => *n == *name,
        };

        let truncated = truncate_to_width(name, inner_width.saturating_sub(1));
        let text = format!(" {}", truncated);
        let text_dw = display_width(&text);
        let padding = " ".repeat(inner_width.saturating_sub(text_dw));

        let style = if is_highlighted {
            Style::default()
                .fg(theme.text_active)
                .bg(theme.selection_bg)
        } else if is_current {
            Style::default().fg(theme.text_active)
        } else {
            Style::default().fg(theme.text_muted)
        };

        let line_rect = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{}{}", text, padding),
                style,
            ))),
            line_rect,
        );
    }
}

fn render_filter_bar_into(frame: &mut Frame, state: &AppState, area: Rect) {
    let line = filter_bar::render_filter_bar(state);
    frame.render_widget(Paragraph::new(vec![line]), area);
}

fn render_secondary_header_into(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let (line, notices_btn_col, repo_btn_col) =
        filter_bar::render_secondary_header(state, area.width);
    state.notices.button_col = notices_btn_col;
    state.layout.repo_button_col = repo_btn_col;
    frame.render_widget(Paragraph::new(vec![line]), area);
}

fn compute_scroll_offset(state: &mut AppState, total_lines: usize, list_area: Rect) -> usize {
    state.scrolls.panes.total_lines = total_lines;
    state.scrolls.panes.visible_height = list_area.height as usize;

    let visible_h = list_area.height as usize;
    let max_offset = total_lines.saturating_sub(visible_h);
    state.scrolls.panes.offset = state.scrolls.panes.offset.min(max_offset);

    // Auto-scroll to keep selected agent visible
    if state.focus_state.sidebar_focused
        && state.focus_state.focus == Focus::Panes
        && visible_h > 0
        && let Some((first, last)) = selected_block_lines(state)
    {
        let offset = state.scrolls.panes.offset;
        let block_height = last.saturating_sub(first).saturating_add(1);

        if block_height > visible_h {
            // An expanded capability tree may be taller than the viewport.
            // Keeping its trailing edge visible would make the next frame
            // scroll back to its leading edge, causing a visible flicker.
            // Keep an already intersecting viewport unchanged; when arriving
            // from outside, align the selected navigation line deterministically.
            let viewport_end = offset.saturating_add(visible_h);
            let intersects_block = offset <= last && viewport_end > first;
            if !intersects_block {
                let anchor = selected_navigation_line(state)
                    .filter(|line| (first..=last).contains(line))
                    .unwrap_or(first);
                state.scrolls.panes.offset = anchor.min(max_offset);
            }
        } else if first < offset {
            state.scrolls.panes.offset = first.saturating_sub(1);
        } else if last >= offset.saturating_add(visible_h) {
            state.scrolls.panes.offset = (last + 1).saturating_sub(visible_h);
        }
    }

    state.scrolls.panes.offset.min(max_offset)
}

fn selected_block_lines(state: &AppState) -> Option<(usize, usize)> {
    let selected_row = state.global.selected_pane_row;
    let first = state
        .layout
        .line_to_row
        .iter()
        .position(|mapping| *mapping == Some(selected_row))?;
    let last = state
        .layout
        .line_to_row
        .iter()
        .rposition(|mapping| *mapping == Some(selected_row))?;
    Some((first, last))
}

fn selected_navigation_line(state: &AppState) -> Option<usize> {
    use crate::state::NavigationTarget;

    match state.selected_navigation_target.as_ref() {
        Some(NavigationTarget::Tree(target)) => state
            .layout
            .tree_line_targets
            .iter()
            .find_map(|(line, candidate)| (candidate == target).then_some(*line)),
        Some(NavigationTarget::Subagent(target)) => state
            .layout
            .subagent_line_targets
            .iter()
            .find_map(|(line, candidate)| (candidate == target).then_some(*line)),
        Some(NavigationTarget::Pane { row }) => state
            .layout
            .line_to_row
            .iter()
            .position(|mapping| *mapping == Some(*row)),
        None => state
            .extensions
            .ui
            .selected
            .as_ref()
            .and_then(|target| {
                state
                    .layout
                    .tree_line_targets
                    .iter()
                    .find_map(|(line, candidate)| (candidate == target).then_some(*line))
            })
            .or_else(|| {
                state.selected_subagent_target.as_ref().and_then(|target| {
                    state
                        .layout
                        .subagent_line_targets
                        .iter()
                        .find_map(|(line, candidate)| (candidate == target).then_some(*line))
                })
            })
            .or_else(|| {
                state
                    .layout
                    .line_to_row
                    .iter()
                    .position(|mapping| *mapping == Some(state.global.selected_pane_row))
            }),
    }
}

fn render_pane_rows(
    frame: &mut Frame,
    lines: Vec<Line<'static>>,
    scroll_offset: usize,
    list_area: Rect,
) {
    let paragraph = Paragraph::new(lines).scroll((scroll_offset as u16, 0));
    frame.render_widget(paragraph, list_area);
}

fn render_flash_banner_into(frame: &mut Frame, state: &mut AppState, area: Rect) {
    // Render flash banner (spawn / remove feedback) before popups so
    // popups stay on top.
    if let Some(text) = state.take_flash() {
        let flash_y = area.y + area.height.saturating_sub(1);
        let flash_rect = Rect::new(area.x, flash_y, area.width, 1);
        frame.render_widget(Clear, flash_rect);
        let theme = &state.theme;
        let color = if text.contains("failed") {
            theme.status_error
        } else {
            theme.accent
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color)))),
            flash_rect,
        );
    }
}

pub fn draw_agents(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let layout = PaneLayout::compute(area);
    render_filter_bar_into(frame, state, layout.filter_area);
    render_secondary_header_into(frame, state, layout.secondary_area);

    let row_collector::CollectedRows {
        lines,
        line_to_row,
        pending_spawn,
        pending_remove,
        pending_subagents,
        pending_tree,
    } = row_collector::collect(state, layout.list_area.width);
    state.layout.line_to_row = line_to_row;
    state.layout.subagent_line_targets = pending_subagents
        .into_iter()
        .map(
            |(line, parent_pane_id, provider_id, session_id, agent_id, node_id)| {
                (
                    line,
                    crate::state::SubagentTarget {
                        parent_pane_id,
                        provider_id,
                        session_id,
                        agent_id,
                        node_id,
                    },
                )
            },
        )
        .collect();
    state.layout.tree_line_targets = pending_tree.into_iter().collect();
    let mut seen_panes = std::collections::HashSet::new();
    state.layout.navigation_targets = (0..lines.len())
        .filter_map(|line| {
            if let Some(target) = state.layout.subagent_line_targets.get(&line) {
                return Some(crate::state::NavigationTarget::Subagent(target.clone()));
            }
            if let Some(target) = state.layout.tree_line_targets.get(&line) {
                return Some(crate::state::NavigationTarget::Tree(target.clone()));
            }
            match state.layout.line_to_row.get(line).copied().flatten() {
                Some(row) if seen_panes.insert(row) => {
                    Some(crate::state::NavigationTarget::Pane { row })
                }
                _ => None,
            }
        })
        .collect();
    let scroll_offset = compute_scroll_offset(state, lines.len(), layout.list_area);
    click_targets::materialize(
        state,
        pending_spawn,
        pending_remove,
        scroll_offset,
        layout.list_area,
    );
    render_pane_rows(frame, lines, scroll_offset, layout.list_area);

    render_flash_banner_into(frame, state, area);
    popups::render_if_open(frame, state, area);
}

/// Inline capability details intentionally take over the complete sidebar.
/// This keeps source text in context without creating a popup or tmux pane.
pub fn draw_detail(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let Some(detail) = state.extensions.ui.detail.as_ref() else {
        return;
    };
    let theme = &state.theme;
    let title = truncate_to_width(
        &format!(" {} ", detail.title),
        area.width.saturating_sub(2) as usize,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let source = if detail.source.is_empty() {
        "Source: unavailable".to_string()
    } else {
        format!("Source: {}", detail.source)
    };
    let source = truncate_to_width(&source, inner.width as usize);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            source,
            Style::default().fg(theme.text_muted),
        ))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let text_area = Rect::new(
        inner.x,
        inner.y.saturating_add(1),
        inner.width,
        inner.height.saturating_sub(1),
    );
    frame.render_widget(
        Paragraph::new(detail.text.clone())
            .style(Style::default().fg(theme.text_active))
            .wrap(Wrap { trim: false })
            .scroll((detail.scroll.min(u16::MAX as usize) as u16, 0)),
        text_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{NavigationTarget, TreeTarget};

    fn focused_scroll_state(line_to_row: Vec<Option<usize>>, offset: usize) -> AppState {
        let mut state = AppState::new("%sidebar".into());
        state.focus_state.sidebar_focused = true;
        state.focus_state.focus = Focus::Panes;
        state.global.selected_pane_row = 0;
        state.layout.line_to_row = line_to_row;
        state.scrolls.panes.offset = offset;
        state
    }

    fn list_area(height: u16) -> Rect {
        Rect::new(0, 0, 40, height)
    }

    #[test]
    fn oversized_selected_block_never_alternates_between_edges() {
        for initial_offset in [0, 18] {
            let mut state = focused_scroll_state(vec![Some(0); 36], initial_offset);
            for _ in 0..30 {
                assert_eq!(
                    compute_scroll_offset(&mut state, 36, list_area(18)),
                    initial_offset
                );
            }
        }
    }

    #[test]
    fn oversized_tree_anchors_the_selected_navigation_line_when_entering() {
        let mut lines = vec![None; 20];
        lines.extend(std::iter::repeat_n(Some(0), 36));
        lines.extend(std::iter::repeat_n(None, 4));
        let mut state = focused_scroll_state(lines, 0);
        let target = TreeTarget {
            parent_pane_id: "%1".into(),
            provider_id: "claude".into(),
            session_id: None,
            agent_id: "agent".into(),
            node_id: "skill:review".into(),
            detail_token: None,
            inline_detail: None,
            is_disclosure: false,
        };
        state.layout.tree_line_targets.insert(38, target.clone());
        state.selected_navigation_target = Some(NavigationTarget::Tree(target));

        assert_eq!(compute_scroll_offset(&mut state, 60, list_area(18)), 38);
        assert_eq!(compute_scroll_offset(&mut state, 60, list_area(18)), 38);
    }

    #[test]
    fn shrinking_viewport_preserves_an_intersecting_oversized_block() {
        let mut lines = vec![None; 20];
        lines.extend(std::iter::repeat_n(Some(0), 36));
        lines.extend(std::iter::repeat_n(None, 4));
        let mut state = focused_scroll_state(lines, 0);

        assert_eq!(compute_scroll_offset(&mut state, 60, list_area(40)), 16);
        assert_eq!(compute_scroll_offset(&mut state, 60, list_area(18)), 16);
        assert_eq!(compute_scroll_offset(&mut state, 60, list_area(8)), 16);
    }

    #[test]
    fn normal_selected_block_uses_existing_autoscroll_and_clamps_offset() {
        let mut lines = vec![None; 20];
        lines.extend(std::iter::repeat_n(Some(0), 3));
        lines.extend(std::iter::repeat_n(None, 7));
        let mut state = focused_scroll_state(lines, 0);

        assert_eq!(compute_scroll_offset(&mut state, 30, list_area(18)), 5);
        assert_eq!(compute_scroll_offset(&mut state, 30, list_area(18)), 5);

        state.scrolls.panes.offset = 99;
        assert_eq!(compute_scroll_offset(&mut state, 30, list_area(18)), 12);
    }

    #[test]
    fn pane_layout_splits_area_into_filter_secondary_list() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let layout = PaneLayout::compute(area);
        assert_eq!(layout.filter_area.x, 0);
        assert_eq!(layout.filter_area.y, 0);
        assert_eq!(layout.filter_area.width, 40);
        assert_eq!(layout.filter_area.height, 1);
        assert_eq!(layout.secondary_area.y, 1);
        assert_eq!(layout.secondary_area.height, 1);
        assert_eq!(layout.list_area.y, 2);
        assert_eq!(layout.list_area.height, 18);
        assert_eq!(layout.list_area.width, 40);
    }

    #[test]
    fn pane_layout_handles_tiny_area() {
        // Only 1 row available — filter gets it, secondary and list collapse to 0.
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 1,
        };
        let layout = PaneLayout::compute(area);
        assert_eq!(layout.filter_area.height, 1);
        assert_eq!(layout.secondary_area.height, 0);
        assert_eq!(layout.list_area.height, 0);
    }

    #[test]
    fn pane_layout_handles_zero_height() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 0,
        };
        let layout = PaneLayout::compute(area);
        assert_eq!(layout.filter_area.height, 0);
        assert_eq!(layout.secondary_area.height, 0);
        assert_eq!(layout.list_area.height, 0);
    }

    #[test]
    fn pane_layout_respects_non_zero_origin() {
        let area = Rect {
            x: 5,
            y: 10,
            width: 30,
            height: 15,
        };
        let layout = PaneLayout::compute(area);
        assert_eq!(layout.filter_area.x, 5);
        assert_eq!(layout.filter_area.y, 10);
        assert_eq!(layout.secondary_area.x, 5);
        assert_eq!(layout.secondary_area.y, 11);
        assert_eq!(layout.list_area.x, 5);
        assert_eq!(layout.list_area.y, 12);
        assert_eq!(layout.list_area.height, 13);
    }
}
