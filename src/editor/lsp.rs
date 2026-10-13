//! The editor's side of the language server: keeping the server's copy of
//! the document current, completion, hover documentation, signature help,
//! and the requests behind Go to Declaration, Find Usages, Rename and
//! Show Context Actions (whose results the workspace presents).

use std::{path::PathBuf, sync::Arc};

use gpui::{
    AnyElement, Context, FontWeight, HighlightStyle, MouseButton, Pixels, Point, SharedString, StyledText, Task,
    WeakEntity, Window, div, font, prelude::*, px,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use serde_json::{Value, json};

use super::{Editor, EditorEvent, EditorMode, Selection};
use crate::{
    document::EditKind,
    lint::Severity,
    lsp::{
        LspStore, Server,
        protocol::{
            self, CodeAction, CompletionItem, Encoding, HoverBlock, Location, Position, offset_to_position,
            offsets_to_range, path_to_uri, position_to_offset,
        },
    },
    theme::Theme,
};

/// Requests the workspace carries out, since they reach beyond this editor.
pub enum EditorLspEvent {
    /// Jump to one location, or let the user pick among several.
    Navigate { title: SharedString, locations: Vec<Location>, encoding: Encoding },
    /// Ask for a new name, then rename through the server.
    Rename { server: Arc<Server>, path: PathBuf, position: Position, current: String },
    /// Offer code actions in a menu at this window position.
    CodeActions { server: Arc<Server>, actions: Vec<CodeAction>, position: Point<Pixels> },
    /// Something to tell the user ("No usages found").
    Message(SharedString),
}

impl gpui::EventEmitter<EditorLspEvent> for Editor {}

const MENU_ROWS: usize = 10;
const MENU_ROW_HEIGHT: f32 = 22.;

pub(super) struct CompletionMenu {
    items: Arc<Vec<CompletionItem>>,
    /// Indices into `items` that match what's typed, best first.
    matches: Vec<usize>,
    selected: usize,
    /// First visible row.
    scroll: usize,
    /// Where the word being completed starts.
    start: usize,
    server: Arc<Server>,
    /// The server's list depends on the prefix; ask again as it grows.
    incomplete: bool,
}

pub(super) struct HoverPopover {
    /// Moving within this range keeps the popover.
    range: std::ops::Range<usize>,
    diagnostics: Vec<(Severity, String)>,
    blocks: Vec<HoverBlock>,
    /// Editor-local position of the popover's bottom-left (it opens upward).
    anchor: Point<Pixels>,
}

pub(super) struct SignatureHelp {
    label: String,
    active: Option<std::ops::Range<usize>>,
    row: usize,
}

/// LSP state an editor carries; part of `Editor`.
#[derive(Default)]
pub(super) struct EditorLsp {
    pub(super) store: Option<WeakEntity<LspStore>>,
    pub(super) completion: Option<CompletionMenu>,
    completion_task: Option<Task<()>>,
    pub(super) hover: Option<HoverPopover>,
    hover_task: Option<Task<()>>,
    /// The word a pending hover request is for.
    hover_pending: Option<std::ops::Range<usize>>,
    pub(super) signature: Option<SignatureHelp>,
    signature_task: Option<Task<()>>,
    /// The server's diagnostics as received, for code action requests.
    raw_diagnostics: Vec<Value>,
    /// The server reports diagnostics, so the built-in syntax check is redundant.
    pub(super) active: bool,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

impl Editor {
    pub fn set_lsp(&mut self, store: WeakEntity<LspStore>) {
        self.lsp.store = Some(store);
    }

    pub(super) fn lsp_server(&self, cx: &gpui::App) -> Option<Arc<Server>> {
        let path = self.doc.path.as_deref()?;
        self.lsp.store.as_ref()?.upgrade()?.read(cx).server_for(path)
    }

    /// Records an edit: tells the server and the workspace, and drops
    /// popovers that no longer apply.
    pub(super) fn edited(&mut self, cx: &mut Context<Self>) {
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.path.clone()) {
            let changes = self.doc.take_lsp_changes();
            store.read(cx).did_change(&path, self.doc.version(), changes, self.doc.rope());
        }
        self.lsp.hover = None;
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
        cx.emit(EditorEvent::Edited);
    }

    pub(super) fn lsp_did_save(&self, cx: &gpui::App) {
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.path.as_deref()) {
            store.read(cx).did_save(path, self.doc.rope());
        }
    }

    pub fn set_lsp_diagnostics(&mut self, params: &Value, encoding: Encoding, server: &str, cx: &mut Context<Self>) {
        let name: &'static str = match server {
            "" => "lsp",
            // Diagnostic sources are 'static; servers are few, so leaking the name once is fine.
            other => Box::leak(other.to_string().into_boxed_str()),
        };
        let diagnostics = protocol::diagnostics(params, self.doc.rope(), encoding, name);
        self.lsp.raw_diagnostics = params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
        self.doc.set_lsp_diagnostics(diagnostics);
        if !self.lsp.active {
            self.lsp.active = true;
            // Re-run the built-in linter without its now-duplicate syntax check.
            self.linted_version = None;
            self.schedule_lint(std::time::Duration::ZERO, cx);
        }
        cx.notify();
    }

    fn text_document(&self) -> Option<Value> {
        Some(json!({ "uri": path_to_uri(self.doc.path.as_deref()?) }))
    }

    fn position_params(&self, offset: usize, server: &Server) -> Option<Value> {
        Some(json!({
            "textDocument": self.text_document()?,
            "position": offset_to_position(self.doc.rope(), offset, server.encoding).to_json(),
        }))
    }

    // ---- Completion ------------------------------------------------------------------

    /// After typing: open, narrow or close completion, and update signature help.
    pub(super) fn after_typing(&mut self, typed: &str, cx: &mut Context<Self>) {
        if self.selections.len() != 1 || self.mode != EditorMode::Full {
            return;
        }
        let Some(server) = self.lsp_server(cx) else { return };
        let Some(last) = typed.chars().last() else { return };
        let head = self.primary_selection().head;
        let trigger = server
            .completion_triggers()
            .into_iter()
            .find(|t| head >= t.len() && self.doc.slice(head - t.len()..head) == *t);
        if server.capability("completionProvider") {
            match (&self.lsp.completion, trigger) {
                (_, Some(trigger)) => self.request_completion(Some(trigger), cx),
                (Some(menu), None) if is_word_char(last) && menu.incomplete => self.request_completion(None, cx),
                (Some(_), None) => {}
                (None, None) if is_word_char(last) => self.request_completion(None, cx),
                (None, None) => {}
            }
        }
        if server.capability("signatureHelpProvider")
            && (self.lsp.signature.is_some() || server.signature_triggers().iter().any(|t| typed.ends_with(t.as_str())))
        {
            self.request_signature_help(cx);
        }
    }

    pub(super) fn show_completions(&mut self, _: &super::ShowCompletions, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == EditorMode::Full {
            self.request_completion(None, cx);
        }
    }

    fn request_completion(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server(cx) else { return };
        let head = self.primary_selection().head;
        let start = self.word_start(head);
        let Some(mut params) = self.position_params(head, &server) else { return };
        params["context"] = match &trigger {
            Some(t) => json!({ "triggerKind": 2, "triggerCharacter": t }),
            None => json!({ "triggerKind": 1 }),
        };
        self.lsp.completion_task = Some(cx.spawn(async move |this, cx| {
            let Ok(value) = server.request("textDocument/completion", params).await else { return };
            let incomplete = value.get("isIncomplete").and_then(Value::as_bool).unwrap_or(false);
            let items = protocol::completion_items(value);
            this.update(cx, |e, cx| {
                e.lsp.completion = Some(CompletionMenu {
                    items: Arc::new(items),
                    matches: Vec::new(),
                    selected: 0,
                    scroll: 0,
                    start,
                    server,
                    incomplete,
                });
                e.refilter_completion(cx);
            })
            .ok();
        }));
    }

    /// Start of the identifier the caret is in or after.
    fn word_start(&self, offset: usize) -> usize {
        let mut start = offset;
        while let Some(c) = self.doc.char_before(start) {
            if !is_word_char(c) {
                break;
            }
            start -= c.len_utf8();
        }
        start
    }

    /// Re-matches the menu against the typed prefix; closes it when the caret
    /// left the word or nothing matches.
    pub(super) fn refilter_completion(&mut self, cx: &mut Context<Self>) {
        let Some(menu) = self.lsp.completion.as_mut() else { return };
        let head = self.selections.get(self.primary).map_or(0, |s| s.head);
        let valid = self.selections.len() == 1
            && head >= menu.start
            && self.doc.row_of(head) == self.doc.row_of(menu.start)
            && self.doc.slice(menu.start..head).chars().all(is_word_char);
        if !valid {
            self.lsp.completion = None;
            cx.notify();
            return;
        }
        let prefix = self.doc.slice(menu.start..head);
        if prefix.is_empty() {
            let mut order: Vec<usize> = (0..menu.items.len()).collect();
            order.sort_by(|&a, &b| menu.items[a].sort_text.cmp(&menu.items[b].sort_text));
            menu.matches = order;
        } else {
            let pattern = Pattern::parse(&prefix, CaseMatching::Smart, Normalization::Smart);
            let mut matcher = Matcher::new(Config::DEFAULT);
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, usize)> = menu
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, item)| Some((pattern.score(Utf32Str::new(&item.filter_text, &mut buf), &mut matcher)?, i)))
                .collect();
            // Best score first; the server's order breaks ties.
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| menu.items[a.1].sort_text.cmp(&menu.items[b.1].sort_text)));
            menu.matches = scored.into_iter().map(|(_, i)| i).collect();
        }
        if menu.matches.is_empty() {
            self.lsp.completion = None;
        } else {
            menu.selected = 0;
            menu.scroll = 0;
        }
        cx.notify();
    }

    /// Up/Down/Enter/Tab while the menu is open. Returns whether it took the key.
    pub(super) fn completion_key(&mut self, key: CompletionKey, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.lsp.completion.as_mut() else { return false };
        match key {
            CompletionKey::Move(delta) => {
                let len = menu.matches.len() as isize;
                menu.selected = ((menu.selected as isize + delta).rem_euclid(len)) as usize;
                if menu.selected < menu.scroll {
                    menu.scroll = menu.selected;
                } else if menu.selected >= menu.scroll + MENU_ROWS {
                    menu.scroll = menu.selected + 1 - MENU_ROWS;
                }
                cx.notify();
            }
            CompletionKey::Accept { replace } => {
                let ix = menu.selected;
                self.accept_completion(ix, replace, cx);
            }
        }
        true
    }

    fn accept_completion(&mut self, ix: usize, replace: bool, cx: &mut Context<Self>) {
        let Some(menu) = self.lsp.completion.take() else { return };
        let Some(mut item) = menu.matches.get(ix).map(|&i| menu.items[i].clone()) else { return };
        let (server, start) = (menu.server, menu.start);
        cx.notify();
        // Auto-imports often arrive only on resolve.
        if item.additional_edits.is_empty() && server.resolves("completionProvider") {
            self.lsp.completion_task = Some(cx.spawn(async move |this, cx| {
                if let Ok(resolved) = server.request("completionItem/resolve", item.raw.clone()).await {
                    item.merge_resolved(&resolved);
                }
                this.update(cx, |e, cx| e.apply_completion(&item, start, server.encoding, replace, cx)).ok();
            }));
        } else {
            self.apply_completion(&item, start, server.encoding, replace, cx);
        }
    }

    /// Inserts a completion at the caret. Enter inserts; Tab also replaces
    /// the rest of the word after the caret (as in JetBrains).
    fn apply_completion(&mut self, item: &CompletionItem, start: usize, encoding: Encoding, replace: bool, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let rope = self.doc.rope().clone();
        let from = item.edit.as_ref().map(|e| position_to_offset(&rope, e.range.start, encoding)).unwrap_or(start).min(head);
        let mut to = head;
        if replace {
            while let Some(c) = self.doc.char_at(to).filter(|c| is_word_char(*c)) {
                to += c.len_utf8();
            }
        }
        let (text, selection) = item.text();
        let mut edits = vec![(from..to, text.clone())];
        for (range, new_text) in protocol::resolve_edits(&rope, &item.additional_edits, encoding) {
            // An extra edit overlapping the main one is a server bug; skip it.
            if range.end <= from || range.start >= to {
                edits.push((range, new_text));
            }
        }
        // Where the inserted text starts once earlier extra edits apply.
        let shift: isize = edits[1..]
            .iter()
            .filter(|(r, _)| r.end <= from)
            .map(|(r, t)| t.len() as isize - r.len() as isize)
            .sum();
        let new_start = (from as isize + shift) as usize;
        self.edit_and_remap(edits, EditKind::Other, cx);
        let caret = match &selection {
            Some(r) => Selection { anchor: new_start + r.start, head: new_start + r.end },
            None => Selection::cursor(new_start + text.len()),
        };
        self.set_selections(vec![caret], 0, cx);
        self.autoscroll = true;
        // Accepting a function often opens its argument list.
        if text.ends_with('(') || selection.is_some() {
            self.request_signature_help(cx);
        }
    }

    // ---- Signature help ----------------------------------------------------------------

    fn request_signature_help(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server(cx) else { return };
        let head = self.primary_selection().head;
        let row = self.doc.row_of(head);
        let Some(params) = self.position_params(head, &server) else { return };
        self.lsp.signature_task = Some(cx.spawn(async move |this, cx| {
            let result = server.request("textDocument/signatureHelp", params).await;
            this.update(cx, |e, cx| {
                e.lsp.signature = result
                    .ok()
                    .as_ref()
                    .and_then(protocol::signature)
                    .map(|(label, active)| SignatureHelp { label, active, row });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Closes popovers that no longer fit where the caret is.
    pub(super) fn lsp_selection_changed(&mut self, cx: &mut Context<Self>) {
        if self.lsp.completion.is_some() {
            self.refilter_completion(cx);
        }
        let head = self.selections.get(self.primary).map_or(0, |s| s.head);
        if self.lsp.signature.as_ref().is_some_and(|s| s.row != self.doc.row_of(head)) {
            self.lsp.signature = None;
            self.lsp.signature_task = None;
        }
    }

    /// Escape: closes completion, signature help or hover. Returns whether
    /// anything was open.
    pub(super) fn dismiss_lsp_popovers(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.lsp.completion.take().is_some() | self.lsp.signature.take().is_some() | self.lsp.hover.take().is_some();
        if open {
            cx.notify();
        }
        open
    }

    // ---- Hover -------------------------------------------------------------------------

    /// Mouse moved over the editor (no button held): show documentation and
    /// problems for the word under it after a short pause.
    pub(super) fn hover_at(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.clone() else { return };
        let over_text = self.mode == EditorMode::Full
            && layout.bounds.contains(&position)
            && position.x >= layout.bounds.left() + layout.gutter_width
            && position.x < layout.bounds.right() - px(super::SCROLLBAR_WIDTH);
        if !over_text {
            self.clear_hover(cx);
            return;
        }
        let offset = self.offset_for_position(position, window, cx);
        let range = super::word_range(&self.doc, offset);
        let range = if range.is_empty() { offset..self.doc.next_char_boundary(offset) } else { range };
        if self.lsp.hover.as_ref().is_some_and(|h| h.range == range) || self.lsp.hover_pending.as_ref() == Some(&range) {
            return;
        }
        self.clear_hover(cx);
        self.lsp.hover_pending = Some(range.clone());
        self.lsp.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(450)).await;
            this.update_in(cx, |e, window, cx| e.show_hover(range, window, cx)).ok();
        }));
    }

    fn clear_hover(&mut self, cx: &mut Context<Self>) {
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
        if self.lsp.hover.take().is_some() {
            cx.notify();
        }
    }

    pub(super) fn quick_documentation(&mut self, _: &super::QuickDocumentation, window: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let range = super::word_range(&self.doc, head);
        self.show_hover(range, window, cx);
    }

    /// Shows problems at `range` right away, then the server's documentation.
    fn show_hover(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.local_position(range.start, window, cx) else { return };
        let diagnostics: Vec<(Severity, String)> = self
            .doc
            .diagnostics
            .iter()
            .filter(|d| d.range.start <= range.end && d.range.end >= range.start)
            .map(|d| (d.severity, d.message.clone()))
            .collect();
        if !diagnostics.is_empty() {
            self.lsp.hover = Some(HoverPopover { range: range.clone(), diagnostics: diagnostics.clone(), blocks: Vec::new(), anchor });
            cx.notify();
        }
        let server = self.lsp_server(cx).filter(|s| s.capability("hoverProvider"));
        let params = server.as_ref().and_then(|s| self.position_params(range.start, s));
        let (Some(server), Some(params)) = (server, params) else {
            self.lsp.hover_pending = None;
            return;
        };
        self.lsp.hover_task = Some(cx.spawn(async move |this, cx| {
            let result = server.request("textDocument/hover", params).await;
            this.update(cx, |e, cx| {
                e.lsp.hover_pending = None;
                let blocks = result.ok().map(|v| protocol::hover_blocks(&v)).unwrap_or_default();
                if blocks.is_empty() && diagnostics.is_empty() {
                    return;
                }
                e.lsp.hover = Some(HoverPopover { range, diagnostics, blocks, anchor });
                cx.notify();
            })
            .ok();
        }));
    }

    /// Editor-local pixel position of the top-left of `offset`'s character.
    fn local_position(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) -> Option<Point<Pixels>> {
        self.sync_wrap(window, cx);
        let layout = self.layout.clone()?;
        let x = self.x_for_offset(offset, window, cx);
        let drow = self.display_row_of(offset);
        Some(gpui::point(
            layout.text_left - layout.bounds.left() + x - self.scroll.x,
            layout.line_height * drow as f32 - self.scroll.y,
        ))
    }

    // ---- Navigation, rename and code actions ------------------------------------------------

    pub(super) fn go_to_declaration(&mut self, _: &super::GoToDeclaration, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.navigate_from(head, cx);
    }

    /// Go to Declaration from an offset (⌘/Ctrl-click passes the click's).
    pub(super) fn navigate_from(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.locations_request("textDocument/definition", offset, "Declaration", Value::Null, cx);
    }

    pub(super) fn find_usages(&mut self, _: &super::FindUsages, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let name = self.doc.slice(super::word_range(&self.doc, head));
        self.locations_request("textDocument/references", head, &format!("Usages of {name}"), json!({ "includeDeclaration": true }), cx);
    }

    fn locations_request(&mut self, method: &'static str, offset: usize, title: &str, context: Value, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server(cx) else {
            cx.emit(EditorLspEvent::Message("No language server for this file".into()));
            return;
        };
        let Some(mut params) = self.position_params(offset, &server) else { return };
        if !context.is_null() {
            params["context"] = context;
        }
        let title: SharedString = title.to_string().into();
        cx.spawn(async move |this, cx| {
            let result = server.request(method, params).await;
            this.update(cx, |_, cx| match result {
                Ok(value) => {
                    let locations = protocol::locations(&value);
                    if locations.is_empty() {
                        let what = if method.ends_with("references") { "No usages found" } else { "No declaration found" };
                        cx.emit(EditorLspEvent::Message(what.into()));
                    } else {
                        cx.emit(EditorLspEvent::Navigate { title, locations, encoding: server.encoding });
                    }
                }
                Err(err) => cx.emit(EditorLspEvent::Message(err.message.into())),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn rename_symbol(&mut self, _: &super::RenameSymbol, _: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server(cx).filter(|s| s.capability("renameProvider")) else {
            cx.emit(EditorLspEvent::Message("Renaming needs a language server".into()));
            return;
        };
        let head = self.primary_selection().head;
        let word = super::word_range(&self.doc, head);
        let (Some(path), false) = (self.doc.path.clone(), word.is_empty()) else { return };
        let position = offset_to_position(self.doc.rope(), word.start, server.encoding);
        let current = self.doc.slice(word);
        cx.emit(EditorLspEvent::Rename { server, path, position, current });
    }

    pub(super) fn show_context_actions(&mut self, _: &super::ShowContextActions, window: &mut Window, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server(cx).filter(|s| s.capability("codeActionProvider")) else {
            cx.emit(EditorLspEvent::Message("No actions available here".into()));
            return;
        };
        let range = self.primary_selection().range();
        let rope = self.doc.rope().clone();
        // Fixes are offered for the problems under the caret.
        let diagnostics: Vec<Value> = self
            .lsp
            .raw_diagnostics
            .iter()
            .filter(|d| {
                protocol::LspRange::from_json(d.get("range").unwrap_or(&Value::Null))
                    .map(|r| protocol::range_to_offsets(&rope, r, server.encoding))
                    .is_some_and(|r| r.start <= range.end && r.end >= range.start)
            })
            .cloned()
            .collect();
        let Some(document) = self.text_document() else { return };
        let params = json!({
            "textDocument": document,
            "range": offsets_to_range(&rope, range.clone(), server.encoding).to_json(),
            "context": { "diagnostics": diagnostics, "triggerKind": 1 },
        });
        let Some(local) = self.local_position(range.start, window, cx) else { return };
        let Some(layout) = self.layout.clone() else { return };
        let position = layout.bounds.origin + local + gpui::point(px(0.), layout.line_height);
        cx.spawn(async move |this, cx| {
            let result = server.request("textDocument/codeAction", params).await;
            this.update(cx, |_, cx| {
                let actions = result.map(|v| protocol::code_actions(&v)).unwrap_or_default();
                if actions.is_empty() {
                    cx.emit(EditorLspEvent::Message("No actions available here".into()));
                } else {
                    cx.emit(EditorLspEvent::CodeActions { server, actions, position });
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- Popovers ----------------------------------------------------------------------

    pub(super) fn render_lsp_popovers(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let Some(layout) = self.layout.clone() else { return out };
        let theme = Theme::get(cx).clone();
        let mono = font(theme.mono_font.clone());
        let panel = || {
            div()
                .absolute()
                .bg(theme.elevated)
                .border_1()
                .border_color(theme.border)
                .rounded(px(6.))
                .shadow_lg()
                .text_size(px(theme.ui_font_size - 0.5))
                .text_color(theme.text)
        };

        if let Some((label, active)) = self.lsp.signature.as_ref().map(|s| (s.label.clone(), s.active.clone())) {
            let head = self.primary_selection().head;
            if let Some(at) = self.local_position(head, window, cx) {
                let highlights = active.map(|r| {
                    (r, HighlightStyle { color: Some(theme.accent.into()), font_weight: Some(FontWeight::BOLD), ..Default::default() })
                });
                let top = (at.y - px(30.)).max(px(0.));
                out.push(
                    panel()
                        .left(at.x.max(px(0.)))
                        .top(top)
                        .px_2()
                        .py_1()
                        .font(mono.clone())
                        .child(StyledText::new(label).with_highlights(highlights))
                        .into_any_element(),
                );
            }
        }

        if let Some((start, rows)) = self.lsp.completion.as_ref().map(|m| (m.start, m.matches.len().min(MENU_ROWS))) {
            let at = self.local_position(start, window, cx);
            if let (Some(at), Some(menu)) = (at, self.lsp.completion.as_ref()) {
                let height = px(rows as f32 * MENU_ROW_HEIGHT + 8.);
                let below = at.y + layout.line_height;
                let top = if below + height > layout.bounds.size.height && at.y > height { at.y - height } else { below };
                let items = menu.items.clone();
                let rows_el: Vec<AnyElement> = menu
                    .matches
                    .iter()
                    .enumerate()
                    .skip(menu.scroll)
                    .take(MENU_ROWS)
                    .map(|(ix, &item_ix)| {
                        let item = &items[item_ix];
                        let selected = ix == menu.selected;
                        div()
                            .id(("completion", ix))
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(MENU_ROW_HEIGHT))
                            .px_2()
                            .rounded(px(4.))
                            .cursor_pointer()
                            .when(selected, |el| el.bg(theme.accent_soft))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |e, _, _, cx| {
                                cx.stop_propagation();
                                e.accept_completion(ix, false, cx);
                            }))
                            .child(div().flex_shrink_0().font(mono.clone()).child(item.label.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.text_dim)
                                    .child(item.detail.clone().unwrap_or_default()),
                            )
                            .child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.text_dim).child(item.kind.unwrap_or("")))
                            .into_any_element()
                    })
                    .collect();
                out.push(
                    panel()
                        .left(at.x - px(8.))
                        .top(top)
                        .w(px(440.))
                        .p_1()
                        .on_mouse_move(|_, _, cx| cx.stop_propagation())
                        .children(rows_el)
                        .into_any_element(),
                );
            }
        }

        if let Some(hover) = self.lsp.hover.as_ref() {
            let mut body = div().flex().flex_col().gap_2();
            for (severity, message) in &hover.diagnostics {
                let color = super::severity_color(&theme, *severity);
                body = body.child(div().text_color(color).child(message.clone()));
            }
            for block in &hover.blocks {
                body = body.child(if block.code {
                    div()
                        .font(mono.clone())
                        .text_size(px(theme.editor_font_size - 1.))
                        .px_2()
                        .py_1()
                        .rounded(px(4.))
                        .bg(theme.background)
                        .child(block.text.clone())
                } else {
                    div().text_color(theme.text_muted).child(block.text.clone())
                });
            }
            // Above the word, or below it near the top of the editor.
            let below = hover.anchor.y < px(160.);
            let el = panel()
                .id("hover")
                .left(hover.anchor.x.max(px(0.)))
                .max_w(px(560.))
                .max_h(px(320.))
                .overflow_y_scroll()
                .p_2()
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .child(body);
            let el = if below {
                el.top(hover.anchor.y + layout.line_height + px(2.))
            } else {
                el.bottom(layout.bounds.size.height - hover.anchor.y + px(2.))
            };
            out.push(el.into_any_element());
        }
        out
    }
}

pub(super) enum CompletionKey {
    Move(isize),
    Accept { replace: bool },
}
