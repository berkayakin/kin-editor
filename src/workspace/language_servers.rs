//! Language servers: events from editors, navigation results, installs.

use super::*;

impl Workspace {
    // ---- Language servers -----------------------------------------------------------

    pub(super) fn on_editor_lsp_event(&mut self, event: &EditorLspEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            EditorLspEvent::Navigate { title, locations, encoding } => match locations.as_slice() {
                [one] => self.open_location(one.clone(), *encoding, window, cx),
                many => self.show_locations(title.clone(), many.to_vec(), *encoding, window, cx),
            },
            EditorLspEvent::Rename { server, path, position, current } => {
                let pending = PendingInput::RenameSymbol {
                    server: server.clone(),
                    path: path.clone(),
                    position: *position,
                    current: current.clone(),
                };
                self.ask_for_name(pending, window, cx);
            }
            EditorLspEvent::CodeActions { actions, position } => {
                let entries = actions
                    .iter()
                    .map(|(_, a)| if a.preferred { MenuEntry::item(a.title.clone(), Icon::Check) } else { MenuEntry::text(a.title.clone()) })
                    .collect();
                let menu_actions = actions.iter().map(|(server, a)| Some(MenuAction::CodeAction(server.clone(), a.clone()))).collect();
                self.show_context_menu(entries, menu_actions, *position, None, window, cx);
            }
            EditorLspEvent::Message(message) => self.toast(ToastKind::Info, message.clone(), None, cx),
        }
    }

    /// The open tab for `path`, comparing real paths (servers may report
    /// `/tmp/…` for `/private/tmp/…`).
    pub(super) fn tab_for(&self, path: &Path, cx: &App) -> Option<Entity<Editor>> {
        let wanted = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.tabs
            .iter()
            .find(|t| {
                t.read(cx).doc.read(cx).path.as_deref().is_some_and(|p| p == path || p.canonicalize().is_ok_and(|p| p == wanted))
            })
            .cloned()
    }

    pub(super) fn open_location(&mut self, location: Location, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(location.path.clone(), window, cx);
        if let Some(editor) = self.tab_for(&location.path, cx) {
            editor.update(cx, |e, cx| {
                let offset = protocol::position_to_offset(e.doc.read(cx).rope(), location.range.start, encoding);
                e.go_to(offset, window, cx);
            });
        }
    }

    /// Lists usages (or several declarations) to pick from.
    pub(super) fn show_locations(&mut self, title: SharedString, locations: Vec<Location>, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut rows = Vec::with_capacity(locations.len());
        for location in locations {
            let line = location.range.start.line as usize;
            let lines = files.entry(location.path.clone()).or_insert_with(|| match self.tab_for(&location.path, cx) {
                Some(editor) => editor.read(cx).doc.read(cx).rope().to_string().lines().map(str::to_string).collect(),
                None => std::fs::read_to_string(&location.path).unwrap_or_default().lines().map(str::to_string).collect(),
            });
            let preview = lines.get(line).map(|l| l.trim().to_string()).unwrap_or_default();
            rows.push(LocationRow { file: self.location_label(&location.path), line: line + 1, preview, location, encoding });
        }
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Locations, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modal {
            picker.update(cx, |p, cx| p.set_locations(Some(title), rows, cx));
        }
    }

    /// Applies a language server's edits, opening files that aren't open;
    /// each file's edits are one undo step. Returns whether all applied.
    pub(super) fn apply_workspace_edit(&mut self, edits: Vec<(PathBuf, Vec<TextEdit>)>, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let active = self.active_editor().cloned();
        let mut applied = true;
        for (path, file_edits) in edits {
            if self.tab_for(&path, cx).is_none() {
                self.open_path(path.clone(), window, cx);
            }
            let Some(editor) = self.tab_for(&path, cx) else {
                applied = false;
                continue;
            };
            editor.update(cx, |e, cx| {
                let resolved = protocol::resolve_edits(e.doc.read(cx).rope(), &file_edits, encoding);
                e.replace_ranges(resolved, cx);
            });
        }
        // Opening files switched tabs; go back to where the user was.
        if let Some(active) = active {
            self.activate_editor(&active, window, cx);
        }
        applied
    }

    pub(super) fn rename_symbol(&mut self, server: Arc<Server>, path: PathBuf, position: Position, new_name: String, window: &mut Window, cx: &mut Context<Self>) {
        let params = serde_json::json!({
            "textDocument": { "uri": protocol::path_to_uri(&path) },
            "position": position.to_json(),
            "newName": new_name,
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = server.request("textDocument/rename", params).await;
            this.update_in(cx, |ws, window, cx| match result {
                Ok(value) => {
                    let edits = protocol::workspace_edit(&value);
                    let (files, count) = (edits.len(), edits.iter().map(|(_, e)| e.len()).sum::<usize>());
                    if count == 0 {
                        ws.toast(ToastKind::Info, "Nothing to rename here", None, cx);
                    } else if ws.apply_workspace_edit(edits, server.encoding, window, cx) {
                        let files = if files == 1 { "1 file".to_string() } else { format!("{files} files") };
                        ws.toast(ToastKind::Success, format!("Renamed {count} occurrence{} in {files}", plural(count)), None, cx);
                    }
                }
                Err(err) => ws.toast(ToastKind::Error, "Couldn't rename", Some(err.message), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Runs a chosen code action: its edit, then its command; actions that
    /// carry neither are resolved first.
    pub(super) fn apply_code_action(&mut self, server: Arc<Server>, action: serde_json::Value, window: &mut Window, cx: &mut Context<Self>) {
        // A bare Command rather than a CodeAction.
        if action.get("command").and_then(serde_json::Value::as_str).is_some() {
            self.execute_command(server, action, cx);
            return;
        }
        let (edit, command) = (action.get("edit"), action.get("command"));
        if edit.is_none() && command.is_none() {
            if !server.resolves("codeActionProvider") {
                return;
            }
            cx.spawn_in(window, async move |this, cx| {
                let resolved = server.request("codeAction/resolve", action).await;
                this.update_in(cx, |ws, window, cx| match resolved {
                    Ok(resolved) if resolved.get("edit").is_some() || resolved.get("command").is_some() => {
                        ws.apply_code_action(server, resolved, window, cx);
                    }
                    Ok(_) => {}
                    Err(err) => ws.toast(ToastKind::Error, "Couldn't apply the action", Some(err.message), cx),
                })
                .ok();
            })
            .detach();
            return;
        }
        if let Some(edit) = edit {
            self.apply_workspace_edit(protocol::workspace_edit(edit), server.encoding, window, cx);
        }
        if let Some(command) = command.cloned() {
            self.execute_command(server, command, cx);
        }
    }

    /// Asks the server to run a command; it may answer with edits
    /// (`workspace/applyEdit`), which the LSP store forwards.
    pub(super) fn execute_command(&mut self, server: Arc<Server>, command: serde_json::Value, cx: &mut Context<Self>) {
        let params = serde_json::json!({
            "command": command.get("command").cloned().unwrap_or_default(),
            "arguments": command.get("arguments").cloned().unwrap_or_else(|| serde_json::json!([])),
        });
        cx.spawn(async move |this, cx| {
            if let Err(err) = server.request("workspace/executeCommand", params).await {
                this.update(cx, |ws, cx| ws.toast(ToastKind::Error, "The action failed", Some(err.message), cx)).ok();
            }
        })
        .detach();
    }

    pub(super) fn restart_language_server(&mut self, _: &RestartLanguageServer, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_editor().and_then(|e| e.read(cx).doc.read(cx).path.clone()) else { return };
        self.lsp.update(cx, |s, cx| s.restart(&path, cx));
    }

    pub(super) fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::Preferences(_))) {
            self.close_modal(window, cx);
            return;
        }
        self.close_modal(window, cx);
        let preferences = cx.new(|cx| Preferences::new(self.preferences_page, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&preferences, window, |ws, _, event: &PreferencesEvent, window, cx| match event {
            PreferencesEvent::OpenSettingsFile => {
                ws.close_modal(window, cx);
                ws.open_settings_file(&OpenSettingsFile, window, cx);
            }
            PreferencesEvent::OpenKeymapFile => {
                ws.close_modal(window, cx);
                ws.open_keymap(&OpenKeymap, window, cx);
            }
            PreferencesEvent::Problems(problems) => ws.report_config_problems(problems.clone(), cx),
            PreferencesEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Preferences(preferences));
        cx.notify();
    }

    /// Extensions…: the Extensions page of Settings.
    pub(super) fn open_extensions(&mut self, _: &OpenExtensions, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.preferences_page = preferences::Page::Extensions;
        self.open_settings(&OpenSettings, window, cx);
    }

    pub(super) fn reload_extensions(&mut self, _: &ReloadExtensions, _: &mut Window, cx: &mut Context<Self>) {
        extensions::reload(cx);
        self.toast(ToastKind::Success, "Extensions reloaded", None, cx);
    }

    pub(super) fn open_settings_file(&mut self, _: &OpenSettingsFile, window: &mut Window, cx: &mut Context<Self>) {
        self.open_config_file(Settings::path(), settings::SETTINGS_TEMPLATE, window, cx);
    }

    pub(super) fn open_keymap(&mut self, _: &OpenKeymap, window: &mut Window, cx: &mut Context<Self>) {
        self.open_config_file(keymap::keymap_path(), keymap::KEYMAP_TEMPLATE, window, cx);
    }
}
