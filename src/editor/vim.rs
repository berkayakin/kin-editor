//! Vim mode (the `vim_mode` setting): Normal, Insert, Visual and Visual Line
//! modes on top of the editor's own selections and edits.
//!
//! Keys reach Vim through a keystroke interceptor, which runs before key
//! bindings: in Normal and Visual mode it takes every plain key (⌘/Ctrl
//! shortcuts other than Vim's own still work); in Insert mode only Escape.
//! A command is collected in `pending` until it's complete ("2d", "di(").
//! Yanks and deletes go to the system clipboard.

use std::ops::Range;

use gpui::{ClipboardItem, Context, Keystroke, Subscription, Window};

use super::{Backspace, Editor, EditorMode, Indent, JoinLines, Newline, NewlineAbove, NewlineBelow, Outdent, Redo, Selection, Undo};
use crate::{document::EditKind, settings::Settings};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VimMode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

pub(super) struct Vim {
    pub(super) mode: VimMode,
    /// Keys of the command being typed.
    pending: String,
    /// Where Visual mode started, and the cursor (both on characters).
    visual_anchor: usize,
    visual_cursor: usize,
    /// The column `j`/`k` keep to (in characters).
    want_col: Option<usize>,
    /// The last `f`/`F`/`t`/`T` and its character, for `;` and `,`.
    last_find: Option<(char, char)>,
    /// What was last yanked or deleted, and whether it was whole lines.
    register: Option<(String, bool)>,
    /// The undo history before the current command: an Insert session after
    /// it becomes one undo step with it.
    command_mark: usize,
    insert_mark: Option<usize>,
    /// Keys of the change being made, then of the last one (for `.`).
    recording: Option<Vec<String>>,
    last_change: Vec<String>,
    replaying: bool,
    _interceptor: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// What a complete command asks for.
enum Command {
    Motion(usize, String),
    Operator { op: char, count: usize, target: Target },
    Simple(usize, String),
}

enum Target {
    /// `dd`, `cc`, `yy`, `>>`: whole lines.
    Lines,
    Motion(String),
    Object(char, char),
}

enum Parse {
    Incomplete,
    Invalid,
    Done(Command),
}

const MOTIONS: &str = "hjklwbeWBE0^$G{}%;, +-_";
const OBJECTS: &str = "wW\"'`()b[]{}B<>fc";

/// Splits a leading count off `s`.
fn take_count(s: &str) -> (Option<usize>, &str) {
    let digits = s.char_indices().take_while(|(i, c)| c.is_ascii_digit() && !(*i == 0 && *c == '0')).count();
    if digits == 0 {
        return (None, s);
    }
    (s[..digits].parse().ok(), &s[digits..])
}

/// A motion at the start of `s`: complete (with its length), incomplete, or
/// not a motion.
fn motion_len(s: &str) -> Option<Option<usize>> {
    let mut chars = s.chars();
    let first = chars.next()?;
    match first {
        'f' | 'F' | 't' | 'T' => Some(chars.next().map(|c| 1 + c.len_utf8())),
        'g' => match chars.next() {
            None => Some(None),
            Some('g') => Some(Some(2)),
            Some(_) => None,
        },
        c if MOTIONS.contains(c) => Some(Some(1)),
        _ => None,
    }
}

fn parse(pending: &str, visual: bool) -> Parse {
    let (count1, rest) = take_count(pending);
    let count1 = count1.unwrap_or(1);
    let Some(first) = rest.chars().next() else { return Parse::Incomplete };
    let operators = if visual { "" } else { "dcy<>" };
    if operators.contains(first) {
        let after = &rest[1..];
        let (count2, target) = take_count(after);
        let count = count1 * count2.unwrap_or(1);
        let Some(t) = target.chars().next() else { return Parse::Incomplete };
        if t == first {
            return if target.len() == 1 { Parse::Done(Command::Operator { op: first, count, target: Target::Lines }) } else { Parse::Invalid };
        }
        if t == 'i' || t == 'a' {
            return match target[1..].chars().next() {
                None => Parse::Incomplete,
                Some(o) if OBJECTS.contains(o) => Parse::Done(Command::Operator { op: first, count, target: Target::Object(t, o) }),
                Some(_) => Parse::Invalid,
            };
        }
        return match motion_len(target) {
            Some(Some(n)) if n == target.len() => Parse::Done(Command::Operator { op: first, count, target: Target::Motion(target.to_string()) }),
            Some(None) => Parse::Incomplete,
            _ => Parse::Invalid,
        };
    }
    if visual && (first == 'i' || first == 'a') {
        return match rest[1..].chars().next() {
            None => Parse::Incomplete,
            Some(o) if OBJECTS.contains(o) => Parse::Done(Command::Simple(count1, rest.to_string())),
            Some(_) => Parse::Invalid,
        };
    }
    match motion_len(rest) {
        Some(Some(n)) if n == rest.len() => return Parse::Done(Command::Motion(count1, rest.to_string())),
        Some(None) => return Parse::Incomplete,
        _ => {}
    }
    // Visual mode's operators act on the selection right away.
    if visual && rest.len() == 1 && "dcy<>uUoJpPxXsSDCY~vV:".contains(first) {
        return Parse::Done(Command::Simple(count1, rest.to_string()));
    }
    match first {
        'r' => match rest[1..].chars().next() {
            None => Parse::Incomplete,
            Some(_) => Parse::Done(Command::Simple(count1, rest.to_string())),
        },
        _ if rest.len() == first.len_utf8() && "xXsSDCYp P~JuiaIAoOvV.nN/?:*#".contains(first) && first != ' ' => {
            Parse::Done(Command::Simple(count1, rest.to_string()))
        }
        _ if rest == "gd" || rest == "gJ" => Parse::Done(Command::Simple(count1, rest.to_string())),
        _ => Parse::Invalid,
    }
}

/// A key as Vim sees it, or None for keys Vim leaves alone (⌘ shortcuts).
fn token(k: &Keystroke) -> Option<String> {
    let m = &k.modifiers;
    if m.platform || m.function || m.alt {
        return None;
    }
    if m.control {
        return Some(format!("ctrl-{}", k.key));
    }
    match k.key.as_str() {
        "escape" | "enter" | "backspace" | "tab" | "delete" | "left" | "right" | "up" | "down" | "home" | "end" | "pageup"
        | "pagedown" => Some(k.key.clone()),
        _ => k.key_char.clone().filter(|c| c.chars().count() == 1),
    }
}

/// 0 whitespace, 1 word characters, 2 punctuation; `big` merges 1 and 2.
fn class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

impl Editor {
    /// Turns Vim on or off to match the settings.
    pub(super) fn sync_vim(&mut self, cx: &mut Context<Self>) {
        let on = self.mode == EditorMode::Full && Settings::get(cx).vim_mode == Some(true);
        if on == self.vim.is_some() {
            return;
        }
        if !on {
            self.vim = None;
            cx.notify();
            return;
        }
        let editor = cx.weak_entity();
        let interceptor = cx.intercept_keystrokes(move |event, window, cx| {
            let Some(editor) = editor.upgrade() else { return };
            if !editor.read(cx).focus_handle.is_focused(window) {
                return;
            }
            let keystroke = event.keystroke.clone();
            editor.update(cx, |e, cx| e.vim_keystroke(&keystroke, window, cx));
        });
        self.vim = Some(Vim {
            mode: VimMode::Normal,
            pending: String::new(),
            visual_anchor: 0,
            visual_cursor: 0,
            want_col: None,
            last_find: None,
            register: None,
            command_mark: 0,
            insert_mark: None,
            recording: None,
            last_change: Vec::new(),
            replaying: false,
            _interceptor: interceptor,
        });
        let head = self.primary_selection().head;
        self.set_selections(vec![Selection::cursor(head)], 0, cx);
        self.vim_clamp(cx);
        cx.notify();
    }

