//! The editor's side of the language server: keeping the server's copy of
//! the document current, completion, hover documentation, signature help,
//! and the requests behind Go to Declaration, Find Usages, Rename and
//! Show Context Actions (whose results the workspace presents).

use std::{cmp::Reverse, path::PathBuf, sync::Arc};

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
use crate::document::CodeLens;
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
    /// Run a server's command (a code lens's): `workspace/executeCommand`.
    Command { server: Arc<Server>, command: Value },
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
    /// The word under the pointer while ⌘/Ctrl is held, when the server knows
    /// its declaration (where ⌘/Ctrl-click goes): drawn as a link.
    pub(super) link: Option<std::ops::Range<usize>>,
    /// The word a declaration request is out for, and the request.
    link_pending: Option<std::ops::Range<usize>>,
    link_task: Option<Task<()>>,
    /// Words with a declaration, for a document version: going back over one
    /// doesn't ask again.
    link_found: (u64, Vec<std::ops::Range<usize>>),
    /// The word under the pointer the server had no declaration for. Only
    /// until the pointer leaves it or ⌘/Ctrl comes up: a server still loading
    /// the project answers nothing (rust-analyzer: `[]`, `null`, "content
    /// modified"), and the next hover should ask again.
    link_none: Option<std::ops::Range<usize>>,
    pub(super) signature: Option<SignatureHelp>,
    signature_task: Option<Task<()>>,
    /// The server reports diagnostics, so the built-in syntax check is redundant.
    pub(super) active: bool,
    /// Other uses of the symbol at the caret (`textDocument/documentHighlight`),
    /// for the document version they were computed on.
    pub(super) symbol_highlights: (u64, Vec<std::ops::Range<usize>>),
    symbol_highlight_task: Option<Task<()>>,
    inlay_task: Option<Task<()>>,
    lens_task: Option<Task<()>>,
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

/// How a completion item ranks against what's typed (lower first).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank<'a> {
    /// The filter text starts with what's typed: an exact match first, then
    /// the server's ranking, which knows what fits here.
    Prefix { exact: Reverse<bool>, sort_text: &'a str, score: Reverse<u32> },
    /// Any other fuzzy match: the best match first.
    Fuzzy { score: Reverse<u32>, sort_text: &'a str },
}

impl Rank<'_> {
    /// Items alike in this are equally good matches, among which the
    /// server's preselected item is chosen.
    fn tier(&self) -> (u8, u32) {
        match *self {
            Rank::Prefix { exact, .. } => (u8::from(!exact.0), 0),
            Rank::Fuzzy { score, .. } => (2, score.0),
        }
    }
}

/// The items matching what's typed, best first, and which of them to
/// select. Ties go by label, as in VS Code: servers rank many items alike
/// (rust-analyzer gives most of a type's methods one `sortText`) and list
/// them in no useful order. The selection is the first item the server
/// preselects among the best matches, else the first.
fn rank_completions(items: &[CompletionItem], typed: &str) -> (Vec<usize>, usize) {
    let pattern = Pattern::parse(typed, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut ranked: Vec<(Rank, String, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let score = match typed.is_empty() {
                true => 0,
                false => pattern.score(Utf32Str::new(&item.filter_text, &mut buf), &mut matcher)?,
            };
            let sort_text = item.sort_text.as_str();
            let rank = match starts_with_typed(&item.filter_text, typed) {
                Some(exact) => Rank::Prefix { exact: Reverse(exact), sort_text, score: Reverse(score) },
                None => Rank::Fuzzy { score: Reverse(score), sort_text },
            };
            Some((rank, item.label.to_lowercase(), i))
        })
        .collect();
    // The server's order last.
    ranked.sort_unstable();
    let best = ranked.first().map(|(rank, ..)| rank.tier());
    let selected = ranked
        .iter()
        .take_while(|(rank, ..)| Some(rank.tier()) == best)
        .position(|&(.., i)| items[i].preselect)
        .unwrap_or(0);
    (ranked.into_iter().map(|(.., i)| i).collect(), selected)
}

