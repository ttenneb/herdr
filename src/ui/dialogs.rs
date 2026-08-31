use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
    Frame,
};

use super::text::{display_width_u16, truncate_end};
use super::widgets::{
    action_button_row_rects, centered_popup_rect, panel_contrast_fg, render_action_button,
    render_modal_header, render_modal_shell, render_panel_shell, ActionButtonSpec,
};
use crate::app::{state::WorktreeOpenState, AppState, Mode};
use crate::terminal::TerminalRuntimeRegistry;

const NEW_LINKED_WORKTREE_POPUP_WIDTH: u16 = 68;
const NEW_LINKED_WORKTREE_POPUP_HEIGHT: u16 = 15;

const COLLECTION_CLOSE_POPUP_WIDTH: u16 = 76;
const COLLECTION_CLOSE_POPUP_HEIGHT: u16 = 12;

pub(crate) fn collection_close_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(
        area,
        COLLECTION_CLOSE_POPUP_WIDTH,
        COLLECTION_CLOSE_POPUP_HEIGHT,
    )
}

pub(crate) fn collection_close_button_rects(inner: Rect) -> (Rect, Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("p"),
                label: "promote to tabs",
            },
            ActionButtonSpec {
                hint: Some("c"),
                label: "cascade close",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1], rects[2])
}

pub(super) fn render_collection_close_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(pending) = app.pending_collection_close.as_ref() else {
        return;
    };
    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        COLLECTION_CLOSE_POPUP_WIDTH,
        COLLECTION_CLOSE_POPUP_HEIGHT,
        &app.palette,
    ) else {
        return;
    };
    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        if pending.cleanup_archive {
            "clear collection archive"
        } else {
            "close terminal collection"
        },
        &app.palette,
    );
    let counts = format!(
        "{} active · {} archived · {} live · {} exited",
        pending.active, pending.archived, pending.live, pending.exited
    );
    frame.render_widget(
        Paragraph::new(counts).style(Style::default().fg(app.palette.text)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );
    let warning = if pending.working > 0 || pending.blocked > 0 {
        format!(
            "Warning: {} working and {} blocked terminal(s) would be terminated.",
            pending.working, pending.blocked
        )
    } else if pending.live > 0 {
        format!(
            "Warning: cascade close terminates {} live terminal(s).",
            pending.live
        )
    } else {
        "Cascade close permanently removes all member terminals.".to_owned()
    };
    frame.render_widget(
        Paragraph::new(warning)
            .style(Style::default().fg(app.palette.peach))
            .wrap(Wrap { trim: true }),
        Rect::new(inner.x, inner.y.saturating_add(4), inner.width, 2),
    );
    frame.render_widget(
        Paragraph::new(if pending.cleanup_archive {
            "Only archived members are closed; active members and the collection remain."
        } else {
            "Promote keeps processes running in one standalone tab per member."
        })
        .style(Style::default().fg(app.palette.subtext0)),
        Rect::new(inner.x, inner.y.saturating_add(7), inner.width, 1),
    );
    let (promote, cascade, cancel) = collection_close_button_rects(inner);
    if !pending.cleanup_archive {
        render_action_button(
            frame,
            promote,
            Some("p"),
            "promote to tabs",
            Style::default()
                .fg(panel_contrast_fg(&app.palette))
                .bg(app.palette.accent)
                .add_modifier(Modifier::BOLD),
        );
    }
    render_action_button(
        frame,
        cascade,
        Some("c"),
        "cascade close",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(crate) fn rename_button_rects(inner: Rect) -> (Rect, Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "save",
            },
            ActionButtonSpec {
                hint: Some("^c"),
                label: "clear",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1], rects[2])
}

/// Draws the shared `name_input` field and puts the host cursor on its caret.
///
/// IMEs draw their composition preview at the host terminal cursor. Without an
/// explicit cursor the frame carries none, the client keeps the position last
/// reported by the focused pane, and composition lands behind the dialog.
fn render_name_input_field(app: &AppState, frame: &mut Frame, input_rect: Rect) {
    frame.render_widget(Clear, input_rect);

    // The text stops one column short of the field so the clamped caret always
    // lands on a blank cell: a host terminal inverts the cell under its cursor,
    // and an IME composes there.
    let text_rect = Rect {
        width: input_rect.width.saturating_sub(1),
        ..input_rect
    };
    frame.render_widget(
        Paragraph::new(format!(" {}", app.name_input)).style(
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0),
        ),
        text_rect,
    );

    if input_rect.width == 0 {
        return;
    }
    let caret_x = input_rect
        .x
        .saturating_add(1)
        .saturating_add(display_width_u16(&app.name_input))
        .min(input_rect.right().saturating_sub(1));
    frame.set_cursor_position((caret_x, input_rect.y));
}

