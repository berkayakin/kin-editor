//! Panes and tabs: splitting, moving, pinning, closing.

use super::*;

impl Workspace {
    // ---- Panes and tab management ---------------------------------------------------------

    pub(super) fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(PaneAxis::Row, window, cx);
    }

    pub(super) fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(PaneAxis::Column, window, cx);
    }

    /// A new pane after the current one, showing the current file too (a
    /// second editor of the same document, with its own carets and scroll).
    pub(super) fn split(&mut self, axis: PaneAxis, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() >= 6 {
            return;
        }
        // One direction at a time: splitting the other way re-flows the panes.
        self.pane_axis = axis;
        let source = self.active_pane;
        let shown = self.active_editor().cloned();
        let at = source + 1;
        self.panes.insert(at, Pane::new());
        // The new pane takes half of the current one's space.
        let half = self.pane_sizes[source] / 2.;
        self.pane_sizes[source] = half;
        self.pane_sizes.insert(at, half);
        self.active_pane = at;
        match shown {
            Some(editor) => {
                self.load_tab(&editor, cx);
                let view = cx.new(|cx| Editor::view_of(&editor, cx));
                self.add_tab(view, window, cx);
            }
            None => window.focus(&self.focus_handle),
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Moves a tab to `pane` (before index `before`, or at the end).
    pub(super) fn move_tab(&mut self, editor: &Entity<Editor>, pane: usize, before: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((from, ix)) = self.locate(editor) else { return };
        let mut before = before;
        self.panes[from].tabs.remove(ix);
        if from == pane && before.is_some_and(|b| b > ix) {
            before = before.map(|b| b - 1);
        }
        let source = &mut self.panes[from];
        if ix < source.active || source.active >= source.tabs.len() {
            source.active = source.active.saturating_sub(1);
        }
        let target = &mut self.panes[pane];
        let at = before.unwrap_or(target.tabs.len()).min(target.tabs.len());
        target.tabs.insert(at, editor.clone());
        self.keep_pinned_first(pane);
        let target_pane = pane;
        // The source pane closes if that was its last tab.
        let editor = editor.clone();
        if from != target_pane && self.panes[from].tabs.is_empty() && self.panes.len() > 1 {
            self.remove_pane(from);
        }
        self.activate_editor(&editor, window, cx);
        self.schedule_session_save(cx);
    }

    pub(super) fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() < 2 {
            return;
        }
        let next = (self.active_pane + 1) % self.panes.len();
        self.active_pane = next;
        match self.panes[next].tabs.get(self.panes[next].active).cloned() {
            Some(editor) => self.activate_editor(&editor, window, cx),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    /// Pinned tabs first, in their order.
    pub(super) fn keep_pinned_first(&mut self, pane: usize) {
        let active = self.panes[pane].tabs.get(self.panes[pane].active).cloned();
        let pinned = &self.pinned;
        self.panes[pane].tabs.sort_by_key(|t| !pinned.contains(&t.entity_id()));
        if let Some(active) = active {
            self.panes[pane].active = self.panes[pane].tabs.iter().position(|t| t == &active).unwrap_or(0);
        }
    }

    pub(super) fn toggle_pin_tab(&mut self, _: &TogglePinTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.run_tab_action(TabAction::TogglePin, &editor, window, cx);
        }
    }

    pub(super) fn close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.run_tab_action(TabAction::CloseOthers, &editor, window, cx);
        }
    }

    pub(super) fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.run_tab_action(TabAction::CloseAll, &editor, window, cx);
        }
    }

    pub(super) fn run_tab_action(&mut self, action: TabAction, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane, ix)) = self.locate(editor) else { return };
        let others = |ws: &Self, keep: &dyn Fn(usize, &Entity<Editor>) -> bool| -> Vec<Entity<Editor>> {
            ws.panes[pane]
                .tabs
                .iter()
                .enumerate()
                .filter(|(i, t)| !keep(*i, t) && !ws.pinned.contains(&t.entity_id()))
                .map(|(_, t)| t.clone())
                .collect()
        };
        let to_close = match action {
            TabAction::Close => vec![editor.clone()],
            TabAction::CloseOthers => others(self, &|_, t| t == editor),
            TabAction::CloseAll => others(self, &|_, _| false),
            TabAction::CloseToTheRight => others(self, &|i, _| i <= ix),
            TabAction::TogglePin => {
                let id = editor.entity_id();
                if !self.pinned.remove(&id) {
                    self.pinned.insert(id);
                }
                self.keep_pinned_first(pane);
                self.schedule_session_save(cx);
                cx.notify();
                return;
            }
            TabAction::SelectInTree => {
                self.activate_editor(editor, window, cx);
                self.select_opened_file(&SelectOpenedFile, window, cx);
                return;
            }
            TabAction::Activate => {
                self.activate_editor(editor, window, cx);
                return;
            }
            TabAction::SplitRight | TabAction::SplitDown => {
                self.activate_editor(editor, window, cx);
                let axis = if action == TabAction::SplitRight { PaneAxis::Row } else { PaneAxis::Column };
                self.split(axis, window, cx);
                return;
            }
        };
        for editor in to_close {
            self.close_editor(&editor, window, cx);
        }
    }

    pub(super) fn open_tab_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let pinned = self.pinned.contains(&editor.entity_id());
        let path = editor.read(cx).doc.read(cx).path.clone();
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        let tab = |action: TabAction| Some(MenuAction::Tab(action, editor.clone()));
        add(MenuEntry::item("Close", Icon::X).shortcut(binding_label(&CloseTab, window)), tab(TabAction::Close));
        add(MenuEntry::text("Close Others"), tab(TabAction::CloseOthers));
        add(MenuEntry::text("Close All"), tab(TabAction::CloseAll));
        add(MenuEntry::text("Close Tabs to the Right"), tab(TabAction::CloseToTheRight));
        add(MenuEntry::Separator, None);
        add(MenuEntry::item(if pinned { "Unpin Tab" } else { "Pin Tab" }, Icon::Pin), tab(TabAction::TogglePin));
        add(MenuEntry::Separator, None);
        add(
            MenuEntry::item("Split Right", Icon::SplitRight).shortcut(binding_label(&SplitRight, window)),
            tab(TabAction::SplitRight),
        );
        add(
            MenuEntry::item("Split Down", Icon::SplitDown).shortcut(binding_label(&SplitDown, window)),
            tab(TabAction::SplitDown),
        );
        if let Some(path) = path {
            add(MenuEntry::Separator, None);
            if self.root.as_deref().is_some_and(|r| path.starts_with(r)) {
                add(
                    MenuEntry::item("Select in File Tree", Icon::FolderTree).shortcut(binding_label(&SelectOpenedFile, window)),
                    tab(TabAction::SelectInTree),
                );
            }
            add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(MenuAction::Tree(TreeAction::CopyPath(path.clone()))));
            add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(MenuAction::Tree(TreeAction::Reveal(path))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    pub(super) fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        // Like JetBrains tool windows: show Files, or hide if Files is showing.
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Files {
            self.show_sidebar = false;
        } else {
            self.show_sidebar = true;
            self.sidebar_mode = SidebarMode::Files;
        }
        // Leaving the Commit view: its message box must not keep focus while hidden, or
        // key presses (⌘0 included) go nowhere.
        if self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        self.save_state();
        cx.notify();
    }

    /// Shows or hides the sidebar, keeping the view it has.
    pub(super) fn set_sidebar_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = visible;
        // A hidden Commit view's message box must not keep focus (see `toggle_sidebar`).
        if !visible && self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        if visible && self.sidebar_mode == SidebarMode::Commit {
            self.refresh_git(cx);
        }
        self.save_state();
        cx.notify();
    }

    /// The sidebar switch's Files segment.
    pub(super) fn show_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_mode == SidebarMode::Files {
            return;
        }
        self.sidebar_mode = SidebarMode::Files;
        if self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        self.save_state();
        cx.notify();
    }

    pub(super) fn show_commit(&mut self, _: &ShowCommit, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = true;
        self.sidebar_mode = SidebarMode::Commit;
        self.refresh_git(cx);
        self.git_panel.read(cx).focus_message(window, cx);
        self.save_state();
        cx.notify();
    }

    pub(super) fn toggle_commit(&mut self, _: &ToggleCommit, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Commit {
            self.show_sidebar = false;
            self.focus_active(window, cx);
            self.save_state();
            cx.notify();
        } else {
            self.show_commit(&ShowCommit, window, cx);
        }
    }
}
