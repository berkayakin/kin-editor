//! Running targets and debugging.

use super::*;

impl Workspace {
    // ---- Running ----------------------------------------------------------------------

    /// Runs a configuration in a Run tab, replacing the previous run of it.
    /// The editor keeps focus, like JetBrains' Run window.
    pub(super) fn run_target(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        // Unsaved edits would make the run disagree with what's on screen.
        for editor in self.tabs.clone() {
            if editor.read(cx).doc.read(cx).is_dirty() && editor.read(cx).doc.read(cx).path.is_some() {
                editor.update(cx, |e, cx| e.save(cx)).ok();
            }
        }
        self.restore_terminals(window, cx);
        // A rerun takes the previous run's place, in whichever group it is.
        let previous = self
            .all_terminals()
            .find(|t| t.read(cx).run().is_some_and(|(r, _)| r.name == target.name && r.command == target.command))
            .cloned()
            .and_then(|t| self.terminal_position(&t));
        if let Some((group, ix)) = previous {
            self.terminal_groups[group].tabs.remove(ix);
        }
        let place = match previous {
            Some((group, ix)) => TerminalPlace::Tab { group, ix: Some(ix) },
            None => TerminalPlace::Tab { group: self.active_group, ix: None },
        };
        self.last_run = Some(target.clone());
        let focused = self.active_editor().cloned();
        self.open_terminal(target.cwd.clone(), Some(target), place, false, window, cx);
        if let Some(editor) = focused {
            window.focus(&editor.focus_handle(cx));
        }
    }

    /// Run (⌃R / Shift+F10): the last configuration again, or the list.
    pub(super) fn rerun(&mut self, _: &Rerun, window: &mut Window, cx: &mut Context<Self>) {
        match self.last_run.clone() {
            Some(target) => self.run_target(target, window, cx),
            None => self.run_configuration(&RunConfiguration, window, cx),
        }
    }

    /// Stop (⌘F2 / Ctrl+F2): interrupts the newest running program.
    pub(super) fn stop_run(&mut self, _: &StopRun, _: &mut Window, cx: &mut Context<Self>) {
        // A debug session is what's running, if there is one.
        if let Some(session) = self.debug.as_ref().map(|(s, _)| s.clone()).filter(|s| s.read(cx).is_active()) {
            session.update(cx, |s, cx| s.stop(cx));
            return;
        }
        let running = self.all_terminals().filter(|t| t.read(cx).run().is_some_and(|(_, s)| *s == RunState::Running)).last().cloned();
        if let Some(terminal) = running {
            terminal.update(cx, |t, cx| t.interrupt(cx));
        }
    }
}

impl Workspace {
    // ---- Debugging --------------------------------------------------------------------

    /// A gutter ▶ (Run or Debug, as a menu when both work), or Run / Debug at caret.
    pub(super) fn on_run_event(&mut self, event: &EditorRunEvent, window: &mut Window, cx: &mut Context<Self>) {
        let target = event.target.clone();
        match event.menu_at {
            Some(position) if target.debug.is_some() => {
                let name = target.name.trim_start_matches("Run ").trim_start_matches("Test ").to_string();
                let entries = vec![
                    MenuEntry::item(format!("Run {name}"), Icon::Play).shortcut(binding_label(&editor::RunAtCaret, window)),
                    MenuEntry::item(format!("Debug {name}"), Icon::Bug).shortcut(binding_label(&editor::DebugAtCaret, window)),
                ];
                let actions = vec![Some(MenuAction::Launch(target.clone(), false)), Some(MenuAction::Launch(target, true))];
                self.show_context_menu(entries, actions, position, None, window, cx);
            }
            _ if event.debug => self.debug_target(target, window, cx),
            _ => self.run_target(target, window, cx),
        }
    }