    /// "NORMAL", "INSERT" … for the status bar, with a command being typed.
    pub fn vim_status(&self) -> Option<String> {
        let vim = self.vim.as_ref()?;
        let mode = match vim.mode {
            VimMode::Normal => "NORMAL",
            VimMode::Insert => "INSERT",
            VimMode::Visual => "VISUAL",
            VimMode::VisualLine => "VISUAL LINE",
        };
        Some(if vim.pending.is_empty() { mode.to_string() } else { format!("{mode}  {}", vim.pending) })
    }

    /// Whether carets draw as blocks (Normal and Visual mode).
    pub(super) fn vim_block_cursor(&self) -> bool {
        self.vim.as_ref().is_some_and(|v| v.mode != VimMode::Insert)
    }

    /// Where the Vim cursor is for a selection (Visual mode's selection ends
    /// after the character the cursor is on).
    pub(super) fn vim_cursor_of(&self, sel: &Selection) -> usize {
        match self.vim.as_ref() {
            Some(v) if matches!(v.mode, VimMode::Visual | VimMode::VisualLine) => v.visual_cursor,
            _ => sel.head,
        }
    }

    /// A mouse selection in Normal mode becomes Visual mode.
    pub(super) fn vim_after_mouse(&mut self, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_ref() else { return };
        let sel = self.primary_selection();
        if vim.mode == VimMode::Normal && !sel.is_empty() {
            let cursor = if sel.head > sel.anchor { self.prev(sel.head) } else { sel.head };
            let anchor = if sel.anchor > sel.head { self.prev(sel.anchor) } else { sel.anchor };
            if let Some(vim) = self.vim.as_mut() {
                vim.mode = VimMode::Visual;
                vim.visual_anchor = anchor;
                vim.visual_cursor = cursor;
            }
            self.vim_show_visual(cx);
        } else if vim.mode == VimMode::Normal {
            self.vim_clamp(cx);
        }
    }

    fn vim_keystroke(&mut self, keystroke: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_mut() else { return };
        let Some(tok) = token(keystroke) else { return };
        if vim.mode == VimMode::Insert {
            if tok == "escape" || tok == "ctrl-[" {
                cx.stop_propagation();
                self.vim_leave_insert(cx);
            } else if !vim.replaying
                && let Some(recording) = vim.recording.as_mut()
            {
                recording.push(tok);
            }
            return;
        }
        // Escape with nothing to cancel goes on (closing the find bar, panels).
        if tok == "escape" && vim.pending.is_empty() && vim.mode == VimMode::Normal {
            return;
        }
        // Ctrl shortcuts Vim doesn't use stay the app's.
        if tok.starts_with("ctrl-") && !matches!(tok.as_str(), "ctrl-r" | "ctrl-d" | "ctrl-u" | "ctrl-[") {
            return;
        }
        // Page keys keep their usual meaning.
        if matches!(tok.as_str(), "pageup" | "pagedown") {
            return;
        }
        cx.stop_propagation();
        self.vim_key(tok, window, cx);
    }

    /// One key in Normal or Visual mode.
    fn vim_key(&mut self, tok: String, window: &mut Window, cx: &mut Context<Self>) {
        let mapped = match tok.as_str() {
            "left" | "backspace" => "h",
            "right" => "l",
            "up" => "k",
            "down" => "j",
            "home" => "0",
            "end" => "$",
            "enter" => "+",
            "delete" => "x",
            "tab" => return,
            other => other,
        }
        .to_string();
        let Some(vim) = self.vim.as_mut() else { return };
        if mapped == "escape" || mapped == "ctrl-[" {
            vim.pending.clear();
            vim.recording = None;
            if vim.mode != VimMode::Normal {
                self.vim_enter_normal(cx);
            }
            cx.notify();
            return;
        }
        if vim.pending.is_empty() && !vim.replaying {
            vim.command_mark = self.doc.undo_mark();
            vim.recording = Some(Vec::new());
        }
        if let Some(recording) = vim.recording.as_mut().filter(|_| !vim.replaying) {
            recording.push(tok);
        }
        if let Some(ctrl) = mapped.strip_prefix("ctrl-") {
            vim.pending.clear();
            match ctrl {
                "r" => {
                    self.redo(&Redo, window, cx);
                    self.vim_collapse(cx);
                }
                "d" | "u" => {
                    let rows = self.layout.as_ref().map_or(20, |l| (l.bounds.size.height / l.line_height / 2.) as usize).max(1);
                    let motion = if ctrl == "d" { "j" } else { "k" };
                    self.vim_run(Command::Motion(rows, motion.into()), window, cx);
                }
                _ => {}
            }
            self.vim_finish(false);
            cx.notify();
            return;
        }
        vim.pending.push_str(&mapped);
        let visual = matches!(vim.mode, VimMode::Visual | VimMode::VisualLine);
        match parse(&vim.pending, visual) {
            Parse::Incomplete => {}
            Parse::Invalid => {
                vim.pending.clear();
                vim.recording = None;
            }
            Parse::Done(command) => {
                vim.pending.clear();
                self.vim_run(command, window, cx);
            }
        }
        cx.notify();
    }