/// Whether `text` starts with `typed` ignoring case, and if so, whether
/// that's all of it.
fn starts_with_typed(text: &str, typed: &str) -> Option<bool> {
    let mut rest = text.chars().flat_map(char::to_lowercase);
    typed.chars().flat_map(char::to_lowercase).all(|c| rest.next() == Some(c)).then(|| !typed.is_empty() && rest.next().is_none())
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
            if !changes.is_empty() {
                // What servers answer for this version is read in this text.
                self.doc.as_mut(cx).note_lsp_version();
            }
            store.read(cx).did_change(&path, self.doc.read(cx).version(), changes, self.doc.read(cx).rope());
        }
        self.lsp.hover = None;
        self.lsp.hover_task = None;
        self.lsp.hover_pending = None;
        self.link_word(None, cx);
        cx.emit(EditorEvent::Edited);
    }

    pub(super) fn lsp_did_save(&self, cx: &gpui::App) {
        if let (Some(store), Some(path)) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.as_deref()) {
            store.read(cx).did_save(path, self.doc.read(cx).rope());
        }
    }

    /// One server's diagnostics for the document, as it published them.
    /// They're read in the text of the version they were computed for (the
    /// server may be some edits behind) and moved to today's text; one for
    /// a version no longer known is dropped (a newer one follows). The other servers' diagnostics stay as they are.
    pub fn set_server_diagnostics(&mut self, source: crate::lsp::LspDiagnostics, cx: &mut Context<Self>) {
        let name = diagnostic_source(&source.server);
        let doc = self.doc.read(cx);
        let diagnostics = match source.params.get("version").and_then(Value::as_u64) {
            Some(version) => doc
                .text_at_lsp_version(version)
                .map(|text| protocol::diagnostics(&source.params, text, source.encoding, name))
                .and_then(|found| doc.diagnostics_since(version, found)),
            None => Some(protocol::diagnostics(&source.params, doc.rope(), source.encoding, name)),
        };
        let Some(mut diagnostics) = diagnostics else { return };
        diagnostics.sort_by_key(|d| d.range.start);
        self.doc.as_mut(cx).set_server_diagnostics(&source.id, diagnostics);
        // The server has (re)analysed the file: its hints may have changed too.
        self.schedule_inlay_hints(cx);
        self.schedule_code_lenses(cx);
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
        let (matches, selected) = rank_completions(&menu.items, &prefix);
        if matches.is_empty() {
            self.lsp.completion = None;
        } else {
            menu.matches = matches;
            menu.selected = selected;
            menu.scroll = selected.saturating_sub(MENU_ROWS - 1);
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
        let own = item.raw.get("kuanaUserSnippet").is_some();
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
        self.link_word(None, cx);
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

    // ---- ⌘/Ctrl-hover links --------------------------------------------------------------

    /// The pointer moved, or ⌘/Ctrl went down or up (`held`): the word under
    /// the pointer is a link while ⌘/Ctrl-click would go somewhere (like VS
    /// Code, only once the server knows its declaration).
    pub(super) fn update_link(&mut self, position: Point<Pixels>, held: bool, window: &mut Window, cx: &mut Context<Self>) {
        let word = if held && self.drag.is_none() { self.word_under(position, window, cx) } else { None };
        self.link_word(word, cx);
    }

    /// ⌘/Ctrl went down or up. The workspace tells every shown editor, since
    /// only the focused one hears it: the pointer may rest on another.
    pub fn secondary_modifier_changed(&mut self, held: bool, window: &mut Window, cx: &mut Context<Self>) {
        if held || self.lsp.link.is_some() || self.lsp.link_pending.is_some() {
            self.update_link(window.mouse_position(), held, window, cx);
        }
    }

    /// Shows `word` as a link if the server has a declaration for it (a found
    /// one is remembered for the document version); `None` hides the link.
    pub(super) fn link_word(&mut self, word: Option<std::ops::Range<usize>>, cx: &mut Context<Self>) {
        if word.is_some() && [&self.lsp.link, &self.lsp.link_pending, &self.lsp.link_none].contains(&&word) {
            return;
        }
        // Another word: what was asked about the last one no longer matters.
        self.lsp.link_task = None;
        self.lsp.link_pending = None;
        self.lsp.link_none = None;
        if self.lsp.link.take().is_some() {
            cx.notify();
        }
        let Some(word) = word else { return };
        let version = self.doc.read(cx).version();
        if self.lsp.link_found.0 != version {
            self.lsp.link_found = (version, Vec::new());
        }
        if self.lsp.link_found.1.contains(&word) {
            self.lsp.link = Some(word);
            cx.notify();
            return;
        }
        let Some(server) = self.lsp_server_with("definitionProvider", cx) else { return };
        let Some(params) = self.position_params(word.start, &server, cx) else { return };
        self.lsp.link_pending = Some(word.clone());
        self.lsp.link_task = Some(cx.spawn(async move |this, cx| {
            let found = server.request("textDocument/definition", params).await.is_ok_and(|v| !protocol::locations(&v).is_empty());
            this.update(cx, |e, cx| {
                e.lsp.link_pending = None;
                if !found {
                    e.lsp.link_none = Some(word);
                } else if e.lsp.link_found.0 == version {
                    e.lsp.link_found.1.push(word.clone());
                    e.lsp.link = Some(word);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// The word whose text is under `position`: not past the end of its line,
    /// on rows between lines (problems, lenses), the gutter or pinned lines.
    fn word_under(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> Option<std::ops::Range<usize>> {
        let layout = self.layout.clone()?;
        let over_text = self.mode == EditorMode::Full
            && layout.bounds.contains(&position)
            && position.x >= layout.bounds.left() + layout.gutter_width
            && position.x < layout.bounds.right() - px(super::SCROLLBAR_WIDTH);
        if !over_text || self.sticky_at(position).is_some() {
            return None;
        }
        self.sync_wrap(window, cx);
        let word = super::word_range(self.doc.read(cx), self.offset_for_position(position, window, cx));
        let drow = ((position.y - layout.bounds.top() + self.scroll.y) / layout.line_height).floor() as usize;
        if word.is_empty() || self.display_row_of(word.start, cx) != drow || self.display_row_of(word.end, cx) != drow {
            return None;
        }
        let left = layout.text_left - self.scroll.x;
        let start = left + self.x_for_offset(word.start, window, cx);
        let end = left + self.x_for_offset(word.end, window, cx);
        (start <= position.x && position.x <= end).then_some(word)
    }

    // ---- Inlay hints ---------------------------------------------------------------------

    /// Asks for the file's inlay hints once typing pauses (they follow edits
    /// in the meantime).
    pub(super) fn schedule_inlay_hints(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full || self.doc.read(cx).large || !self.file_settings.inlay_hints {
            // Off here: another editor of the file may show the document's
            // hints, so they stay (this one just doesn't draw them).
            self.lsp.inlay_task = None;
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

    // ---- Code lens ------------------------------------------------------------------------

    /// After edits settle, asks every server that has them for code lenses
    /// (resolving the ones sent without their command), shown above lines.
    pub(super) fn schedule_code_lenses(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full || self.doc.read(cx).large || !self.file_settings.code_lens {
            self.lsp.lens_task = None;
            if !self.doc.read(cx).code_lenses().is_empty() {
                self.doc.as_mut(cx).set_code_lenses(Vec::new());
                cx.notify();
            }
            return;
        }
        self.lsp.lens_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(400)).await;
            let Ok(Some((servers, params, version))) = this.update(cx, |e, cx| {
                let servers: Vec<Arc<Server>> = e.lsp_servers(cx).into_iter().filter(|s| s.capability("codeLensProvider")).collect();
                if servers.is_empty() {
                    return None;
                }
                Some((servers, json!({ "textDocument": e.text_document(cx)? }), e.doc.read(cx).version()))
            }) else {
                return;
            };
            let mut found: Vec<(Arc<Server>, Value)> = Vec::new();
            for server in servers {
                let Ok(Value::Array(lenses)) = server.request("textDocument/codeLens", params.clone()).await else { continue };
                let resolves = server.capability_value("codeLensProvider")["resolveProvider"] == Value::Bool(true);
                let resolved = lenses.into_iter().map(|lens| {
                    let server = server.clone();
                    async move {
                        if !resolves || lens.get("command").is_some_and(|c| !c.is_null()) {
                            return lens;
                        }
                        server.request("codeLens/resolve", lens.clone()).await.unwrap_or(lens)
                    }
                });
                for lens in futures::future::join_all(resolved).await {
                    found.push((server.clone(), lens));
                }
            }
            this.update(cx, |e, cx| {
                // Edited meanwhile: the next request answers for the new text.
                if e.doc.read(cx).version() != version {
                    return;
                }
                let doc = e.doc.read(cx);
                let lenses = found
                    .into_iter()
                    .filter_map(|(server, lens)| {
                        let command = lens.get("command").filter(|c| !c.is_null())?.clone();
                        let title = command.get("title")?.as_str()?.trim().to_string();
                        let start = protocol::Position::from_json(lens.pointer("/range/start")?)?;
                        let offset = protocol::position_to_offset(doc.rope(), start, server.encoding);
                        (!title.is_empty()).then(|| CodeLens { line_start: doc.line_start(doc.row_of(offset)), title, command, server: server.id.clone() })
                    })
                    .collect();
                e.doc.as_mut(cx).set_code_lenses(lenses);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Carries out a clicked code lens. References and locations open the
    /// usages list; rust-analyzer's Run and Debug run Kuana's target for the
    /// line; anything else goes to the server if it runs that command.
    pub(super) fn run_code_lens(&mut self, lens: CodeLens, cx: &mut Context<Self>) {
        let name = lens.command["command"].as_str().unwrap_or_default().to_string();
        let args = lens.command.get("arguments").cloned().unwrap_or(Value::Null);
        let server = self.lsp_servers(cx).into_iter().find(|s| s.id == lens.server);
        let row = self.doc.read(cx).row_of(lens.line_start);
        let navigate = |locations: Vec<protocol::Location>, server: Option<Arc<Server>>, cx: &mut Context<Self>| match server {
            Some(server) if !locations.is_empty() => {
                cx.emit(EditorLspEvent::Navigate { title: lens.title.clone().into(), locations, encoding: server.encoding })
            }
            _ => cx.emit(EditorLspEvent::Message("Nothing found".into())),
        };
        match name.as_str() {
            "editor.action.showReferences" | "rust-analyzer.showReferences" => navigate(protocol::locations(&args[2]), server, cx),
            "rust-analyzer.gotoLocation" => navigate(protocol::locations(&args[0]), server, cx),
            "rust-analyzer.runSingle" | "rust-analyzer.debugSingle" => {
                let debug = name.ends_with("debugSingle");
                // What the lens describes, else Kuana's own target on that line.
                let target = crate::run::from_rust_analyzer(&args[0]).filter(|t| !debug || t.debug.is_some()).or_else(|| {
                    let targets = self.run_targets(cx);
                    targets
                        .iter()
                        .filter(|t| t.row == Some(row) || t.rows.contains(&row))
                        .filter(|t| !debug || t.debug.is_some())
                        .min_by_key(|t| t.rows.len())
                        .cloned()
                });
                match target {
                    Some(target) => cx.emit(super::EditorRunEvent { target, debug, menu_at: None }),
                    None => cx.emit(EditorLspEvent::Message(if debug { "Nothing to debug here" } else { "Nothing to run here" }.into())),
                }
            }
            _ => match server.filter(|s| s.executes(&name)) {
                Some(server) => cx.emit(EditorLspEvent::Command { server, command: lens.command.clone() }),
                None => cx.emit(EditorLspEvent::Message(format!("Kuana can't run “{}”", lens.title).into())),
            },
        }
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

    /// Go to Declaration from an offset (⌘/Ctrl-click passes the click's). On
    /// the declaration itself it lists the usages instead, like JetBrains' Go to
    /// Declaration or Usages (and VS Code's ⌘-click on a definition).
    pub(super) fn navigate_from(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.locations_request("textDocument/definition", offset, "Declaration", "No declaration found", Value::Null, true, cx);
    }

    pub(super) fn go_to_implementation(&mut self, _: &super::GoToImplementation, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        let name = self.doc.read(cx).slice(super::word_range(self.doc.read(cx), head));
        let title = format!("Implementations of {name}");
        self.locations_request("textDocument/implementation", head, &title, "No implementations found", Value::Null, false, cx);
    }

    pub(super) fn go_to_type_definition(&mut self, _: &super::GoToTypeDefinition, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.locations_request("textDocument/typeDefinition", head, "Type Definition", "No type definition found", Value::Null, false, cx);
    }

    pub(super) fn find_usages(&mut self, _: &super::FindUsages, _: &mut Window, cx: &mut Context<Self>) {
        let head = self.primary_selection().head;
        self.find_usages_at(head, cx);
    }

    fn find_usages_at(&mut self, offset: usize, cx: &mut Context<Self>) {
        let name = self.doc.read(cx).slice(super::word_range(self.doc.read(cx), offset));
        let context = json!({ "includeDeclaration": true });
        self.locations_request("textDocument/references", offset, &format!("Usages of {name}"), "No usages found", context, false, cx);
    }

    /// `or_usages`: when the answer is where the request was made (the
    /// declaration itself), list the usages there instead.
    #[allow(clippy::too_many_arguments)]
    fn locations_request(
        &mut self,
        method: &'static str,
        offset: usize,
        title: &str,
        none: &'static str,
        context: Value,
        or_usages: bool,
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
            this.update(cx, |this, cx| match result {
                Ok(value) => {
                    let locations = protocol::locations(&value);
                    if or_usages && this.is_declaration_at(offset, &locations, server.encoding, cx) {
                        this.find_usages_at(offset, cx);
                    } else if locations.is_empty() {
                        cx.emit(EditorLspEvent::Message(none.into()));
                    } else {
                        cx.emit(EditorLspEvent::Navigate { title, locations, encoding: server.encoding });
                    }
                }
                // The server dropped it (the text changed meanwhile): not an error.
                Err(err) if err.is_cancelled() => {}
                Err(err) => cx.emit(EditorLspEvent::Message(err.message.into())),
            })
            .ok();
        })
        .detach();
    }

    /// Whether `locations` are just the name at `offset`. A server that answers
    /// with the whole item (from its first keyword) isn't recognized, and Go to
    /// Declaration jumps there.
    fn is_declaration_at(&self, offset: usize, locations: &[Location], encoding: Encoding, cx: &App) -> bool {
        let doc = self.doc.read(cx);
        let word = super::word_range(doc, offset);
        !locations.is_empty()
            && locations.iter().all(|l| {
                doc.path.as_ref() == Some(&l.path) && word.contains(&protocol::range_to_offsets(doc.rope(), l.range, encoding).start)
            })
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
        let (store, path) = (self.lsp.store.as_ref().and_then(|s| s.upgrade()), self.doc.read(cx).path.clone());
        // Each server is asked for fixes for its own problems under the caret,
        // as it published them (kept by the store, for every editor of the file).
        let requests: Vec<(Arc<Server>, Value)> = servers
            .into_iter()
            .map(|server| {
                let published = match (&store, &path) {
                    (Some(store), Some(path)) => store.read(cx).published(path, &server.id),
                    _ => Vec::new(),
                };
                let diagnostics: Vec<Value> = published
                    .iter()
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

    #[test]
    fn completions_starting_with_what_is_typed_keep_the_servers_ranking() {
        let item = |label: &str, sort: &str| CompletionItem::from_json(json!({ "label": label, "sortText": sort })).unwrap();
        // rust-analyzer on `self.tasks.cl` (a Vec): most methods share one
        // sortText, in no useful order; trait methods rank lower.
        let mut items = vec![
            item("select_nth_unstable", "7fffffff"),
            item("clone_from_slice", "7fffffff"),
            item("copy_from_slice", "0"),
            item("clone", "80000004"),
            item("clear", "7fffffff"),
            item("split_off", "7ffffff1"),
        ];
        let rank = |items: &[CompletionItem], typed: &str| {
            let (matches, selected) = rank_completions(items, typed);
            (matches.iter().map(|&i| items[i].label.clone()).collect::<Vec<_>>(), selected)
        };
        // Fuzzy matches follow, however the server ranks them.
        assert_eq!(rank(&items, "cl").0, ["clear", "clone_from_slice", "clone", "copy_from_slice", "select_nth_unstable"]);
        // An exact match goes first.
        assert_eq!(rank(&items, "clone").0, ["clone", "clone_from_slice"]);
        // Nothing typed: the server's ranking, then labels.
        assert_eq!(rank(&items, "").0[..4], ["copy_from_slice", "split_off", "clear", "clone_from_slice"]);
        // The server's preselected item is chosen among matches as good as
        // the first, but not over an exact match or a prefix match.
        items[1].preselect = true;
        assert_eq!(rank(&items, "cl").1, 1);
        assert_eq!(rank(&items, "clone").1, 0);
        items[1].preselect = false;
        items[2].preselect = true;
        assert_eq!(rank(&items, "cl").1, 0);
    }

    #[gpui::test]
    fn code_lenses_come_resolved_sit_above_their_line_and_run(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-lens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("lens", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        // Asked after a pause in editing; resolved for its title.
        editor.update(cx, |e, cx| e.schedule_code_lenses(cx));
        wait_for(cx, "the lens", |cx| {
            cx.executor().advance_clock(std::time::Duration::from_millis(500));
            editor.read_with(cx, |e, cx| !e.doc.read(cx).code_lenses().is_empty())
        });
        let lens = editor.read_with(cx, |e, cx| e.doc.read(cx).code_lenses()[0].clone());
        assert_eq!((lens.title.as_str(), lens.line_start), ("lens lens", 0));
        editor.read_with(cx, |e, cx| assert_eq!(e.block_rows(cx), [(0, 1)]));

        // Clicked: a references lens opens the usages list.
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { title, locations, .. } = event {
                    seen.borrow_mut().push((title.to_string(), locations.len()));
                }
            })
        });
        editor.update(cx, |e, cx| e.run_code_lens(lens, cx));
        assert_eq!(*events.borrow(), [("lens lens".to_string(), 1)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn inlay_hints_are_asked_again_when_the_file_changes_on_disk(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-hints-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "let index = 1;\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("hints", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let hints = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).inlay_hints().iter().map(|h| h.offset).collect::<Vec<_>>());
        // The server's hint goes at the end of the first line.
        wait_for(cx, "the hint", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            hints(cx) == [14]
        });

        // A merge adds a line above: the hint moves with its line until the
        // server answers for the new text.
        std::fs::write(&path, "// merged\nlet index = 1;\n").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        editor.update(cx, |e, cx| e.reload_if_changed(cx));
        assert_eq!(hints(cx), [24]);
        wait_for(cx, "the new hint", |cx| {
            cx.executor().advance_clock(Duration::from_millis(500));
            hints(cx) == [9]
        });
        // The file is saved as it is now: a server that checks on save checks again.
        wait_for(cx, "the server's check", |cx| {
            editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().any(|d| &*d.message == "hints checked // merged"))
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn go_to_declaration_on_the_declaration_lists_its_usages(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-decl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\nfn b() { a() }\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("decl", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&editor, move |_, event: &EditorLspEvent, _| {
                if let EditorLspEvent::Navigate { title, locations, .. } = event {
                    seen.borrow_mut().push((title.to_string(), locations.len()));
                }
            })
        });
        // On a usage: the declaration.
        editor.update(cx, |e, cx| e.navigate_from(19, cx));
        wait_for(cx, "the declaration", |_| events.borrow().len() == 1);
        // On the declaration's name: its usages.
        editor.update(cx, |e, cx| e.navigate_from(3, cx));
        wait_for(cx, "the usages", |_| events.borrow().len() == 2);
        assert_eq!(*events.borrow(), [("Declaration".to_string(), 1), ("Usages of a".to_string(), 2)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn cmd_hover_links_only_words_with_a_declaration(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        // The fake server knows declarations on the first two lines only.
        std::fs::write(&path, "fn a() {}\nfn b() { a() }\nlet x = 1;\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("link", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        wait_for(cx, "the server", |cx| editor.read_with(cx, |e, cx| !e.doc.read(cx).diagnostics.is_empty()));
        let link = |cx: &mut TestAppContext| editor.read_with(cx, |e, _| e.lsp.link.clone());
        let answered = |cx: &mut TestAppContext| editor.read_with(cx, |e, _| e.lsp.link_pending.is_none());

        // A usage of `a`: a link once the server answers.
        editor.update(cx, |e, cx| e.link_word(Some(19..20), cx));
        assert_eq!(link(cx), None);
        wait_for(cx, "the link", |cx| link(cx) == Some(19..20));
        // `x` has no declaration: no link, and moving over it doesn't ask again.
        editor.update(cx, |e, cx| e.link_word(Some(29..30), cx));
        wait_for(cx, "the answer", answered);
        assert_eq!(link(cx), None);
        editor.update(cx, |e, cx| e.link_word(Some(29..30), cx));
        assert!(answered(cx));
        // Back on `a`: known already, a link right away.
        editor.update(cx, |e, cx| e.link_word(Some(19..20), cx));
        assert_eq!(link(cx), Some(19..20));
        // Back on `x`: asked again (a server still loading answers nothing).
        editor.update(cx, |e, cx| e.link_word(Some(29..30), cx));
        assert!(!answered(cx));
        wait_for(cx, "the answer", answered);
        assert_eq!(link(cx), None);
        // ⌘/Ctrl up, or an edit: gone.
        editor.update(cx, |e, cx| e.link_word(None, cx));
        assert_eq!(link(cx), None);
        editor.update(cx, |e, cx| e.link_word(Some(19..20), cx));
        wait_for(cx, "the link", |cx| link(cx) == Some(19..20));
        editor.update(cx, |e, cx| e.edit_and_remap(vec![(0..0, " ".into())], EditKind::Other, cx));
        assert_eq!(link(cx), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn servers_hear_of_the_files_they_watch(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.rs");
        std::fs::write(&path, "fn a() {}\n").unwrap();
        let store = cx.new(|_| {
            let mut store = LspStore::new();
            store.test_specs = Some(vec![fake_server("watch", "")]);
            store
        });
        let editor: Entity<Editor> = cx.new(|cx| Editor::new(Document::open(&path).unwrap(), cx));
        store.update(cx, |s, cx| s.register(&editor, cx));
        let messages = |cx: &mut TestAppContext| editor.read_with(cx, |e, cx| e.doc.read(cx).diagnostics.iter().map(|d| d.message.to_string()).collect::<Vec<_>>());
        wait_for(cx, "the server", |cx| messages(cx).iter().any(|m| m == "watch problem"));

        // It registered **/*.toml: it hears of Cargo.toml, not of b.rs.
        let changes = [(dir.join("Cargo.toml"), crate::lsp::FileChange::Changed), (dir.join("b.rs"), crate::lsp::FileChange::Created)];
        wait_for(cx, "the watched change", |cx| {
            store.read_with(cx, |s, _| s.files_changed(&changes));
            messages(cx).iter().any(|m| m.starts_with("watch watched"))
        });
        assert_eq!(messages(cx), ["watch watched 2 Cargo.toml"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[gpui::test]
    fn a_document_gets_every_server(cx: &mut TestAppContext) {
        if crate::lsp::servers::which("python3").is_none() {
            return;
        }
        cx.executor().allow_parking();
        crate::trust::set_trust_all(true);
        let dir = std::env::temp_dir().join(format!("kuana-lsp-servers-{}", std::process::id()));
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
            let store = store.read(cx);
            assert_eq!((store.published(&path, "fake-main").len(), store.published(&path, "fake-extra").len()), (1, 1));
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
        let dir = std::env::temp_dir().join(format!("kuana-lsp-restart-{}", std::process::id()));
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
        let dir = std::env::temp_dir().join(format!("kuana-lsp-settings-{}", std::process::id()));
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
