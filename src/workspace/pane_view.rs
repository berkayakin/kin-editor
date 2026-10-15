//! Panes: tab strips, the editor island, breadcrumbs and the diff overlay.

use super::*;

impl Workspace {
    /// The pane's tabs, like a browser's: on the chrome above the pane's
    /// island, the current one filled like the island and joined to it.
    /// `edge` is the island's border color, which the current tab continues.
    pub(super) fn render_tab_bar(&self, pane_ix: usize, edge: Hsla, available: Option<f32>, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let pane = &self.panes[pane_ix];
        let focused_pane = pane_ix == self.active_pane;
        let accent: Hsla = theme.accent.into();
        let names: Vec<String> = pane.tabs.iter().map(|e| e.read(cx).doc.read(cx).file_name()).collect();
        let name_widths = measure_tab_names(&names, window, &theme);
        let naturals: Vec<f32> = pane
            .tabs
            .iter()
            .zip(&name_widths)
            .map(|(e, (w, _))| {
                let errors = e.read(cx).doc.read(cx).diagnostics.iter().any(|d| d.severity == Severity::Error);
                (TAB_CHROME + w + if errors { TAB_ERROR_DOT } else { 0. }).ceil()
            })
            .collect();
        let slots = fit_tabs(&naturals, pane.active, available.unwrap_or(f32::INFINITY));
        let visible: Vec<usize> = (0..slots.len()).filter(|&i| slots[i] != TabSlot::Hidden).collect();
        let hidden: Vec<Entity<Editor>> =
            (0..slots.len()).filter(|&i| slots[i] == TabSlot::Hidden).map(|i| pane.tabs[i].clone()).collect();
        div()
            .id(("tabs", pane_ix))
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT))
            .flex()
            .flex_row()
            .items_end()
            .pt(px(TAB_STRIP_TOP))
            // Clear of the island's rounded corners on both sides, so the first
            // and last tabs' feet sit on its straight top edge.
            .pl(px(TAB_STRIP_INSET))
            .pr(px(TAB_STRIP_INSET))
            // No scrolling, which would cut the current tab's shape at the edge:
            // like a browser, tabs shrink to fit, down to their icon.
            .overflow_hidden()
            // Dropping on the bar's empty part moves the tab to the end.
            .on_drop(cx.listener(move |ws, dragged: &DraggedTab, window, cx| {
                ws.move_tab(&dragged.editor, pane_ix, None, window, cx);
            }))
            .children(visible.iter().enumerate().map(|(pos, &ix)| {
                let editor = &pane.tabs[ix];
                let slot = slots[ix];
                let e = editor.read(cx);
                let active = ix == pane.active;
                let next = visible.get(pos + 1).copied();
                // Separators only between two tabs that sit on the chrome.
                let separator = !active && next.is_some_and(|n| n != pane.active);
                let after_active = pos > 0 && visible[pos - 1] == pane.active;
                let icon_only = matches!(slot, TabSlot::Icon(_));
                let compact = matches!(slot, TabSlot::Compact(_));
                let pinned = self.pinned.contains(&editor.entity_id());
                let dirty = e.doc.read(cx).is_dirty();
                let errors = e.doc.read(cx).diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
                let icon = file_icon(e.doc.read(cx).path.as_deref().unwrap_or(Path::new("")), px(15.), &theme);
                let name = e.doc.read(cx).file_name();
                let shown_name = tab_label(&name, slot, naturals[ix], name_widths[ix]);
                let name_color: Hsla = if active && focused_pane { theme.text.into() } else { theme.text_muted.into() };
                let group = SharedString::from(format!("tab-{pane_ix}-{ix}"));
                let (for_close, for_menu, for_drag, for_unpin) = (editor.clone(), editor.clone(), editor.clone(), editor.clone());
                div()
                    .id(SharedString::from(format!("tab-{pane_ix}-{ix}")))
                    .group(group.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h_full()
                    .flex_shrink_0()
                    .map(|el| tab_slot_width(el, slot, naturals[ix]))
                    .cursor_pointer()
                    .map(|el| tab_shape(el, active, after_active, separator, group.clone(), edge, &theme))
                    .when(icon_only || compact, |el| {
                        let name = name.clone();
                        el.tooltip(move |_, cx| tooltip_view(name.clone(), cx))
                    })
                    .on_click(cx.listener(move |ws, _, window, cx| ws.activate_in(pane_ix, ix, window, cx)))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |ws, _, window, cx| ws.close_editor(&for_close, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |ws, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            ws.open_tab_menu(for_menu.clone(), event.position, window, cx);
                        }),
                    )
                    .on_drag(DraggedTab { editor: for_drag, name: name.clone().into() }, |tab, _, _, cx| {
                        cx.new(|_| DragLabel(tab.name.clone()))
                    })
                    // A drop lands before this tab; the accent edge shows where.
                    .drag_over::<DraggedTab>(move |style, _, _, _| style.border_l_2().border_color(accent))
                    .on_drop(cx.listener(move |ws, dragged: &DraggedTab, window, cx| {
                        cx.stop_propagation();
                        ws.move_tab(&dragged.editor, pane_ix, Some(ix), window, cx);
                    }))
                    .child(icon)
                    .when(!icon_only, |el| el.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(name_color)
                            .when(active, |el| el.font_weight(FontWeight::MEDIUM))
                            .when(!active, |el| el.group_hover(group.clone(), |s| s.text_color(theme.text)))
                            .child(shown_name),
                    ))
                    // Files with errors get a small dot rather than a recolored name.
                    .when(errors > 0 && !icon_only && !compact, |el| el.child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(theme.error)))
                    .when(!icon_only && !compact, |el| el.child(
                        div()
                            .id(SharedString::from(format!("close-{pane_ix}-{ix}")))
                            .flex_shrink_0()
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .hover_bg(theme.active)
                            .on_click(cx.listener(move |ws, _, window, cx| {
                                cx.stop_propagation();
                                // A pinned tab's pin unpins it; otherwise this closes.
                                let action = if ws.pinned.contains(&for_unpin.entity_id()) { TabAction::TogglePin } else { TabAction::Close };
                                ws.run_tab_action(action, &for_unpin, window, cx);
                            }))
                            .map(|el| {
                                if pinned {
                                    el.child(icon_el(Icon::Pin, px(12.), theme.text_muted.into()))
                                } else if dirty {
                                    // A dot that turns into a close button on hover.
                                    el.child(
                                        div()
                                            .size(px(7.))
                                            .rounded_full()
                                            .bg(theme.accent)
                                            .group_hover(group.clone(), |s| s.invisible()),
                                    )
                                    .child(
                                        div()
                                            .absolute()
                                            .invisible()
                                            .group_hover(group.clone(), |s| s.visible())
                                            .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                                    )
                                } else {
                                    el.child(
                                        div()
                                            .when(!active, |d| d.invisible())
                                            .group_hover(group.clone(), |s| s.visible())
                                            .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                                    )
                                }
                            }),
                    ))
            }))
            // The tabs that don't fit, in a menu.
            .when(!hidden.is_empty(), |el| {
                let count = hidden.len();
                el.child(
                    div()
                        .id(("tabs-overflow", pane_ix))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .w(px(TAB_OVERFLOW_WIDTH - 8.))
                        .h(px(24.))
                        .ml_1()
                        .mb(px(4.))
                        .rounded(px(8.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(theme.text_muted)
                        .hover_bg(theme.hover)
                        .hover(|s| s.text_color(theme.text))
                        .tooltip(move |_, cx| tooltip_view(format!("{count} more tab{}", plural(count)), cx))
                        .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                            let entries = hidden.iter().map(|e| MenuEntry::item(e.read(cx).doc.read(cx).file_name(), Icon::File)).collect();
                            let actions = hidden.iter().map(|e| Some(MenuAction::Tab(TabAction::Activate, e.clone()))).collect();
                            ws.show_context_menu(entries, actions, event.position(), None, window, cx);
                        }))
                        .child("⋯")
                        .child(count.to_string()),
                )
            })
    }

    /// The room for a pane's tabs, from the panes' bounds in the last frame
    /// (None before the first).
    pub(super) fn tab_strip_width(&self, pane_ix: usize) -> Option<f32> {
        let total = f32::from(self.panes_bounds.get().size.width);
        if total <= 0. {
            return None;
        }
        let pane = match self.pane_axis {
            PaneAxis::Row => {
                let gaps = GAP * (self.panes.len() - 1) as f32;
                (total - gaps) * self.pane_sizes.get(pane_ix).copied().unwrap_or(1.)
            }
            PaneAxis::Column => total,
        };
        Some(pane - 2. * TAB_STRIP_INSET)
    }

    /// The editor area: panes side by side or stacked, with dividers.
    pub(super) fn render_panes(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.panes_bounds.clone();
        div()
            .relative()
            .flex()
            .gap(px(GAP))
            .size_full()
            .when(self.pane_axis == PaneAxis::Column, |el| el.flex_col())
            .child(
                gpui::canvas(move |b, window, _| track_bounds(&bounds, b, window), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .children((0..self.panes.len()).map(|ix| self.render_pane(ix, window, cx)))
            .when_some(self.diff_view(), |el, view| el.child(self.render_diff_overlay(view, cx)))
    }

    pub(super) fn diff_view(&self) -> Option<Entity<DiffView>> {
        match &self.modal {
            Some(Modal::Diff(view)) => Some(view.clone()),
            _ => None,
        }
    }

    /// The Git diff covers only the editor area, so the sidebar (Commit view)
    /// and the bottom panels (Git log) stay usable beside it.
    /// The Git diff over the editor area, with a tab of its own in place of
    /// the file tabs (hidden meanwhile, so they don't seem to control it).
    pub(super) fn render_diff_overlay(&self, view: Entity<DiffView>, cx: &App) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (path, rel) = view.read(cx).tab_file();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| rel.clone());
        let edge = Hsla::from(theme.border).alpha(0.75);
        let close_view = view.clone();
        let tab = div()
            .id("diff-tab")
            .relative()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1p5()
            .h_full()
            .pl_3()
            .pr_1p5()
            .max_w(px(360.))
            .text_color(theme.text)
            .font_weight(FontWeight::MEDIUM);
        let tab = tab_shape(tab, true, false, false, "diff-tab".into(), edge, &theme)
            .tooltip(move |_, cx| tooltip_view(format!("Changes in {rel}"), cx))
            .child(icon_el(Icon::GitCompare, px(14.), theme.accent.into()))
            .child(div().min_w_0().truncate().child(name))
            .child(
                div()
                    .id("close-diff-tab")
                    .flex_shrink_0()
                    .size(px(18.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .cursor_pointer()
                    .hover_bg(theme.active)
                    .on_click(move |_, _, cx| close_view.update(cx, |v, cx| v.close(cx)))
                    .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
            );
        gpui::deferred(
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .child(div().flex_shrink_0().h(px(TAB_STRIP_HEIGHT - 1.)))
                .child(
                    island(&theme)
                        .id("diff-layer")
                        .flex_1()
                        .min_h_0()
                        .occlude()
                        .child(div().size_full().overflow_hidden().child(view)),
                )
                // After the island, so the tab paints over its top border.
                .child(
                    div()
                        .id("diff-tab-strip")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(TAB_STRIP_HEIGHT))
                        .flex()
                        .items_end()
                        .pt(px(TAB_STRIP_TOP))
                        .pl(px(TAB_STRIP_INSET))
                        .occlude()
                        .child(tab),
                ),
        )
        .with_priority(1)
    }

    pub(super) fn render_pane(&self, pane_ix: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let pane = &self.panes[pane_ix];
        let share = relative(self.pane_sizes.get(pane_ix).copied().unwrap_or(1.));
        let content: AnyElement = match pane.tabs.get(pane.active) {
            Some(editor) => cached(editor).into_any_element(),
            None if self.panes.len() == 1 => self.render_welcome(window, cx).into_any_element(),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .bg(theme.background)
                .text_color(theme.text_dim)
                .child("No open files")
                .child(div().text_size(px(12.)).child("Open a file, or drag a tab here"))
                .into_any_element(),
        };
        let multiple = self.panes.len() > 1;
        let drop_group = SharedString::from(format!("pane-drop-{pane_ix}"));
        // With splits, the pane that has the keyboard is outlined (its current tab too).
        let edge: Hsla = if multiple && pane_ix == self.active_pane {
            Hsla::from(theme.accent).alpha(0.35)
        } else {
            Hsla::from(theme.border).alpha(0.75)
        };
        let island = island(&theme)
            .flex_1()
            .min_w_0()
            .min_h_0()
            .border_color(edge)
            .when_some(pane.tabs.get(pane.active).cloned(), |el, editor| el.children(self.render_breadcrumbs(&editor, cx)))
            .when(self.show_find && pane_ix == self.active_pane && !pane.tabs.is_empty(), |el| {
                el.child(self.find_bar.clone())
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    // Clicking into a pane makes it the current one.
                    .capture_any_mouse_down(cx.listener(move |ws, _, window, cx| {
                        if ws.active_pane != pane_ix {
                            let active = ws.panes[pane_ix].active;
                            ws.active_pane = pane_ix;
                            ws.activate_in(pane_ix, active, window, cx);
                        }
                    }))
                    .on_drop(cx.listener(move |ws, dragged: &DraggedTab, window, cx| {
                        ws.move_tab(&dragged.editor, pane_ix, None, window, cx);
                    }))
                    .child(content),
            )
            .when(pane_ix + 1 < self.panes.len(), |el| el.child(self.resize_handle(Edge::Pane(pane_ix), cx)));
        div()
            .relative()
            .flex()
            .flex_col()
            .min_w_0()
            .min_h_0()
            .map(|el| match self.pane_axis {
                PaneAxis::Row => el.h_full().w(share),
                PaneAxis::Column => el.w_full().h(share),
            })
            // The tab strip's room on the chrome (kept without tabs too, so the
            // island lines up with the sidebar's).
            .child(div().flex_shrink_0().h(px(TAB_STRIP_HEIGHT - 1.)))
            .child(island)
            // After the island, so the current tab paints over its top border.
                        // A Git diff over the panes shows its own tab instead.
            .when(!pane.tabs.is_empty() && self.diff_view().is_none(), |el| {
                el.child(self.render_tab_bar(pane_ix, edge, self.tab_strip_width(pane_ix), window, cx))
            })
            // Finder files dragged anywhere over the pane (tabs included) open in it.
            .group(drop_group.clone())
            .child(
                // Shown by GPUI while Finder files are over the pane, so there's no
                // state to go stale. Being the drop target also gives it the
                // hitbox GPUI needs to apply drag styles; hidden, it takes no events.
                div()
                    .invisible()
                    .group_drag_over::<gpui::ExternalPaths>(drop_group, |s| s.visible())
                    .on_drop(cx.listener(move |ws, paths: &gpui::ExternalPaths, window, cx| {
                        cx.stop_propagation();
                        ws.active_pane = pane_ix;
                        ws.open_dropped(paths.paths(), window, cx);
                    }))
                    .absolute()
                    .inset_0()
                    .p_2()
                    .child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(10.))
                        .border_2()
                        .border_dashed()
                        .border_color(Hsla::from(theme.accent).alpha(0.8))
                        .bg(Hsla::from(theme.accent).alpha(0.08))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_1p5()
                                .rounded(px(8.))
                                .bg(theme.elevated)
                                .border_1()
                                .border_color(theme.border)
                                .shadow(soft_shadow(&theme, 2))
                                .text_color(theme.text)
                                .child(icon_el(Icon::FileCode, px(15.), theme.accent.into()))
                                .child("Drop to open"),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// Under the tabs: the file's folders and name, then the definitions
    /// around the caret; clicking one goes to it.
    pub(super) fn render_breadcrumbs(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (path, symbols) = editor.update(cx, |e, cx| (e.doc.read(cx).path.clone(), e.doc.read(cx).image.is_none().then(|| e.breadcrumbs(cx))));
        let path = path?;
        let theme = Theme::get(cx).clone();
        let inside = self.root.as_deref().is_some_and(|r| path.starts_with(r));
        let rel = self.display_rel(&path);
        let mut parts: Vec<String> = rel.split(['/', '\\']).map(str::to_string).collect();
        let file = parts.pop().unwrap_or_default();
        // A file outside the project: its folder is enough context.
        if !inside {
            parts = parts.pop().into_iter().collect();
        }
        let chevron = || icon_el(Icon::ChevronRight, px(11.), theme.text_dim.into());
        let mut row = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .h(px(24.))
            .px_3()
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(theme.background)
            .text_size(px(12.))
            .text_color(theme.text_dim);
        for part in parts {
            row = row.child(div().flex_shrink_0().child(part)).child(chevron());
        }
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .text_color(theme.text_muted)
                .child(file_icon(&path, px(13.), &theme))
                .child(file),
        );
        let symbols = symbols.unwrap_or_default();
        let last = symbols.len().saturating_sub(1);
        for (ix, (name, offset)) in symbols.into_iter().enumerate() {
            let editor = editor.clone();
            row = row.child(chevron()).child(
                div()
                    .id(("breadcrumb", ix))
                    .flex_shrink_0()
                    .px_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_color(if ix == last { theme.text } else { theme.text_muted })
                    .hover_bg(theme.hover)
                    .on_click(move |_, window, cx| {
                        editor.update(cx, |e, cx| e.go_to(offset, window, cx));
                        window.focus(&editor.focus_handle(cx));
                    })
                    .child(name),
            );
        }
        Some(row.into_any_element())
    }
}