    /// Starts debugging `target` in the Debug panel; one session at a time.
    pub(super) fn debug_target(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        if target.debug.is_none() {
            self.toast(ToastKind::Info, "This can't be debugged", Some("Rust, Go, Python and JavaScript tests and programs can be".into()), cx);
            return;
        }
        // What's debugged is what's on screen.
        for editor in self.tabs.clone() {
            if editor.read(cx).doc.read(cx).is_dirty() && editor.read(cx).doc.read(cx).path.is_some() {
                editor.update(cx, |e, cx| e.save(cx)).ok();
            }
        }
        if let Some((old, _)) = self.debug.take() {
            old.update(cx, |s, cx| s.stop(cx));
        }
        self.clear_debug_location(cx);
        let breakpoints = self.all_breakpoints(cx);
        let session = cx.new(|cx| DebugSession::new(target.clone(), breakpoints, cx));
        let subscription = cx.subscribe_in(&session, window, |ws, _, event: &DebugEvent, window, cx| ws.on_debug_event(event, window, cx));
        self.debug = Some((session, subscription));
        // Debug… offers the last one again; a process attached to may be gone.
        if !matches!(target.debug, Some(DebugSpec::Attach { .. })) {
            self.last_debug = Some(target);
        }
        self.show_debug = true;
        self.show_problems = false;
        self.show_terminal = false;
        self.show_git_log = false;
        cx.notify();
    }

