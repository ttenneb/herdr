use std::collections::{HashMap, HashSet};

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

use super::scrollbar::{
    render_scrollbar, render_scrollbar_clipped, reserve_terminal_scrollbar_gutter,
    should_show_scrollbar,
};

const COLLECTION_SELECTOR_WIDTH: usize = 2;
const COLLECTION_INDENT_WIDTH: usize = 2;

use crate::{
    app::{
        collection_view::{
            CollectionHitKind, CollectionHitRegion, CollectionLayout, CollectionRowView,
            CollectionSection,
        },
        AppState,
    },
    layout::{CollectionId, LayoutLeaf, PaneId},
    terminal::TerminalRuntimeRegistry,
};

fn clipped_line_rect(inner: Rect, virtual_y: usize, scroll: usize) -> Option<Rect> {
    let screen_y = inner.y as isize + virtual_y as isize - scroll as isize;
    (screen_y >= inner.y as isize && screen_y < inner.bottom() as isize).then_some(Rect::new(
        inner.x,
        screen_y as u16,
        inner.width,
        1,
    ))
}

fn clipped_block_rect(
    inner: Rect,
    virtual_y: usize,
    height: u16,
    scroll: usize,
) -> Option<(Rect, u16)> {
    let top = inner.y as isize + virtual_y as isize - scroll as isize;
    let bottom = top + height as isize;
    let visible_top = top.max(inner.y as isize);
    let visible_bottom = bottom.min(inner.bottom() as isize);
    (visible_bottom > visible_top).then_some((
        Rect::new(
            inner.x,
            visible_top as u16,
            inner.width,
            (visible_bottom - visible_top) as u16,
        ),
        (visible_top - top) as u16,
    ))
}

fn ordered_section(app: &AppState, members: &[PaneId]) -> Vec<(PaneId, usize, bool)> {
    let member_set: HashSet<_> = members.iter().copied().collect();
    let projected = app.delegations.preorder_for_panes(&member_set);
    let mut result = Vec::with_capacity(members.len());
    let mut included = HashSet::new();
    for entry in projected {
        if let Some(pane_id) = app
            .delegations
            .get(entry.id)
            .and_then(|record| record.pane_id)
        {
            included.insert(pane_id);
            result.push((pane_id, entry.depth, entry.external_parent_id.is_some()));
        }
    }
    result.extend(
        members
            .iter()
            .copied()
            .filter(|pane| !included.contains(pane))
            .map(|pane| (pane, 0, false)),
    );
    result
}