    fn vim_mode(&self) -> VimMode {
        self.vim.as_ref().map_or(VimMode::Insert, |v| v.mode)
    }

    /// Ends a command: a change is kept for `.` (unless it went on into
    /// Insert mode, which keeps recording until Escape).
    fn vim_finish(&mut self, change: bool) {
        let Some(vim) = self.vim.as_mut() else { return };
        if vim.replaying {
            return;
        }
        if vim.mode == VimMode::Insert {
            return;
        }
        let recording = vim.recording.take();
        if change && let Some(recording) = recording {
            vim.last_change = recording;
        }
    }

    fn vim_run(&mut self, command: Command, window: &mut Window, cx: &mut Context<Self>) {
        let visual = matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine);
        match command {
            Command::Motion(count, motion) => {
                if visual {
                    let cursor = self.vim.as_ref().map_or(0, |v| v.visual_cursor);
                    if let Some((to, _)) = self.vim_motion(cursor, &motion, count, false) {
                        if let Some(vim) = self.vim.as_mut() {
                            vim.visual_cursor = to;
                        }
                        self.vim_show_visual(cx);
                    }
                } else {
                    let vertical = matches!(motion.as_str(), "j" | "k");
                    let heads: Vec<usize> = self.selections.iter().map(|s| s.head).collect();
                    let mut selections = Vec::new();
                    for head in heads {
                        let to = self.vim_motion(head, &motion, count, false).map_or(head, |(to, _)| to);
                        selections.push(Selection::cursor(to));
                    }
                    let primary = self.primary;
                    self.set_selections(selections, primary, cx);
                    if !vertical && let Some(vim) = self.vim.as_mut() {
                        vim.want_col = None;
                    }
                    self.vim_clamp(cx);
                }
                self.vim_finish(false);
            }
            Command::Operator { op, count, target } => {
                let mut ranges = Vec::new();
                for sel in self.selections.clone() {
                    let head = sel.head;
                    let range = match &target {
                        Target::Lines => {
                            let row = self.doc.row_of(head);
                            let last = (row + count - 1).min(self.doc.line_count() - 1);
                            Some((self.doc.line_start(row)..self.doc.line_start(last), Kind::Linewise))
                        }
                        Target::Object(around, object) => self.vim_object(head, *around == 'a', *object).map(|r| (r, Kind::Exclusive)),
                        Target::Motion(motion) => {
                            // `cw` changes to the end of the word (Vim's exception).
                            let motion = if op == 'c' && (motion == "w" || motion == "W") && self.doc.char_at(head).is_some_and(|c| !c.is_whitespace()) {
                                if motion == "w" { "e".to_string() } else { "E".to_string() }
                            } else {
                                motion.clone()
                            };
                            self.vim_motion(head, &motion, count, true).map(|(to, kind)| (head.min(to)..head.max(to), kind))
                        }
                    };
                    if let Some(range) = range {
                        ranges.push(range);
                    }
                }
                if !ranges.is_empty() {
                    self.vim_operate(op, ranges, window, cx);
                }
                self.vim_finish(op != 'y');
            }
            Command::Simple(count, key) => {
                let change = self.vim_simple(count, &key, window, cx);
                self.vim_finish(change);
            }
        }
    }

    /// Applies `d`, `c`, `y`, `>` or `<` to ranges (exclusive ends; linewise
    /// ranges go from a line's start to the last line's start).
    fn vim_operate(&mut self, op: char, ranges: Vec<(Range<usize>, Kind)>, window: &mut Window, cx: &mut Context<Self>) {
        let linewise = ranges.iter().all(|(_, k)| *k == Kind::Linewise);
        // Real ranges: inclusive ones take their last character, linewise
        // ones whole lines.
        let spans: Vec<(Range<usize>, Kind)> = ranges
            .into_iter()
            .map(|(r, kind)| match kind {
                Kind::Exclusive => (r, kind),
                Kind::Inclusive => (r.start..self.next(r.end), kind),
                Kind::Linewise => {
                    let last = self.doc.row_of(r.end);
                    (self.doc.line_start(self.doc.row_of(r.start))..self.doc.line_end(last), kind)
                }
            })
            .collect();
        let text: Vec<String> = spans.iter().map(|(r, _)| self.doc.slice(r.clone())).collect();
        let mut yanked = text.join("\n");
        if linewise {
            yanked.push('\n');
        }
        self.vim_set_register(yanked, linewise, cx);
        match op {
            'y' => {
                let starts: Vec<Selection> = spans
                    .iter()
                    .map(|(r, k)| if *k == Kind::Linewise { Selection::cursor(self.primary_selection().head.max(r.start).min(r.end)) } else { Selection::cursor(r.start) })
                    .collect();
                self.set_selections(starts, 0, cx);
                self.vim_enter_normal(cx);
            }
            '>' | '<' => {
                let selections: Vec<Selection> = spans.iter().map(|(r, _)| Selection { anchor: r.start, head: r.end.max(r.start + 1).min(self.doc.len()) }).collect();
                self.set_selections(selections, 0, cx);
                if op == '>' {
                    self.indent(&Indent, window, cx);
                } else {
                    self.outdent(&Outdent, window, cx);
                }
                let start = self.primary_selection().range().start;
                self.set_selections(vec![Selection::cursor(self.first_non_blank(self.doc.row_of(start)))], 0, cx);
                self.vim_enter_normal(cx);
            }
            'd' => {
                let edits: Vec<(Range<usize>, String)> = spans
                    .iter()
                    .map(|(r, k)| {
                        if *k != Kind::Linewise {
                            return (r.clone(), String::new());
                        }
                        // The line break goes too: after the lines, or before them at the end.
                        if r.end < self.doc.len() {
                            (r.start..r.end + 1, String::new())
                        } else if r.start > 0 {
                            (r.start - 1..r.end, String::new())
                        } else {
                            (r.clone(), String::new())
                        }
                    })
                    .collect();
                let cursors: Vec<Selection> = spans.iter().map(|(r, _)| Selection::cursor(r.start)).collect();
                self.set_selections(cursors, 0, cx);
                self.edit_and_remap(edits, EditKind::Other, cx);
                if linewise {
                    let row = self.doc.row_of(self.primary_selection().head);
                    self.set_selections(vec![Selection::cursor(self.first_non_blank(row))], 0, cx);
                }
                self.vim_enter_normal(cx);
            }
            'c' => {
                // Changing lines keeps the first one's indentation.
                let edits: Vec<(Range<usize>, String)> = spans
                    .iter()
                    .map(|(r, k)| if *k == Kind::Linewise { (self.first_non_blank(self.doc.row_of(r.start)).min(r.end)..r.end, String::new()) } else { (r.clone(), String::new()) })
                    .collect();
                let cursors: Vec<Selection> = edits.iter().map(|(r, _)| Selection::cursor(r.start)).collect();
                self.set_selections(cursors, 0, cx);
                self.edit_and_remap(edits, EditKind::Other, cx);
                self.vim_enter_insert(cx);
            }
            _ => {}
        }
    }

