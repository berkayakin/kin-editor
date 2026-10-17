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

    /// A new pane after the current one, showing the current tab too (for a
    /// file, a second editor of the same document, with its own carets and
    /// scroll; see `PaneItem::split`).
    pub(super) fn split(&mut self, axis: PaneAxis, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() >= MAX_PANES {
            return;
        }
        // One direction at a time: splitting the other way re-flows the panes.
        self.pane_axis = axis;
        let shown = self.active_item().cloned();
        self.active_pane = self.insert_pane_after(self.active_pane);
        match shown {
            Some(item) => {
                self.load_item(&item, cx);
                let split = item.split(cx);
                self.add_tab(split, window, cx);
            }
            None => window.focus(&self.focus_handle),
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// An empty pane right after `source`, taking half of its space; returns
    /// its index.
    fn insert_pane_after(&mut self, source: usize) -> usize {
        let at = source + 1;
        self.panes.insert(at, Pane::new());
        let half = self.pane_sizes[source] / 2.;
        self.pane_sizes[source] = half;
        self.pane_sizes.insert(at, half);
        at
    }

    /// Moves a tab to `pane` (before index `before`, or at the end).
    pub(super) fn move_tab(&mut self, item: &PaneItem, pane: usize, before: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((from, ix)) = self.locate(item.entity_id()) else { return };
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
        target.tabs.insert(at, item.clone());
        self.keep_pinned_first(pane);
        // The source pane closes if that was its last tab.
        if from != pane && self.panes[from].tabs.is_empty() && self.panes.len() > 1 {
            self.remove_pane(from);
        }
        self.activate_item(item, window, cx);
        self.schedule_session_save(cx);
    }

    pub(super) fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() < 2 {
            return;
        }
        let next = (self.active_pane + 1) % self.panes.len();
        self.active_pane = next;
        match self.panes[next].tabs.get(self.panes[next].active).cloned() {
            Some(item) => self.activate_item(&item, window, cx),
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
        if let Some(item) = self.active_item().cloned() {
            self.run_tab_action(TabAction::TogglePin, &item, window, cx);
        }
    }

    pub(super) fn close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.active_item().cloned() {
            self.run_tab_action(TabAction::CloseOthers, &item, window, cx);
        }
    }

    pub(super) fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(item) = self.active_item().cloned() {
            self.run_tab_action(TabAction::CloseAll, &item, window, cx);
        }
    }

    pub(super) fn run_tab_action(&mut self, action: TabAction, item: &PaneItem, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane, ix)) = self.locate(item.entity_id()) else { return };
        let others = |ws: &Self, keep: &dyn Fn(usize, &PaneItem) -> bool| -> Vec<PaneItem> {
            ws.panes[pane]
                .tabs
                .iter()
                .enumerate()
                .filter(|(i, t)| !keep(*i, t) && !ws.pinned.contains(&t.entity_id()))
                .map(|(_, t)| t.clone())
                .collect()
        };
        let to_close = match action {
            TabAction::Close => vec![item.clone()],
            TabAction::CloseOthers => others(self, &|_, t| t == item),
            TabAction::CloseAll => others(self, &|_, _| false),
            TabAction::CloseToTheRight => others(self, &|i, _| i <= ix),
            TabAction::TogglePin => {
                let id = item.entity_id();
                if !self.pinned.remove(&id) {
                    self.pinned.insert(id);
                }
                self.keep_pinned_first(pane);
                self.schedule_session_save(cx);
                cx.notify();
                return;
            }
            TabAction::SelectInTree => {
                self.activate_item(item, window, cx);
                self.select_opened_file(&SelectOpenedFile, window, cx);
                return;
            }
            TabAction::Activate => {
                self.activate_item(item, window, cx);
                return;
            }
            TabAction::SplitRight | TabAction::SplitDown => {
                self.activate_item(item, window, cx);
                let axis = if action == TabAction::SplitRight { PaneAxis::Row } else { PaneAxis::Column };
                self.split(axis, window, cx);
                return;
            }
        };
        for item in to_close {
            self.close_item(&item, window, cx);
        }
    }

    pub(super) fn open_tab_menu(&mut self, item: PaneItem, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let pinned = self.pinned.contains(&item.entity_id());
        let path = item.path(cx);
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        let tab = |action: TabAction| Some(MenuAction::Tab(action, item.clone()));
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

    /// ⌘K V: a Markdown file's preview, beside it (see `toggle_preview_of`);
    /// in a preview, closes it.
    pub(super) fn toggle_markdown_preview(&mut self, _: &ToggleMarkdownPreview, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_item().cloned() {
            Some(PaneItem::Editor(editor)) => self.toggle_preview_of(&editor, window, cx),
            Some(preview @ PaneItem::Preview(_)) => self.close_item(&preview, window, cx),
            _ => {}
        }
    }

    /// Shows the preview of `editor`'s Markdown document in the pane after
    /// the editor's (a new split beside it if there's none), leaving the
    /// keyboard in the editor; one showing already closes instead.
    pub(super) fn toggle_preview_of(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if editor.read(cx).doc.read(cx).language != Language::Markdown {
            return;
        }
        let Some((editor_pane, _)) = self.locate(editor.entity_id()) else { return };
        let previews = self.previews_of(&editor.read(cx).doc.clone(), cx);
        let shown = |ws: &Self, item: &PaneItem| ws.panes.iter().any(|p| p.tabs.get(p.active) == Some(item));
        if previews.iter().any(|p| shown(self, p)) {
            for preview in previews {
                self.remove_item(&preview, window, cx);
            }
            if let Some((pane, ix)) = self.locate(editor.entity_id()) {
                self.activate_in(pane, ix, window, cx);
            }
            return;
        }
        // Open in another pane, behind another tab: brought to the front (one
        // behind the editor itself couldn't show beside it).
        let found = previews.iter().filter_map(|p| self.locate(p.entity_id())).find(|(pane, _)| *pane != editor_pane);
        let (pane, ix) = match found {
            Some(found) => found,
            None => {
                let pane = if editor_pane + 1 < self.panes.len() {
                    editor_pane + 1
                } else if self.panes.len() < MAX_PANES {
                    if self.panes.len() == 1 {
                        self.pane_axis = PaneAxis::Row;
                    }
                    self.insert_pane_after(editor_pane)
                } else {
                    editor_pane - 1
                };
                let doc = editor.read(cx).doc.clone();
                let preview = cx.new(|cx| MarkdownPreview::new(doc, cx));
                self.active_pane = pane;
                (pane, self.insert_tab(preview.into(), window, cx))
            }
        };
        self.panes[pane].active = ix;
        self.sync_previewed(cx);
        // The keyboard stays where the typing is.
        self.activate_editor(editor, window, cx);
    }

    /// The Markdown previews of a document, in pane order.
    pub(super) fn previews_of(&self, doc: &Entity<Document>, cx: &App) -> Vec<PaneItem> {
        self.items().filter(|t| t.preview().is_some_and(|p| p.read(cx).document() == doc)).cloned().collect()
    }

    /// Tells Markdown editors whether a preview of their document is open,
    /// which their preview toggle shows.
    pub(super) fn sync_previewed(&self, cx: &mut App) {
        let docs: Vec<Entity<Document>> = self.items().filter_map(|t| Some(t.preview()?.read(cx).document().clone())).collect();
        for editor in &self.editors {
            let previewed = docs.contains(&editor.read(cx).doc);
            editor.update(cx, |e, cx| e.set_previewed(previewed, cx));
        }
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
        self.save_state(cx);
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
            self.refresh_commit_view(cx);
        }
        self.save_state(cx);
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
        self.save_state(cx);
        cx.notify();
    }

    pub(super) fn show_commit(&mut self, _: &ShowCommit, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = true;
        self.sidebar_mode = SidebarMode::Commit;
        self.refresh_commit_view(cx);
        self.git_panel.read(cx).focus_message(window, cx);
        self.save_state(cx);
        cx.notify();
    }

    pub(super) fn toggle_commit(&mut self, _: &ToggleCommit, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Commit {
            self.show_sidebar = false;
            self.focus_active(window, cx);
            self.save_state(cx);
            cx.notify();
        } else {
            self.show_commit(&ShowCommit, window, cx);
        }
    }
}
