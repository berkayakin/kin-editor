//! The editor's side of the language server: keeping the server's copy of
//! the document current, completion, hover documentation, signature help,
//! and the requests behind Go to Declaration, Find Usages, Rename and
//! Show Context Actions (whose results the workspace presents).

use std::{path::PathBuf, sync::Arc};

use gpui::{App, 
    AnyElement, Context, FontWeight, HighlightStyle, MouseButton, Pixels, Point, SharedString, StyledText, Task,
    WeakEntity, Window, div, font, prelude::*, px,
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use serde_json::{Value, json};

use super::{Editor, EditorEvent, EditorMode};
use crate::{
    assets::Icon,
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
    ui::{HoverFill, icon_el, tooltip_view},
};

/// Requests the workspace carries out, since they reach beyond this editor.
pub enum EditorLspEvent {
    /// Jump to one location, or let the user pick among several.
    Navigate { title: SharedString, locations: Vec<Location>, encoding: Encoding },
    /// Ask for a new name, then rename through the server.
    Rename { server: Arc<Server>, path: PathBuf, position: Position, current: String },
    /// Offer code actions (each with the server that offered it) in a menu
    /// at this window position.
    CodeActions { actions: Vec<(Arc<Server>, CodeAction)>, position: Point<Pixels> },
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
    /// The servers that answered, and for each item the one it came from.
    servers: Vec<Arc<Server>>,
    sources: Vec<usize>,
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
    /// Closes a shown hover shortly after the pointer leaves its word, unless
    /// the pointer reaches the popover first.
    hover_hide: Option<Task<()>>,
    pub(super) signature: Option<SignatureHelp>,
    signature_task: Option<Task<()>>,
    /// Each server's diagnostics as received (by `Server::id`), for code
    /// action requests: a server gets its own back.
    raw_diagnostics: Vec<(String, Vec<Value>)>,
    /// The server reports diagnostics, so the built-in syntax check is redundant.
    pub(super) active: bool,
    /// Other uses of the symbol at the caret (`textDocument/documentHighlight`),
    /// for the document version they were computed on.
    pub(super) symbol_highlights: (u64, Vec<std::ops::Range<usize>>),
    symbol_highlight_task: Option<Task<()>>,
    inlay_task: Option<Task<()>>,
}

/// A server's name as a diagnostic source (sources are `&'static str`):
/// each name is leaked once, and servers are few.
fn diagnostic_source(server: &str) -> &'static str {
    static NAMES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if server.is_empty() {
        return "lsp";
    }
    let mut names = NAMES.lock().unwrap();
    if let Some(name) = names.iter().find(|n| **n == server) {
        return name;
    }
    let name: &'static str = Box::leak(server.to_string().into_boxed_str());
    names.push(name);
    name
}

pub(super) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

impl Editor {
    pub fn set_lsp(&mut self, store: WeakEntity<LspStore>) {
        self.lsp.store = Some(store);
    }

    /// The document's servers that are up, the main one first.
    pub(super) fn lsp_servers(&self, cx: &gpui::App) -> Vec<Arc<Server>> {
        let (Some(path), Some(store)) = (self.doc.read(cx).path.as_deref(), self.lsp.store.as_ref().and_then(|s| s.upgrade())) else {
            return Vec::new();
        };
        store.read(cx).servers_for(path)
    }

    /// The main server.
    pub(super) fn lsp_server(&self, cx: &gpui::App) -> Option<Arc<Server>> {
        self.lsp_servers(cx).into_iter().next()
    }

    /// The first server that has `capability` (`hoverProvider`…).
    pub(super) fn lsp_server_with(&self, capability: &str, cx: &gpui::App) -> Option<Arc<Server>> {
        self.lsp_servers(cx).into_iter().find(|s| s.capability(capability))
    }