pub(super) fn render_rename_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    super::dim_background(frame, area);

    let title = match app.mode {
        Mode::RenameWorkspace if app.pending_workspace_create_cwd.is_some() => {
            "new standalone space"
        }
        Mode::RenameWorkspace if app.rename_repository_target.is_some() => "rename repository",
        Mode::RenameWorkspace
            if app
                .workspaces
                .get(app.selected)
                .is_some_and(|workspace| workspace.checkout.is_some()) =>
        {
            "rename checkout"
        }
        Mode::RenameWorkspace => "rename space",
        Mode::RenameTab if app.creating_new_tab => "new tab",
        Mode::RenameTab => "rename tab",
        Mode::RenamePane => "rename pane",
        _ => return,
    };

    let Some(inner) = render_modal_shell(frame, area, 56, 7, &app.palette) else {
        return;
    };
    if inner.height < 4 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<5>(inner);

    render_modal_header(frame, rows[0], title, &app.palette);

    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    render_name_input_field(app, frame, input_rect);

    let (save_rect, clear_rect, cancel_rect) = rename_button_rects(inner);

    render_action_button(
        frame,
        save_rect,
        Some("↵"),
        "save",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        clear_rect,
        Some("^c"),
        "clear",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(crate) fn new_linked_worktree_inner_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(
        area,
        NEW_LINKED_WORKTREE_POPUP_WIDTH,
        NEW_LINKED_WORKTREE_POPUP_HEIGHT,
    )
    .map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn new_linked_worktree_base_change_rect(inner: Rect) -> Rect {
    let width = 10.min(inner.width);
    Rect::new(
        inner.x + inner.width.saturating_sub(width),
        inner.y.saturating_add(4),
        width,
        1,
    )
}

pub(crate) fn new_linked_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "create and open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn remove_worktree_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 72, 10)
}

pub(crate) fn remove_worktree_button_rects(inner: Rect, force_confirmation: bool) -> (Rect, Rect) {
    let primary_label = if force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: primary_label,
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(crate) fn open_existing_worktree_inner_rect(area: Rect, entry_count: usize) -> Option<Rect> {
    let height = (entry_count as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    centered_popup_rect(area, 96, height).map(|popup| {
        Rect::new(
            popup.x + 1,
            popup.y + 1,
            popup.width.saturating_sub(2),
            popup.height.saturating_sub(2),
        )
    })
}

pub(crate) fn open_existing_worktree_max_visible_rows(inner: Rect) -> usize {
    usize::from(inner.height.saturating_sub(5) / 2)
}

pub(crate) fn open_existing_worktree_visible_start(
    open: &WorktreeOpenState,
    max_rows: usize,
) -> usize {
    let filtered = open.filtered_indices();
    let selected = open.selected_entry_index().unwrap_or(open.selected);
    let selected_pos = filtered
        .iter()
        .position(|idx| *idx == selected)
        .unwrap_or(0);
    selected_pos.saturating_sub(max_rows.saturating_sub(1))
}

pub(crate) fn open_existing_worktree_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "open",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        inner.height.saturating_sub(1),
    );
    (rects[0], rects[1])
}