    /// Commands that aren't motions or operators. Returns whether it changed the text.
    fn vim_simple(&mut self, count: usize, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let visual = matches!(self.vim_mode(), VimMode::Visual | VimMode::VisualLine);
        if visual {
            return self.vim_visual_command(count, key, window, cx);
        }
        let head = self.primary_selection().head;
        let row = self.doc.row_of(head);
        match key {
            "x" => self.vim_run(Command::Operator { op: 'd', count, target: Target::Motion("l".into()) }, window, cx),
            "X" => self.vim_run(Command::Operator { op: 'd', count, target: Target::Motion("h".into()) }, window, cx),
            "s" => self.vim_run(Command::Operator { op: 'c', count, target: Target::Motion("l".into()) }, window, cx),
            "S" => self.vim_run(Command::Operator { op: 'c', count, target: Target::Lines }, window, cx),
            "D" => self.vim_run(Command::Operator { op: 'd', count, target: Target::Motion("$".into()) }, window, cx),
            "C" => self.vim_run(Command::Operator { op: 'c', count, target: Target::Motion("$".into()) }, window, cx),
            "Y" => {
                self.vim_run(Command::Operator { op: 'y', count, target: Target::Lines }, window, cx);
                return false;
            }
            "p" | "P" => self.vim_paste(key == "P", count, cx),
            "~" => {
                let end = (0..count).fold(head, |at, _| if at < self.doc.line_end(row) { self.next(at) } else { at });
                let text: String = self.doc.slice(head..end).chars().map(|c| if c.is_uppercase() { c.to_lowercase().collect::<String>() } else { c.to_uppercase().collect() }).collect();
                self.edit_and_remap(vec![(head..end, text)], EditKind::Other, cx);
                self.set_selections(vec![Selection::cursor(end)], 0, cx);
                self.vim_clamp(cx);
            }
            "J" | "gJ" => {
                let last = (row + count.max(2) - 1).min(self.doc.line_count() - 1);
                if last > row {
                    self.set_selections(vec![Selection { anchor: self.doc.line_start(row), head: self.doc.line_start(last) }], 0, cx);
                    self.join_lines(&JoinLines, window, cx);
                    let at = self.primary_selection().range().start;
                    self.set_selections(vec![Selection::cursor(at)], 0, cx);
                    self.vim_clamp(cx);
                }
            }
            "u" => {
                for _ in 0..count {
                    self.undo(&Undo, window, cx);
                }
                self.vim_collapse(cx);
                return false;
            }
            "i" => self.vim_enter_insert(cx),
            "a" => {
                let at = if head < self.doc.line_end(row) { self.next(head) } else { head };
                self.set_selections(vec![Selection::cursor(at)], 0, cx);
                self.vim_enter_insert(cx);
            }
            "I" => {
                self.set_selections(vec![Selection::cursor(self.first_non_blank(row))], 0, cx);
                self.vim_enter_insert(cx);
            }
            "A" => {
                self.set_selections(vec![Selection::cursor(self.doc.line_end(row))], 0, cx);
                self.vim_enter_insert(cx);
            }
            "o" => {
                self.newline_below(&NewlineBelow, window, cx);
                self.vim_enter_insert(cx);
            }
            "O" => {
                self.newline_above(&NewlineAbove, window, cx);
                self.vim_enter_insert(cx);
            }
            "v" | "V" => {
                if let Some(vim) = self.vim.as_mut() {
                    vim.mode = if key == "v" { VimMode::Visual } else { VimMode::VisualLine };
                    vim.visual_anchor = head;
                    vim.visual_cursor = head;
                }
                self.vim_show_visual(cx);
                return false;
            }
            "." => {
                self.vim_repeat(window, cx);
                return false;
            }
            "n" | "N" | "*" | "#" | "/" | "?" => {
                self.vim_search(key, window, cx);
                return false;
            }
            ":" => {
                window.dispatch_action(Box::new(crate::workspace::VimCommandLine), cx);
                return false;
            }
            "gd" => {
                self.go_to_declaration(&super::GoToDeclaration, window, cx);
                return false;
            }
            _ if key.starts_with('r') => {
                let Some(c) = key[1..].chars().next() else { return false };
                let mut end = head;
                for _ in 0..count {
                    if end >= self.doc.line_end(row) {
                        return false;
                    }
                    end = self.next(end);
                }
                let replaced = c.to_string().repeat(self.doc.slice(head..end).chars().count());
                self.edit_and_remap(vec![(head..end, replaced)], EditKind::Other, cx);
                self.set_selections(vec![Selection::cursor(self.prev(end))], 0, cx);
            }
            _ => return false,
        }
        true
    }

