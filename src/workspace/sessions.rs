//! The project session: tabs, panes and panels saved per project and brought back.

use super::*;

impl Workspace {
    // ---- Project session --------------------------------------------------------

    /// Writes the project's tabs, carets and unsaved text for next time.
    pub(super) fn save_session(&self, cx: &App) -> std::io::Result<()> {
        self.session_snapshot(cx).map_or(Ok(()), SessionSnapshot::write)
    }

    /// The session to save, with unsaved texts as rope clones: making them
    /// strings, serializing and writing can happen off the UI thread.
    pub(super) fn session_snapshot(&self, cx: &App) -> Option<SessionSnapshot> {
        let root = self.root.clone()?;
        let mut tabs = Vec::new();
        let mut unsaved = Vec::new();
        let mut active = 0;
        let mut pane_active = Vec::new();
        for (pane_ix, pane) in self.panes.iter().enumerate() {
            let mut active_in_pane = 0;
            let mut count = 0;
            for (ix, tab) in pane.tabs.iter().enumerate() {
                let e = tab.read(cx);
                let dirty = e.doc.read(cx).is_dirty();
                // A tab not read yet goes back as it was restored.
                if let Some(u) = e.unloaded() {
                    if ix == pane.active {
                        active_in_pane = count;
                        if pane_ix == self.active_pane {
                            active = tabs.len();
                        }
                    }
                    count += 1;
                    if let Some(text) = &u.unsaved {
                        unsaved.push((tabs.len(), text.clone()));
                    }
                    tabs.push(TabSession {
                        path: e.doc.read(cx).path.clone(),
                        cursor: u.cursor,
                        unsaved: None,
                        pane: pane_ix,
                        pinned: self.pinned.contains(&tab.entity_id()),
                        folds: u.folds.clone(),
                        encoding: u.encoding.filter(|&enc| enc != TextEncoding::UTF8).map(TextEncoding::key),
                    });
                    continue;
                }
                // An untitled buffer without edits has nothing to bring back.
                if e.doc.read(cx).path.is_none() && !dirty {
                    continue;
                }
                if ix == pane.active {
                    active_in_pane = count;
                    if pane_ix == self.active_pane {
                        active = tabs.len();
                    }
                }
                count += 1;
                if dirty {
                    unsaved.push((tabs.len(), e.doc.read(cx).rope().clone()));
                }
                tabs.push(TabSession {
                    path: e.doc.read(cx).path.clone(),
                    cursor: e.primary_selection().head,
                    unsaved: None,
                    pane: pane_ix,
                    pinned: self.pinned.contains(&tab.entity_id()),
                    folds: e.folded_rows(cx),
                    encoding: (e.doc.read(cx).encoding != TextEncoding::UTF8).then(|| e.doc.read(cx).encoding.key()),
                });
            }
            pane_active.push(active_in_pane);
        }
        let layout = (self.panes.len() > 1).then(|| PaneLayout {
            column: self.pane_axis == PaneAxis::Column,
            sizes: self.pane_sizes.clone(),
            active: pane_active,
        });
        let mut expanded: Vec<PathBuf> =
            self.tree.read(cx).expanded().iter().filter_map(|p| p.strip_prefix(&root).ok()).filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).collect();
        expanded.sort();
        let mut breakpoints: Vec<FileBreakpoints> = self
            .all_breakpoints(cx)
            .into_iter()
            .filter_map(|(path, breakpoints)| Some(FileBreakpoints { path: path.strip_prefix(&root).ok()?.to_path_buf(), breakpoints }))
            .collect();
        breakpoints.sort_by(|a, b| a.path.cmp(&b.path));
        let python_venv = crate::python::choice(&root).map(|c| c.to_saved());
        let terminals = Some(self.terminal_layout(cx));
        let session = ProjectSession { root, tabs, active, layout, expanded, breakpoints, python_venv, terminals };
        Some(SessionSnapshot { session, unsaved })
    }

    /// The terminal panel as the session keeps it: each shell's folder and
    /// name, by group. Run tabs are left out (they'd run their program again).
    pub(super) fn terminal_layout(&self, cx: &App) -> TerminalLayout {
        if self.terminal_groups.is_empty()
            && let Some(saved) = &self.saved_terminals
        {
            return TerminalLayout { open: self.shows(BottomPanel::Terminal), ..saved.clone() };
        }
        let mut groups = Vec::new();
        let mut active = 0;
        for (g, group) in self.terminal_groups.iter().enumerate() {
            let mut tabs = Vec::new();
            let mut current = 0;
            for (ix, terminal) in group.tabs.iter().enumerate() {
                let terminal = terminal.read(cx);
                if terminal.run().is_some() {
                    continue;
                }
                // The current tab, else the closest one before it.
                if ix <= group.active {
                    current = tabs.len();
                }
                tabs.push(TerminalTabSession {
                    name: terminal.custom_title().map(|t| t.to_string()),
                    cwd: Some(terminal.current_dir()),
                });
            }
            if tabs.is_empty() {
                continue;
            }
            if g <= self.active_group {
                active = groups.len();
            }
            groups.push(TerminalGroupSession { tabs, active: current });
        }
        TerminalLayout { open: self.shows(BottomPanel::Terminal), groups, active }
    }

    pub(super) fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if self.root.is_none() {
            return;
        }
        self.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            let Ok(Some(snapshot)) = this.update(cx, |ws, cx| ws.session_snapshot(cx)) else { return };
            if let Err(err) = cx.background_executor().spawn(async move { snapshot.write() }).await {
                eprintln!("couldn't save the session: {err}");
            }
        }));
    }

    /// Reopens the tabs the project had last time, with their unsaved edits.
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.root.as_deref().and_then(ProjectSession::load) else {
            return;
        };
        // Tabs read their files when first shown (see `Editor::load`): the
        // current tab of each pane right away, the others when activated.
        let mut restored: Vec<(PathBuf, Entity<Editor>)> = Vec::new();
        for tab in session.tabs {
            // The file is gone and there's nothing unsaved to keep.
            if tab.unsaved.is_none() && tab.path.as_deref().is_none_or(|p| !p.is_file()) {
                continue;
            }
            let unloaded = Unloaded {
                cursor: tab.cursor,
                unsaved: tab.unsaved.as_deref().map(ropey::Rope::from_str),
                folds: tab.folds,
                encoding: tab.encoding.as_deref().and_then(TextEncoding::from_key),
            };
            // A file shown in two panes is one document, in two editors.
            let shown = tab.path.as_ref().and_then(|p| restored.iter().find(|(q, _)| q == p)).map(|(_, e)| e.clone());
            let editor = match shown {
                Some(first) => cx.new(|cx| Editor::view_of(&first, cx)),
                None => cx.new(|cx| Editor::new_unloaded(tab.path.clone(), unloaded, cx)),
            };
            if let Some(path) = &tab.path {
                restored.push((path.clone(), editor.clone()));
            }
            // Into the pane it was in, creating panes as needed.
            while self.panes.len() <= tab.pane.min(7) {
                self.panes.push(Pane::new());
                self.pane_sizes.push(0.);
            }
            self.active_pane = tab.pane.min(7);
            if tab.pinned {
                self.pinned.insert(editor.entity_id());
            }
            let at = self.insert_tab(editor, window, cx);
            self.panes[self.active_pane].active = at;
            if let Some(path) = tab.path {
                self.recent.retain(|p| p != &path);
                self.recent.insert(0, path);
            }
        }
        self.recent.truncate(50);
        // Panes left empty (their files are gone) aren't worth keeping.
        self.drop_empty_panes();
        if let Some(layout) = session.layout.filter(|l| l.sizes.len() == self.panes.len()) {
            self.pane_axis = if layout.column { PaneAxis::Column } else { PaneAxis::Row };
            self.pane_sizes = layout.sizes;
            for (pane, active) in self.panes.iter_mut().zip(layout.active) {
                pane.active = active.min(pane.tabs.len().saturating_sub(1));
            }
        } else {
            self.pane_sizes = vec![1. / self.panes.len() as f32; self.panes.len()];
        }
        if let Some(editor) = self.tabs_in_order().get(session.active).cloned() {
            self.activate_editor(&editor, window, cx);
        }
        self.load_shown_tabs(cx);
    }

    /// Tabs in pane order, as the session lists them.
    pub(super) fn tabs_in_order(&self) -> Vec<Entity<Editor>> {
        self.panes.iter().flat_map(|p| p.tabs.iter().cloned()).collect()
    }

    /// Removes empty panes (keeping at least one) and rebalances sizes.
    pub(super) fn drop_empty_panes(&mut self) {
        let mut ix = 0;
        while ix < self.panes.len() {
            if self.panes.len() > 1 && self.panes[ix].tabs.is_empty() {
                self.remove_pane(ix);
            } else {
                ix += 1;
            }
        }
    }

    pub(super) fn remove_pane(&mut self, ix: usize) {
        self.panes.remove(ix);
        let freed = if ix < self.pane_sizes.len() { self.pane_sizes.remove(ix) } else { 0. };
        // The neighbor takes the space.
        if let Some(size) = self.pane_sizes.get_mut(ix.saturating_sub(1).min(self.panes.len().saturating_sub(1))) {
            *size += freed;
        }
        if self.panes.len() == 1 {
            self.pane_sizes = vec![1.0];
        }
        if self.active_pane >= ix && self.active_pane > 0 {
            self.active_pane -= 1;
        }
        self.active_pane = self.active_pane.min(self.panes.len() - 1);
    }

    /// Changes the recent projects list shared by all windows. It's re-read
    /// from disk first so one window doesn't drop another's entries.
    pub(super) fn update_recent_projects(&mut self, f: impl FnOnce(&mut Vec<PathBuf>)) {
        let mut state = SessionState::load();
        f(&mut state.recent_projects);
        state.recent_projects.truncate(MAX_RECENT_PROJECTS);
        self.recent_projects = state.recent_projects.clone();
        if let Err(err) = state.save() {
            eprintln!("couldn't save recent projects: {err}");
        }
    }

    /// Remembers layout and the open folder for the next launch.
    pub(super) fn save_state(&self) {
        let saved = SessionState::load();
        let state = SessionState {
            // Opening a lone file shouldn't forget the remembered project.
            last_root: self.root.clone().or(saved.last_root),
            sidebar_width: Some(self.sidebar_width.into()),
            problems_height: Some(self.problems_height.into()),
            // The sidebar's visibility is remembered for projects only; it's
            // always closed without one.
            show_sidebar: if self.root.is_some() { Some(self.show_sidebar) } else { saved.show_sidebar },
            show_problems: Some(self.shows(BottomPanel::Problems)),
            show_terminal: Some(self.shows(BottomPanel::Terminal)),
            sidebar_mode: Some(match self.sidebar_mode {
                SidebarMode::Files => "files".into(),
                SidebarMode::Commit => "commit".into(),
            }),
            show_git_log: Some(self.shows(BottomPanel::GitLog)),
            // Shared with other windows, so keep what's on disk.
            font_zoom: saved.font_zoom,
            diff_side_by_side: saved.diff_side_by_side,
            recent_projects: saved.recent_projects,
            trusted_folders: saved.trusted_folders,
            window: saved.window,
            welcome_done: saved.welcome_done,
            other: saved.other,
        };
        if let Err(err) = state.save() {
            eprintln!("couldn't save window state: {err}");
        }
    }

    /// Saves where the window is (after moves and resizes settle), so the
    /// next launch opens it there.
    pub(super) fn remember_placement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (bounds, fullscreen) = match window.window_bounds() {
            WindowBounds::Fullscreen(bounds) => (bounds, true),
            WindowBounds::Windowed(bounds) | WindowBounds::Maximized(bounds) => (bounds, false),
        };
        let placement = WindowPlacement {
            display: window.display(cx).and_then(|d| d.uuid().ok()).map(|u| u.to_string()),
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
            fullscreen,
        };
        self.placement_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let mut state = SessionState::load();
            state.window = Some(placement);
            if let Err(err) = state.save() {
                eprintln!("couldn't save window state: {err}");
            }
        }));
    }

    /// Shows Welcome Setup (theme, keymap, editor basics) over the window.
    pub fn show_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let onboarding = cx.new(Onboarding::new);
        let subscription = cx.subscribe_in(&onboarding, window, |ws, _, event: &OnboardingEvent, window, cx| match event {
            OnboardingEvent::Done => ws.finish_onboarding(window, cx),
            OnboardingEvent::OpenSettings => {
                ws.finish_onboarding(window, cx);
                ws.open_settings(&OpenSettings, window, cx);
            }
            OnboardingEvent::BrowseThemes => ws.select_theme(&SelectTheme, window, cx),
            OnboardingEvent::Problems(problems) => ws.report_config_problems(problems.clone(), cx),
        });
        window.focus(&onboarding.focus_handle(cx));
        self.onboarding = Some((onboarding, subscription));
        cx.notify();
    }

    pub(super) fn welcome_setup(&mut self, _: &WelcomeSetup, window: &mut Window, cx: &mut Context<Self>) {
        self.show_onboarding(window, cx);
    }

    pub(super) fn finish_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.onboarding = None;
        SessionState::set_onboarded();
        self.focus_active(window, cx);
        cx.notify();
    }
}


/// A session to write, its unsaved texts still ropes (see `session_snapshot`).
pub(super) struct SessionSnapshot {
    pub(super) session: ProjectSession,
    /// Tabs with unsaved edits: index in `session.tabs` and the text.
    pub(super) unsaved: Vec<(usize, ropey::Rope)>,
}


impl SessionSnapshot {
    pub(super) fn write(mut self) -> std::io::Result<()> {
        for (ix, text) in self.unsaved {
            self.session.tabs[ix].unsaved = Some(text.to_string());
        }
        self.session.save()
    }
}