pub(super) fn render_new_linked_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(create) = app.worktree_create.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(inner) = render_modal_shell(
        frame,
        area,
        NEW_LINKED_WORKTREE_POPUP_WIDTH,
        NEW_LINKED_WORKTREE_POPUP_HEIGHT,
        &app.palette,
    ) else {
        return;
    };
    if inner.height < 12 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<10>(inner);

    render_modal_header(
        frame,
        rows[0],
        &format!("new worktree in {}", create.repo_name),
        &app.palette,
    );

    frame.render_widget(
        Paragraph::new(" branch").style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    let input_rect = Rect::new(rows[2].x, rows[2].y, rows[2].width, 1);
    if create.editing_base {
        frame.render_widget(Clear, input_rect);
        frame.render_widget(
            Paragraph::new(format!(" {}", create.branch)).style(
                Style::default()
                    .fg(app.palette.text)
                    .bg(app.palette.surface0),
            ),
            input_rect,
        );
    } else {
        render_name_input_field(app, frame, input_rect);
    }

    frame.render_widget(
        Paragraph::new(" base").style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    if create.editing_base {
        render_name_input_field(app, frame, rows[4]);
    } else {
        let short_commit = create.base_commit.chars().take(8).collect::<String>();
        let base_detail = if create.branch_exists {
            format!(" existing branch {} (base ignored)", create.branch)
        } else {
            format!(" {} @ {}", create.base_ref, short_commit)
        };
        frame.render_widget(
            Paragraph::new(base_detail).style(
                Style::default()
                    .fg(if create.branch_exists {
                        app.palette.yellow
                    } else {
                        app.palette.subtext0
                    })
                    .bg(app.palette.panel_bg),
            ),
            rows[4],
        );
    }
    if !create.branch_exists && !create.editing_base {
        let change = new_linked_worktree_base_change_rect(inner);
        frame.render_widget(
            Paragraph::new("[change]")
                .alignment(ratatui::layout::Alignment::Right)
                .style(Style::default().fg(app.palette.accent)),
            change,
        );
    }

    let checkout = create.checkout_path.display().to_string();
    frame.render_widget(
        Paragraph::new(" checkout").style(Style::default().fg(app.palette.overlay0)),
        rows[5],
    );
    frame.render_widget(
        Paragraph::new(format!(" {checkout}")).style(Style::default().fg(app.palette.subtext0)),
        rows[6],
    );

    if create.creating {
        frame.render_widget(
            Paragraph::new(" creating…").style(Style::default().fg(app.palette.overlay0)),
            rows[7],
        );
    } else if let Some(error) = &create.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}"))
                .style(Style::default().fg(app.palette.red))
                .wrap(Wrap { trim: false }),
            rows[7],
        );
    }

    let (create_rect, cancel_rect) = new_linked_worktree_button_rects(inner);
    render_action_button(
        frame,
        create_rect,
        Some("↵"),
        "create and open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_remove_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(remove) = app.worktree_remove.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let Some(popup) = remove_worktree_popup_rect(area) else {
        return;
    };
    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas::<8>(inner);

    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " delete worktree checkout?",
            Style::default()
                .fg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        )])),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(" This removes the checkout folder:")
            .style(Style::default().fg(app.palette.overlay0)),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(format!(" {}", remove.path.display()))
            .style(Style::default().fg(app.palette.text)),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(" The branch is not deleted. The Herdr workspace will close.")
            .style(Style::default().fg(app.palette.overlay0)),
        rows[3],
    );
    if remove.force_confirmation {
        frame.render_widget(
            Paragraph::new(" Dirty or untracked files will be permanently deleted.")
                .style(Style::default().fg(app.palette.red)),
            rows[4],
        );
    }
    if remove.removing {
        frame.render_widget(
            Paragraph::new(" removing…").style(Style::default().fg(app.palette.overlay0)),
            rows[5],
        );
    } else if let Some(error) = &remove.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            rows[5],
        );
    }

    let (remove_rect, cancel_rect) = remove_worktree_button_rects(inner, remove.force_confirmation);
    let remove_label = if remove.force_confirmation {
        "delete anyway"
    } else {
        "remove"
    };
    render_action_button(
        frame,
        remove_rect,
        Some("↵"),
        remove_label,
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.red)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