pub(crate) fn compute_collection_layouts(
    app: &mut AppState,
    terminal_runtimes: &TerminalRuntimeRegistry,
    area: Rect,
    resize_panes: bool,
    cell_size: crate::kitty_graphics::HostCellSize,
) -> Vec<CollectionLayout> {
    if resize_panes {
        app.collection_geometry.clear();
    }
    let Some(ws_idx) = app.active else {
        return Vec::new();
    };
    let Some(ws) = app.workspaces.get(ws_idx) else {
        return Vec::new();
    };
    let Some(tab) = ws.active_tab() else {
        return Vec::new();
    };
    let focused_leaf = tab.layout.focused_leaf();
    let zoomed = ws.zoomed;
    let collections: Vec<_> = tab.layout.collections().cloned().collect();
    let mut layouts = Vec::new();
    let mut desired_geometry = Vec::new();

    for collection in collections {
        let rect = if zoomed {
            if focused_leaf == LayoutLeaf::Collection(collection.id) {
                area
            } else {
                continue;
            }
        } else if let Some(rect) = tab
            .layout
            .leaf_rect(LayoutLeaf::Collection(collection.id), area)
        {
            rect
        } else {
            continue;
        };
        let collection_viewport = Rect::new(
            rect.x.saturating_add(1),
            rect.y.saturating_add(1),
            rect.width.saturating_sub(2),
            rect.height.saturating_sub(2),
        );
        // Collection scrolling owns an outer stable gutter. Child previews reserve a
        // second, independent gutter inside this content rectangle.
        let (inner, collection_gutter) = reserve_terminal_scrollbar_gutter(collection_viewport);
        let members = collection.members().to_vec();
        let view = app.collection_views.entry(collection.id).or_default();
        view.retain_members(&members);
        let view = view.clone();
        let maximized = view.maximized.filter(|pane| members.contains(pane));
        if let Some(pane_id) = maximized {
            let (preview_rect, gutter) = reserve_terminal_scrollbar_gutter(rect);
            let runtime = app.runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, pane_id);
            let scrollbar_rect = runtime
                .and_then(|rt| rt.scroll_metrics())
                .filter(|metrics| should_show_scrollbar(*metrics))
                .and(gutter);
            let mut hits = vec![CollectionHitRegion {
                collection_id: collection.id,
                pane_id: Some(pane_id),
                kind: CollectionHitKind::Preview,
                rect: preview_rect,
                terminal_row_offset: 0,
            }];
            if let Some(scrollbar_rect) = scrollbar_rect {
                hits.push(CollectionHitRegion {
                    collection_id: collection.id,
                    pane_id: Some(pane_id),
                    kind: CollectionHitKind::PreviewScrollbar,
                    rect: scrollbar_rect,
                    terminal_row_offset: 0,
                });
            }
            if resize_panes && preview_rect.width > 0 && preview_rect.height > 0 {
                desired_geometry.push((pane_id, preview_rect));
            }
            layouts.push(CollectionLayout {
                id: collection.id,
                rect,
                inner_rect: rect,
                active_header: None,
                archive_header: None,
                rows: Vec::new(),
                hits,
                content_height: rect.height as usize,
                viewport_height: rect.height as usize,
                scroll: 0,
                scrollbar_rect: None,
                maximized,
                maximized_preview_rect: Some(preview_rect),
                maximized_scrollbar_rect: scrollbar_rect,
            });
            continue;
        }

        let active: Vec<_> = collection.active_members().collect();
        let archived: Vec<_> = collection.archived_members().collect();
        let active = ordered_section(app, &active);
        let archived = ordered_section(app, &archived);
        let heights: HashMap<_, _> = members
            .iter()
            .map(|pane| {
                (
                    *pane,
                    view.preview_height_for_collection(*pane, rect.height),
                )
            })
            .collect();
        let expanded = view.expanded.clone();
        let mut content_height = 1usize;
        for (pane, _, _) in &active {
            content_height += 1 + if expanded.contains(pane) {
                heights[pane] as usize + 1
            } else {
                0
            };
        }
        if !archived.is_empty() {
            content_height += 1;
            for (pane, _, _) in &archived {
                content_height += 1 + if expanded.contains(pane) {
                    heights[pane] as usize + 1
                } else {
                    0
                };
            }
        }
        let viewport_height = inner.height as usize;
        let max_scroll = content_height.saturating_sub(viewport_height);
        let scroll = view.scroll.min(max_scroll);
        if let Some(stored) = app.collection_views.get_mut(&collection.id) {
            stored.scroll = scroll;
        }
        let mut y = 0usize;
        let active_header = clipped_line_rect(inner, y, scroll);
        y += 1;
        let mut rows = Vec::new();
        let mut hits = vec![CollectionHitRegion {
            collection_id: collection.id,
            pane_id: None,
            kind: CollectionHitKind::Chrome,
            rect: inner,
            terminal_row_offset: 0,
        }];
        if content_height > viewport_height {
            if let Some(gutter) = collection_gutter.filter(|rect| rect.width > 0 && rect.height > 0)
            {
                hits.push(CollectionHitRegion {
                    collection_id: collection.id,
                    pane_id: None,
                    kind: CollectionHitKind::CollectionScrollbar,
                    rect: gutter,
                    terminal_row_offset: 0,
                });
            }
        }

        let mut append_rows =
            |section: CollectionSection, entries: &[(PaneId, usize, bool)], y: &mut usize| {
                for (pane_id, depth, external_parent) in entries.iter().copied() {
                    let virtual_y = *y;
                    let row_rect = clipped_line_rect(inner, virtual_y, scroll).unwrap_or_default();
                    *y += 1;
                    let disclosure_offset = COLLECTION_SELECTOR_WIDTH
                        .saturating_add(depth.saturating_mul(COLLECTION_INDENT_WIDTH));
                    let disclosure_rect = u16::try_from(disclosure_offset)
                        .ok()
                        .and_then(|offset| row_rect.x.checked_add(offset))
                        .filter(|x| *x < row_rect.right())
                        .map(|x| Rect::new(x, row_rect.y, 1, 1));
                    let mut preview_rect = None;
                    let mut preview_scrollbar_rect = None;
                    let mut preview_row_offset = 0;
                    let mut resize_rect = None;
                    let mut preview_cols = inner.width;
                    if expanded.contains(&pane_id) {
                        if let Some((slot, offset)) =
                            clipped_block_rect(inner, *y, heights[&pane_id], scroll)
                        {
                            let (content, gutter) = reserve_terminal_scrollbar_gutter(slot);
                            preview_rect = Some(content);
                            preview_row_offset = offset;
                            preview_cols = content.width;
                            preview_scrollbar_rect = app
                                .runtime_for_pane_in_workspace(terminal_runtimes, ws_idx, pane_id)
                                .and_then(|rt| rt.scroll_metrics())
                                .filter(|metrics| should_show_scrollbar(*metrics))
                                .and(gutter);
                        } else {
                            preview_cols = reserve_terminal_scrollbar_gutter(Rect::new(
                                0,
                                0,
                                inner.width,
                                heights[&pane_id],
                            ))
                            .0
                            .width;
                        }
                        *y += heights[&pane_id] as usize;
                        resize_rect = clipped_line_rect(inner, *y, scroll);
                        *y += 1;
                    }
                    if row_rect.width > 0 {
                        hits.push(CollectionHitRegion {
                            collection_id: collection.id,
                            pane_id: Some(pane_id),
                            kind: CollectionHitKind::Row,
                            rect: row_rect,
                            terminal_row_offset: 0,
                        });
                        if let Some(rect) = disclosure_rect {
                            hits.push(CollectionHitRegion {
                                collection_id: collection.id,
                                pane_id: Some(pane_id),
                                kind: CollectionHitKind::Disclosure,
                                rect,
                                terminal_row_offset: 0,
                            });
                        }
                    }
                    if let Some(rect) = preview_rect {
                        hits.push(CollectionHitRegion {
                            collection_id: collection.id,
                            pane_id: Some(pane_id),
                            kind: CollectionHitKind::Preview,
                            rect,
                            terminal_row_offset: preview_row_offset,
                        });
                    }
                    if let Some(rect) = preview_scrollbar_rect {
                        hits.push(CollectionHitRegion {
                            collection_id: collection.id,
                            pane_id: Some(pane_id),
                            kind: CollectionHitKind::PreviewScrollbar,
                            rect,
                            terminal_row_offset: preview_row_offset,
                        });
                    }
                    if let Some(rect) = resize_rect {
                        hits.push(CollectionHitRegion {
                            collection_id: collection.id,
                            pane_id: Some(pane_id),
                            kind: CollectionHitKind::ResizeHandle,
                            rect,
                            terminal_row_offset: 0,
                        });
                    }
                    rows.push(CollectionRowView {
                        pane_id,
                        section,
                        depth,
                        external_parent,
                        virtual_y,
                        row_rect,
                        preview_rect,
                        preview_scrollbar_rect,
                        preview_size: expanded
                            .contains(&pane_id)
                            .then_some((heights[&pane_id], preview_cols)),
                        preview_row_offset,
                        resize_rect,
                    });
                }
            };
        append_rows(CollectionSection::Active, &active, &mut y);
        let archive_header = if archived.is_empty() {
            None
        } else {
            let rect = clipped_line_rect(inner, y, scroll);
            y += 1;
            append_rows(CollectionSection::Archived, &archived, &mut y);
            rect
        };

        if resize_panes {
            for row in &rows {
                if let Some((rows, cols)) = row
                    .preview_size
                    .filter(|(rows, cols)| *rows > 0 && *cols > 0)
                {
                    desired_geometry.push((row.pane_id, Rect::new(0, 0, cols, rows)));
                }
            }
        }
        layouts.push(CollectionLayout {
            id: collection.id,
            rect,
            inner_rect: inner,
            active_header,
            archive_header,
            rows,
            hits,
            content_height,
            viewport_height,
            scroll,
            scrollbar_rect: (content_height > viewport_height)
                .then_some(collection_gutter)
                .flatten()
                .filter(|rect| rect.width > 0 && rect.height > 0),
            maximized: None,
            maximized_preview_rect: None,
            maximized_scrollbar_rect: None,
        });
    }
    for (pane_id, rect) in desired_geometry {
        resize_preview(app, terminal_runtimes, ws_idx, pane_id, rect, cell_size);
    }
    layouts
}