    pub(super) fn on_debug_event(&mut self, event: &DebugEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            DebugEvent::Stopped { path, line } => {
                self.clear_debug_location(cx);
                self.debug_location = Some((path.clone(), *line));
                self.open_at(path.clone(), Some(*line), None, window, cx);
                if let Some(editor) = self.tab_for(path, cx) {
                    editor.update(cx, |e, cx| e.set_debug_line(Some(line.saturating_sub(1) as usize), cx));
                }
                // A breakpoint hit brings Kin forward, like JetBrains.
                window.activate_window();
            }
            DebugEvent::Resumed | DebugEvent::Ended => self.clear_debug_location(cx),
            DebugEvent::Hide => {
                self.show_debug = false;
                self.focus_active(window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn clear_debug_location(&mut self, cx: &mut Context<Self>) {
        if let Some((path, _)) = self.debug_location.take()
            && let Some(editor) = self.tab_for(&path, cx)
        {
            editor.update(cx, |e, cx| e.set_debug_line(None, cx));
        }
    }

    /// Every breakpoint: open files' (which move with edits) and the rest.
    pub(super) fn all_breakpoints(&self, cx: &App) -> HashMap<PathBuf, Vec<Breakpoint>> {
        let mut all = self.breakpoints.clone();
        for editor in &self.tabs {
            let e = editor.read(cx);
            // A tab not read yet has its file's breakpoints in `self.breakpoints`.
            if let Some(path) = e.doc.read(cx).path.clone().filter(|_| e.is_loaded()) {
                let breakpoints = e.breakpoints(cx);
                if breakpoints.is_empty() {
                    all.remove(&path);
                } else {
                    all.insert(path, breakpoints);
                }
            }
        }
        all
    }

    pub(super) fn breakpoints_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let (path, breakpoints) = {
            let e = editor.read(cx);
            (e.doc.read(cx).path.clone(), e.breakpoints(cx))
        };
        if let (Some(path), Some((session, _))) = (path, &self.debug) {
            session.update(cx, |s, _| s.set_breakpoints(path, breakpoints));
        }
        self.schedule_session_save(cx);
    }

    /// Right-click on a line number: add or remove its breakpoint, or give it
    /// a condition or a log message.
    pub(super) fn open_breakpoint_menu(&mut self, editor: Entity<Editor>, row: usize, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = editor.read(cx).breakpoint(row, cx).is_some();
        let toggle = binding_label(&editor::ToggleBreakpoint, window);
        let entries = if existing {
            vec![
                MenuEntry::item("Remove Breakpoint", Icon::Minus).shortcut(toggle),
                MenuEntry::item("Edit Condition…", Icon::Pencil).shortcut(binding_label(&AddConditionalBreakpoint, window)),
                MenuEntry::item("Edit Log Message…", Icon::Terminal).shortcut(binding_label(&AddLogpoint, window)),
            ]
        } else {
            vec![
                MenuEntry::item("Add Breakpoint", Icon::Plus).shortcut(toggle),
                MenuEntry::item("Add Conditional Breakpoint…", Icon::Pencil).shortcut(binding_label(&AddConditionalBreakpoint, window)),
                MenuEntry::item("Add Logpoint…", Icon::Terminal).shortcut(binding_label(&AddLogpoint, window)),
            ]
        };
        let actions = [BreakpointEdit::Toggle, BreakpointEdit::Condition, BreakpointEdit::LogMessage]
            .into_iter()
            .map(|edit| Some(MenuAction::Breakpoint(edit, editor.clone(), row)))
            .collect();
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Add Conditional Breakpoint… (⇧⌘F8 / Ctrl+Shift+F8 in JetBrains): on the caret's line.
    pub(super) fn add_conditional_breakpoint(&mut self, _: &AddConditionalBreakpoint, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_breakpoint_at_caret(BreakpointEdit::Condition, window, cx);
    }

    /// Add Logpoint…: on the caret's line.
    pub(super) fn add_logpoint(&mut self, _: &AddLogpoint, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_breakpoint_at_caret(BreakpointEdit::LogMessage, window, cx);
    }

    pub(super) fn edit_breakpoint_at_caret(&mut self, edit: BreakpointEdit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        if !editor.read(cx).can_break(cx) {
            self.toast(ToastKind::Info, "Breakpoints work in Rust, Go, Python, JavaScript and TypeScript files", None, cx);
            return;
        }
        let row = editor.read(cx).caret_row(cx);
        self.edit_breakpoint(edit, editor, row, window, cx);
    }

    /// Asks for a breakpoint's condition or log message; confirming adds the
    /// breakpoint if the line has none, and an empty text clears the option.
    pub(super) fn edit_breakpoint(&mut self, edit: BreakpointEdit, editor: Entity<Editor>, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = editor.read(cx).breakpoint(row, cx).unwrap_or(Breakpoint::at(row));
        let line = row + 1;
        let (title, hint, initial) = match edit {
            BreakpointEdit::Condition => (
                format!("Breakpoint Condition · Line {line}"),
                "Stops only when this expression is true, e.g. count > 10",
                current.condition.clone(),
            ),
            _ => (
                format!("Log Message · Line {line}"),
                "Logs instead of stopping; {expression} shows a value, e.g. count = {count}",
                current.log_message.clone(),
            ),
        };
        let initial = initial.unwrap_or_default();
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| InputDialog::new(title, Icon::Bug, hint, &initial, Some(0..initial.len()), window, cx).allow_empty());
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            if let InputDialogEvent::Confirm(text) = event {
                let text = Some(text.trim().to_string()).filter(|t| !t.is_empty());
                let mut breakpoint = editor.read(cx).breakpoint(row, cx).unwrap_or(Breakpoint::at(row));
                match edit {
                    BreakpointEdit::Condition => breakpoint.condition = text,
                    _ => breakpoint.log_message = text,
                }
                editor.update(cx, |e, cx| e.set_breakpoint(breakpoint, cx));
            }
            ws.close_modal(window, cx);
        }));
        self.modal = Some(Modal::Input(dialog));
        cx.notify();
    }

    pub(super) fn debug_session(&self) -> Option<Entity<DebugSession>> {
        self.debug.as_ref().map(|(s, _)| s.clone())
    }

    /// Resume (⌥⌘R / F9; F5 in VS Code and Zed): continue, or with nothing
    /// to continue, debug the last configuration again (else at the caret).
    pub(super) fn resume(&mut self, _: &Resume, window: &mut Window, cx: &mut Context<Self>) {
        match self.debug_session() {
            Some(session) if session.read(cx).is_paused() => session.update(cx, |s, cx| s.resume(cx)),
            Some(session) if session.read(cx).is_active() => {}
            _ => match self.last_debug.clone() {
                Some(target) => self.debug_target(target, window, cx),
                None => window.dispatch_action(Box::new(editor::DebugAtCaret), cx),
            },
        }
    }

    pub(super) fn pause_program(&mut self, _: &PauseProgram, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.pause(cx));
        }
    }

    pub(super) fn step_over(&mut self, _: &StepOver, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_over(cx));
        }
    }

    pub(super) fn step_into(&mut self, _: &StepInto, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_into(cx));
        }
    }

    pub(super) fn step_out(&mut self, _: &StepOut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_out(cx));
        }
    }

    pub(super) fn toggle_debug_panel(&mut self, _: &ToggleDebugPanel, window: &mut Window, cx: &mut Context<Self>) {
        if self.debug.is_none() {
            self.toast(ToastKind::Info, "Not debugging", Some("Start with Debug at Caret or Debug… in the Run menu".into()), cx);
            return;
        }
        self.show_debug = !self.show_debug;
        if self.show_debug {
            self.show_problems = false;
            self.show_terminal = false;
            self.show_git_log = false;
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    pub(super) fn render_debug_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (session, _) = self.debug.as_ref()?;
        let theme = Theme::get(cx).clone();
        Some(
            island(&theme)
                .h(self.problems_height)
                .flex_shrink_0()
                .child(cached(session))
                .child(self.resize_handle(Edge::Problems, cx)),
        )
    }

    /// Run… (⌃⌥R / Alt+Shift+F10): the project's configurations and the current file.
    pub(super) fn run_configuration(&mut self, _: &RunConfiguration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(targets) = self.configurations(self.last_run.clone(), false, cx) else { return };
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Run, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modal {
            picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
        }
    }

    /// Debug… (⌃⌥D / Alt+Shift+F9): the configurations that can be debugged.
    pub(super) fn debug_configuration(&mut self, _: &DebugConfiguration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut targets) = self.configurations(self.last_debug.clone(), true, cx) else { return };
        targets.retain(|t| t.debug.is_some());
        if targets.is_empty() {
            self.toast(ToastKind::Info, "Nothing to debug here", Some("Rust, Go, Python and JavaScript tests and programs can be debugged".into()), cx);
            return;
        }
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Debug, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modal {
            picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
        }
    }

    /// Attach to Process… (⌥⇧F5 / Ctrl+Alt+F5 in JetBrains): debug a program
    /// that's already running; the adapter depends on what it runs.
    pub(super) fn attach_to_process(&mut self, _: &AttachToProcess, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(windows) {
            self.toast(ToastKind::Info, "Attaching to a process isn't supported on Windows yet", None, cx);
            return;
        }
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Attach, window, cx);
        let cwd = self.root.clone().or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_default();
        let task = cx.spawn(async move |ws, cx| {
            let processes = cx.background_executor().spawn(async move { launch::processes() }).await;
            let targets: Vec<RunTarget> = processes
                .into_iter()
                .map(|p| RunTarget {
                    name: format!("{} ({})", p.name, p.pid),
                    command: p.command,
                    cwd: cwd.clone(),
                    row: None,
                    rows: 0..0,
                    debug: Some(DebugSpec::Attach { pid: p.pid, program: p.program }),
                })
                .collect();
            ws.update(cx, |ws, cx| {
                if let Some(Modal::Picker(picker)) = &ws.modal
                    && picker.read(cx).mode() == PickerMode::Attach
                {
                    picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
                }
            })
            .ok();
        });
        task.detach();
    }

    /// Run… and Debug…'s list: settings' configurations, the project's, the
    /// current file's; `last` first. None (after a notice) when there's nothing.
    /// What Run… (or with `debug`, Debug…) offers: settings' configurations,
    /// detected project commands, the current file's tests and mains, and for
    /// Debug the project's own debug configurations (`.vscode/launch.json`,
    /// `.zed/debug.json`).
    pub(super) fn configurations(&mut self, last: Option<RunTarget>, debug: bool, cx: &mut Context<Self>) -> Option<Vec<RunTarget>> {
        let root = self
            .root
            .clone()
            .or_else(|| self.active_editor().and_then(|e| e.read(cx).doc.read(cx).path.as_deref()?.parent().map(Path::to_path_buf)));
        let Some(root) = root else {
            self.toast(ToastKind::Info, "Open a folder or a file to run something", None, cx);
            return None;
        };
        let current = self.active_editor().and_then(|e| {
            let doc = e.read(cx).doc.read(cx);
            doc.path.clone().map(|p| (p, doc.language))
        });
        let mut targets: Vec<RunTarget> = Settings::get(cx)
            .run_configurations
            .iter()
            .map(|c| RunTarget {
                name: c.name.clone(),
                command: c.command.clone(),
                cwd: c.cwd.as_ref().map(|d| root.join(d)).unwrap_or_else(|| root.clone()),
                row: None,
                rows: 0..0,
                debug: None,
            })
            .collect();
        if debug {
            targets.extend(launch::project_configurations(&root, current.as_ref().map(|(p, _)| p.as_path())));
        }
        targets.extend(run::project_targets(&root, current.as_ref().map(|(p, l)| (p.as_path(), *l))));
        // Then the current file's tests and mains, by name.
        if let Some(editor) = self.active_editor() {
            let file = editor.update(cx, |e, cx| e.run_targets(cx));
            targets.extend(file.iter().filter(|t| !targets.iter().any(|o| o.command == t.command)).cloned().collect::<Vec<_>>());
        }
        // The last one comes first, so Enter repeats it.
        if let Some(last) = last {
            // Debug configurations have no command: those go by name.
            targets.retain(|t| !(t.command == last.command && t.cwd == last.cwd && (!t.command.is_empty() || t.name == last.name)));
            targets.insert(0, last);
        }
        if targets.is_empty() {
            self.toast(ToastKind::Info, "Nothing to run here", Some("Add run_configurations to settings.json".into()), cx);
            return None;
        }
        Some(targets)
    }

    pub(super) fn close_terminal(&mut self, _: &CloseTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.active_terminal().cloned() {
            self.remove_terminal(&terminal, window, cx);
        }
    }

    /// Opens a new shell in a group of its own beside the current group.
    pub(super) fn split_terminal(&mut self, _: &SplitTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.restore_terminals(window, cx);
        let cwd = self.terminal_cwd(cx);
        let place = if self.terminal_groups.is_empty() {
            TerminalPlace::Tab { group: 0, ix: None }
        } else {
            TerminalPlace::GroupAfter(self.active_group)
        };
        self.open_terminal(cwd, None, place, true, window, cx);
    }

    pub(super) fn remove_terminal(&mut self, terminal: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((g, ix)) = self.terminal_position(terminal) else { return };
        let was_focused = terminal.focus_handle(cx).contains_focused(window, cx);
        let group = &mut self.terminal_groups[g];
        group.tabs.remove(ix);
        // The current tab stays current when another one closes.
        if ix < group.active {
            group.active -= 1;
        }
        group.active = group.active.min(group.tabs.len().saturating_sub(1));
        // A group goes with its last tab; the one after it (else before) takes over.
        if group.tabs.is_empty() {
            self.terminal_groups.remove(g);
            if self.active_group > g {
                self.active_group -= 1;
            }
            self.active_group = self.active_group.min(self.terminal_groups.len().saturating_sub(1));
        }
        if self.terminal_groups.is_empty() {
            self.show_terminal = false;
            self.active_group = 0;
            self.save_state();
            self.focus_active(window, cx);
        } else if was_focused {
            self.focus_terminal(window, cx);
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Renames the current terminal tab (Find Action's Rename Terminal).
    pub(super) fn rename_terminal(&mut self, _: &RenameTerminal, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_terminal().cloned() {
            Some(terminal) => self.open_rename_terminal(terminal, window, cx),
            None => self.toast(ToastKind::Info, "No terminal to rename", None, cx),
        }
    }

    pub(super) fn open_rename_terminal(&mut self, terminal: Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let initial = terminal.read(cx).title().to_string();
        self.close_modal(window, cx);
        let hint = "Leave empty to show the shell's own title";
        let dialog =
            cx.new(|cx| InputDialog::new("Rename Terminal", Icon::Terminal, hint, &initial, Some(0..initial.len()), window, cx).allow_empty());
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, move |ws, _, event: &InputDialogEvent, window, cx| {
            if let InputDialogEvent::Confirm(name) = event {
                terminal.update(cx, |t, cx| t.set_custom_title(Some(name.clone()), cx));
                ws.schedule_session_save(cx);
            }
            ws.close_modal(window, cx);
            // Back to the terminal, rather than the editor `close_modal` focuses.
            if let Some((g, ix)) = ws.terminal_position(&terminal) {
                ws.activate_terminal(g, ix, window, cx);
            }
        }));
        self.modal = Some(Modal::Input(dialog));
        cx.notify();
    }

    /// A terminal tab's right-click menu.
    pub(super) fn open_terminal_tab_menu(&mut self, terminal: Entity<TerminalView>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((g, ix)) = self.terminal_position(&terminal) else { return };
        let count = self.terminal_groups[g].tabs.len();
        let run = terminal.read(cx).run().map(|(_, state)| *state);
        let renamed = terminal.read(cx).custom_title().is_some();
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        let tab = |action: TerminalTabAction| Some(MenuAction::TerminalTab(action, terminal.clone()));
        add(MenuEntry::item("Rename…", Icon::Pencil), tab(TerminalTabAction::Rename));
        if renamed {
            add(MenuEntry::text("Reset Name"), tab(TerminalTabAction::ResetName));
        }
        add(MenuEntry::text("Clear"), tab(TerminalTabAction::Clear));
        match run {
            Some(RunState::Running) => add(MenuEntry::item("Stop", Icon::Stop).shortcut(binding_label(&StopRun, window)), tab(TerminalTabAction::Stop)),
            Some(RunState::Exited(_)) => add(MenuEntry::item("Rerun", Icon::Refresh), tab(TerminalTabAction::Rerun)),
            None => {}
        }
        add(MenuEntry::Separator, None);
        add(MenuEntry::item("New Terminal", Icon::Plus).shortcut(binding_label(&NewTerminal, window)), tab(TerminalTabAction::New));
        add(
            MenuEntry::item("Split Terminal", Icon::SplitRight).shortcut(binding_label(&SplitTerminal, window)),
            tab(TerminalTabAction::Split),
        );
        add(MenuEntry::Separator, None);
        add(MenuEntry::item("Close", Icon::X), tab(TerminalTabAction::Close));
        if count > 1 {
            add(MenuEntry::text("Close Others"), tab(TerminalTabAction::CloseOthers));
        }
        if ix + 1 < count {
            add(MenuEntry::text("Close Tabs to the Right"), tab(TerminalTabAction::CloseToTheRight));
        }
        if count > 1 {
            add(MenuEntry::text("Close All"), tab(TerminalTabAction::CloseAll));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    pub(super) fn run_terminal_tab_action(&mut self, action: TerminalTabAction, terminal: Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((g, ix)) = self.terminal_position(&terminal) else { return };
        let closing: Vec<Entity<TerminalView>> = match action {
            TerminalTabAction::Rename => return self.open_rename_terminal(terminal, window, cx),
            TerminalTabAction::ResetName => {
                terminal.update(cx, |t, cx| t.set_custom_title(None, cx));
                self.schedule_session_save(cx);
                return;
            }
            TerminalTabAction::Clear => {
                terminal.update(cx, |t, cx| t.clear_all(cx));
                return self.activate_terminal(g, ix, window, cx);
            }
            TerminalTabAction::Stop => {
                terminal.update(cx, |t, cx| t.interrupt(cx));
                return;
            }
            TerminalTabAction::Rerun => {
                if let Some((target, _)) = terminal.read(cx).run().cloned() {
                    self.run_target(target, window, cx);
                }
                return;
            }
            TerminalTabAction::New => {
                self.active_group = g;
                return self.new_terminal(&NewTerminal, window, cx);
            }
            TerminalTabAction::Split => {
                self.active_group = g;
                return self.split_terminal(&SplitTerminal, window, cx);
            }
            TerminalTabAction::Close => vec![terminal],
            TerminalTabAction::CloseOthers => self.terminal_groups[g].tabs.iter().filter(|t| **t != terminal).cloned().collect(),
            TerminalTabAction::CloseToTheRight => self.terminal_groups[g].tabs[ix + 1..].to_vec(),
            TerminalTabAction::CloseAll => self.terminal_groups[g].tabs.clone(),
        };
        if action != TerminalTabAction::CloseAll && action != TerminalTabAction::Close {
            // The tab that was right-clicked stays, as the current one.
            self.activate_terminal(g, ix, window, cx);
        }
        for terminal in closing {
            self.remove_terminal(&terminal, window, cx);
        }
    }

    /// The bottom panel's terminals: each group is an island with its own
    /// tab strip, side by side like split panes.
    /// The bottom panel's terminals: each group is an island with its own
    /// browser-like tabs on the chrome above it, side by side like split panes.
    pub(super) fn render_terminal_panel(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A panel reopened at launch gets its shell in `window_ready`; until
        // then it's one empty group.
        let count = self.terminal_groups.len().max(1);
        let bounds = self.terminal_bounds.clone();
        div()
            .relative()
            .flex()
            .gap(px(GAP))
            .h(self.problems_height)
            .flex_shrink_0()
            .child(gpui::canvas(move |b, window, _| track_bounds(&bounds, b, window), |_, _, _, _| {}).absolute().size_full())
            .children((0..count).map(|g| self.render_terminal_group(g, count, window, cx)))
            .child(self.resize_handle(Edge::Problems, cx))
    }

    pub(super) fn render_terminal_group(&self, g: usize, count: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let group = self.terminal_groups.get(g);
        let current = g == self.active_group;
        let last = g + 1 == count;
        // With splits, the group that has the keyboard is outlined (its current tab too), like a pane.
        let edge: Hsla = if count > 1 && current {
            Hsla::from(theme.accent).alpha(0.35)
        } else {
            Hsla::from(theme.border).alpha(0.75)
        };
        let tabs: Vec<Entity<TerminalView>> = group.map(|g| g.tabs.clone()).unwrap_or_default();
        let active = group.map_or(0, |g| g.active);
        // The strip's room for tabs: the group's share of the panel, less the
        // strip's insets and its buttons (+, split, and hide on the last group).
        let buttons = TERMINAL_BUTTON * if last { 3. } else { 2. } + 8.;
        let total = f32::from(self.terminal_bounds.get().size.width);
        let available = (total > 0.).then(|| (total - GAP * (count - 1) as f32) / count as f32 - 2. * TAB_STRIP_INSET - buttons);
        let names: Vec<String> = tabs.iter().map(|t| t.read(cx).title().to_string()).collect();
        let name_widths = measure_tab_names(&names, window, &theme);
        let naturals: Vec<f32> = name_widths.iter().map(|(w, _)| (TAB_CHROME + w).ceil()).collect();
        let slots = fit_tabs(&naturals, active, available.unwrap_or(f32::INFINITY));
        let visible: Vec<usize> = (0..slots.len()).filter(|&i| slots[i] != TabSlot::Hidden).collect();
        let hidden: Vec<(Entity<TerminalView>, String)> =
            (0..slots.len()).filter(|&i| slots[i] == TabSlot::Hidden).map(|i| (tabs[i].clone(), names[i].clone())).collect();

        let tab_elements = visible.iter().enumerate().map(|(pos, &ix)| {
            let terminal = &tabs[ix];
            let slot = slots[ix];
            let is_active = ix == active;
            let separator = !is_active && visible.get(pos + 1).is_some_and(|&n| n != active);
            let after_active = pos > 0 && visible[pos - 1] == active;
            let icon_only = matches!(slot, TabSlot::Icon(_));
            let compact = matches!(slot, TabSlot::Compact(_));
            let hover_group = SharedString::from(format!("terminal-tab-{g}-{ix}"));
            let (for_close, for_middle, for_menu) = (terminal.clone(), terminal.clone(), terminal.clone());
            let label = tab_label(&names[ix], slot, naturals[ix], name_widths[ix]);
            let name_color: Hsla = if is_active && current { theme.text.into() } else { theme.text_muted.into() };
            let tab = div()
                .id(SharedString::from(format!("terminal-tab-{g}-{ix}")))
                .group(hover_group.clone())
                .relative()
                .flex()
                .flex_shrink_0()
                .items_center()
                .gap_1p5()
                .h_full()
                .cursor_pointer();
            tab_slot_width(tab_shape(tab, is_active, after_active, separator, hover_group.clone(), edge, &theme), slot, naturals[ix])
                .when(icon_only || compact, |el| {
                    let name = names[ix].clone();
                    el.tooltip(move |_, cx| tooltip_view(name.clone(), cx))
                })
                .on_click(cx.listener(move |ws, _, window, cx| ws.activate_terminal(g, ix, window, cx)))
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |ws, _, window, cx| ws.remove_terminal(&for_middle, window, cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |ws, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        ws.open_terminal_tab_menu(for_menu.clone(), event.position, window, cx);
                    }),
                )
                .child(match terminal.read(cx).run().map(|(_, state)| *state) {
                    // A Run tab shows how its program is doing.
                    Some(RunState::Running) => icon_el(Icon::Play, px(13.), theme.success.into()),
                    Some(RunState::Exited(Some(0))) => icon_el(Icon::CircleCheck, px(13.), theme.success.into()),
                    Some(RunState::Exited(_)) => icon_el(Icon::CircleX, px(13.), theme.error.into()),
                    None => icon_el(Icon::Terminal, px(13.), if is_active { theme.accent.into() } else { theme.text_dim.into() }),
                })
                .when(!icon_only, |el| {
                    el.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(name_color)
                            .when(is_active, |el| el.font_weight(FontWeight::MEDIUM))
                            .when(!is_active, |el| el.group_hover(hover_group.clone(), |s| s.text_color(theme.text)))
                            .child(label),
                    )
                })
                .when(!icon_only && !compact, |el| {
                    el.child(
                        div()
                            .id(SharedString::from(format!("close-terminal-{g}-{ix}")))
                            .size(px(18.))
                            .flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .when(!is_active, |el| el.invisible())
                            .group_hover(hover_group.clone(), |s| s.visible())
                            .hover_bg(theme.active)
                            .on_click(cx.listener(move |ws, _, window, cx| {
                                cx.stop_propagation();
                                ws.remove_terminal(&for_close, window, cx);
                            }))
                            .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                    )
                })
                .into_any_element()
        });
        // Buttons on the chrome, centered on the tabs.
        let button = |button: Button| button.flex_shrink_0().size(px(TERMINAL_BUTTON - 2.)).mb(px(4.)).mr(px(2.));
        let strip = div()
            .id(("terminal-tabs", g))
            .absolute()
            .top_0()
            .left_0()
            .right_0()
            .h(px(TAB_STRIP_HEIGHT))
            .flex()
            .items_end()
            .pt(px(TAB_STRIP_TOP))
            .pl(px(TAB_STRIP_INSET))
            .pr(px(TAB_STRIP_INSET - 6.))
            .overflow_hidden()
            .children(tab_elements)
            // The tabs that don't fit, in a menu.
            .when(!hidden.is_empty(), |el| {
                let count = hidden.len();
                el.child(
                    div()
                        .id(("terminal-overflow", g))
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
                        .tooltip(move |_, cx| tooltip_view(format!("{count} more terminal{}", plural(count)), cx))
                        .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                            let entries = hidden.iter().map(|(_, name)| MenuEntry::item(name.clone(), Icon::Terminal)).collect();
                            let actions = hidden.iter().map(|(t, _)| Some(MenuAction::Terminal(t.clone()))).collect();
                            ws.show_context_menu(entries, actions, event.position(), None, window, cx);
                        }))
                        .child("⋯")
                        .child(count.to_string()),
                )
            })
            .child(
                button(self.icon_button("new-terminal", Icon::Plus, false, cx, move |ws, _, window, cx| {
                    ws.active_group = g;
                    ws.new_terminal(&NewTerminal, window, cx);
                }))
                .id(("new-terminal", g))
                .ml_1()
                .tooltip(|window, cx| status_tooltip("New Terminal", &NewTerminal, window, cx)),
            )
            .child(div().flex_1())
            .child(
                button(self.icon_button("split-terminal", Icon::SplitRight, false, cx, move |ws, _, window, cx| {
                    ws.active_group = g;
                    ws.split_terminal(&SplitTerminal, window, cx);
                }))
                .id(("split-terminal", g))
                .tooltip(|window, cx| status_tooltip("Split Terminal", &SplitTerminal, window, cx)),
            )
            // The panel's own close button, on the group at its right end.
            .when(last, |el| {
                el.child(
                    button(self.icon_button("hide-terminal", Icon::X, false, cx, |ws, _, window, cx| {
                        ws.show_terminal = false;
                        ws.focus_active(window, cx);
                        ws.save_state();
                        ws.schedule_session_save(cx);
                        cx.notify();
                    }))
                    .tooltip(|window, cx| status_tooltip("Hide Terminal", &ToggleTerminal, window, cx)),
                )
            });
        div()
            .id(("terminal-group", g))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            // Clicking into a group makes it the current one.
            .capture_any_mouse_down(cx.listener(move |ws, _, _, cx| {
                if ws.active_group != g && g < ws.terminal_groups.len() {
                    ws.active_group = g;
                    cx.notify();
                }
            }))
            .child(div().flex_shrink_0().h(px(TAB_STRIP_HEIGHT - 1.)))
            .child(
                island(&theme)
                    .flex_1()
                    .min_h_0()
                    .border_color(edge)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .children(tabs.get(active).map(|t| div().flex_1().min_w_0().child(cached(t)))),
                    ),
            )
            // After the island, so the current tab paints over its top border.
            .child(strip)
            .into_any_element()
    }
}