pub(super) fn render_open_existing_worktree_overlay(app: &AppState, frame: &mut Frame, area: Rect) {
    let Some(open) = app.worktree_open.as_ref() else {
        return;
    };

    super::dim_background(frame, area);
    let height = (open.entries.len() as u16)
        .saturating_mul(2)
        .saturating_add(7)
        .clamp(12, 26);
    let Some(inner) = render_modal_shell(frame, area, 96, height, &app.palette) else {
        return;
    };
    if inner.height < 8 {
        return;
    }

    render_modal_header(
        frame,
        Rect::new(inner.x, inner.y, inner.width, 1),
        "open worktree",
        &app.palette,
    );
    render_open_worktree_search(
        app,
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        open,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(inner.width as usize))
            .style(Style::default().fg(app.palette.surface1)),
        Rect::new(inner.x, inner.y.saturating_add(2), inner.width, 1),
    );

    let filtered = open.filtered_indices();
    let max_rows = open_existing_worktree_max_visible_rows(inner);
    let start = open_existing_worktree_visible_start(open, max_rows);
    for (visible_idx, entry_idx) in filtered.iter().skip(start).take(max_rows).enumerate() {
        let Some(entry) = open.entries.get(*entry_idx) else {
            continue;
        };
        let selected = Some(*entry_idx) == open.selected_entry_index();
        let y = inner.y.saturating_add(3 + (visible_idx as u16 * 2));
        let marker = if selected { "›" } else { " " };
        let row_style = if selected {
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.palette.subtext0)
        };
        let path_style = if selected {
            Style::default()
                .fg(app.palette.subtext0)
                .bg(app.palette.surface0)
        } else {
            Style::default().fg(app.palette.overlay0)
        };
        let status = entry.status_label();
        let title_width = inner
            .width
            .saturating_sub(display_width_u16(status))
            .saturating_sub(4) as usize;
        let mut title = format!(
            "{marker} {}",
            truncate_end(&entry.display_name(), title_width)
        );
        if !status.is_empty() {
            let pad = inner
                .width
                .saturating_sub(display_width_u16(&title))
                .saturating_sub(display_width_u16(status))
                .max(1);
            title.push_str(&" ".repeat(pad as usize));
            title.push_str(status);
        }
        frame.render_widget(
            Paragraph::new(truncate_end(&title, inner.width as usize)).style(row_style),
            Rect::new(inner.x, y, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(truncate_end(
                &format!("  {}", entry.path.display()),
                inner.width as usize,
            ))
            .style(path_style),
            Rect::new(inner.x, y.saturating_add(1), inner.width, 1),
        );
    }

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(" no matching worktrees")
                .style(Style::default().fg(app.palette.overlay0)),
            Rect::new(inner.x, inner.y.saturating_add(3), inner.width, 1),
        );
    }

    if let Some(error) = &open.error {
        frame.render_widget(
            Paragraph::new(format!(" {error}")).style(Style::default().fg(app.palette.red)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(2),
                inner.width,
                1,
            ),
        );
    }

    let (open_rect, cancel_rect) = open_existing_worktree_button_rects(inner);
    render_action_button(
        frame,
        open_rect,
        Some("↵"),
        "open",
        Style::default()
            .fg(panel_contrast_fg(&app.palette))
            .bg(app.palette.accent)
            .add_modifier(Modifier::BOLD),
    );
    render_action_button(
        frame,
        cancel_rect,
        Some("esc"),
        "cancel",
        Style::default()
            .fg(app.palette.text)
            .bg(app.palette.surface0)
            .add_modifier(Modifier::BOLD),
    );
}

