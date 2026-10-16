//! The title bar: project and branch chips, the run widget, panel toggles, search and settings, window controls.

use super::*;
use gpui::svg;

/// Free space on the window's chrome (the title bar's empty parts, the room
/// after the top tab strips' tabs, the status bar's): drags the window, and a
/// double click zooms it. Only where nothing else is: a press reaches every
/// element under the pointer, so behind a tab it would move the window too.
/// Windows: the system does both over a drag area that no GPUI listener
/// handles, so it's occluded (the workspace root's focus-on-click would
/// count), which hides it from whatever is behind: drops that should land
/// there go on it too. macOS: a press hands the window to the window server.
pub(super) fn window_drag_area(id: impl Into<gpui::ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .h_full()
        .window_control_area(WindowControlArea::Drag)
        .when(cfg!(windows), |el| el.occlude())
        .when(cfg!(target_os = "macos"), |el| {
            el.on_mouse_down(MouseButton::Left, |event, window, cx| {
                cx.stop_propagation();
                if event.click_count >= 2 {
                    window.titlebar_double_click();
                } else {
                    #[cfg(target_os = "macos")]
                    drag_window();
                }
            })
        })
}

/// Moves the key window with the pointer, from the mouse down being handled.
#[cfg(target_os = "macos")]
fn drag_window() {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};

    let Some(app) = AnyClass::get(c"NSApplication") else { return };
    unsafe {
        let app: *mut AnyObject = msg_send![app, sharedApplication];
        let Some(app) = app.as_ref() else { return };
        let event: *mut AnyObject = msg_send![app, currentEvent];
        let Some(event) = event.as_ref() else { return };
        let window: *mut AnyObject = msg_send![event, window];
        if let Some(window) = window.as_ref() {
            let _: () = msg_send![window, performWindowDragWithEvent: event];
        }
    }
}

