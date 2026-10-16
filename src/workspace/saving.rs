//! Saving, auto-save and closing tabs.

use super::*;

impl Workspace {
    // ---- Auto-save -------------------------------------------------------------------

    /// Restarts the idle timer for an edited tab in `after_delay` mode.
    pub(super) fn schedule_auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if self.auto_save != AutoSave::AfterDelay || !editor.read(cx).doc.read(cx).is_dirty() {
            return;
        }
        let delay = self.auto_save_delay;
        let target = editor.downgrade();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |ws, cx| {
                if let Some(editor) = target.upgrade() {
                    ws.auto_save(&editor, cx);
                }
            })
            .ok();
        });
        self.auto_save_tasks.insert(editor.entity_id(), task);
    }

    /// Writes a tab's edits to disk if it has a file and unsaved changes.
    /// Files changed or deleted by another program are left alone (and
    /// reported once) so auto-save never clobbers them. Returns whether the
    /// tab is saved afterwards.
    pub(super) fn auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> bool {
        let id = editor.entity_id();
        self.auto_save_tasks.remove(&id);
        let doc = editor.read(cx).doc.read(cx);
        if !doc.is_dirty() {
            return true;
        }
        if doc.path.is_none() {
            return false;
        }
        if doc.changed_on_disk() {
            let name = doc.file_name();
            if self.auto_save_warned.insert(id) {
                self.toast(
                    ToastKind::Error,
                    format!("{name} changed on disk"),
                    Some("Auto-save is paused for this file so it doesn't overwrite the other change. Save it manually to keep your version.".into()),
                    cx,
                );
            }
            return false;
        }
        match editor.update(cx, |e, cx| e.save(cx)) {
            Ok(()) => {
                self.auto_save_warned.remove(&id);
                self.refresh_git(cx);
                self.check_config(cx);
                // The session may hold the text as unsaved.
                self.schedule_session_save(cx);
                true
            }
            Err(err) => {
                let name = editor.read(cx).doc.read(cx).file_name();
                if self.auto_save_warned.insert(id) {
                    self.toast(ToastKind::Error, format!("Couldn't auto-save {name}"), Some(err.to_string()), cx);
                }
                false
            }
        }
    }

    pub(super) fn auto_save_all(&mut self, cx: &mut Context<Self>) {
        for editor in self.tabs.clone() {
            self.auto_save(&editor, cx);
        }
        if !self.documents.read(cx).kept(cx).is_empty() {
            self.save_background(true, cx);
        }
    }

    pub(super) fn toggle_auto_save(&mut self, _: &ToggleAutoSave, window: &mut Window, cx: &mut Context<Self>) {
        self.auto_save = if self.auto_save == AutoSave::Off { AutoSave::AfterDelay } else { AutoSave::Off };
        if let Err(err) = Settings::set("auto_save", self.auto_save.key().into()) {
            let _ = window.prompt(PromptLevel::Warning, "Couldn't save the auto-save choice", Some(&err.to_string()), &["OK"], cx);
        }
        self.check_config(cx);
        if self.auto_save == AutoSave::Off {
            self.auto_save_tasks.clear();
            self.toast(ToastKind::Success, "Auto-save off", None, cx);
        } else {
            self.auto_save_all(cx);
            self.toast(ToastKind::Success, "Auto-save on", None, cx);
        }
    }

    pub(super) fn check_for_updates(&mut self, _: &CheckForUpdates, _: &mut Window, cx: &mut Context<Self>) {
        let check = updater::check(cx);
        cx.spawn(async move |this, cx| {
            let result = check.await;
            this.update(cx, |ws, cx| match result {
                Ok(updater::Check::UpToDate) => {
                    ws.toast(ToastKind::Success, format!("Kuana {} is up to date", env!("CARGO_PKG_VERSION")), None, cx)
                }
                Ok(updater::Check::Ready(version)) => ws.toast(
                    ToastKind::Success,
                    format!("Kuana {version} is ready"),
                    Some("Restart to Update in the status bar installs it.".into()),
                    cx,
                ),
                Err(err) => ws.toast(ToastKind::Error, "Couldn't check for updates", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Downloads in progress: the first one's name and a bar (how many more
    /// after it); each one's size, progress and speed in the tooltip, which
    /// keeps up while it's shown.
    pub(super) fn render_downloads(&self, theme: &Theme) -> Option<AnyElement> {
        let downloads = crate::downloads::active();
        let first = downloads.first()?;
        let mut label = first.label.clone();
        if downloads.len() > 1 {
            label.push_str(&format!(" +{}", downloads.len() - 1));
        }
        let track = Hsla::from(theme.text_muted).alpha(0.25);
        // Without a percentage yet, a short stub shows it's started.
        let fill = first.fraction.unwrap_or(0.04).max(0.04);
        Some(
            div()
                .id("downloads")
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(24.))
                .px_2()
                .text_color(theme.text_muted)
                .child(icon_el(Icon::Download, px(13.), theme.accent.into()))
                .child(label)
                .child(
                    div()
                        .w(px(64.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(track)
                        .child(div().h_full().rounded_full().bg(theme.accent).w(relative(fill))),
                )
                .tooltip(|_, cx| crate::ui::live_tooltip_view(crate::downloads::summary, cx))
                .into_any_element(),
        )
    }

    pub(super) fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = updater::restart(cx) {
            self.toast(ToastKind::Error, "Couldn't install the update", Some(err), cx);
        }
    }

    /// Saves what it can before the window closes, the app quits or the
    /// project is replaced. A project's unsaved edits go into its session, so
    /// that always proceeds. Without a project, returns false (and asks the
    /// user) when edits would still be lost: untitled files, files that
    /// couldn't be saved, or any edits with auto-save off.
    pub(super) fn prepare_to_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.auto_save != AutoSave::Off {
            self.auto_save_all(cx);
        }
        // A project keeps unsaved edits in its session and brings them back
        // next time ("hot exit"), so there's nothing to ask.
        if self.root.is_some() {
            self.session_task = None;
            match self.save_session(cx) {
                Ok(()) => return true,
                Err(err) => eprintln!("couldn't save the session: {err}"),
            }
        }
        if self.close_confirmed && !matches!(intent, CloseIntent::ReplaceProject(_)) {
            return true;
        }
        let background = self.documents.read(cx).kept(cx);
        let unsaved: Vec<String> = self
            .tabs
            .iter()
            .map(|t| t.read(cx).doc.read(cx))
            .chain(background.iter().map(|d| d.read(cx)))
            .filter(|d| d.is_dirty())
            .map(|d| d.file_name())
            .collect();
        if unsaved.is_empty() {
            return true;
        }
        let message = match unsaved.as_slice() {
            [one] => format!("Do you want to save the changes you made to {one}?"),
            many => format!("Do you want to save the changes you made to {} files?", many.len()),
        };
        let rx = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Your changes will be lost if you don't save them."),
            &["Save All", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = rx.await else { return };
            this.update_in(cx, |ws, window, cx| match answer {
                0 => {
                    ws.save_background(false, cx);
                    for editor in ws.tabs.clone() {
                        if editor.read(cx).doc.read(cx).is_dirty()
                            && editor.read(cx).doc.read(cx).path.is_some()
                            && let Err(err) = editor.update(cx, |e, cx| e.save(cx))
                        {
                            let name = editor.read(cx).doc.read(cx).file_name();
                            ws.toast(ToastKind::Error, format!("Couldn't save {name}"), Some(err.to_string()), cx);
                        }
                    }
                    // Untitled files need a name first; show the first one.
                    if let Some(dirty) = ws.tabs.iter().find(|t| t.read(cx).doc.read(cx).is_dirty()).cloned() {
                        ws.activate_editor(&dirty, window, cx);
                        if dirty.read(cx).doc.read(cx).path.is_none() {
                            ws.save_as(&SaveAs, window, cx);
                        }
                    } else if ws.documents.read(cx).kept(cx).is_empty() {
                        ws.finish_close(intent, window, cx);
                    }
                }
                1 => ws.finish_close(intent, window, cx),
                // Windows that already agreed to quit must ask again next time.
                _ if matches!(intent, CloseIntent::Quit) => cx.defer(cancel_quit),
                _ => {}
            })
            .ok();
        })
        .detach();
        false
    }

    pub(super) fn finish_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) {
        match intent {
            CloseIntent::CloseWindow => {
                self.close_confirmed = true;
                window.remove_window();
            }
            CloseIntent::Quit => {
                self.close_confirmed = true;
                // Go on to the next window with unsaved edits.
                cx.defer(quit);
            }
            CloseIntent::ReplaceProject(path) => {
                self.close_project(window, cx);
                self.set_root(path, window, cx);
            }
        }
    }

    pub(super) fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        let current = editor.read(cx).doc.read(cx).path.clone();
        let dir = current
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| self.root.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let name = editor.read(cx).doc.read(cx).file_name();
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    let result = editor.update(cx, |e, cx| {
                        e.set_path(path, cx);
                        e.save(cx)
                    });
                    if let Err(err) = result {
                        let _ = window.prompt(PromptLevel::Critical, "Couldn't save file", Some(&err.to_string()), &["OK"], cx);
                    }
                    let doc = editor.read(cx).doc.clone();
                    ws.documents.update(cx, |s, cx| s.add(&doc, cx));
                    match &current {
                        Some(old) => ws.lsp.update(cx, |s, cx| s.moved(old, &editor, cx)),
                        None => ws.lsp.update(cx, |s, cx| s.register(&editor, cx)),
                    }
                    ws.tree.update(cx, |t, cx| t.rebuild(cx));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    pub(super) fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.close_editor(&editor, window, cx);
        }
    }

    pub(super) fn close_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let editor = editor.clone();
        if self.auto_save != AutoSave::Off {
            self.auto_save(&editor, cx);
        }
        // Another tab still shows the document: nothing is lost.
        if editor.read(cx).doc.read(cx).is_dirty() && self.siblings(&editor, cx).is_empty() {
            let name = editor.read(cx).doc.read(cx).file_name();
            let rx = window.prompt(
                PromptLevel::Warning,
                &format!("Do you want to save the changes you made to {name}?"),
                Some("Your changes will be lost if you don't save them."),
                &["Save", "Don't Save", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let Ok(answer) = rx.await else { return };
                this.update_in(cx, |ws, window, cx| match answer {
                    0 => {
                        if editor.read(cx).doc.read(cx).path.is_some() && editor.update(cx, |e, cx| e.save(cx)).is_ok() {
                            ws.remove_tab(&editor, window, cx);
                        } else {
                            ws.activate_editor(&editor, window, cx);
                            ws.save_as(&SaveAs, window, cx);
                        }
                    }
                    1 => ws.remove_tab(&editor, window, cx),
                    _ => {}
                })
                .ok();
            })
            .detach();
        } else {
            self.remove_tab(&editor, window, cx);
        }
    }

    pub(super) fn remove_tab(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t == editor) else { return };
        self.tabs.remove(ix);
        self.pinned.remove(&editor.entity_id());
        self.tab_subscriptions.remove(&editor.entity_id());
        let was_active_pane = self.locate(editor).is_some_and(|(p, _)| p == self.active_pane);
        if let Some(sibling) = self.siblings(editor, cx).into_iter().next() {
            // The document stays open in another tab: it takes over.
            if let Some(path) = editor.read(cx).doc.read(cx).path.clone() {
                self.lsp.update(cx, |s, _| s.rebind(&path, &sibling));
            }
            if let Some(unloaded) = editor.update(cx, |e, _| e.take_unloaded()) {
                sibling.update(cx, |e, _| e.put_unloaded(unloaded));
            }
        } else if let Some(path) = editor.read(cx).doc.read(cx).path.clone() {
            // Closed without saving: its edits go with it, so the store doesn't keep it.
            let doc = editor.read(cx).doc.clone();
            self.documents.update(cx, |s, cx| s.release(&doc, cx));
            self.lsp.update(cx, |s, _| s.did_close(&path));
            let breakpoints = editor.read(cx).breakpoints(cx);
            if !breakpoints.is_empty() {
                self.breakpoints.insert(path, breakpoints);
            }
        }
        self.schedule_session_save(cx);
        self.auto_save_tasks.remove(&editor.entity_id());
        self.auto_save_warned.remove(&editor.entity_id());
        if let Some((pane_ix, tab_ix)) = self.locate(editor) {
            let pane = &mut self.panes[pane_ix];
            pane.tabs.remove(tab_ix);
            if tab_ix < pane.active || pane.active >= pane.tabs.len() {
                pane.active = pane.active.saturating_sub(1);
            }
            // A pane closes with its last tab (unless it's the only one).
            if pane.tabs.is_empty() && self.panes.len() > 1 {
                self.remove_pane(pane_ix);
            }
        }
        if self.tabs.is_empty() {
            self.show_find = false;
            self.find_bar.update(cx, |f, cx| f.set_target(None, cx));
            window.focus(&self.focus_handle);
        } else if was_active_pane || self.active_editor().is_none() {
            let pane = self.active_pane;
            match self.panes[pane].tabs.is_empty() {
                true => window.focus(&self.focus_handle),
                false => self.activate_in(pane, self.panes[pane].active, window, cx),
            }
        }
        cx.notify();
    }

    pub(super) fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let pane = &self.panes[self.active_pane];
        if !pane.tabs.is_empty() {
            let ix = (pane.active + 1) % pane.tabs.len();
            self.activate_in(self.active_pane, ix, window, cx);
        }
    }

    pub(super) fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        let pane = &self.panes[self.active_pane];
        if !pane.tabs.is_empty() {
            let ix = (pane.active + pane.tabs.len() - 1) % pane.tabs.len();
            self.activate_in(self.active_pane, ix, window, cx);
        }
    }
}