fn render_open_worktree_search(
    app: &AppState,
    frame: &mut Frame,
    area: Rect,
    open: &WorktreeOpenState,
) {
    let focus_style = if open.search_focused {
        Style::default()
            .fg(app.palette.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.palette.overlay0)
    };
    let filtered_count = open.filtered_indices().len();
    let count = if open.query.trim().is_empty() {
        format!("{} checkouts", open.entries.len())
    } else {
        format!("{filtered_count}/{} checkouts", open.entries.len())
    };
    let mut spans = vec![Span::styled(" / ", focus_style)];
    if open.query.trim().is_empty() {
        spans.push(Span::styled(
            "filter worktrees",
            Style::default().fg(app.palette.overlay0),
        ));
    } else {
        spans.push(Span::styled(
            open.query.clone(),
            Style::default().fg(app.palette.text),
        ));
    }
    spans.push(Span::styled(
        format!(
            "{count:>width$}",
            width = area.width.saturating_sub(18) as usize
        ),
        Style::default().fg(app.palette.overlay0),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn confirm_close_overlay_text(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
) -> (String, String) {
    let ws_name = app
        .workspaces
        .get(app.selected)
        .map(|ws| ws.display_name_from(&app.terminals, terminal_runtimes))
        .unwrap_or_else(|| "?".to_string());
    let group_member_indices = app
        .confirm_repository_close_target
        .as_deref()
        .and_then(|repository_id| app.repository(repository_id))
        .map(|repository| {
            repository
                .checkout_workspace_ids
                .iter()
                .filter_map(|id| {
                    app.workspaces
                        .iter()
                        .position(|workspace| &workspace.id == id)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let closes_group = app.confirm_repository_close_target.is_some();
    let pane_count = if closes_group {
        group_member_indices
            .iter()
            .filter_map(|idx| app.workspaces.get(*idx))
            .map(|ws| ws.public_pane_numbers.len())
            .sum()
    } else {
        app.workspaces
            .get(app.selected)
            .map(|ws| ws.public_pane_numbers.len())
            .unwrap_or(0)
    };

    let pane_text = if pane_count == 1 {
        "1 pane".to_string()
    } else {
        format!("{pane_count} panes")
    };
    let active_agent_count = group_member_indices
        .iter()
        .filter_map(|idx| app.workspaces.get(*idx))
        .flat_map(|workspace| workspace.tabs.iter())
        .flat_map(|tab| tab.panes.values())
        .filter(|pane| {
            app.terminals
                .get(&pane.attached_terminal_id)
                .is_some_and(|terminal| terminal.effective_agent_label().is_some())
        })
        .count();
    let agent_text = if active_agent_count == 1 {
        ", 1 active agent".to_string()
    } else if active_agent_count > 1 {
        format!(", {active_agent_count} active agents")
    } else {
        String::new()
    };
    let workspace_text = if closes_group {
        let count = group_member_indices.len();
        if count == 1 {
            "1 checkout, ".to_string()
        } else {
            format!("{count} checkouts, ")
        }
    } else {
        String::new()
    };

    let title = if closes_group {
        "Close repository?"
    } else {
        "Close workspace?"
    };
    let detail = format!("{ws_name} — {workspace_text}{pane_text}{agent_text}");
    (title.to_string(), detail)
}

pub(super) fn render_confirm_close_overlay(
    app: &AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    frame: &mut Frame,
    area: Rect,
) {
    let (title, detail) = confirm_close_overlay_text(app, terminal_runtimes);

    super::dim_background(frame, area);

    let Some(popup) = confirm_close_popup_rect(area) else {
        return;
    };

    let warn = Style::default()
        .fg(app.palette.red)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(app.palette.overlay0);

    let title_line = Line::from(vec![Span::styled(format!(" {title}"), warn)]);

    let detail_line = Line::from(vec![
        Span::styled(
            format!(" {}", detail.split(" — ").next().unwrap_or(&detail)),
            Style::default()
                .fg(app.palette.text)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            detail
                .split_once(" — ")
                .map(|(_, rest)| format!(" — {rest}"))
                .unwrap_or_default(),
            dim,
        ),
    ]);

    let Some(inner) = render_panel_shell(frame, popup, app.palette.red, app.palette.panel_bg)
    else {
        return;
    };

    if inner.height >= 3 {
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas::<4>(inner);

        frame.render_widget(Paragraph::new(title_line), rows[0]);
        frame.render_widget(Paragraph::new(detail_line), rows[1]);

        let (confirm_rect, cancel_rect) = confirm_close_button_rects(inner);
        render_action_button(
            frame,
            confirm_rect,
            Some("↵"),
            "confirm",
            Style::default()
                .fg(panel_contrast_fg(&app.palette))
                .bg(app.palette.red)
                .add_modifier(Modifier::BOLD),
        );
        render_action_button(
            frame,
            cancel_rect,
            Some("esc"),
            "cancel",
            Style::default()
                .fg(app.palette.text)
                .bg(app.palette.surface0)
                .add_modifier(Modifier::BOLD),
        );
    }
}

pub(crate) fn confirm_close_popup_rect(area: Rect) -> Option<Rect> {
    centered_popup_rect(area, 64, 6)
}

pub(crate) fn confirm_close_button_rects(inner: Rect) -> (Rect, Rect) {
    let rects = action_button_row_rects(
        inner,
        &[
            ActionButtonSpec {
                hint: Some("↵"),
                label: "confirm",
            },
            ActionButtonSpec {
                hint: Some("esc"),
                label: "cancel",
            },
        ],
        2,
        3,
    );
    (rects[0], rects[1])
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{state::WorktreeCreateState, AppState, Mode},
        workspace::Workspace,
    };
    use ratatui::{
        backend::TestBackend,
        buffer::Buffer,
        layout::{Position, Rect},
        Terminal,
    };

    use super::{
        confirm_close_overlay_text, render_collection_close_overlay,
        render_new_linked_worktree_overlay, render_rename_overlay,
    };

    #[test]
    fn collection_close_dialog_shows_disposition_counts_and_state_warning() {
        let mut app = AppState::test_new();
        app.pending_collection_close = Some(crate::app::collection_view::PendingCollectionClose {
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            collection_id: crate::layout::CollectionId::from_raw(1).expect("valid collection id"),
            member_ids: Vec::new(),
            collection_revision: 0,
            cleanup_archive: false,
            active: 3,
            archived: 2,
            live: 4,
            exited: 1,
            working: 2,
            blocked: 1,
        });
        app.mode = Mode::CollectionClose;
        let backend = ratatui::backend::TestBackend::new(100, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| render_collection_close_overlay(&app, frame, frame.area()))
            .expect("draw");
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("3 active · 2 archived · 4 live · 1 exited"));
        assert!(text.contains("2 working and 1 blocked"));
        assert!(text.contains("promote to tabs"));
        assert!(text.contains("cascade close"));
    }

    #[test]
    fn confirm_close_text_uses_live_workspace_cwd_label() {
        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("initial");
        workspace.custom_name = None;
        workspace.identity_cwd = "/projects/original".into();
        let root_pane = workspace.tabs[0].root_pane.expect("test tab has root pane");
        let terminal_id = workspace.tabs[0].panes[&root_pane]
            .attached_terminal_id
            .clone();
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        app.terminals.get_mut(&terminal_id).unwrap().cwd = "/projects/current".into();
        app.selected = 0;

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let (title, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(title, "Close workspace?");
        assert_eq!(detail, "current — 1 pane");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn confirm_close_text_prefers_live_runtime_cwd_over_stale_terminal_cwd() {
        let root = std::env::temp_dir().join(format!(
            "herdr-confirm-close-runtime-cwd-{}",
            std::process::id()
        ));
        let stale_cwd = root.join("original");
        let live_cwd = root.join("current");
        std::fs::create_dir_all(&live_cwd).unwrap();

        let mut app = AppState::test_new();
        let mut workspace = Workspace::test_new("initial");
        workspace.custom_name = None;
        workspace.identity_cwd = stale_cwd.clone();
        let root_pane = workspace.tabs[0].root_pane.expect("test tab has root pane");
        let terminal_id = workspace.tabs[0].panes[&root_pane]
            .attached_terminal_id
            .clone();
        app.workspaces = vec![workspace];
        app.ensure_test_terminals();
        app.selected = 0;

        let (events, _) = tokio::sync::mpsc::channel(4);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            root_pane,
            24,
            80,
            live_cwd,
            0,
            crate::terminal_theme::TerminalTheme::default(),
            None,
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            &crate::pane::PaneLaunchEnv::default(),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .unwrap();
        let mut terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        terminal_runtimes.insert(terminal_id, runtime);

        let (_, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(detail, "current — 1 pane");

        drop(terminal_runtimes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn confirm_close_text_uses_selected_custom_name_instead_of_active_workspace_cwd() {
        let mut app = AppState::test_new();
        let active = Workspace::test_new("active");
        let selected = Workspace::test_new("selected");
        let selected_root = selected.tabs[0].root_pane.expect("test tab has root pane");
        let selected_terminal_id = selected.tabs[0].panes[&selected_root]
            .attached_terminal_id
            .clone();
        app.workspaces = vec![active, selected];
        app.ensure_test_terminals();
        app.terminals.get_mut(&selected_terminal_id).unwrap().cwd = "/projects/current".into();
        app.active = Some(0);
        app.selected = 1;

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let (_, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(detail, "selected — 1 pane");
    }

    #[test]
    fn confirm_close_text_reports_parent_group_scope() {
        let mut app = AppState::test_new();
        let mut parent = Workspace::test_new("main");
        parent.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr".into(),
            is_linked_worktree: false,
        });
        let mut child = Workspace::test_new("issue");
        child.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "repo-key".into(),
            label: "herdr".into(),
            repo_root: "/repo/herdr".into(),
            checkout_path: "/repo/herdr-issue".into(),
            is_linked_worktree: true,
        });
        app.workspaces = vec![parent, child];
        let ids = app
            .workspaces
            .iter()
            .map(|workspace| workspace.id.clone())
            .collect::<Vec<_>>();
        for (idx, workspace) in app.workspaces.iter_mut().enumerate() {
            workspace.checkout = Some(crate::repository::CheckoutProvenance {
                repository_id: "rtest".into(),
                checkout_path: workspace
                    .worktree_space
                    .as_ref()
                    .unwrap()
                    .checkout_path
                    .clone(),
                kind: if idx == 0 {
                    crate::repository::CheckoutKind::Primary
                } else {
                    crate::repository::CheckoutKind::Linked
                },
            });
        }
        app.repositories = vec![crate::repository::Repository {
            id: "rtest".into(),
            git_common_dir: "/repo/herdr/.git".into(),
            label: "herdr".into(),
            custom_name: None,
            preferred_base: None,
            checkout_workspace_ids: ids.clone(),
            last_focused_workspace_id: Some(ids[0].clone()),
        }];
        app.space_order = vec![crate::repository::SpaceRef::Repository("rtest".into())];
        app.confirm_repository_close_target = Some("rtest".into());
        app.selected = 0;

        let terminal_runtimes = crate::terminal::TerminalRuntimeRegistry::new();
        let (title, detail) = confirm_close_overlay_text(&app, &terminal_runtimes);

        assert_eq!(title, "Close repository?");
        assert_eq!(detail, "main — 2 checkouts, 2 panes");
    }

    #[test]
    fn new_worktree_error_renders_fatal_stderr_line() {
        let mut app = AppState::test_new();
        app.name_input = "foo".into();
        app.worktree_create = Some(WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            repo_key: "repo-key".into(),
            repository_id: None,
            repo_name: "herdr".into(),
            branch: "foo".into(),
            base_ref: "HEAD".into(),
            base_commit: "00000000".into(),
            editing_base: false,
            branch_exists: false,
            checkout_path: "/repo/.worktrees/herdr/foo".into(),
            error: Some(
                "Preparing worktree (new branch 'foo')\nfatal: a branch named 'foo' already exists"
                    .into(),
            ),
            creating: false,
        });

        let mut terminal =
            Terminal::new(TestBackend::new(100, 30)).expect("test terminal should initialize");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(&app, frame, Rect::new(0, 0, 100, 30)))
            .expect("new worktree overlay should render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("fatal: a branch named 'foo' already exists"));
        assert!(rendered.contains("[change]"));
    }

    #[test]
    fn new_worktree_hit_test_geometry_matches_modal_size() {
        let area = Rect::new(0, 0, 100, 30);
        let inner = super::new_linked_worktree_inner_rect(area).unwrap();
        let (create, cancel) = super::new_linked_worktree_button_rects(inner);
        let change = super::new_linked_worktree_base_change_rect(inner);

        assert_eq!(inner.width, super::NEW_LINKED_WORKTREE_POPUP_WIDTH - 2);
        assert_eq!(inner.height, super::NEW_LINKED_WORKTREE_POPUP_HEIGHT - 2);
        assert_eq!(create.y, inner.y + inner.height - 1);
        assert_eq!(cancel.y, inner.y + inner.height - 1);
        assert_eq!(change.y, inner.y + 4);
        assert_eq!(change.x + change.width, inner.x + inner.width);
    }

    const RENAME_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 80,
        height: 20,
    };
    const WORKTREE_AREA: Rect = Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 30,
    };

    /// Reproduces the input row that `render_rename_overlay` lays out: the
    /// centred popup, the border inset, then the third row of the vertical
    /// split.
    fn rename_input_rect(area: Rect) -> Rect {
        let popup = super::centered_popup_rect(area, 56, 7).expect("popup fits");
        let inner = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
        Rect::new(inner.x, inner.y + 2, inner.width, 1)
    }

    fn rename_overlay_caret_in(mode: Mode, name: &str) -> (Position, Buffer) {
        let mut app = AppState::test_new();
        app.mode = mode;
        app.name_input = name.into();

        let mut terminal = Terminal::new(TestBackend::new(RENAME_AREA.width, RENAME_AREA.height))
            .expect("test terminal");
        terminal
            .draw(|frame| render_rename_overlay(&app, frame, RENAME_AREA))
            .expect("rename overlay should render");
        let caret = terminal.get_cursor_position().expect("cursor position");
        (caret, terminal.backend().buffer().clone())
    }

    fn rename_overlay_caret(name: &str) -> Position {
        rename_overlay_caret_in(Mode::RenameWorkspace, name).0
    }

    fn worktree_overlay_caret(branch: &str) -> Position {
        let mut app = AppState::test_new();
        app.name_input = branch.into();
        app.worktree_create = Some(WorktreeCreateState {
            source_workspace_id: "source".into(),
            source_checkout_path: "/repo/herdr".into(),
            source_existing_membership: None,
            source_repo_root: "/repo/herdr".into(),
            repo_key: "repo-key".into(),
            repository_id: None,
            repo_name: "herdr".into(),
            branch: branch.into(),
            base_ref: "HEAD".into(),
            base_commit: "00000000".into(),
            editing_base: false,
            branch_exists: false,
            checkout_path: "/repo/.worktrees/herdr/foo".into(),
            error: None,
            creating: false,
        });

        let mut terminal =
            Terminal::new(TestBackend::new(WORKTREE_AREA.width, WORKTREE_AREA.height))
                .expect("test terminal");
        terminal
            .draw(|frame| render_new_linked_worktree_overlay(&app, frame, WORKTREE_AREA))
            .expect("new worktree overlay should render");
        terminal.get_cursor_position().expect("cursor position")
    }

    #[test]
    fn rename_overlay_anchors_the_host_cursor_to_the_input_caret() {
        let input = rename_input_rect(RENAME_AREA);

        // Without an explicit cursor the frame carries none, the client parks the
        // host cursor where the focused pane last reported it, and the IME
        // composes there instead of in the dialog.
        assert_eq!(
            rename_overlay_caret(""),
            Position::new(input.x + 1, input.y),
            "empty input should put the caret past the one-column left padding"
        );
        assert_eq!(
            rename_overlay_caret("abcd"),
            Position::new(input.x + 5, input.y)
        );

        // The cell under the caret has to be blank: a host terminal draws its
        // cursor by inverting that cell, so a glyph there would swallow it.
        let (caret, buffer) = rename_overlay_caret_in(Mode::RenameWorkspace, "ab");
        assert_eq!(caret, Position::new(input.x + 3, input.y));
        assert_eq!(buffer[(caret.x, caret.y)].symbol(), " ");
        assert_eq!(buffer[(caret.x - 1, caret.y)].symbol(), "b");
    }

    #[test]
    fn rename_overlay_anchors_the_cursor_in_every_rename_mode() {
        let input = rename_input_rect(RENAME_AREA);
        let expected = Position::new(input.x + 3, input.y);

        for mode in [Mode::RenameWorkspace, Mode::RenameTab, Mode::RenamePane] {
            assert_eq!(
                rename_overlay_caret_in(mode, "ab").0,
                expected,
                "{mode:?} should anchor the caret like the other rename modes"
            );
        }
    }

    #[test]
    fn rename_overlay_caret_counts_wide_characters_as_two_columns() {
        let input = rename_input_rect(RENAME_AREA);

        // "あい" is two columns per character, so the caret sits two cells further
        // right than the two-column "ab".
        assert_eq!(
            rename_overlay_caret("あい"),
            Position::new(input.x + 5, input.y)
        );
        assert_eq!(
            rename_overlay_caret("aあ"),
            Position::new(input.x + 4, input.y)
        );
    }

    #[test]
    fn rename_overlay_caret_stays_inside_the_input_when_the_name_overflows() {
        let input = rename_input_rect(RENAME_AREA);
        let last_column = input.right() - 1;

        // The field is 54 columns wide. 51 characters is the last name whose
        // caret still lands strictly inside it; from 52 on the unclamped column
        // would leave the field and gets pinned to the final cell.
        assert_eq!(
            rename_overlay_caret(&"a".repeat(51)),
            Position::new(input.x + 52, input.y)
        );
        assert_eq!(
            rename_overlay_caret(&"a".repeat(53)),
            Position::new(last_column, input.y)
        );
        assert_eq!(
            rename_overlay_caret(&"a".repeat(200)),
            Position::new(last_column, input.y)
        );

        // The clamped cell has to stay blank as well, or the host cursor would
        // sit on a glyph and the IME would compose over it.
        let (caret, buffer) = rename_overlay_caret_in(Mode::RenameWorkspace, &"a".repeat(200));
        assert_eq!(caret, Position::new(last_column, input.y));
        assert_eq!(buffer[(caret.x, caret.y)].symbol(), " ");
        assert_eq!(buffer[(caret.x - 1, caret.y)].symbol(), "a");
    }

    #[test]
    fn rename_overlay_caret_reaches_the_frame_the_server_sends() {
        let input = rename_input_rect(RENAME_AREA);
        let mut app = AppState::test_new();
        app.mode = Mode::RenameWorkspace;
        app.name_input = "ab".into();

        // The widget tests above stop at the ratatui frame. This one goes through
        // the server's cursor resolution, which is where the bug lived: the frame
        // used to leave here with `cursor: None`.
        let (_, cursor) =
            crate::server::render_stream::render_virtual(&mut app, RENAME_AREA, false);
        let cursor = cursor.expect("the modal caret should survive cursor resolution");

        assert_eq!((cursor.x, cursor.y), (input.x + 3, input.y));
        assert!(cursor.visible);
    }

    #[test]
    fn new_worktree_overlay_anchors_the_host_cursor_to_the_input_caret() {
        let popup = super::new_linked_worktree_inner_rect(WORKTREE_AREA).expect("popup fits");
        let input = Rect::new(popup.x, popup.y + 2, popup.width, 1);

        assert_eq!(
            worktree_overlay_caret(""),
            Position::new(input.x + 1, input.y)
        );
        assert_eq!(
            worktree_overlay_caret("ab"),
            Position::new(input.x + 3, input.y)
        );
        assert_eq!(
            worktree_overlay_caret("あい"),
            Position::new(input.x + 5, input.y)
        );
    }
}
