//! The status bar.

use super::*;

impl Workspace {
    /// The active file's language server: its name, what it's busy with, or
    /// what's wrong (click to copy the install command or to restart).
    pub(super) fn render_lsp_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.active_editor()?.read(cx).doc.read(cx).path.clone()?;
        let status = self.lsp.read(cx).status(&path)?;
        let dim: Hsla = theme.text_dim.into();
        let (label, color, tip): (String, Hsla, Option<String>) = match &status {
            LspStatus::Starting => ("Starting language server…".into(), dim, None),
            LspStatus::Busy(work) => {
                let mut short = work.clone();
                if short.chars().count() > 48 {
                    short = short.chars().take(47).collect::<String>() + "…";
                }
                (short, dim, Some(work.clone()))
            }
            LspStatus::Ready(name) => (name.clone(), dim, None),
            LspStatus::Missing { installer: Some(installer), .. } => (
                format!("Install {}", installer.command),
                theme.warning.into(),
                Some(format!("No language server yet. {}, into Kin's own folder; one you install yourself is used instead.", installer.describe())),
            ),
            LspStatus::Missing { command, installer: None } => {
                ("No language server".into(), theme.warning.into(), Some(format!("Install one with `{command}`. Click to copy.")))
            }
            LspStatus::Installing(command) => (format!("Installing {command}…"), dim, None),
            LspStatus::Failed(message) => ("Language server stopped".into(), theme.error.into(), Some(format!("{message}. Click to restart."))),
        };
        // A small light: green when ready, the accent while working.
        let light: Hsla = match &status {
            LspStatus::Ready(_) => theme.success.into(),
            LspStatus::Starting | LspStatus::Busy(_) | LspStatus::Installing(_) => theme.accent.into(),
            LspStatus::Missing { .. } => theme.warning.into(),
            LspStatus::Failed(_) => theme.error.into(),
        };
        Some(
            div()
                .id("lsp-status")
                .flex()
                .items_center()
                .gap_1p5()
                .text_color(color)
                .child(
                    div().size(px(6.)).rounded_full().bg(light).shadow(crate::ui::glow(theme, light.alpha(0.6), 4., 0.)),
                )
                .when(matches!(status, LspStatus::Missing { .. } | LspStatus::Failed(_)), |el| el.cursor_pointer())
                .child(label)
                .when_some(tip, |el, tip| el.tooltip(move |_, cx| tooltip_view(tip.clone(), cx)))
                .on_click(cx.listener(move |ws, _, window, cx| match &status {
                    LspStatus::Missing { installer: Some(_), .. } => {
                        if let Some(path) = ws.active_editor().and_then(|e| e.read(cx).doc.read(cx).path.clone()) {
                            ws.lsp.update(cx, |s, cx| s.install_server(&path, cx));
                        }
                    }
                    LspStatus::Missing { command: install, installer: None } => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(install.to_string()));
                        ws.toast(
                            ToastKind::Success,
                            "Install command copied",
                            Some(format!("Run `{install}` in a terminal, then restart the language server.")),
                            cx,
                        );
                    }
                    LspStatus::Failed(_) => ws.restart_language_server(&RestartLanguageServer, window, cx),
                    _ => {}
                }))
                .into_any_element(),
        )
    }

    pub(super) fn render_statusbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let lsp_item = self.render_lsp_status(&theme, cx);
        let active_editor = self.active_editor().cloned();
        let editor = self.active_editor().map(|e| e.read(cx));
        // The file's problems and the last Run's.
        let run_problems = self.problems.read(cx).run_shown(editor.map(|e| e.doc.read(cx)));
        let count = |sev: Severity| {
            editor.map_or(0, |e| e.doc.read(cx).diagnostics.iter().filter(|d| d.severity == sev).count())
                + run_problems.iter().filter(|p| p.severity == sev).count()
        };
        let (errors, warnings, infos) = (count(Severity::Error), count(Severity::Warning), count(Severity::Info));

        let item = |child: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_full()
                .px_2()
                .child(child)
        };
        let counter = |icon: Icon, n: usize, color: Hsla| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .when(n > 0, |el| el.text_color(color))
                .child(icon_el(icon, px(13.), if n > 0 { color } else { theme.text_dim.into() }))
                .child(n.to_string())
        };

        let mut right: Vec<AnyElement> = Vec::new();
        // An image tab has no caret, indentation or line endings.
        if let Some(e) = editor.filter(|e| e.doc.read(cx).image.is_none()) {
            if let Some(mode) = e.vim_status() {
                right.push(
                    item(div().font_family(theme.mono_font.clone()).text_size(px(11.)).text_color(theme.text).child(mode).into_any_element())
                        .into_any_element(),
                );
            }
            let (line, col) = e.cursor_position(cx);
            let selected = e.selected_chars(cx);
            let cursors = e.selections().len();
            let mut pos = format!("Ln {line}, Col {col}");
            if cursors > 1 {
                pos.push_str(&format!("  ({cursors} cursors)"));
            } else if selected > 0 {
                pos.push_str(&format!("  ({selected} selected)"));
            }
            right.push(
                item(div().font_family(theme.mono_font.clone()).text_size(px(11.)).text_color(theme.text_muted).child(pos).into_any_element())
                    .into_any_element(),
            );
            if let Some((name, error)) = e.external_linter_status() {
                let (icon, color, label) = match error {
                    None => (Icon::CircleCheck, Hsla::from(theme.success), name.to_string()),
                    Some(_) => (Icon::TriangleAlert, Hsla::from(theme.warning), format!("{name} failed")),
                };
                let detail = error.map(|e| e.to_string());
                right.push(
                    item(
                        div()
                            .id("external-linter")
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(icon_el(icon, px(13.), color))
                            .child(label)
                            .when_some(detail, |el, detail| {
                                el.tooltip(move |_, cx| tooltip_view(detail.clone(), cx))
                            })
                            .into_any_element(),
                    )
                    .into_any_element(),
                );
            }
            if let Some(lsp_item) = lsp_item {
                right.push(item(lsp_item).into_any_element());
            }
            right.push(item(e.indent_label().into_any_element()).into_any_element());
            let menu_item = |id: &'static str, label: String, tip: &'static str| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    // Same size and rounding as the docks and the problem counts.
                    .h(px(24.))
                    .px_2()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover_bg(theme.hover)
                    .hover(|s| s.text_color(theme.text))
                    .tooltip(move |_, cx| tooltip_view(tip, cx))
                    .child(label)
            };
            let tab = active_editor.clone();
            right.push(
                menu_item("line-endings", if e.doc.read(cx).uses_crlf { "CRLF".into() } else { "LF".into() }, "Line Endings")
                    .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                        if let Some(editor) = tab.clone() {
                            ws.open_line_endings_menu(editor, event.position(), window, cx);
                        }
                    }))
                    .into_any_element(),
            );
            let tab = active_editor.clone();
            right.push(
                menu_item("encoding", e.doc.read(cx).encoding.label(), "File Encoding")
                    .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                        if let Some(editor) = tab.clone() {
                            ws.open_encoding_menu(editor, event.position(), window, cx);
                        }
                    }))
                    .into_any_element(),
            );
            if e.doc.read(cx).language == Language::Python
                && self.root.is_some()
                && let Some(dir) = e.doc.read(cx).path.as_deref().and_then(Path::parent)
            {
                let label = match self.shown_venv(dir) {
                    Some(venv) => crate::python::label(&venv, self.root.as_deref()),
                    None => "No venv".into(),
                };
                right.push(
                    menu_item("python-venv", label, "Python Environment")
                        .on_click(cx.listener(|ws, event: &ClickEvent, window, cx| ws.open_venv_menu(event.position(), window, cx)))
                        .into_any_element(),
                );
            }
            let (icon, color) = match e.doc.read(cx).language {
                Language::Html => (Icon::CodeXml, Hsla::from(theme.text_muted)),
                Language::Markdown => (Icon::FileText, Hsla::from(theme.text_muted)),
                Language::PlainText => (Icon::FileText, theme.text_dim.into()),
                _ => (Icon::Braces, Hsla::from(theme.text_muted)),
            };
            right.push(
                item(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(icon_el(icon, px(13.), color))
                        .child(e.doc.read(cx).language.name())
                        .into_any_element(),
                )
                .into_any_element(),
            );
        }

        // Panel toggles sit in two small docks, like segmented controls.
        let dock = || dock(&theme);
        let toggle = |button: Button, on: bool| {
            button.size(px(20.)).rounded(px(7.)).when(on, |el| segment_on(el, &theme))
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .h(px(STATUSBAR_HEIGHT))
            .w_full()
            .px(px(GAP))
            .text_size(px(11.5))
            .text_color(theme.text_dim)
            // Shows or hides the sidebar; its Files / Commit switch is inside it.
            .child(
                dock().child(
                    toggle(
                        self.icon_button("toggle-sidebar", Icon::PanelLeft, self.show_sidebar, cx, |ws, _, window, cx| {
                            ws.set_sidebar_visible(!ws.show_sidebar, window, cx)
                        }),
                        self.show_sidebar,
                    )
                    .tooltip(|_, cx| tooltip_view("Toggle Sidebar", cx)),
                ),
            )
            .child(
                div()
                    .id("diagnostic-counts")
                    .flex()
                    .items_center()
                    .gap_2()
                    // Same size and rounding as the docks beside it.
                    .h(px(24.))
                    .px_2()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover_bg(theme.hover)
                    .hover(|s| s.text_color(theme.text_muted))
                    .on_click(cx.listener(|ws, _, window, cx| ws.toggle_problems(&ToggleProblems, window, cx)))
                    .child(counter(Icon::CircleX, errors, theme.error.into()))
                    .child(counter(Icon::TriangleAlert, warnings, theme.warning.into()))
                    .when(infos > 0, |el| el.child(counter(Icon::CircleAlert, infos, theme.info.into()))),
            )
            .when_some(self.git_busy, |el, busy| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .text_color(theme.text_muted)
                        .child(icon_el(Icon::Refresh, px(12.), theme.accent.into()))
                        .child(busy),
                )
            })
            .child(div().flex_1())
            .when_some(self.render_downloads(&theme), |el, downloads| el.child(downloads))
            .when_some(updater::pending_version(cx).map(|v| format!("Kin {v} is downloaded")), |el, tip| {
                el.child(
                    div()
                        .id("restart-to-update")
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(24.))
                        .px_2()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .hover_fill(theme.accent_soft, Hsla::from(theme.accent).alpha(0.24))
                        .on_click(cx.listener(|ws, _, _, cx| ws.restart_to_update(cx)))
                        .child(icon_el(Icon::Download, px(13.), theme.accent.into()))
                        .child("Restart to Update")
                        .tooltip(move |_, cx| tooltip_view(tip.clone(), cx)),
                )
            })
            .children(right)
            .child(
                dock()
                    .ml_1()
                    .when(self.repo.is_some(), |el| {
                        el.child(
                            toggle(
                                self.icon_button("toggle-git-log", Icon::GitCommit, self.shows(BottomPanel::GitLog), cx, |ws, _, window, cx| {
                                    ws.toggle_git_log(&ToggleGitLog, window, cx)
                                }),
                                self.shows(BottomPanel::GitLog),
                            )
                            .tooltip(|window, cx| status_tooltip("Git Log", &ToggleGitLog, window, cx)),
                        )
                    })
                    .when(self.debug.is_some(), |el| {
                        el.child(
                            toggle(
                                self.icon_button("toggle-debug", Icon::Bug, self.shows(BottomPanel::Debug), cx, |ws, _, window, cx| {
                                    ws.toggle_debug_panel(&ToggleDebugPanel, window, cx)
                                }),
                                self.shows(BottomPanel::Debug),
                            )
                            .tooltip(|window, cx| status_tooltip("Debug", &ToggleDebugPanel, window, cx)),
                        )
                    })
                    .child(
                        toggle(
                            self.icon_button("toggle-terminal", Icon::Terminal, self.shows(BottomPanel::Terminal), cx, |ws, _, window, cx| {
                                ws.toggle_terminal(&ToggleTerminal, window, cx)
                            }),
                            self.shows(BottomPanel::Terminal),
                        )
                        .tooltip(|window, cx| status_tooltip("Terminal", &ToggleTerminal, window, cx)),
                    )
                    .child(
                        toggle(
                            self.icon_button("toggle-problems", Icon::PanelBottom, self.shows(BottomPanel::Problems), cx, |ws, _, window, cx| {
                                ws.toggle_problems(&ToggleProblems, window, cx)
                            }),
                            self.shows(BottomPanel::Problems),
                        )
                        .tooltip(|window, cx| status_tooltip("Toggle Problems", &ToggleProblems, window, cx)),
                    ),
            )
            // Windows and Linux have no app menu, so Settings needs a way in.
            .child({
                let on = matches!(self.modal, Some(Modal::Preferences(_)));
                toggle(self.icon_button("open-settings", Icon::Settings, on, cx, |ws, _, window, cx| ws.open_settings(&OpenSettings, window, cx)), on)
                    .size(px(24.))
                    .tooltip(|window, cx| status_tooltip("Settings", &OpenSettings, window, cx))
            })
    }
}