impl Workspace {
    pub(super) fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        let theme = Theme::get(cx).clone();
        let width = f32::from(window.viewport_size().width);
        // Long project and branch names are cut shorter in narrow windows.
        let narrow = width < 900.;
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
            // Drag areas (Windows) are the bar's empty parts, not the bar itself:
            // GPUI picks the first control area under the pointer, and a parent's
            // comes before its children's (the window buttons).
            //
            // Room for the traffic lights, which macOS moves out of the window in full screen.
            .child(window_drag_area("titlebar-start").flex_shrink_0().w(px(if mac && !window.is_fullscreen() { 86. } else { GAP + 4. })))
            .when(app_menu::APP_MENU_IN_TITLE_BAR, |el| {
                el.child(self.render_app_menu(cx)).child(window_drag_area("titlebar-menu-gap").flex_shrink_0().w(px(GAP)))
            })
            // The chips give way (their names cut) before the tools would. An
            // open app menu's names take their place.
            .when(!self.app_menu_open(), |el| el.child(
                div()
                    .flex()
                    .min_w_0()
                    .overflow_hidden()
                    .items_center()
                    .gap_1p5()
                    .when(self.root.is_some(), |el| el.child(self.render_project_chip(narrow, cx)))
                    .when(self.repo.is_some(), |el| el.child(self.render_branch_chip(narrow, cx)))
                    .when_some(self.render_sync_counts(window, cx), |el, counts| el.child(counts))
                    .when(self.restricted(cx), |el| el.child(self.render_restricted_chip(cx))),
            ))
            .child(window_drag_area("titlebar-middle").flex_1().min_w(px(GAP)))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap_2p5()
                    .child(self.render_run_widget(width >= 820., window, cx))
                    .child(separator(&theme))
                    .child(self.render_panel_dock(window, cx))
                    .child(separator(&theme))
                    .child(self.render_search_button(window, cx))
                    .child(self.render_extensions_button(window, cx))
                    .child(self.render_settings_button(window, cx)),
            )
            .map(|el| match mac {
                true => el.child(window_drag_area("titlebar-end").flex_shrink_0().w(px(GAP + 2.))),
                false => el.child(div().w(px(GAP)).h_full().flex_shrink_0()).child(self.render_window_controls(cx)),
            })
    }

    /// The selected run configuration (the last one launched, run or
    /// debugged) with Run, Debug and, while something runs, Stop; like
    /// JetBrains' run widget.
    pub(super) fn render_run_widget(&self, labeled: bool, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let selected = self.last_run.clone();
        let terminals = self.all_terminals(cx);
        let running_selected = selected.as_ref().is_some_and(|target| {
            terminals.iter().any(|t| {
                t.read(cx).run().is_some_and(|(r, s)| *s == RunState::Running && r.name == target.name && r.command == target.command)
            })
        });
        let running = self.debug.as_ref().is_some_and(|(s, _)| s.read(cx).is_active())
            || terminals.iter().any(|t| t.read(cx).run().is_some_and(|(_, s)| *s == RunState::Running));
        let debuggable = selected.as_ref().is_some_and(|t| t.debug.is_some());

        let run_tip = match (&selected, running_selected) {
            (Some(t), true) => format!("Rerun {}", t.name.trim_start_matches("Run ")),
            (Some(t), false) => t.name.clone(),
            (None, _) => "Run…".into(),
        };

        let debug_tip = match &selected {
            Some(t) if debuggable => format!("Debug {}", t.name.trim_start_matches("Run ").trim_start_matches("Test ")),
            _ => "Debug…".into(),
        };
        let with_keys = |label: String, action: &dyn gpui::Action| match binding_label(action, window) {
            Some(keys) => format!("{label} ({keys})"),
            None => label,
        };
        let (run_tip, debug_tip, stop_tip) = (with_keys(run_tip, &Rerun), with_keys(debug_tip, &DebugConfiguration), with_keys("Stop".into(), &StopRun));

        // Plain icons, no fills or shadows: Run and Stop keep their colors
        // (what they do), the rest light up on hover.
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .when_some(selected.filter(|_| labeled), |el, target| {
                el.child(
                    div()
                        .id("run-configuration")
                        .group("run-configuration")
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .h(px(28.))
                        .pl_1()
                        .pr_1p5()
                        .cursor_pointer()
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .hover(|s| s.text_color(theme.text))
                        .tooltip(|window, cx| status_tooltip("Run Configuration…", &RunConfiguration, window, cx))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(|ws, _, window, cx| ws.run_configuration(&RunConfiguration, window, cx)))
                        .child(ellipsize(&target.name, 26))
                        .child(
                            svg()
                                .path(Icon::ChevronDown.path())
                                .size(px(14.))
                                .flex_shrink_0()
                                .text_color(theme.text_dim)
                                .group_hover("run-configuration", |s| s.text_color(theme.text)),
                        ),
                )
            })
            .child(
                self.bar_button("run", if running_selected { Icon::Refresh } else { Icon::Play }, theme.success.into(), None, cx, |ws, _, window, cx| {
                    ws.rerun(&Rerun, window, cx)
                })
                .tooltip(move |_, cx| tooltip_view(run_tip.clone(), cx)),
            )
            .child(
                self.bar_button("debug", Icon::Bug, theme.text_dim.into(), Some(theme.text.into()), cx, |ws, _, window, cx| match ws.last_run.clone() {
                    Some(target) if target.debug.is_some() => ws.debug_target(target, window, cx),
                    _ => ws.debug_configuration(&DebugConfiguration, window, cx),
                })
                .tooltip(move |_, cx| tooltip_view(debug_tip.clone(), cx)),
            )
            .when(running, |el| {
                el.child(
                    self.bar_button("stop", Icon::Stop, theme.error.into(), None, cx, |ws, _, window, cx| ws.stop_run(&StopRun, window, cx))
                        .tooltip(move |_, cx| tooltip_view(stop_tip.clone(), cx)),
                )
            })
    }

    /// Show/hide toggles for the sidebar and the bottom panels: an open
    /// one's icon takes the accent color.
    pub(super) fn render_panel_dock(&self, _: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let toggle = |id, icon, on: bool, cx: &mut Context<Self>, f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            let (rest, hover) = if on { (theme.accent, None) } else { (theme.text_dim, Some(theme.text.into())) };
            self.bar_button(id, icon, rest.into(), hover, cx, move |ws, _, window, cx| f(ws, window, cx))
        };
        div()
            .flex()
            .items_center()
            .gap(px(2.))
            .child(
                toggle("toggle-sidebar", Icon::PanelLeft, self.show_sidebar, cx, |ws, window, cx| ws.set_sidebar_visible(!ws.show_sidebar, window, cx))
                    .tooltip(|_, cx| tooltip_view("Toggle Sidebar", cx)),
            )
            .child(
                toggle("toggle-terminal", Icon::Terminal, self.shows(BottomPanel::Terminal), cx, |ws, window, cx| {
                    ws.set_terminal_shown(!ws.shows(BottomPanel::Terminal), window, cx)
                })
                .tooltip(|window, cx| status_tooltip("Terminal", &ToggleTerminal, window, cx)),
            )
            .child(
                toggle("toggle-problems", Icon::PanelBottom, self.shows(BottomPanel::Problems), cx, |ws, window, cx| {
                    ws.toggle_problems(&ToggleProblems, window, cx)
                })
                .tooltip(|window, cx| status_tooltip("Problems", &ToggleProblems, window, cx)),
            )
            .when(self.repo.is_some(), |el| {
                el.child(
                    toggle("toggle-git-log", Icon::GitCommit, self.shows(BottomPanel::GitLog), cx, |ws, window, cx| {
                        ws.toggle_git_log(&ToggleGitLog, window, cx)
                    })
                    .tooltip(|window, cx| status_tooltip("Git Log", &ToggleGitLog, window, cx)),
                )
            })
            .when(self.debug.is_some(), |el| {
                el.child(
                    toggle("toggle-debug", Icon::Bug, self.shows(BottomPanel::Debug), cx, |ws, window, cx| {
                        ws.toggle_debug_panel(&ToggleDebugPanel, window, cx)
                    })
                    .tooltip(|window, cx| status_tooltip("Debug", &ToggleDebugPanel, window, cx)),
                )
            })
    }

    /// Opens Search Everywhere, like JetBrains' magnifier.
    pub(super) fn render_search_button(&self, _: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        self.bar_button("search-everywhere", Icon::Search, theme.text_dim.into(), Some(theme.text.into()), cx, |ws, _, window, cx| {
            ws.search_everywhere(&SearchEverywhere, window, cx)
        })
        .tooltip(|_, cx| tooltip_view(format!("Search Everywhere ({})", double_shift_label()), cx))
    }

    /// Opens Settings' Extensions page; lit while it shows.
    pub(super) fn render_extensions_button(&self, _: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let on = self.preferences_shown(cx) == Some(true);
        let (rest, hover) = if on { (theme.accent, None) } else { (theme.text_dim, Some(theme.text.into())) };
        self.bar_button("open-extensions", Icon::Puzzle, rest.into(), hover, cx, |ws, _, window, cx| ws.open_extensions(&OpenExtensions, window, cx))
            .tooltip(|window, cx| status_tooltip("Extensions", &OpenExtensions, window, cx))
    }

    /// Whether Settings is open, and if so whether on its Extensions page.
    fn preferences_shown(&self, cx: &App) -> Option<bool> {
        match &self.modals.current {
            Some(Modal::Preferences(p)) => Some(matches!(p.read(cx).page(), preferences::Page::Extensions)),
            _ => None,
        }
    }

    /// Windows and Linux have no app menu, so Settings needs a way in.
    pub(super) fn render_settings_button(&self, _: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // The Extensions button stands for its own page.
        let on = self.preferences_shown(cx) == Some(false);
        let (rest, hover) = if on { (theme.accent, None) } else { (theme.text_dim, Some(theme.text.into())) };
        self.bar_button("open-settings", Icon::Settings, rest.into(), hover, cx, |ws, _, window, cx| ws.open_settings(&OpenSettings, window, cx))
            .tooltip(|window, cx| status_tooltip("Settings", &OpenSettings, window, cx))
    }

    /// A title bar icon button: just the icon (18 px in a 30 px target), `color` at rest and `hover`
    /// under the mouse (`None` = the same color, a little brighter for being
    /// full strength: at rest it's slightly faded).
    pub(super) fn bar_button(
        &self,
        id: &'static str,
        icon: Icon,
        color: Hsla,
        hover: Option<Hsla>,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let (rest, hover) = match hover {
            Some(hover) => (color, hover),
            None => (color.alpha(color.a * 0.8), color),
        };
        div()
            .id(id)
            .group(id)
            .size(px(30.))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(on_click))
            .child(svg().path(icon.path()).size(px(18.)).flex_shrink_0().text_color(rest).group_hover(id, move |s| s.text_color(hover)))
    }

    /// The open project: a pill like the branch chip beside it, with a round
    /// tinted monogram and the folder name. Clicking lists recent projects.
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
            .h(px(CHIP_HEIGHT))
            .pl(px(4.))
            .pr(px(12.))
            .rounded_full()
            .cursor_pointer()
            .text_size(px(13.))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .hover_fill(theme.hover, theme.active)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.recent_projects(&RecentProjects, window, cx)))
            .tooltip(|window, cx| status_tooltip("Recent Projects…", &RecentProjects, window, cx))
            .max_w(px(if narrow { 180. } else { 300. }))
            .child(
                div()
                    .size(px(CHIP_HEIGHT - 8.))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded_full()
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
        // Uncommitted changes aren't marked here: the Commit tab counts them.
        div()
            .id("branch-chip")
            .flex()
            .items_center()
            .gap_1p5()
            .h(px(CHIP_HEIGHT))
            .max_w(px(if narrow { 150. } else { 280. }))
            .pl(px(10.))
            .pr(px(12.))
            .rounded_full()
            .cursor_pointer()
            // The same size as the project's name beside it.
            .text_size(px(13.))
            .text_color(theme.text_muted)
            .hover_fill(theme.hover, theme.active)
            .hover(|st| st.text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.git_branches(&GitBranches, window, cx)))
            .tooltip(|window, cx| status_tooltip("Branches…", &GitBranches, window, cx))
            .child(icon_el(Icon::GitBranch, px(14.), theme.text_dim.into()))
            .child(div().min_w_0().truncate().child(ellipsize(&self.branch_label(), if narrow { 12 } else { 28 })))
    }

    /// Commits to push and to pull, beside the branch pill: Push… when there
    /// are some to push, else Update Project.
    pub(super) fn render_sync_counts(&self, window: &Window, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let s = &self.git_status;
        let (ahead, behind) = (s.ahead, s.behind);
        if self.repo.is_none() || (ahead == 0 && behind == 0) {
            return None;
        }
        let theme = Theme::get(cx).clone();
        let commits = |n: u32| if n == 1 { "1 commit".to_string() } else { format!("{n} commits") };
        let tip = match (ahead, behind) {
            (0, b) => format!("{} to pull", commits(b)),
            (a, 0) => format!("{} to push", commits(a)),
            (a, b) => format!("{} to push, {b} to pull", commits(a)),
        };
        let (label, action): (&str, &dyn gpui::Action) = if ahead > 0 { ("Push…", &GitPush) } else { ("Update Project", &GitPull) };
        let tip = match binding_label(action, window) {
            Some(keys) => format!("{tip}. {label} ({keys})"),
            None => format!("{tip}. {label}"),
        };
        let dim: Hsla = theme.text_dim.into();
        let bright: Hsla = theme.text.into();
        let count = move |icon: Icon, n: u32| {
            div().flex().items_center().gap(px(2.)).child(
                svg().path(icon.path()).size(px(14.)).flex_shrink_0().text_color(dim).group_hover("sync-counts", move |s| s.text_color(bright)),
            )
            .child(n.to_string())
        };
        Some(
            div()
                .id("sync-counts")
                .group("sync-counts")
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1p5()
                .h(px(CHIP_HEIGHT))
                .px(px(8.))
                .rounded_full()
                .cursor_pointer()
                .text_size(px(12.5))
                .text_color(theme.text_dim)
                .hover_bg(theme.hover)
                .hover(|st| st.text_color(theme.text))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(move |ws, _, window, cx| {
                    if ahead > 0 {
                        ws.git_push(&GitPush, window, cx)
                    } else {
                        ws.git_pull(&GitPull, window, cx)
                    }
                }))
                .tooltip(move |_, cx| tooltip_view(tip.clone(), cx))
                .when(ahead > 0, |el| el.child(count(Icon::ArrowUp, ahead)))
                .when(behind > 0, |el| el.child(count(Icon::ArrowDown, behind))),
        )
    }

    /// Shown while the project isn't trusted; opens the trust dialog.
    pub(super) fn render_restricted_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .id("restricted-chip")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1p5()
            // A pill like the branch chip beside it: it's a button too.
            .h(px(CHIP_HEIGHT))
            .pl(px(10.))
            .pr(px(12.))
            .rounded_full()
            .cursor_pointer()
            .text_size(px(12.5))
            .text_color(theme.warning)
            .hover_fill(Hsla::from(theme.warning).alpha(0.12), Hsla::from(theme.warning).alpha(0.2))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.open_trust_dialog(window, cx)))
            .tooltip(|_, cx| tooltip_view("Language servers, formatters and project tools are off until you trust this folder", cx))
            .child(icon_el(Icon::ShieldAlert, px(14.), theme.warning.into()))
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
        let mut title = match self.active_item() {
            Some(item) => format!("{}{}", item.tab_title(cx), if item.is_dirty(cx) { " •" } else { "" }),
            None => "Kuana".into(),
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

/// The project, branch and Restricted Mode pills' height.
const CHIP_HEIGHT: f32 = 28.;

/// A short hairline between groups of title bar buttons.
fn separator(theme: &Theme) -> Div {
    div().w(px(1.)).h(px(18.)).flex_shrink_0().bg(theme.border)
}

/// A stable tint per project name for its monogram.
pub(super) fn monogram_color(name: &str, theme: &Theme) -> Hsla {
    let palette = [theme.accent, theme.success, theme.info, theme.warning, theme.syntax.function, theme.syntax.keyword, theme.git_added, theme.git_modified];
    let hash = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    palette[hash as usize % palette.len()].into()
}