fn resize_preview(
    app: &mut AppState,
    runtimes: &TerminalRuntimeRegistry,
    ws_idx: usize,
    pane_id: PaneId,
    rect: Rect,
    cell_size: crate::kitty_graphics::HostCellSize,
) {
    let Some(terminal_id) = app
        .workspaces
        .get(ws_idx)
        .and_then(|ws| ws.terminal_id(pane_id))
    else {
        return;
    };
    let terminal_id = terminal_id.clone();
    let geometry = crate::app::collection_view::TerminalGeometry {
        rows: rect.height,
        cols: rect.width,
        cell_width_px: cell_size.width_px,
        cell_height_px: cell_size.height_px,
    };
    app.collection_geometry
        .insert(terminal_id.clone(), geometry);
    if app.defer_collection_geometry_claims || app.direct_attach_resize_locks.contains(&terminal_id)
    {
        return;
    }
    if let Some(runtime) = runtimes.get(&terminal_id) {
        runtime.resize(
            geometry.rows,
            geometry.cols,
            geometry.cell_width_px,
            geometry.cell_height_px,
        );
    }
}

pub(crate) fn render_collections(
    app: &AppState,
    runtimes: &TerminalRuntimeRegistry,
    layouts: &[CollectionLayout],
    frame: &mut Frame,
) {
    let Some(ws_idx) = app.active else { return };
    let Some(tab) = app.workspaces.get(ws_idx).and_then(|ws| ws.active_tab()) else {
        return;
    };
    for layout in layouts {
        let collection = tab.collection(layout.id);
        let focused = tab.layout.focused_leaf() == LayoutLeaf::Collection(layout.id);
        let border_style = if focused {
            Style::default().fg(app.palette.accent)
        } else {
            Style::default().fg(app.palette.surface1)
        };
        let title = collection
            .and_then(|c| c.label.as_deref())
            .unwrap_or("Terminals");
        if layout.maximized.is_none() {
            frame.render_widget(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
                layout.rect,
            );
        }
        if let Some(pane_id) = layout.maximized {
            if let Some(runtime) = app.runtime_for_pane_in_workspace(runtimes, ws_idx, pane_id) {
                let preview_rect = layout.maximized_preview_rect.unwrap_or(layout.inner_rect);
                runtime.render(
                    frame,
                    preview_rect,
                    focused && collection_terminal_entered(app, layout.id),
                );
                if let (Some(track), Some(metrics)) =
                    (layout.maximized_scrollbar_rect, runtime.scroll_metrics())
                {
                    render_scrollbar(
                        frame,
                        metrics,
                        track,
                        app.palette.overlay0,
                        app.palette.overlay1,
                        "▐",
                    );
                }
            }
            continue;
        }
        if let Some(rect) = layout.active_header {
            let concurrent = collection
                .map(|collection| {
                    collection
                        .active_members()
                        .filter(|pane_id| {
                            tab.panes
                                .get(pane_id)
                                .and_then(|pane| app.terminals.get(&pane.attached_terminal_id))
                                .is_some_and(|terminal| {
                                    matches!(
                                        terminal.state,
                                        crate::detect::AgentState::Working
                                            | crate::detect::AgentState::Blocked
                                    )
                                })
                        })
                        .count()
                })
                .unwrap_or_default();
            let concurrency_warning = app.collection_lifecycle.concurrency > 0
                && concurrent > app.collection_lifecycle.concurrency;
            let label = if !layout
                .rows
                .iter()
                .any(|row| row.section == CollectionSection::Active)
            {
                "Active · empty".to_owned()
            } else if concurrency_warning {
                format!("Active · {concurrent} working/blocked ⚠ advisory limit")
            } else {
                "Active".to_owned()
            };
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(if concurrency_warning {
                    app.palette.peach
                } else {
                    app.palette.overlay1
                })),
                rect,
            );
        }
        if let Some(rect) = layout.archive_header {
            let archived = collection
                .map(|collection| collection.archived_members().count())
                .unwrap_or_default();
            let policy = app.collection_lifecycle;
            let age_warning = policy.archive_age_days > 0
                && collection.is_some_and(|collection| {
                    let limit = std::time::Duration::from_secs(
                        policy.archive_age_days.saturating_mul(24 * 60 * 60),
                    );
                    collection.archived_members().any(|pane| {
                        app.collection_archive_times
                            .get(&pane)
                            .and_then(|at| at.elapsed().ok())
                            .is_some_and(|age| age >= limit)
                    })
                });
            let warning =
                age_warning || (policy.archive_count > 0 && archived > policy.archive_count);
            let label = if warning {
                format!("Archived · {archived} ⚠ advisory limit")
            } else {
                format!("Archived · {archived}")
            };
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(if warning {
                    app.palette.peach
                } else {
                    app.palette.overlay1
                })),
                rect,
            );
        }
        if let Some(track) = layout.scrollbar_rect {
            let max_offset = layout.content_height.saturating_sub(layout.viewport_height);
            render_scrollbar(
                frame,
                crate::pane::ScrollMetrics {
                    offset_from_bottom: max_offset.saturating_sub(layout.scroll),
                    max_offset_from_bottom: max_offset,
                    viewport_rows: layout.viewport_height,
                },
                track,
                app.palette.surface_dim,
                app.palette.overlay1,
                "█",
            );
        }
        for row in &layout.rows {
            if row.row_rect.width == 0 {
                continue;
            }
            let selected = collection.and_then(|c| c.selected()) == Some(row.pane_id);
            let (state, seen, label) = tab
                .panes
                .get(&row.pane_id)
                .and_then(|pane| {
                    app.terminals
                        .get(&pane.attached_terminal_id)
                        .map(|terminal| {
                            let identity = terminal
                                .agent_name
                                .clone()
                                .or_else(|| {
                                    terminal
                                        .detected_agent
                                        .map(crate::detect::agent_label)
                                        .map(str::to_owned)
                                })
                                .or_else(|| terminal.manual_label.clone())
                                .or_else(|| {
                                    terminal
                                        .launch_argv
                                        .as_ref()
                                        .and_then(|argv| argv.first().cloned())
                                })
                                .unwrap_or_else(|| format!("pane {}", row.pane_id.raw()));
                            let label = app
                                .delegations
                                .delegation_for_pane(row.pane_id)
                                .and_then(|delegation| delegation.purpose.clone())
                                .unwrap_or(identity);
                            (terminal.state, pane.seen, label)
                        })
                })
                .unwrap_or((
                    crate::detect::AgentState::Unknown,
                    true,
                    format!("pane {}", row.pane_id.raw()),
                ));
            let (dot, dot_style) = super::status::state_dot(state, seen, &app.palette);
            let disclosure = if app
                .collection_views
                .get(&layout.id)
                .is_some_and(|v| v.expanded.contains(&row.pane_id))
            {
                "⌄"
            } else {
                "›"
            };
            let indent = " ".repeat(row.depth.saturating_mul(COLLECTION_INDENT_WIDTH));
            let external = if row.external_parent { "↰ " } else { "" };
            let spans = vec![
                Span::styled(
                    if selected { "▸ " } else { "  " },
                    if selected {
                        Style::default()
                            .fg(app.palette.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw(indent),
                Span::raw(disclosure),
                Span::raw(" "),
                Span::styled(dot, dot_style),
                Span::raw(" "),
                Span::styled(
                    format!("{external}{label}"),
                    Style::default().fg(app.palette.text),
                ),
            ];
            let style = if selected {
                Style::default().bg(app.palette.surface0)
            } else if row.section == CollectionSection::Archived {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            frame.render_widget(Paragraph::new(Line::from(spans)).style(style), row.row_rect);
            if let (Some(rect), Some((logical_rows, logical_cols))) =
                (row.preview_rect, row.preview_size)
            {
                if let Some(runtime) =
                    app.runtime_for_pane_in_workspace(runtimes, ws_idx, row.pane_id)
                {
                    runtime.render_clipped(
                        frame,
                        rect,
                        logical_rows,
                        logical_cols,
                        row.preview_row_offset,
                        0,
                        focused && selected && collection_terminal_entered(app, layout.id),
                    );
                    if let (Some(track), Some(metrics)) =
                        (row.preview_scrollbar_rect, runtime.scroll_metrics())
                    {
                        let child_focused = focused && selected;
                        render_scrollbar_clipped(
                            frame,
                            metrics,
                            track,
                            logical_rows,
                            row.preview_row_offset,
                            if child_focused {
                                app.palette.overlay0
                            } else {
                                app.palette.surface_dim
                            },
                            if child_focused {
                                app.palette.overlay1
                            } else {
                                app.palette.overlay0
                            },
                            if child_focused { "▐" } else { "▕" },
                        );
                    }
                }
            }
            if let Some(rect) = row.resize_rect {
                frame.render_widget(
                    Paragraph::new("─".repeat(rect.width as usize))
                        .style(Style::default().fg(app.palette.surface1)),
                    rect,
                );
            }
        }
    }
}

pub(crate) fn collection_terminal_entered(app: &AppState, id: CollectionId) -> bool {
    let selected = app
        .active
        .and_then(|idx| app.workspaces.get(idx))
        .and_then(|ws| ws.active_tab())
        .and_then(|tab| tab.collection(id))
        .and_then(|collection| collection.selected());
    app.collection_views
        .get(&id)
        .is_some_and(|view| view.terminal_entered(selected))
}

pub(crate) fn collection_preview_region(
    layouts: &[CollectionLayout],
    pane_id: PaneId,
) -> Option<(Rect, u16, u16, u16)> {
    for layout in layouts {
        if layout.maximized == Some(pane_id) {
            let rect = layout.maximized_preview_rect.unwrap_or(layout.inner_rect);
            return Some((rect, 0, rect.height, rect.width));
        }
        if let Some(row) = layout.rows.iter().find(|row| row.pane_id == pane_id) {
            if let (Some(rect), Some((rows, cols))) = (row.preview_rect, row.preview_size) {
                return Some((rect, row.preview_row_offset, rows, cols));
            }
        }
    }
    None
}

pub(crate) fn collection_preview_infos(
    layouts: &[CollectionLayout],
) -> impl Iterator<Item = (PaneId, Rect)> + '_ {
    layouts.iter().flat_map(|layout| {
        let maximized = layout.maximized.map(|pane| {
            (
                pane,
                layout.maximized_preview_rect.unwrap_or(layout.inner_rect),
            )
        });
        maximized.into_iter().chain(
            layout
                .rows
                .iter()
                .filter_map(|row| row.preview_rect.map(|rect| (row.pane_id, rect))),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{layout::LayoutLeaf, workspace::Workspace};
    use ratatui::{backend::TestBackend, layout::Direction, Terminal};

    #[test]
    fn empty_collection_has_chrome_and_active_header() {
        let mut ws = Workspace::test_new("collection");
        let root = ws.tabs[0].root_pane.expect("root");
        let id = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                Direction::Horizontal,
                0.5,
                Some("Helpers".into()),
            )
            .expect("collection");
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;
        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        assert_eq!(layouts.len(), 1);
        assert_eq!(layouts[0].id, id);
        assert!(layouts[0].rows.is_empty());
        assert!(layouts[0].active_header.is_some());
    }

    #[test]
    fn zoomed_collection_does_not_render_selected_member_as_tiled_pane() {
        let mut ws = Workspace::test_new("collection");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        ws.collect_pane(child, id).expect("collect");
        let _ = ws.tabs[0].layout.focus_leaf(LayoutLeaf::Collection(id));
        ws.zoomed = true;
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        let surface = crate::ui::compute_tab_surface(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        assert!(surface.pane_infos.is_empty());
        assert_eq!(surface.collection_layouts.len(), 1);
        assert_eq!(surface.collection_layouts[0].rect, Rect::new(0, 0, 80, 20));
    }

    #[test]
    fn fresh_expanded_preview_uses_half_height_and_manual_resize_stays_fixed() {
        let mut ws = Workspace::test_new("adaptive-preview");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let terminal_id = ws.terminal_id(child).expect("terminal ID").clone();
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        ws.collect_pane(child, id).expect("collect");
        ws.tabs[0].layout.focus_leaf(LayoutLeaf::Collection(id));
        ws.zoomed = true;

        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.collection_views
            .entry(id)
            .or_default()
            .expanded
            .insert(child);
        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 100),
            true,
            Default::default(),
        );
        assert_eq!(layouts[0].rows[0].preview_size.map(|size| size.0), Some(50));
        assert_eq!(app.collection_geometry[&terminal_id].rows, 50);

        app.collection_views
            .get_mut(&id)
            .expect("view")
            .set_preview_height(child, 11);
        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 60),
            true,
            Default::default(),
        );
        assert_eq!(layouts[0].rows[0].preview_size.map(|size| size.0), Some(11));
        assert_eq!(app.collection_geometry[&terminal_id].rows, 11);
    }

    #[test]
    fn disclosure_hit_matches_rendered_column_and_is_absent_when_clipped() {
        let mut ws = Workspace::test_new("nested-disclosure");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let grandchild = ws.test_split(Direction::Horizontal);
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        for pane in [child, grandchild] {
            ws.collect_pane(pane, id).expect("collect");
        }
        let mut app = AppState::test_new();
        let parent = app
            .delegations
            .create(Some(root), None, Some("parent".into()))
            .expect("parent delegation");
        let child_delegation = app
            .delegations
            .create(Some(child), Some(parent), Some("child".into()))
            .expect("child delegation");
        app.delegations
            .create(
                Some(grandchild),
                Some(child_delegation),
                Some("leaf".into()),
            )
            .expect("grandchild delegation");
        app.workspaces = vec![ws];
        app.active = Some(0);

        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        let row = layouts[0]
            .rows
            .iter()
            .find(|row| row.pane_id == grandchild)
            .expect("nested row");
        let disclosure = layouts[0]
            .hits
            .iter()
            .find(|hit| {
                hit.pane_id == Some(grandchild) && hit.kind == CollectionHitKind::Disclosure
            })
            .expect("disclosure hit");
        assert_eq!(
            disclosure.rect.x,
            row.row_rect.x
                + COLLECTION_SELECTOR_WIDTH as u16
                + (row.depth * COLLECTION_INDENT_WIDTH) as u16
        );
        assert_eq!(
            layouts[0]
                .hit_at(disclosure.rect.x, disclosure.rect.y)
                .map(|hit| hit.kind),
            Some(CollectionHitKind::Disclosure)
        );

        let narrow = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 4, 20),
            false,
            Default::default(),
        );
        assert!(!narrow[0].hits.iter().any(|hit| {
            hit.pane_id == Some(grandchild) && hit.kind == CollectionHitKind::Disclosure
        }));
    }

    #[test]
    fn empty_collection_render_includes_label_and_empty_state() {
        let mut ws = Workspace::test_new("collection");
        let root = ws.tabs[0].root_pane.expect("root");
        ws.create_collection_near(
            0,
            LayoutLeaf::Pane(root),
            Direction::Horizontal,
            0.5,
            Some("Helpers".into()),
        )
        .expect("collection");
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            false,
            Default::default(),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        terminal
            .draw(|frame| {
                render_collections(&app, &TerminalRuntimeRegistry::new(), &layouts, frame)
            })
            .expect("render");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Helpers"));
        assert!(rendered.contains("Active · empty"));
    }

    #[test]
    fn zero_width_collection_interior_does_not_draw_scrollbar_over_border() {
        let mut ws = Workspace::test_new("tiny-collection");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        ws.collect_pane(child, id).expect("collect");
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        let area = Rect::new(0, 0, 2, 4);
        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            area,
            false,
            Default::default(),
        );
        assert_eq!(layouts[0].inner_rect.width, 0);
        assert!(layouts[0].content_height > layouts[0].viewport_height);
        let mut terminal = Terminal::new(TestBackend::new(2, 4)).expect("terminal");
        terminal
            .draw(|frame| {
                render_collections(&app, &TerminalRuntimeRegistry::new(), &layouts, frame)
            })
            .expect("render");
        assert_ne!(terminal.backend().buffer()[(0, 1)].symbol(), "█");
    }

    #[test]
    fn geometry_projection_uses_full_logical_preview_not_clipped_render_rect() {
        let mut ws = Workspace::test_new("collection");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let terminal_id = ws.terminal_id(child).expect("terminal id").clone();
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        ws.collect_pane(child, id).expect("collect");
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.defer_collection_geometry_claims = true;
        let view = app.collection_views.entry(id).or_default();
        view.expanded.insert(child);
        view.scroll = 3;

        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            true,
            Default::default(),
        );
        let row = &layouts[0].rows[0];
        let preview = row.preview_rect.expect("preview");
        assert!(
            row.preview_row_offset > 0,
            "preview should be clipped at top"
        );
        assert!(preview.height < crate::app::collection_view::DEFAULT_PREVIEW_HEIGHT);
        assert_eq!(
            app.collection_geometry[&terminal_id].rows,
            crate::app::collection_view::DEFAULT_PREVIEW_HEIGHT
        );
        assert_eq!(app.collection_geometry[&terminal_id].cols, preview.width);

        app.collection_views
            .get_mut(&id)
            .expect("view")
            .expanded
            .clear();
        compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            true,
            Default::default(),
        );
        assert!(!app.collection_geometry.contains_key(&terminal_id));

        app.collection_views.get_mut(&id).expect("view").maximized = Some(child);
        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 20),
            true,
            Default::default(),
        );
        assert_eq!(layouts[0].inner_rect, layouts[0].rect);
        assert_eq!(
            app.collection_geometry[&terminal_id].rows,
            layouts[0].rect.height
        );
        assert_eq!(
            app.collection_geometry[&terminal_id].cols,
            layouts[0]
                .maximized_preview_rect
                .expect("maximized content")
                .width
        );
    }

    #[tokio::test]
    async fn expanded_child_and_collection_scrollbars_use_independent_gutters() {
        let mut ws = Workspace::test_new("collection-gutters");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        ws.collect_pane(child, id).expect("collect");
        for _ in 0..10 {
            let pane = ws.test_split(Direction::Horizontal);
            ws.collect_pane(pane, id).expect("collect extra");
        }
        let history = (0..40).map(|n| format!("line {n}\r\n")).collect::<String>();
        ws.tabs[0].runtimes.insert(
            child,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                40,
                8,
                16 * 1024,
                history.as_bytes(),
            ),
        );
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.collection_views
            .entry(id)
            .or_default()
            .expanded
            .insert(child);

        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 80, 14),
            false,
            Default::default(),
        );
        let layout = &layouts[0];
        let row = layout
            .rows
            .iter()
            .find(|row| row.pane_id == child)
            .expect("child row");
        let collection_track = layout.scrollbar_rect.expect("collection scrollbar");
        let child_track = row
            .preview_scrollbar_rect
            .expect("child terminal scrollbar");
        let preview = row.preview_rect.expect("child content");
        assert_eq!(collection_track.x, child_track.x + 1);
        assert_eq!(preview.right(), child_track.x);
        assert_eq!(row.preview_size.expect("logical preview").1, preview.width);

        let mut terminal = Terminal::new(TestBackend::new(80, 14)).expect("terminal");
        terminal
            .draw(|frame| {
                render_collections(&app, &TerminalRuntimeRegistry::new(), &layouts, frame)
            })
            .expect("render");
        assert_ne!(
            terminal.backend().buffer()[(child_track.x, child_track.y)].symbol(),
            terminal.backend().buffer()[(preview.right().saturating_sub(1), child_track.y)]
                .symbol()
        );
    }

    #[test]
    fn collection_scrollbar_hit_is_omitted_when_tiny_and_never_has_zero_extent() {
        let mut ws = Workspace::test_new("tiny-collection-scrollbar");
        let root = ws.tabs[0].root_pane.expect("root");
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        for _ in 0..12 {
            let pane = ws.test_split(Direction::Horizontal);
            ws.collect_pane(pane, id).expect("collect member");
        }
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);

        for width in 0..=16 {
            let layouts = compute_collection_layouts(
                &mut app,
                &TerminalRuntimeRegistry::new(),
                Rect::new(0, 0, width, 8),
                false,
                Default::default(),
            );
            let layout = layouts
                .iter()
                .find(|layout| layout.id == id)
                .expect("layout");
            let scrollbar_hits: Vec<_> = layout
                .hits
                .iter()
                .filter(|hit| hit.kind == CollectionHitKind::CollectionScrollbar)
                .collect();
            match layout.scrollbar_rect {
                Some(track) => {
                    assert!(track.width > 0 && track.height > 0);
                    assert_eq!(scrollbar_hits.len(), 1);
                    assert_eq!(scrollbar_hits[0].rect, track);
                }
                None => assert!(scrollbar_hits.is_empty()),
            }
        }
    }

    #[tokio::test]
    async fn clipped_child_keeps_logical_geometry_and_scrollbar_coordinates() {
        let mut ws = Workspace::test_new("clipped-child-scrollbar");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let terminal_id = ws.terminal_id(child).expect("terminal").clone();
        let id = ws
            .create_collection_near(0, LayoutLeaf::Pane(root), Direction::Vertical, 0.5, None)
            .expect("collection");
        ws.collect_pane(child, id).expect("collect");
        let history = (0..40).map(|n| format!("line {n}\r\n")).collect::<String>();
        ws.tabs[0].runtimes.insert(
            child,
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(
                40,
                8,
                16 * 1024,
                history.as_bytes(),
            ),
        );
        let mut app = AppState::test_new();
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.defer_collection_geometry_claims = true;
        let view = app.collection_views.entry(id).or_default();
        view.expanded.insert(child);
        view.scroll = 4;

        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 50, 9),
            true,
            Default::default(),
        );
        let row = &layouts[0].rows[0];
        assert!(row.preview_row_offset > 0);
        assert_eq!(
            row.preview_scrollbar_rect
                .expect("visible child scrollbar")
                .height,
            row.preview_rect.expect("visible content").height
        );
        assert_eq!(app.collection_geometry[&terminal_id].rows, 8);
        assert_eq!(
            app.collection_geometry[&terminal_id].cols,
            row.preview_size.expect("logical preview").1
        );
    }

    #[test]
    fn projection_flattens_delegation_and_separates_archive() {
        let mut ws = Workspace::test_new("collection");
        let root = ws.tabs[0].root_pane.expect("root");
        let child = ws.test_split(Direction::Horizontal);
        let grandchild = ws.test_split(Direction::Horizontal);
        let archived = ws.test_split(Direction::Horizontal);
        let id = ws
            .create_collection_near(
                0,
                LayoutLeaf::Pane(root),
                Direction::Vertical,
                0.5,
                Some("Helpers".into()),
            )
            .expect("collection");
        for pane in [child, grandchild, archived] {
            ws.collect_pane(pane, id).expect("collect member");
        }
        ws.set_collection_member_archived(archived, id, true)
            .expect("archive");

        let mut app = AppState::test_new();
        let parent = app
            .delegations
            .create(Some(root), None, Some("parent".into()))
            .expect("parent delegation");
        let child_delegation = app
            .delegations
            .create(Some(child), Some(parent), Some("research".into()))
            .expect("child delegation");
        app.delegations
            .create(
                Some(grandchild),
                Some(child_delegation),
                Some("tests".into()),
            )
            .expect("grandchild delegation");
        app.workspaces = vec![ws];
        app.active = Some(0);
        app.selected = 0;

        let layouts = compute_collection_layouts(
            &mut app,
            &TerminalRuntimeRegistry::new(),
            Rect::new(0, 0, 100, 30),
            false,
            Default::default(),
        );
        let rows = &layouts[0].rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (rows[0].pane_id, rows[0].depth, rows[0].external_parent),
            (child, 0, true)
        );
        assert_eq!((rows[1].pane_id, rows[1].depth), (grandchild, 1));
        assert_eq!(rows[2].section, CollectionSection::Archived);
        assert!(layouts[0].archive_header.is_some());
    }
}