    fn vim_visual_command(&mut self, count: usize, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(vim) = self.vim.as_ref() else { return false };
        let linewise = vim.mode == VimMode::VisualLine;
        let (anchor, cursor) = (vim.visual_anchor, vim.visual_cursor);
        let range = if linewise {
            let (a, b) = (self.doc.row_of(anchor.min(cursor)), self.doc.row_of(anchor.max(cursor)));
            (self.doc.line_start(a)..self.doc.line_start(b), Kind::Linewise)
        } else {
            (anchor.min(cursor)..anchor.max(cursor), Kind::Inclusive)
        };
        match key {
            "d" | "x" | "X" | "D" => self.vim_operate('d', vec![range], window, cx),
            "c" | "s" | "S" | "C" => self.vim_operate('c', vec![range], window, cx),
            "y" | "Y" => {
                self.vim_operate('y', vec![range], window, cx);
                return false;
            }
            ">" | "<" => self.vim_operate(key.chars().next().unwrap_or('>'), vec![range], window, cx),
            "~" | "u" | "U" => {
                let span = self.primary_selection().range();
                let text = self.doc.slice(span.clone());
                let new: String = match key {
                    "u" => text.to_lowercase(),
                    "U" => text.to_uppercase(),
                    _ => text.chars().map(|c| if c.is_uppercase() { c.to_lowercase().collect::<String>() } else { c.to_uppercase().collect() }).collect(),
                };
                self.edit_and_remap(vec![(span.clone(), new)], EditKind::Other, cx);
                self.set_selections(vec![Selection::cursor(span.start)], 0, cx);
                self.vim_enter_normal(cx);
            }
            "J" => {
                self.join_lines(&JoinLines, window, cx);
                let at = self.primary_selection().range().start;
                self.set_selections(vec![Selection::cursor(at)], 0, cx);
                self.vim_enter_normal(cx);
            }
            "p" | "P" => {
                let (text, _) = self.vim_register(cx);
                let span = self.primary_selection().range();
                self.edit_and_remap(vec![(span.clone(), text.trim_end_matches('\n').to_string())], EditKind::Other, cx);
                self.set_selections(vec![Selection::cursor(span.start)], 0, cx);
                self.vim_enter_normal(cx);
            }
            "o" => {
                if let Some(vim) = self.vim.as_mut() {
                    std::mem::swap(&mut vim.visual_anchor, &mut vim.visual_cursor);
                }
                self.vim_show_visual(cx);
                return false;
            }
            "v" | "V" => {
                let target = if key == "v" { VimMode::Visual } else { VimMode::VisualLine };
                if self.vim_mode() == target {
                    self.vim_enter_normal(cx);
                } else if let Some(vim) = self.vim.as_mut() {
                    vim.mode = target;
                    self.vim_show_visual(cx);
                }
                return false;
            }
            _ if key.len() == 2 && (key.starts_with('i') || key.starts_with('a')) => {
                let mut chars = key.chars();
                let (around, object) = (chars.next() == Some('a'), chars.next().unwrap_or('w'));
                if let Some(r) = self.vim_object(cursor, around, object).filter(|r| !r.is_empty()) {
                    let last = self.prev(r.end);
                    if let Some(vim) = self.vim.as_mut() {
                        vim.visual_anchor = r.start;
                        vim.visual_cursor = last;
                    }
                    self.vim_show_visual(cx);
                }
                return false;
            }
            ":" => {
                window.dispatch_action(Box::new(crate::workspace::VimCommandLine), cx);
                return false;
            }
            _ => {
                let _ = count;
                return false;
            }
        }
        true
    }

    fn vim_paste(&mut self, before: bool, count: usize, cx: &mut Context<Self>) {
        let (text, linewise) = self.vim_register(cx);
        if text.is_empty() {
            return;
        }
        let head = self.primary_selection().head;
        let row = self.doc.row_of(head);
        let text = text.repeat(count.max(1));
        if linewise {
            let body = text.trim_end_matches('\n');
            let (at, insert) = if before {
                (self.doc.line_start(row), format!("{body}\n"))
            } else {
                (self.doc.line_end(row), format!("\n{body}"))
            };
            self.edit_and_remap(vec![(at..at, insert)], EditKind::Other, cx);
            let first = if before { row } else { row + 1 };
            self.set_selections(vec![Selection::cursor(self.first_non_blank(first))], 0, cx);
        } else {
            let at = if before || head >= self.doc.line_end(row) { head } else { self.next(head) };
            self.set_selections(vec![Selection::cursor(at)], 0, cx);
            self.edit_and_remap(vec![(at..at, text.clone())], EditKind::Other, cx);
            self.set_selections(vec![Selection::cursor(self.prev(at + text.len()))], 0, cx);
        }
        self.vim_clamp(cx);
    }

    fn vim_set_register(&mut self, text: String, linewise: bool, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        if let Some(vim) = self.vim.as_mut() {
            vim.register = Some((text, linewise));
        }
    }

    /// The clipboard, linewise when Vim put it there as lines (or it ends
    /// with a line break).
    fn vim_register(&self, cx: &mut Context<Self>) -> (String, bool) {
        let clipboard = cx.read_from_clipboard().and_then(|c| c.text()).map(|t| t.replace("\r\n", "\n"));
        let ours = self.vim.as_ref().and_then(|v| v.register.clone());
        match (clipboard, ours) {
            (Some(text), Some((ours, linewise))) if text == ours => (text, linewise),
            (Some(text), _) => {
                let linewise = text.ends_with('\n');
                (text, linewise)
            }
            (None, Some(ours)) => ours,
            (None, None) => (String::new(), false),
        }
    }

    fn vim_search(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        use crate::workspace::{Find, FindNext, FindPrevious};
        match key {
            "n" => window.dispatch_action(Box::new(FindNext), cx),
            "N" => window.dispatch_action(Box::new(FindPrevious), cx),
            "*" | "#" => {
                // Selecting the word makes it the find bar's query.
                let head = self.primary_selection().head;
                let word = super::word_range(&self.doc, head);
                self.set_selections(vec![Selection { anchor: word.start, head: word.end }], 0, cx);
                window.dispatch_action(Box::new(Find), cx);
            }
            _ => window.dispatch_action(Box::new(Find), cx),
        }
    }