    /// Records an edit: tells the server and the workspace, and drops
    /// popovers that no longer apply.
    pub(super) fn edited(&mut self, cx: &mut Context<Self>) {
        self.schedule_parse(cx);
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.clone()) {
            let changes = self.doc.as_mut(cx).take_lsp_changes();
            store.read(cx).did_change(&path, self.doc.read(cx).version(), changes, self.doc.read(cx).rope());
        }
        self.lsp.hover = None;
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
        cx.emit(EditorEvent::Edited);
    }

    pub(super) fn lsp_did_save(&self, cx: &gpui::App) {
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.as_deref()) {
            store.read(cx).did_save(path, self.doc.read(cx).rope());
        }
    }

    /// The document's diagnostics from its servers, all of them together.
    pub fn set_lsp_diagnostics(&mut self, sources: Vec<crate::lsp::LspDiagnostics>, cx: &mut Context<Self>) {
        let mut diagnostics = Vec::new();
        self.lsp.raw_diagnostics.clear();
        for source in sources {
            let name = diagnostic_source(&source.server);
            diagnostics.extend(protocol::diagnostics(&source.params, self.doc.read(cx).rope(), source.encoding, name));
            let raw = source.params.get("diagnostics").and_then(Value::as_array).cloned().unwrap_or_default();
            self.lsp.raw_diagnostics.push((source.id, raw));
        }
        diagnostics.sort_by_key(|d| d.range.start);
        self.doc.as_mut(cx).set_lsp_diagnostics(diagnostics);
        // The server has (re)analysed the file: its hints may have changed too.
        self.schedule_inlay_hints(cx);
        if !self.lsp.active {
            self.lsp.active = true;
            // Re-run the built-in linter without its now-duplicate syntax check.
            self.linted_version = None;
            self.schedule_lint(std::time::Duration::ZERO, cx);
        }
        cx.notify();
    }

    fn text_document(&self, cx: &App) -> Option<Value> {
        Some(json!({ "uri": path_to_uri(self.doc.read(cx).path.as_deref()?) }))
    }

    fn position_params(&self, offset: usize, server: &Server, cx: &App) -> Option<Value> {
        Some(json!({
            "textDocument": self.text_document(cx)?,
            "position": offset_to_position(self.doc.read(cx).rope(), offset, server.encoding).to_json(),
        }))
    }

    // ---- Completion ------------------------------------------------------------------

    /// After typing: open, narrow or close completion, and update signature help.
    pub(super) fn after_typing(&mut self, typed: &str, cx: &mut Context<Self>) {
        if self.selections.len() != 1 || self.mode != EditorMode::Full {
            return;
        }
        let servers = self.lsp_servers(cx);
        let Some(last) = typed.chars().last() else { return };
        let head = self.primary_selection().head;
        // Any completing server's trigger (`.` for TypeScript, `"` for Tailwind).
        let trigger = servers
            .iter()
            .filter(|s| s.capability("completionProvider"))
            .flat_map(|s| s.completion_triggers())
            .find(|t| head >= t.len() && self.doc.read(cx).slice(head - t.len()..head) == *t);
        if servers.iter().any(|s| s.capability("completionProvider")) {
            match (&self.lsp.completion, trigger) {
                (_, Some(trigger)) => self.request_completion(Some(trigger), cx),
                (Some(menu), None) if is_word_char(last) && menu.incomplete => self.request_completion(None, cx),
                (Some(_), None) => {}
                (None, None) if is_word_char(last) => self.request_completion(None, cx),
                (None, None) => {}
            }
        }
        if let Some(server) = servers.iter().find(|s| s.capability("signatureHelpProvider"))
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

    /// Asks every completing server (for a trigger character, the ones it
    /// triggers) and shows their items together.
    fn request_completion(&mut self, trigger: Option<String>, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let start = self.word_start(head, cx);
        let requests: Vec<(Arc<Server>, Value)> = self
            .lsp_servers(cx)
            .into_iter()
            .filter(|s| s.capability("completionProvider"))
            .filter(|s| trigger.as_ref().is_none_or(|t| s.completion_triggers().contains(t)))
            .filter_map(|server| {
                let mut params = self.position_params(head, &server, cx)?;
                params["context"] = match &trigger {
                    Some(t) => json!({ "triggerKind": 2, "triggerCharacter": t }),
                    None => json!({ "triggerKind": 1 }),
                };
                Some((server, params))
            })
            .collect();
        if requests.is_empty() {
            return;
        }
        self.lsp.completion_task = Some(cx.spawn(async move |this, cx| {
            let servers: Vec<Arc<Server>> = requests.iter().map(|(s, _)| s.clone()).collect();
            let answers = futures::future::join_all(requests.into_iter().map(|(server, params)| async move {
                server.request("textDocument/completion", params).await
            }))
            .await;
            let (mut items, mut sources, mut incomplete) = (Vec::new(), Vec::new(), false);
            for (ix, answer) in answers.into_iter().enumerate() {
                let Ok(value) = answer else { continue };
                incomplete |= value.get("isIncomplete").and_then(Value::as_bool).unwrap_or(false);
                let found = protocol::completion_items(value);
                sources.extend(std::iter::repeat_n(ix, found.len()));
                items.extend(found);
            }
            this.update(cx, |e, cx| {
                let snippets = super::snippet::user_snippet_items(e.doc.read(cx).language);
                sources.extend(std::iter::repeat_n(0, snippets.len()));
                items.extend(snippets);
                e.lsp.completion = Some(CompletionMenu {
                    items: Arc::new(items),
                    matches: Vec::new(),
                    selected: 0,
                    scroll: 0,
                    start,
                    servers,
                    sources,
                    incomplete,
                });
                e.refilter_completion(cx);
            })
            .ok();
        }));
    }

    /// Start of the identifier the caret is in or after.
    fn word_start(&self, offset: usize, cx: &App) -> usize {
        let mut start = offset;
        while let Some(c) = self.doc.read(cx).char_before(start) {
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
            && self.doc.read(cx).row_of(head) == self.doc.read(cx).row_of(menu.start)
            && self.doc.read(cx).slice(menu.start..head).chars().all(is_word_char);
        if !valid {
            self.lsp.completion = None;
            cx.notify();
            return;
        }
        let prefix = self.doc.read(cx).slice(menu.start..head);
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
        let Some(&i) = menu.matches.get(ix) else { return };
        let mut item = menu.items[i].clone();
        // Resolved by, and positioned in the encoding of, the server it came from.
        let Some(server) = menu.servers.get(menu.sources[i]).cloned() else { return };
        let start = menu.start;
        cx.notify();
        // Auto-imports often arrive only on resolve.
        let own = item.raw.get("kinUserSnippet").is_some();
        if item.additional_edits.is_empty() && !own && server.resolves("completionProvider") {
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
        let rope = self.doc.read(cx).rope().clone();
        let from = item.edit.as_ref().map(|e| position_to_offset(&rope, e.range.start, encoding)).unwrap_or(start).min(head);
        let mut to = head;
        if replace {
            while let Some(c) = self.doc.read(cx).char_at(to).filter(|c| is_word_char(*c)) {
                to += c.len_utf8();
            }
        }
        let (text, stops) = match item.is_snippet {
            true => protocol::expand_snippet(&self.indented_snippet(item.raw_text(), from, cx)),
            false => (item.raw_text().to_string(), Vec::new()),
        };
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
        let has_stops = stops.len() > 1;
        self.select_snippet(new_start, text.len(), stops, cx);
        self.autoscroll = true;
        // Accepting a function often opens its argument list.
        if text.ends_with('(') || has_stops {
            self.request_signature_help(cx);
        }
    }

    // ---- Signature help ----------------------------------------------------------------

    fn request_signature_help(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.lsp_server_with("signatureHelpProvider", cx) else { return };
        let head = self.primary_selection().head;
        let row = self.doc.read(cx).row_of(head);
        let Some(params) = self.position_params(head, &server, cx) else { return };
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
        if self.lsp.signature.as_ref().is_some_and(|s| s.row != self.doc.read(cx).row_of(head)) {
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
            self.cancel_pending_hover();
            self.hide_hover_soon(window, cx);
            return;
        }
        let offset = self.offset_for_position(position, window, cx);
        let range = super::word_range(self.doc.read(cx), offset);
        let range = if range.is_empty() { offset..self.doc.read(cx).next_char_boundary(offset) } else { range };
        if self.lsp.hover.as_ref().is_some_and(|h| h.range == range) {
            // Back on the word: keep its popover.
            self.lsp.hover_hide = None;
            return;
        }
        if self.lsp.hover_pending.as_ref() == Some(&range) {
            return;
        }
        // On the way to the popover the pointer crosses other words, so the
        // shown one stays a moment longer.
        self.hide_hover_soon(window, cx);
        self.lsp.hover_pending = Some(range.clone());
        self.lsp.hover_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(450)).await;
            this.update_in(cx, |e, window, cx| e.show_hover(range, window, cx)).ok();
        }));
    }

    /// The pointer left the editor (onto tabs, the title bar, a menu): a
    /// hover that was about to open would describe code it's no longer over,
    /// and a shown one goes away.
    pub(super) fn pointer_left(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_pending_hover();
        self.hide_hover_soon(window, cx);
    }

    fn cancel_pending_hover(&mut self) {
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
    }

    fn hide_hover_soon(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lsp.hover.is_none() || self.lsp.hover_hide.is_some() {
            return;
        }
        self.lsp.hover_hide = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(350)).await;
            this.update(cx, |e, cx| {
                e.lsp.hover_hide = None;
                e.lsp.hover = None;
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn quick_documentation(&mut self, _: &super::QuickDocumentation, window: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let range = super::word_range(self.doc.read(cx), head);
        self.show_hover(range, window, cx);
    }

    /// Shows problems at `range` right away, then the server's documentation.
    fn show_hover(&mut self, range: std::ops::Range<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(anchor) = self.local_position(range.start, window, cx) else { return };
        let diagnostics: Vec<(Severity, String)> = self
            .doc
            .read(cx)
            .diagnostics
            .iter()
            .filter(|d| d.range.start <= range.end && d.range.end >= range.start)
            .map(|d| (d.severity, d.message.to_string()))
            .collect();
        if !diagnostics.is_empty() {
            self.lsp.hover = Some(HoverPopover { range: range.clone(), diagnostics: diagnostics.clone(), blocks: Vec::new(), anchor });
            cx.notify();
        }
        let server = self.lsp_server_with("hoverProvider", cx);
        let params = server.as_ref().and_then(|s| self.position_params(range.start, s, cx));
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
                e.lsp.hover_hide = None;
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
        let drow = self.display_row_of(offset, cx);
        Some(gpui::point(
            layout.text_left - layout.bounds.left() + x - self.scroll.x,
            layout.line_height * drow as f32 - self.scroll.y,
        ))
    }

    // ---- Inlay hints ---------------------------------------------------------------------

    /// Asks for the file's inlay hints once typing pauses (they follow edits
    /// in the meantime).
    pub(super) fn schedule_inlay_hints(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full || self.doc.read(cx).large || !self.file_settings.inlay_hints {
            self.lsp.inlay_task = None;
            if !self.doc.read(cx).inlay_hints().is_empty() {
                self.doc.as_mut(cx).set_inlay_hints(Vec::new());
                cx.notify();
            }
            return;
        }
        self.lsp.inlay_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(350)).await;
            let Ok(Some((server, params, version))) = this.update(cx, |e, cx| {
                let server = e.lsp_server_with("inlayHintProvider", cx)?;
                let rope = e.doc.read(cx).rope();
                let end = offset_to_position(rope, rope.len_bytes(), server.encoding);
                let params = json!({
                    "textDocument": e.text_document(cx)?,
                    "range": { "start": { "line": 0, "character": 0 }, "end": end.to_json() },
                });
                Some((server, params, e.doc.read(cx).version()))
            }) else {
                return;
            };
            let result = server.request("textDocument/inlayHint", params).await;
            this.update(cx, |e, cx| {
                // Edited meanwhile: the next request answers for the new text.
                if e.doc.read(cx).version() != version {
                    return;
                }
                let rope = e.doc.read(cx).rope();
                let hints = result
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|h| {
                        let position = protocol::Position::from_json(h.get("position")?)?;
                        // A string, or parts with a `value` each.
                        let label = match h.get("label")? {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => parts.iter().filter_map(|p| p.get("value")?.as_str()).collect(),
                            _ => return None,
                        };
                        let flag = |key: &str| h.get(key).and_then(Value::as_bool).unwrap_or(false);
                        Some(crate::document::InlayHint {
                            offset: protocol::position_to_offset(rope, position, server.encoding),
                            label: label.trim().to_string(),
                            pad_left: flag("paddingLeft"),
                            pad_right: flag("paddingRight"),
                        })
                    })
                    .filter(|h| !h.label.is_empty())
                    .collect();
                e.doc.as_mut(cx).set_inlay_hints(hints);
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- Symbol highlights ---------------------------------------------------------------

    /// After the caret settles, asks for the other uses of the symbol under it.
    pub(super) fn schedule_symbol_highlights(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full {
            return;
        }
        let head = self.primary_selection().head;
        // Leaving the highlighted symbol drops them now rather than after the request.
        let (version, ranges) = &self.lsp.symbol_highlights;
        if *version != self.doc.read(cx).version() || !ranges.iter().any(|r| r.start <= head && head <= r.end) {
            self.lsp.symbol_highlights.1.clear();
        }
        self.lsp.symbol_highlight_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
            let Ok(Some((server, params, version))) = this.update(cx, |e, cx| {
                let server = e.lsp_server_with("documentHighlightProvider", cx)?;
                let params = e.position_params(e.primary_selection().head, &server, cx)?;
                Some((server, params, e.doc.read(cx).version()))
            }) else {
                return;
            };
            let result = server.request("textDocument/documentHighlight", params).await;
            this.update(cx, |e, cx| {
                if e.doc.read(cx).version() != version {
                    return;
                }
                let rope = e.doc.read(cx).rope();
                let mut ranges: Vec<std::ops::Range<usize>> = result
                    .ok()
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|h| protocol::LspRange::from_json(h.get("range")?))
                    .map(|r| protocol::range_to_offsets(rope, r, server.encoding))
                    .collect();
                ranges.sort_by_key(|r| r.start);
                // A lone match is just the word itself: nothing worth pointing out.
                if ranges.len() < 2 {
                    ranges.clear();
                }
                e.lsp.symbol_highlights = (version, ranges);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The highlights that still match the text.
    pub(super) fn current_symbol_highlights(&self, cx: &App) -> &[std::ops::Range<usize>] {
        let (version, ranges) = &self.lsp.symbol_highlights;
        if *version == self.doc.read(cx).version() { ranges } else { &[] }
    }

    // ---- Navigation, rename and code actions ------------------------------------------------

    pub(super) fn go_to_declaration(&mut self, _: &super::GoToDeclaration, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.navigate_from(head, cx);
    }

    /// Go to Declaration from an offset (⌘/Ctrl-click passes the click's).
    pub(super) fn navigate_from(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.locations_request("textDocument/definition", offset, "Declaration", "No declaration found", Value::Null, cx);
    }

    pub(super) fn go_to_implementation(&mut self, _: &super::GoToImplementation, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let name = self.doc.read(cx).slice(super::word_range(self.doc.read(cx), head));
        let title = format!("Implementations of {name}");
        self.locations_request("textDocument/implementation", head, &title, "No implementations found", Value::Null, cx);
    }

    pub(super) fn go_to_type_definition(&mut self, _: &super::GoToTypeDefinition, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.locations_request("textDocument/typeDefinition", head, "Type Definition", "No type definition found", Value::Null, cx);
    }

    pub(super) fn find_usages(&mut self, _: &super::FindUsages, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let name = self.doc.read(cx).slice(super::word_range(self.doc.read(cx), head));
        let context = json!({ "includeDeclaration": true });
        self.locations_request("textDocument/references", head, &format!("Usages of {name}"), "No usages found", context, cx);
    }

    fn locations_request(
        &mut self,
        method: &'static str,
        offset: usize,
        title: &str,
        none: &'static str,
        context: Value,
        cx: &mut Context<Self>,
    ) {
        // `textDocument/definition` → `definitionProvider`.
        let capability = format!("{}Provider", method.trim_start_matches("textDocument/"));
        let Some(server) = self.lsp_server_with(&capability, cx).or_else(|| self.lsp_server(cx)) else {
            cx.emit(EditorLspEvent::Message("No language server for this file".into()));
            return;
        };
        let Some(mut params) = self.position_params(offset, &server, cx) else { return };
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
                        cx.emit(EditorLspEvent::Message(none.into()));
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
        let Some(server) = self.lsp_server_with("renameProvider", cx) else {
            cx.emit(EditorLspEvent::Message("Renaming needs a language server".into()));
            return;
        };
        let head = self.primary_selection().head;
        let word = super::word_range(self.doc.read(cx), head);
        let (Some(path), false) = (self.doc.read(cx).path.clone(), word.is_empty()) else { return };
        let position = offset_to_position(self.doc.read(cx).rope(), word.start, server.encoding);
        let current = self.doc.read(cx).slice(word);
        cx.emit(EditorLspEvent::Rename { server, path, position, current });
    }

    pub(super) fn show_context_actions(&mut self, _: &super::ShowContextActions, window: &mut Window, cx: &mut Context<Self>) {
        let servers: Vec<Arc<Server>> = self.lsp_servers(cx).into_iter().filter(|s| s.capability("codeActionProvider")).collect();
        if servers.is_empty() {
            cx.emit(EditorLspEvent::Message("No actions available here".into()));
            return;
        }
        let range = self.primary_selection().range();
        let rope = self.doc.read(cx).rope().clone();
        let Some(document) = self.text_document(cx) else { return };
        // Each server is asked for fixes for its own problems under the caret.
        let requests: Vec<(Arc<Server>, Value)> = servers
            .into_iter()
            .map(|server| {
                let diagnostics: Vec<Value> = self
                    .lsp
                    .raw_diagnostics
                    .iter()
                    .filter(|(id, _)| *id == server.id)
                    .flat_map(|(_, raw)| raw)
                    .filter(|d| {
                        protocol::LspRange::from_json(d.get("range").unwrap_or(&Value::Null))
                            .map(|r| protocol::range_to_offsets(&rope, r, server.encoding))
                            .is_some_and(|r| r.start <= range.end && r.end >= range.start)
                    })
                    .cloned()
                    .collect();
                let params = json!({
                    "textDocument": document,
                    "range": offsets_to_range(&rope, range.clone(), server.encoding).to_json(),
                    "context": { "diagnostics": diagnostics, "triggerKind": 1 },
                });
                (server, params)
            })
            .collect();
        let Some(local) = self.local_position(range.start, window, cx) else { return };
        let Some(layout) = self.layout.clone() else { return };
        let position = layout.bounds.origin + local + gpui::point(px(0.), layout.line_height);
        cx.spawn(async move |this, cx| {
            let answers = futures::future::join_all(requests.into_iter().map(|(server, params)| async move {
                let result = server.request("textDocument/codeAction", params).await;
                (server, result)
            }))
            .await;
            let actions: Vec<(Arc<Server>, CodeAction)> = answers
                .into_iter()
                .flat_map(|(server, result)| {
                    let found = result.map(|v| protocol::code_actions(&v)).unwrap_or_default();
                    found.into_iter().map(move |action| (server.clone(), action))
                })
                .collect();
            this.update(cx, |_, cx| {
                if actions.is_empty() {
                    cx.emit(EditorLspEvent::Message("No actions available here".into()));
                } else {
                    cx.emit(EditorLspEvent::CodeActions { actions, position });
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
                .border_color(gpui::Hsla::from(theme.border).alpha(0.85))
                .rounded(px(10.))
                .shadow(crate::ui::soft_shadow(&theme, 2))
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
                            .relative()
                            .flex()
                            .items_center()
                            .gap_2()
                            .h(px(MENU_ROW_HEIGHT))
                            .px_2()
                            .rounded(px(6.))
                            .cursor_pointer()
                            .when(selected, |el| el.bg(theme.accent_soft).child(crate::ui::selected_edge(&theme).top(px(4.)).bottom(px(4.))))
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
            // Above the word when it fits, else on the side with more room,
            // never taller than that side so it's always fully visible.
            const MAX_WIDTH: f32 = 560.;
            const MAX_HEIGHT: f32 = 320.;
            let gap = px(2.);
            let space_above = hover.anchor.y - gap - px(6.);
            let space_below = layout.bounds.size.height - (hover.anchor.y + layout.line_height) - gap - px(6.);
            let below = space_above < px(MAX_HEIGHT) && space_below > space_above;
            let max_h = if below { space_below } else { space_above }.min(px(MAX_HEIGHT)).max(px(48.));
            let right_edge = layout.bounds.size.width - px(super::SCROLLBAR_WIDTH + 4.);
            let left = hover.anchor.x.min(right_edge - px(MAX_WIDTH)).max(px(0.));
            // Its text, for the Copy button (the popover's text isn't selectable).
            let text: String = hover
                .diagnostics
                .iter()
                .map(|(_, m)| m.clone())
                .chain(hover.blocks.iter().map(|b| b.text.clone()))
                .collect::<Vec<_>>()
                .join("\n\n");
            let copy = div()
                .id("hover-copy")
                .absolute()
                .top(px(4.))
                .right(px(4.))
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .invisible()
                .group_hover("hover-popover", |s| s.visible())
                .hover_fill(theme.elevated, theme.hover)
                .tooltip(|_, cx| tooltip_view("Copy", cx))
                .on_click(move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone())))
                .child(icon_el(Icon::Copy, px(13.), theme.text_muted.into()));
            let el = panel()
                .group("hover-popover")
                .left(left)
                .max_w(px(MAX_WIDTH))
                // Clicks stay in the popover (they'd move the caret and close it).
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                // The wheel scrolls the popover, not the editor under it.
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                // Reaching the popover keeps it open (and stops another word's
                // popover from replacing it).
                .on_mouse_move(cx.listener(|e, _, _, cx| {
                    cx.stop_propagation();
                    e.lsp.hover_hide = None;
                    e.cancel_pending_hover();
                }))
                .child(div().id("hover").max_h(max_h).overflow_y_scroll().p_2().child(body))
                .child(copy);
            let el = if below {
                el.top(hover.anchor.y + layout.line_height + gap)
            } else {
                el.bottom(layout.bounds.size.height - hover.anchor.y + gap)
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use gpui::{AppContext, Entity, TestAppContext};

    use super::*;
    use crate::{
        document::Document,
        lsp::{LspStatus, fake_server},
    };

    /// Runs the app until `done` holds; servers answer from other threads.
    fn wait_for(cx: &mut TestAppContext, what: &str, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let started = Instant::now();
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(started.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[gpui::test]
    fn a_document_gets_every_server(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kin-lsp-servers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        std::fs::write(&path, "x.y\n").unwrap();

        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("main", "."), fake_server("extra", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));

        // Both get the text; their diagnostics are shown together, each
        // server's kept apart for code actions.
        let from_servers = |e: &Editor, cx: &gpui::App| e.doc.read(cx).diagnostics.iter().filter(|d| d.message.ends_with(" problem")).count();
        wait_for(cx, "diagnostics", |cx| {
            let found = editor.read_with(cx, |e, cx| from_servers(e, cx));
            if found < 2 {
                eprintln!("status: {:?}, diagnostics: {found}", store.read_with(cx, |s, _| s.status(&path)));
            }
            found == 2
        });
        editor.read_with(cx, |e, cx| {
            let mut messages: Vec<&str> = e.doc.read(cx).diagnostics.iter().map(|d| &*d.message).filter(|m| m.ends_with(" problem")).collect();
            messages.sort();
            assert_eq!(messages, ["extra problem", "main problem"]);
            let ids: Vec<&str> = e.lsp.raw_diagnostics.iter().map(|(id, raw)| (id.as_str(), raw.len())).map(|(id, _)| id).collect();
            assert_eq!(ids.len(), 2);
            assert!(ids.contains(&"fake-main") && ids.contains(&"fake-extra"));
        });
        store.read_with(cx, |s, _| assert_eq!(s.status(&path), Some(LspStatus::Ready("python3, python3".into()))));

        // Completion asks both and lists their items together.
        editor.update(cx, |e, cx| {
            // Just after the dot: nothing typed yet, so every item shows.
            e.select_range(2..2, cx);
            e.request_completion(None, cx);
        });
        wait_for(cx, "completion", |cx| editor.read_with(cx, |e, _| e.lsp.completion.is_some()));
        let labels = |cx: &mut TestAppContext| {
            editor.read_with(cx, |e, _| {
                let menu = e.lsp.completion.as_ref().unwrap();
                let mut labels: Vec<String> = menu.items.iter().map(|i| i.label.clone()).collect();
                labels.sort();
                labels
            })
        };
        assert_eq!(labels(cx), ["extra-item", "main-item"]);

        // A trigger character asks only the servers it triggers.
        editor.update(cx, |e, cx| {
            e.lsp.completion = None;
            e.request_completion(Some(".".into()), cx);
        });
        wait_for(cx, "triggered completion", |cx| editor.read_with(cx, |e, _| e.lsp.completion.is_some()));
        assert_eq!(labels(cx), ["main-item"]);

        store.update(cx, |s, _| s.shutdown_all());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn a_restarted_server_outlives_its_predecessor(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kin-lsp-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        std::fs::write(&path, "x\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("restarted", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let ready = |cx: &mut TestAppContext| store.read_with(cx, |s, _| matches!(s.status(&path), Some(LspStatus::Ready(_))));
        wait_for(cx, "the first server", ready);
        store.update(cx, |s, cx| s.restart(&path, cx));
        wait_for(cx, "the second server", ready);
        // The first one exits after the restart; that's not the new one failing.
        std::thread::sleep(Duration::from_millis(600));
        cx.run_until_parked();
        store.read_with(cx, |s, _| assert!(matches!(s.status(&path), Some(LspStatus::Ready(_))), "{:?}", s.status(&path)));
        store.update(cx, |s, _| s.shutdown_all());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[gpui::test]
    fn servers_get_their_settings(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kin-lsp-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ts");
        std::fs::write(&path, "x\n").unwrap();
        let settings = crate::settings::Settings::parse(
            r#"{ "lsp": { "fake-configured": {
                "initialization_options": { "greeting": "hello" },
                "settings": { "fake.mode": "strict" }
            } } }"#,
        )
        .unwrap();
        cx.update(|cx| cx.set_global(settings));

        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("configured", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        // The server echoes its initialization options and the setting it asked for.
        wait_for(cx, "diagnostics", |cx| {
            editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().any(|d| &*d.message == "configured problem hello strict"))
        });
        store.update(cx, |s, _| s.shutdown_all());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
