//! The title bar: project and branch chips, the command capsule, window controls.

use super::*;

impl Workspace {
    pub(super) fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        let width = f32::from(window.viewport_size().width);
        // Long project and branch names are cut shorter in narrow windows.
        let narrow = width < 900.;
        // The capsule gives up its extras as the window narrows, then becomes
        // a round search button.
        let capsule = if width < 720. {
            Capsule::Button
        } else {
            let w = (width * 0.36).clamp(240., 460.);
            if w < 360. { Capsule::Compact(w) } else { Capsule::Full(w) }
        };
        div()
            .id("titlebar")
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            // On Windows a title bar click must reach no listener, or GPUI reports
            // it handled and the system's drag, double-click and window buttons
            // don't happen. Blocking the mouse keeps the workspace root from
            // taking focus on it.
            .when(cfg!(windows), |el| el.occlude())
            .when(mac, |el| {
                el.on_click(|event: &ClickEvent, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                })
            })
            // Drag areas (Windows) are the bar's empty parts, not the bar itself:
            // GPUI picks the first control area under the pointer, and a parent's
            // comes before its children's (the window buttons).
            //
            // Two sides that share the space around the capsule equally keep it
            // centered; the left one never gets less than its chips need (its
            // minimum is their width), so when they need more the capsule moves
            // right, and shrinks, instead of covering them.
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_basis(px(0.))
                    .h_full()
                    .items_center()
                    // Room for the traffic lights, which macOS moves out of the window in full screen.
                    .child(
                        div()
                            .h_full()
                            .w(px(if mac && !window.is_fullscreen() { 86. } else { GAP + 4. }))
                            .flex_shrink_0()
                            .window_control_area(WindowControlArea::Drag),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .gap_1()
                            .when(self.root.is_some(), |el| el.child(self.render_project_chip(narrow, cx)))
                            .when(self.repo.is_some(), |el| el.child(self.render_branch_chip(narrow, cx)))
                            .when(self.restricted(cx), |el| el.child(self.render_restricted_chip(cx))),
                    )
                    .child(div().flex_1().min_w(px(GAP)).h_full().window_control_area(WindowControlArea::Drag)),
            )
            .child(self.render_command_capsule(capsule, cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_basis(px(0.))
                    .min_w_0()
                    .h_full()
                    .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
                    .when(!mac, |el| el.child(self.render_window_controls(cx))),
            )
    }

    /// The title bar's center: the current file, and a way into Search
    /// Everywhere (which it opens), like a command palette's field.
    pub(super) fn render_command_capsule(&self, capsule: Capsule, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (edge, lit) = (Hsla::from(theme.border), Hsla::from(theme.accent).alpha(0.5));
        let hover = move |el: Stateful<Div>, t| el.border_color(mix(edge, lit, t));
        let base = div()
            .id("command-capsule")
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(28.))
            .rounded(px(14.))
            // Set off the chrome by its fill (the elevated color) and a
            // full-strength edge, so it reads in dark themes too. No shadow:
            // those mark the main areas (islands, popovers), not controls.
            .bg(theme.elevated)
            .border_1()
            .cursor_pointer()
            .text_size(px(12.5))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.search_everywhere(&SearchEverywhere, window, cx)));
        let width = match capsule {
            Capsule::Button => {
                return base
                    .w(px(28.))
                    .justify_center()
                    .tooltip(|_, cx| tooltip_view(format!("Search Everywhere ({})", double_shift_label()), cx))
                    .child(icon_el(Icon::Search, px(13.), theme.text_muted.into()))
                    .with_hover_transition("command-capsule-hover", transition::FOCUS, hover)
                    .into_any_element();
            }
            Capsule::Compact(w) | Capsule::Full(w) => w,
        };
        let full = matches!(capsule, Capsule::Full(_));
        let file = self.active_editor().map(|e| {
            let doc = e.read(cx).doc.read(cx);
            (doc.path.clone(), doc.file_name(), doc.is_dirty())
        });
        // The folder only when there's room for it.
        let folder = file.as_ref().filter(|_| full).and_then(|(path, _, _)| path.as_deref()).and_then(|path| {
            let rel = self.display_rel(path);
            let (dir, _) = rel.rsplit_once(['/', '\\'])?;
            Some(format!("{dir}/"))
        });
        base.gap_2()
            .w(px(width))
            // Gives way to the chips beside it before they'd be cut.
            .flex_shrink()
            .min_w(px(160.))
            .pl_2p5()
            .pr(px(if full { 4. } else { 10. }))
            .child(icon_el(Icon::Search, px(13.), theme.text_muted.into()))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .justify_center()
                    .gap_1p5()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .map(|el| match file {
                        Some((path, name, dirty)) => el
                            .when_some(path.as_deref().map(|p| file_icon(p, px(13.), &theme)), |el, icon| el.child(icon))
                            .child(
                                div()
                                    .flex()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .when_some(folder, |el, f| el.child(div().flex_shrink().truncate().text_color(theme.text_muted).child(f)))
                                    .child(div().min_w_0().truncate().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(name)),
                            )
                            .when(dirty, |el| el.child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(theme.accent))),
                        None => el.text_color(theme.text_muted).child(div().truncate().child("Search Everywhere")),
                    }),
            )
            .when(full, |el| {
                el.child(
                    div()
                        .flex_shrink_0()
                        .px_1p5()
                        .py(px(1.))
                        .rounded(px(10.))
                        .bg(theme.active)
                        .text_size(px(11.))
                        .text_color(theme.text_muted)
                        .child(double_shift_label()),
                )
            })
            .with_hover_transition("command-capsule-hover", transition::FOCUS, hover)
            .into_any_element()
    }

    /// The open project: a tinted monogram and its folder name. Clicking
    /// lists recent projects.
    pub(super) fn render_project_chip(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let name = self.root.as_deref().map(folder_name).unwrap_or_default();
        let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        let tint = monogram_color(&name, &theme);
        div()
            .id("project-chip")
            .flex()
            .items_center()
            .gap_2()
            .h(px(28.))
            .pl_1()
            .pr_2()
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .hover_bg(theme.hover)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.recent_projects(&RecentProjects, window, cx)))
            .tooltip(|window, cx| status_tooltip("Recent Projects…", &RecentProjects, window, cx))
            .max_w(px(if narrow { 180. } else { 300. }))
            .child(
                div()
                    .size(px(20.))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .map(|el| {
                        if !theme.gradients {
                            el.bg(tint)
                        } else {
                            el.bg(gpui::linear_gradient(
                                135.,
                                gpui::linear_color_stop(tint, 0.),
                                gpui::linear_color_stop(tint.alpha(0.55), 1.),
                            ))
                        }
                    })
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(gpui::hsla(0., 0., 0., 0.78))
                    .child(initial),
            )
            .child(div().min_w_0().truncate().child(ellipsize(&name, if narrow { 16 } else { 30 })))
    }

    /// The current branch beside the project name. Clicking opens Branches….
    pub(super) fn render_branch_chip(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let s = &self.git_status;
        let dirty = !s.files.is_empty();
        div()
            .id("branch-chip")
            .flex()
            .items_center()
            .gap_1p5()
            .h(px(24.))
            .max_w(px(if narrow { 150. } else { 280. }))
            .px_2()
            .rounded(px(12.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(theme.text_muted)
            .hover_fill(theme.hover, theme.active)
            .hover(|st| st.text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.git_branches(&GitBranches, window, cx)))
            .tooltip(|window, cx| status_tooltip("Branches…", &GitBranches, window, cx))
            .child(icon_el(Icon::GitBranch, px(12.), theme.text_dim.into()))
            .child(div().min_w_0().truncate().child(ellipsize(&self.branch_label(), if narrow { 12 } else { 28 })))
            // Uncommitted changes, like a shell prompt's marker.
            .when(dirty, |el| el.child(div().flex_shrink_0().size(px(5.)).rounded_full().bg(theme.git_modified)))
            .when(s.ahead > 0, |el| el.child(div().flex_shrink_0().text_color(theme.text_dim).child(format!("↑{}", s.ahead))))
            .when(s.behind > 0, |el| el.child(div().flex_shrink_0().text_color(theme.text_dim).child(format!("↓{}", s.behind))))
    }

    /// Shown while the project isn't trusted; opens the trust dialog.
    pub(super) fn render_restricted_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .id("restricted-chip")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h(px(22.))
            .px_1p5()
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(theme.warning)
            .hover_fill(Hsla::from(theme.warning).alpha(0.12), Hsla::from(theme.warning).alpha(0.2))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.open_trust_dialog(window, cx)))
            .tooltip(|_, cx| tooltip_view("Language servers, formatters and project tools are off until you trust this folder", cx))
            .child(icon_el(Icon::ShieldAlert, px(13.), theme.warning.into()))
            .child("Restricted Mode")
    }

    /// The checked-out branch, or what HEAD is when it isn't one.
    pub(super) fn branch_label(&self) -> String {
        let s = &self.git_status;
        s.branch.clone().unwrap_or_else(|| match &s.head_oid {
            Some(oid) => format!("{} (detached)", &oid[..7.min(oid.len())]),
            None => "No commits yet".into(),
        })
    }

    pub(super) fn render_window_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        let button = |id: &'static str, glyph: &'static str, area: WindowControlArea, hover: Hsla| {
            div()
                .id(id)
                .w(px(46.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(theme.text_muted)
                .font_family("Segoe Fluent Icons")
                .hover(move |s| s.bg(hover))
                .window_control_area(area)
                .child(glyph)
        };
        div()
            .flex()
            .h_full()
            .child(button("min", "\u{E921}", WindowControlArea::Min, theme.hover.into()))
            .child(button("max", "\u{E922}", WindowControlArea::Max, theme.hover.into()))
            .child(button("close", "\u{E8BB}", WindowControlArea::Close, rgb(0xc42b1c).into()))
    }

    pub(super) fn update_window_title(&mut self, window: &mut Window, cx: &App) {
        let mut title = match self.active_editor() {
            Some(e) => {
                let doc = e.read(cx).doc.read(cx);
                format!("{}{}", doc.file_name(), if doc.is_dirty() { " •" } else { "" })
            }
            None => "Kin".into(),
        };
        if let Some(root) = self.root.as_deref() {
            title.push_str(&format!(" — {}", folder_name(root)));
        }
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }
}

/// An on/off pill like a native switch.
/// A stable tint per project name for its monogram.
pub(super) fn monogram_color(name: &str, theme: &Theme) -> Hsla {
    let palette = [theme.accent, theme.success, theme.info, theme.warning, theme.syntax.function, theme.syntax.keyword, theme.git_added, theme.git_modified];
    let hash = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    palette[hash as usize % palette.len()].into()
}