    fn vim_repeat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_mut() else { return };
        let keys = vim.last_change.clone();
        if keys.is_empty() {
            return;
        }
        vim.replaying = true;
        vim.command_mark = self.doc.undo_mark();
        for key in keys {
            if self.vim_mode() == VimMode::Insert {
                match key.as_str() {
                    "escape" | "ctrl-[" => self.vim_leave_insert(cx),
                    "enter" => self.newline(&Newline, window, cx),
                    "backspace" => self.backspace(&Backspace, window, cx),
                    "tab" => self.indent(&Indent, window, cx),
                    text if text.chars().count() == 1 => self.insert(text, cx),
                    _ => {}
                }
            } else {
                self.vim_key(key, window, cx);
            }
        }
        if let Some(vim) = self.vim.as_mut() {
            vim.replaying = false;
        }
    }

    // ---- Modes ------------------------------------------------------------------------

    fn vim_enter_insert(&mut self, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_mut() else { return };
        vim.mode = VimMode::Insert;
        vim.insert_mark = Some(vim.command_mark);
        let heads: Vec<Selection> = self.selections.iter().map(|s| Selection::cursor(s.head)).collect();
        self.set_selections(heads, self.primary, cx);
        cx.notify();
    }

    fn vim_leave_insert(&mut self, cx: &mut Context<Self>) {
        self.dismiss_lsp_popovers(cx);
        let Some(vim) = self.vim.as_mut() else { return };
        vim.mode = VimMode::Normal;
        if let Some(mark) = vim.insert_mark.take() {
            self.doc.merge_undo_since(mark);
        }
        let Some(vim) = self.vim.as_mut() else { return };
        if !vim.replaying
            && let Some(mut recording) = vim.recording.take()
        {
            recording.push("escape".into());
            vim.last_change = recording;
        }
        // Like Vim, the cursor steps back onto the last character typed.
        let heads: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| {
                let start = self.doc.line_start(self.doc.row_of(s.head));
                Selection::cursor(if s.head > start { self.prev(s.head) } else { s.head })
            })
            .collect();
        self.set_selections(heads, self.primary, cx);
        cx.notify();
    }

    fn vim_enter_normal(&mut self, cx: &mut Context<Self>) {
        if let Some(vim) = self.vim.as_mut() {
            vim.mode = VimMode::Normal;
        }
        self.vim_collapse(cx);
    }

    /// One caret per selection, on a character.
    fn vim_collapse(&mut self, cx: &mut Context<Self>) {
        let heads: Vec<Selection> = self.selections.iter().map(|s| Selection::cursor(s.range().start.min(s.head))).collect();
        let primary = self.primary;
        self.set_selections(heads, primary, cx);
        self.vim_clamp(cx);
    }

    /// Normal mode's caret is on a character, never after a line's last one.
    fn vim_clamp(&mut self, cx: &mut Context<Self>) {
        if self.vim_mode() != VimMode::Normal {
            return;
        }
        let clamped: Vec<Selection> = self
            .selections
            .iter()
            .map(|s| {
                let row = self.doc.row_of(s.head);
                let (start, end) = (self.doc.line_start(row), self.doc.line_end(row));
                Selection::cursor(if s.head >= end && end > start { self.prev(end) } else { s.head })
            })
            .collect();
        if clamped != self.selections {
            let primary = self.primary;
            self.set_selections(clamped, primary, cx);
        }
    }

    fn vim_show_visual(&mut self, cx: &mut Context<Self>) {
        let Some(vim) = self.vim.as_ref() else { return };
        let (anchor, cursor) = (vim.visual_anchor, vim.visual_cursor);
        let sel = if vim.mode == VimMode::VisualLine {
            let (a, b) = (self.doc.row_of(anchor), self.doc.row_of(cursor));
            let (first, last) = (a.min(b), a.max(b));
            let start = self.doc.line_start(first);
            let end = if last + 1 < self.doc.line_count() { self.doc.line_start(last + 1) } else { self.doc.len() };
            if b >= a { Selection { anchor: start, head: end } } else { Selection { anchor: end, head: start } }
        } else if cursor >= anchor {
            Selection { anchor, head: self.next(cursor) }
        } else {
            Selection { anchor: self.next(anchor), head: cursor }
        };
        self.set_selections(vec![sel], 0, cx);
    }

    // ---- Motions and text objects -------------------------------------------------------

    fn next(&self, at: usize) -> usize {
        self.doc.char_at(at).map_or(at, |c| at + c.len_utf8())
    }

    fn prev(&self, at: usize) -> usize {
        self.doc.char_before(at).map_or(at, |c| at - c.len_utf8())
    }

    /// The last line as Vim counts them: a file's final line break doesn't
    /// start another line.
    fn vim_last_row(&self) -> usize {
        let last = self.doc.line_count() - 1;
        if last > 0 && self.doc.line_text(last).is_empty() { last - 1 } else { last }
    }

    fn first_non_blank(&self, row: usize) -> usize {
        let start = self.doc.line_start(row);
        let text = self.doc.line_text(row);
        start + (text.len() - text.trim_start().len())
    }

    /// Where a motion goes from `from`, and how an operator treats it.
    /// `for_op`: the target may be a line's end (deleting to it).
    fn vim_motion(&mut self, from: usize, motion: &str, count: usize, for_op: bool) -> Option<(usize, Kind)> {
        let row = self.doc.row_of(from);
        let (line_start, line_end) = (self.doc.line_start(row), self.doc.line_end(row));
        let last_row = self.vim_last_row();
        let mut chars = motion.chars();
        let key = chars.next()?;
        let arg = chars.next();
        let result = match key {
            'h' => {
                let mut at = from;
                for _ in 0..count {
                    if at > line_start {
                        at = self.prev(at);
                    }
                }
                (at, Kind::Exclusive)
            }
            'l' | ' ' => {
                let mut at = from;
                for _ in 0..count {
                    if at < line_end {
                        at = self.next(at);
                    }
                }
                // Normal mode stops on the last character.
                if !for_op && at >= line_end && line_end > line_start {
                    at = self.prev(line_end);
                }
                (at, Kind::Exclusive)
            }
            'j' | 'k' | '+' | '-' | '_' => {
                let target = match key {
                    'j' | '+' => (row + count).min(last_row),
                    '_' => (row + count - 1).min(last_row),
                    _ => row.saturating_sub(count),
                };
                let at = if key == 'j' || key == 'k' {
                    let current = self.doc.slice(line_start..from).chars().count();
                    let col = *self.vim.as_mut()?.want_col.get_or_insert(current);
                    let text = self.doc.line_text(target);
                    let max = text.chars().count().saturating_sub(1);
                    let byte = text.char_indices().nth(col.min(max)).map_or(0, |(i, _)| i);
                    self.doc.line_start(target) + byte
                } else {
                    self.first_non_blank(target)
                };
                (at, Kind::Linewise)
            }
            '0' => (line_start, Kind::Exclusive),
            '^' => (self.first_non_blank(row), Kind::Exclusive),
            '$' => {
                let target = (row + count - 1).min(last_row);
                let (s, e) = (self.doc.line_start(target), self.doc.line_end(target));
                (if e > s { self.prev(e) } else { s }, Kind::Inclusive)
            }
            'G' => {
                let target = if count > 1 || motion.len() > 1 { count.saturating_sub(1).min(last_row) } else { last_row };
                (self.first_non_blank(target), Kind::Linewise)
            }
            'g' if arg == Some('g') => (self.first_non_blank(count.saturating_sub(1).min(last_row)), Kind::Linewise),
            'w' | 'W' => {
                let big = key == 'W';
                let mut at = from;
                for _ in 0..count {
                    at = self.word_forward(at, big);
                }
                // `dw` on a line's last word stops at its end.
                if for_op && self.doc.row_of(at) > row {
                    at = line_end.max(from);
                }
                (at, Kind::Exclusive)
            }
            'e' | 'E' => {
                let big = key == 'E';
                let mut at = from;
                for _ in 0..count {
                    at = self.word_end(at, big);
                }
                (at, Kind::Inclusive)
            }
            'b' | 'B' => {
                let big = key == 'B';
                let mut at = from;
                for _ in 0..count {
                    at = self.word_backward(at, big);
                }
                (at, Kind::Exclusive)
            }
            '{' | '}' => {
                let mut r = row;
                for _ in 0..count {
                    if key == '}' {
                        r = (r + 1..=last_row).find(|&r| self.doc.line_text(r).trim().is_empty()).unwrap_or(last_row);
                    } else {
                        r = (0..r).rev().find(|&r| self.doc.line_text(r).trim().is_empty()).unwrap_or(0);
                    }
                }
                let at = if key == '}' && r == last_row && !self.doc.line_text(r).trim().is_empty() { self.doc.line_end(r) } else { self.doc.line_start(r) };
                (at, Kind::Exclusive)
            }
            '%' => {
                let text = self.doc.slice(from..line_end);
                let offset = text.char_indices().find(|(_, c)| "()[]{}".contains(*c)).map(|(i, _)| from + i)?;
                let (_, partner) = self.doc.syntax().matching_bracket(self.doc.rope(), offset).filter(|(at, _)| *at == offset).or_else(|| self.bracket_partner(offset).map(|p| (offset, p)))?;
                (partner, Kind::Inclusive)
            }
            'f' | 'F' | 't' | 'T' | ';' | ',' => {
                let (kind, target) = match key {
                    ';' | ',' => {
                        let (k, c) = self.vim.as_ref()?.last_find?;
                        let flipped = match k {
                            'f' => 'F',
                            'F' => 'f',
                            't' => 'T',
                            _ => 't',
                        };
                        (if key == ',' { flipped } else { k }, c)
                    }
                    _ => {
                        let c = arg?;
                        self.vim.as_mut()?.last_find = Some((key, c));
                        (key, c)
                    }
                };
                let at = self.find_in_line(from, kind, target, count, matches!(key, ';' | ','))?;
                (at, if kind == 'f' || kind == 't' { Kind::Inclusive } else { Kind::Exclusive })
            }
            _ => return None,
        };
        Some(result)
    }

    fn word_forward(&self, from: usize, big: bool) -> usize {
        let len = self.doc.len();
        let mut at = from;
        let Some(c) = self.doc.char_at(at) else { return at };
        let start_class = class(c, big);
        if start_class != 0 {
            while let Some(c) = self.doc.char_at(at).filter(|c| class(*c, big) == start_class && *c != '\n') {
                at += c.len_utf8();
            }
        }
        // Then past whitespace, stopping at an empty line.
        while let Some(c) = self.doc.char_at(at).filter(|c| c.is_whitespace()) {
            if c == '\n' && self.doc.char_at(at + 1) == Some('\n') && at + 1 > from {
                return at + 1;
            }
            at += c.len_utf8();
        }
        at.min(len)
    }

    fn word_end(&self, from: usize, big: bool) -> usize {
        let mut at = self.next(from);
        while let Some(c) = self.doc.char_at(at).filter(|c| c.is_whitespace()) {
            at += c.len_utf8();
        }
        let Some(c) = self.doc.char_at(at) else { return from };
        let cls = class(c, big);
        while let Some(n) = self.doc.char_at(self.next(at)).filter(|n| class(*n, big) == cls) {
            let _ = n;
            at = self.next(at);
        }
        at
    }

    fn word_backward(&self, from: usize, big: bool) -> usize {
        let mut at = self.prev(from);
        while at > 0 && self.doc.char_at(at).is_some_and(char::is_whitespace) {
            at = self.prev(at);
        }
        let Some(c) = self.doc.char_at(at) else { return at };
        let cls = class(c, big);
        while let Some(p) = self.doc.char_before(at).filter(|p| class(*p, big) == cls && cls != 0) {
            at -= p.len_utf8();
        }
        at
    }

    fn find_in_line(&self, from: usize, kind: char, target: char, count: usize, repeat: bool) -> Option<usize> {
        let row = self.doc.row_of(from);
        let (start, end) = (self.doc.line_start(row), self.doc.line_end(row));
        let mut at = from;
        let mut found = 0;
        if kind == 'f' || kind == 't' {
            // `;` after `t` would find the same spot again; step past it.
            let mut pos = if repeat && kind == 't' { self.next(at) } else { at };
            while pos < end {
                pos = self.next(pos);
                if self.doc.char_at(pos) == Some(target) {
                    found += 1;
                    if found == count {
                        at = if kind == 't' { self.prev(pos) } else { pos };
                        return Some(at);
                    }
                }
            }
        } else {
            let mut pos = if repeat && kind == 'T' { self.prev(at) } else { at };
            while pos > start {
                pos = self.prev(pos);
                if self.doc.char_at(pos) == Some(target) {
                    found += 1;
                    if found == count {
                        at = if kind == 'T' { self.next(pos) } else { pos };
                        return Some(at);
                    }
                }
            }
        }
        None
    }

    /// The bracket matching the one at `at`, by counting (when the syntax
    /// tree has no answer).
    fn bracket_partner(&self, at: usize) -> Option<usize> {
        let c = self.doc.char_at(at)?;
        let (open, close, forward) = match c {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => ('(', ')', false),
            ']' => ('[', ']', false),
            '}' => ('{', '}', false),
            _ => return None,
        };
        let mut depth = 0i32;
        let mut pos = at;
        loop {
            let ch = self.doc.char_at(pos)?;
            if ch == open {
                depth += if forward { 1 } else { -1 };
            } else if ch == close {
                depth += if forward { -1 } else { 1 };
            }
            if depth == 0 {
                return Some(pos);
            }
            if forward {
                pos = self.next(pos);
                if pos >= self.doc.len() {
                    return None;
                }
            } else {
                if pos == 0 {
                    return None;
                }
                pos = self.prev(pos);
            }
        }
    }

    /// A text object around `at`: a word, a quoted string or a bracketed
    /// block, inner (`i`) or with its delimiters (`a`).
    fn vim_object(&self, at: usize, around: bool, object: char) -> Option<Range<usize>> {
        match object {
            // A function or class: the language's textobjects.scm, else
            // the outline's definitions.
            'f' | 'c' => {
                let name = if object == 'f' { "function" } else { "class" };
                if let Some(range) = self.doc.syntax().text_object(self.doc.rope(), at, name, around) {
                    return Some(range);
                }
                let kinds: &[&str] = if object == 'f' { &["function", "method"] } else { &["class", "struct", "enum", "interface", "trait", "impl", "module", "type"] };
                let symbol = self.doc.syntax().symbols(self.doc.rope()).into_iter().filter(|s| kinds.contains(&s.kind) && s.range.start <= at && at < s.range.end).min_by_key(|s| s.range.len())?;
                let range = symbol.range;
                let (first, last) = (self.doc.row_of(range.start), self.doc.row_of(range.end.saturating_sub(1)));
                // Inside: the lines between its first and last.
                Some(if around || last <= first + 1 { range } else { self.doc.line_start(first + 1)..self.doc.line_start(last) })
            }
            'w' | 'W' => {
                let big = object == 'W';
                let cls = class(self.doc.char_at(at)?, big);
                let mut start = at;
                while let Some(p) = self.doc.char_before(start).filter(|p| class(*p, big) == cls && *p != '\n') {
                    start -= p.len_utf8();
                }
                let mut end = at;
                while let Some(c) = self.doc.char_at(end).filter(|c| class(*c, big) == cls && *c != '\n') {
                    end += c.len_utf8();
                }
                if around {
                    let mut after = end;
                    while let Some(c) = self.doc.char_at(after).filter(|c| *c == ' ' || *c == '\t') {
                        after += c.len_utf8();
                    }
                    if after > end {
                        end = after;
                    } else {
                        while let Some(p) = self.doc.char_before(start).filter(|p| *p == ' ' || *p == '\t') {
                            start -= p.len_utf8();
                        }
                    }
                }
                Some(start..end)
            }
            '"' | '\'' | '`' => {
                let row = self.doc.row_of(at);
                let line_start = self.doc.line_start(row);
                let text = self.doc.line_text(row);
                let col = at - line_start;
                let quotes: Vec<usize> = text.char_indices().filter(|(i, c)| *c == object && !text[..*i].ends_with('\\')).map(|(i, _)| i).collect();
                let pair = quotes.chunks(2).filter(|p| p.len() == 2).find(|p| p[0] <= col && col <= p[1]).or_else(|| quotes.chunks(2).filter(|p| p.len() == 2).find(|p| p[0] > col))?;
                let (open, close) = (line_start + pair[0], line_start + pair[1]);
                Some(if around { open..close + 1 } else { open + 1..close })
            }
            _ => {
                let (open, close) = match object {
                    '(' | ')' | 'b' => ('(', ')'),
                    '[' | ']' => ('[', ']'),
                    '{' | '}' | 'B' => ('{', '}'),
                    '<' | '>' => ('<', '>'),
                    _ => return None,
                };
                // The unmatched opening bracket before `at` (or at it).
                let mut depth = 0;
                let mut pos = at;
                let open_at = loop {
                    match self.doc.char_at(pos) {
                        Some(c) if c == open && depth == 0 => break pos,
                        Some(c) if c == open => depth -= 1,
                        Some(c) if c == close && pos != at => depth += 1,
                        _ => {}
                    }
                    if pos == 0 {
                        return None;
                    }
                    pos = self.prev(pos);
                };
                let mut depth = 0;
                let mut pos = open_at;
                let close_at = loop {
                    match self.doc.char_at(pos)? {
                        c if c == open => depth += 1,
                        c if c == close => {
                            depth -= 1;
                            if depth == 0 {
                                break pos;
                            }
                        }
                        _ => {}
                    }
                    pos = self.next(pos);
                };
                if around {
                    return Some(open_at..close_at + 1);
                }
                // A block on its own lines: the lines between the brackets.
                let mut start = open_at + 1;
                let mut end = close_at;
                if self.doc.char_at(start) == Some('\n') {
                    start += 1;
                    let close_row = self.doc.row_of(close_at);
                    if self.doc.slice(self.doc.line_start(close_row)..close_at).trim().is_empty() {
                        end = self.doc.line_start(close_row);
                    }
                }
                Some(start..end.max(start))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert!(matches!(parse("2d", false), Parse::Incomplete));
        assert!(matches!(parse("d3w", false), Parse::Done(Command::Operator { op: 'd', count: 3, .. })));
        assert!(matches!(parse("2dd", false), Parse::Done(Command::Operator { op: 'd', count: 2, target: Target::Lines })));
        assert!(matches!(parse("ci(", false), Parse::Done(Command::Operator { op: 'c', target: Target::Object('i', '('), .. })));
        assert!(matches!(parse("df", false), Parse::Incomplete));
        assert!(matches!(parse("dfx", false), Parse::Done(Command::Operator { .. })));
        assert!(matches!(parse("gg", false), Parse::Done(Command::Motion(1, _))));
        assert!(matches!(parse("10j", false), Parse::Done(Command::Motion(10, _))));
        assert!(matches!(parse("0", false), Parse::Done(Command::Motion(1, _))));
        assert!(matches!(parse("r", false), Parse::Incomplete));
        assert!(matches!(parse("rx", false), Parse::Done(Command::Simple(1, _))));
        assert!(matches!(parse("dz", false), Parse::Invalid));
        assert!(matches!(parse("iw", true), Parse::Done(Command::Simple(1, _))));
        assert!(matches!(parse("d", true), Parse::Done(Command::Simple(1, _))));
    }
}
