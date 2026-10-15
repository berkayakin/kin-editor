//! The text editor view: multi-cursor editing, keyboard/mouse/IME input and a
//! custom element that paints only the visible lines straight to the GPU.

mod fold;
mod format;
mod conflict;
mod derived;
mod lsp;
mod run;
mod snippet;
mod sticky;
mod transform;
mod unloaded;
mod vim;

pub use lsp::EditorLspEvent;
pub use run::{EditorBreakpointMenu, EditorBreakpointsChanged, EditorRunEvent};
pub use unloaded::{EditorLoaded, Unloaded};

use std::{ops::Range, sync::Arc, time::Duration};

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font,
    FontStyle,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ScrollWheelEvent, ShapedLine, SharedString, Style, Task, TextRun, UTF16Selection,
    UnderlineStyle, Window, actions, div, fill, font, point, prelude::*, px, relative, size,
};

use crate::{
    assets::Icon,
    conflict::Resolution,
    document::{Document, EditKind, Selection},
    git::{self, BlameLine, Hunk, HunkKind},
    lint::{self, Severity, external::ExternalLinter},
    settings::{FileSettings, Settings, ShowWhitespace},
    theme::Theme,
    ui::HoverFill,
    wrap::{self, Break, WrapKey, WrapMap},
};

const SCROLLBAR_WIDTH: f32 = 12.0;
const TEXT_PADDING: f32 = 6.0;
/// Distance of the git change bar's center from the gutter's right edge.
const GIT_BAR_OFFSET: f32 = 8.0;
/// The fold arrow sits this far left of the gutter's right edge (to its right edge).
const FOLD_ZONE_RIGHT: f32 = 14.0;
const FOLD_ICON: f32 = 12.0;
/// The ▶ for runnable lines, at the gutter's left edge (where the problem dot goes).
const RUN_ICON: f32 = 11.0;

actions!(
    editor,
    [
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        MoveWordLeft,
        MoveWordRight,
        SelectWordLeft,
        SelectWordRight,
        MoveLineStart,
        MoveLineEnd,
        SelectLineStart,
        SelectLineEnd,
        MoveDocStart,
        MoveDocEnd,
        SelectDocStart,
        SelectDocEnd,
        PageUp,
        PageDown,
        Backspace,
        Delete,
        DeleteWordLeft,
        DeleteWordRight,
        DeleteToLineStart,
        Newline,
        NewlineBelow,
        Indent,
        Outdent,
        SelectAll,
        SelectLine,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        ToggleComment,
        DuplicateLine,
        MoveLineUp,
        MoveLineDown,
        SelectNextOccurrence,
        AddCursorAbove,
        AddCursorBelow,
        Cancel,
        ShowCharacterPalette,
        DeleteLine,
        ExtendSelection,
        ShrinkSelection,
        UnselectOccurrence,
        SelectAllOccurrences,
        ToggleCase,
        ConvertToUpperCase,
        ConvertToLowerCase,
        ConvertToTitleCase,
        ConvertToSnakeCase,
        ConvertToKebabCase,
        ConvertToCamelCase,
        ConvertToPascalCase,
        SortLinesAscending,
        SortLinesDescending,
        ReverseLines,
        RemoveDuplicateLines,
        JoinLines,
        NewlineAbove,
        ToggleBlockComment,
        RollbackHunk,
        StageHunk,
        UnstageHunk,
        AcceptCurrent,
        AcceptIncoming,
        AcceptBoth,
        NextChange,
        PreviousChange,
        ToggleInlineBlame,
        ToggleSoftWrap,
        ToggleInlayHints,
        ToggleMarkdownPreview,
        ShowCompletions,
        QuickDocumentation,
        GoToDeclaration,
        GoToImplementation,
        GoToTypeDefinition,
        FindUsages,
        RenameSymbol,
        ShowContextActions,
        ReformatCode,
        CollapseRegion,
        ExpandRegion,
        CollapseAllRegions,
        ExpandAllRegions,
        RunAtCaret,
        DebugAtCaret,
        ToggleBreakpoint,
        /// ⌘↩ / Ctrl+Enter: "new line below" in code, "submit" in text fields.
        Submit,
    ]
);

/// Where an open file lives in a Git repository.
#[derive(Clone, Debug, PartialEq)]
pub struct GitFile {
    pub root: std::path::PathBuf,
    pub rel: String,
    /// The repository's `user.name`, shown as "You" in blame.
    pub user_name: Option<String>,
}

/// A Git operation started from the editor (staging a change).
pub struct EditorGitEvent(pub crate::git_panel::GitOp);

impl EventEmitter<EditorGitEvent> for Editor {}

/// App-wide toggle for inline blame.
pub struct InlineBlame(pub bool);

impl gpui::Global for InlineBlame {}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    /// A full code editor with gutter, linting and scrollbar.
    Full,
    /// A one-line text field, used by search inputs.
    SingleLine,
    /// A plain multi-line text field without code chrome (commit messages).
    MultiLine,
}

pub enum EditorEvent {
    Edited,
}

impl EventEmitter<EditorEvent> for Editor {}

/// A right-click in the code editor, at this window position. Separate from
/// [`EditorEvent`] because many views treat any `EditorEvent` as an edit.
pub struct EditorContextMenu(pub Point<Pixels>);

impl EventEmitter<EditorContextMenu> for Editor {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DragMode {
    Char,
    Word,
    Line,
}

struct Drag {
    mode: DragMode,
    /// The unit (char/word/line range) where the drag started.
    origin: Range<usize>,
    /// With Alt: the row and display column it started at. Moving the
    /// pointer then selects a box (a selection per line) instead.
    column_from: Option<(usize, usize)>,
}

/// Geometry from the last frame, used for hit-testing and IME positioning.
#[derive(Clone)]
struct LayoutSnapshot {
    bounds: Bounds<Pixels>,
    text_left: Pixels,
    line_height: Pixels,
    char_width: Pixels,
    gutter_width: Pixels,
    rows_per_page: usize,
    /// Text width available to soft-wrapped rows.
    wrap_width: Pixels,
}

pub struct Editor {
    pub doc: Entity<Document>,
    mode: EditorMode,
    placeholder: SharedString,
    search_matches: Vec<Range<usize>>,
    active_match: Option<usize>,
    /// Selections before each ExtendSelection, for ShrinkSelection.
    expand_stack: Vec<Vec<Selection>>,
    expanding: bool,
    selections: Vec<Selection>,
    /// Index of the most recently added selection.
    primary: usize,
    goal_x: Option<Pixels>,
    focus_handle: FocusHandle,
    scroll: Point<Pixels>,
    max_line_width: Pixels,
    autoscroll: bool,
    center_on_next_frame: bool,
    /// Like autoscroll, but centers the cursor when it is off screen.
    reveal_primary: bool,
    /// Vim mode's state while the `vim_mode` setting is on.
    vim: Option<vim::Vim>,
    layout: Option<LayoutSnapshot>,
    drag: Option<Drag>,
    scrollbar_drag: Option<Pixels>,
    marked_range: Option<Range<usize>>,
    indent_unit: String,
    /// Display width of a tab character (and of an indent level with tabs).
    tab_size: usize,
    /// Resolved settings for this file (see `apply_settings`).
    file_settings: FileSettings,
    /// What `apply_settings` last applied (None: not yet).
    applied_settings: Option<FileSettings>,
    cursor_visible: bool,
    blink_epoch: usize,
    blink_task: Option<Task<()>>,
    lint_task: Option<Task<()>>,
    linted_version: Option<u64>,
    external: Option<ExternalLinter>,
    external_error: Option<String>,
    external_task: Option<Task<()>>,
    git_file: Option<GitFile>,
    /// The file's text at HEAD; changes are shown against it.
    diff_base: Option<Arc<str>>,
    /// The file's staged text, to tell staged changes from unstaged ones.
    index_base: Option<Arc<str>>,
    hunks: Arc<Vec<Hunk>>,
    /// Per hunk: whether it's fully staged.
    hunk_staged: Arc<Vec<bool>>,
    diff_task: Option<Task<()>>,
    /// Index into `hunks` of the change popover that is open.
    open_hunk: Option<usize>,
    blame: Option<(usize, u64, BlameLine)>,
    blame_request: Option<(usize, u64)>,
    blame_task: Option<Task<()>>,
    soft_wrap: bool,
    /// Line breaks for soft wrap, recomputed lazily (see `sync_wrap`).
    wrap: Option<WrapMap>,
    /// Language server state: completion, hover, signature help.
    lsp: lsp::EditorLsp,
    /// A parse finishing in the background (see `schedule_parse`).
    parse_task: Option<Task<()>>,
    /// The carets' bracket pairs, for a tree version and caret offsets.
    bracket_cache: Option<((u64, Vec<usize>), Arc<Vec<Range<usize>>>)>,
    /// Highlights of what was on screen: for a tree version, language and ranges.
    highlight_cache: Option<((u64, crate::syntax::Language, Vec<Range<usize>>), Arc<Vec<(Range<usize>, crate::syntax::Highlight)>>)>,
    /// Fold regions for a (version, tab size, language).
    fold_regions: derived::Derived<(u64, usize, crate::syntax::Language), Vec<crate::fold::FoldRegion>>,
    /// `⋯` placeholders on screen last frame, for clicks: (bounds, fold start).
    fold_pills: Vec<(Bounds<Pixels>, usize)>,
    /// File Structure's symbols for a document version (breadcrumbs, sticky scroll).
    outline: derived::Derived<(u64, crate::syntax::Language), Vec<crate::syntax::Symbol>>,
    /// Markdown: the rendered preview shows beside the text.
    md_preview: bool,
    md_blocks: derived::Derived<(u64, Option<std::path::PathBuf>), Vec<crate::markdown_preview::Block>>,
    md_scroll: gpui::ScrollHandle,
    /// The preview's text size, relative to the UI font.
    md_zoom: f32,
    /// ⌘-scrolling not yet turned into a zoom step (trackpads send small deltas).
    md_zoom_rest: f32,
    /// Pinned lines of sticky scroll, as last painted: bounds and where to go.
    sticky_rows: Vec<(Bounds<Pixels>, usize)>,
    /// The pointer is over the gutter, so fold arrows show.
    gutter_hovered: bool,
    /// Runnable targets for a version and path (gutter ▶).
    run_targets: derived::Derived<(u64, Option<std::path::PathBuf>, crate::syntax::Language), Vec<crate::run::RunTarget>>,
    /// Format, dimensions and size of an image document (read on first show).
    image_info: Option<crate::preview::ImageInfo>,
    /// Merge conflicts for a version.
    conflicts: derived::Derived<u64, Vec<crate::conflict::Conflict>>,
    /// Accept labels on screen last frame: (bounds, conflict start row, choice).
    conflict_hits: Vec<(Bounds<Pixels>, usize, crate::conflict::Resolution)>,
    /// A restored tab whose file isn't read yet (`load` reads it).
    unloaded: Option<Unloaded>,
}

impl Editor {
    pub fn new(doc: Document, cx: &mut Context<Self>) -> Self {
        let mut this = Self::blank(doc, cx);
        this.apply_settings(cx);
        this.detect_external_linter(cx);
        this.schedule_lint(Duration::ZERO, cx);
        this.run_external_linter(cx);
        this.schedule_parse(cx);
        this
    }

    /// The editor's state for `doc`, before anything reads settings or
    /// starts work on the text.
    fn blank(doc: Document, cx: &mut Context<Self>) -> Self {
        let file_settings = Settings::get(cx).for_file(doc.language, doc.path.as_deref());
        Self {
            doc: cx.new(|_| doc),
            mode: EditorMode::Full,
            placeholder: SharedString::default(),
            search_matches: Vec::new(),
            active_match: None,
            expand_stack: Vec::new(),
            expanding: false,
            selections: vec![Selection::cursor(0)],
            primary: 0,
            goal_x: None,
            focus_handle: cx.focus_handle(),
            scroll: Point::default(),
            max_line_width: px(0.),
            autoscroll: false,
            center_on_next_frame: false,
            reveal_primary: false,
            vim: None,
            layout: None,
            drag: None,
            scrollbar_drag: None,
            marked_range: None,
            indent_unit: String::new(),
            tab_size: file_settings.tab_size,
            file_settings,
            applied_settings: None,
            cursor_visible: true,
            blink_epoch: 0,
            blink_task: None,
            lint_task: None,
            linted_version: None,
            external: None,
            external_error: None,
            external_task: None,
            git_file: None,
            diff_base: None,
            index_base: None,
            hunks: Arc::new(Vec::new()),
            hunk_staged: Arc::new(Vec::new()),
            diff_task: None,
            open_hunk: None,
            blame: None,
            blame_request: None,
            blame_task: None,
            soft_wrap: false,
            wrap: None,
            lsp: lsp::EditorLsp::default(),
            highlight_cache: None,
            parse_task: None,
            bracket_cache: None,
            fold_regions: derived::Derived::new(),
            fold_pills: Vec::new(),
            outline: derived::Derived::new(),
            md_preview: false,
            md_blocks: derived::Derived::new(),
            md_scroll: gpui::ScrollHandle::new(),
            md_zoom: 1.,
            md_zoom_rest: 0.,
            sticky_rows: Vec::new(),
            gutter_hovered: false,
            run_targets: derived::Derived::new(),
            conflicts: derived::Derived::new(),
            conflict_hits: Vec::new(),
            image_info: None,
            unloaded: None,
        }
    }

    /// Finishes in the background a parse that ran out of time on the UI
    /// thread (a big file opening, an edit in one). One at a time: a parse
    /// that ends behind the edits made meanwhile starts the next.
    pub(super) fn schedule_parse(&mut self, cx: &mut Context<Self>) {
        if self.parse_task.is_some() {
            return;
        }
        let Some((version, job)) = self.doc.read(cx).parse_job() else { return };
        self.parse_task = Some(cx.spawn(async move |this, cx| {
            let syntax = cx.background_executor().spawn(async move { job() }).await;
            this.update(cx, |editor, cx| {
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = editor.parse_task.take() {
                    task.detach();
                }
                if editor.doc.as_mut(cx).finish_parse(version, syntax) {
                    editor.schedule_parse(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn single_line(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(Document::empty(), cx);
        this.mode = EditorMode::SingleLine;
        this.soft_wrap = false;
        this.placeholder = placeholder.into();
        this
    }

    pub fn multi_line(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        let mut this = Self::single_line(placeholder, cx);
        this.mode = EditorMode::MultiLine;
        this.soft_wrap = true;
        this
    }

    pub fn is_single_line(&self) -> bool {
        self.mode == EditorMode::SingleLine
    }

    /// Text fields (single- or multi-line) have no gutter, linting or scrollbar.
    fn is_chromeless(&self) -> bool {
        self.mode != EditorMode::Full
    }

    fn submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_chromeless() {
            cx.propagate();
        } else {
            self.newline_below(&NewlineBelow, window, cx);
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.doc.read(cx).rope().to_string()
    }

    /// Replaces the whole text (as one undoable edit) and selects it.
    pub fn set_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let text = if self.is_single_line() { text.replace('\n', " ") } else { text.to_string() };
        let before = self.selections.clone();
        let len = self.doc.read(cx).len();
        self.doc.as_mut(cx).edit(vec![(0..len, text)], &before, EditKind::Other);
        let len = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor: 0, head: len }], 0, cx);
        self.after_edit(cx);
    }

    pub fn select_all_text(&mut self, cx: &mut Context<Self>) {
        let len = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor: 0, head: len }], 0, cx);
    }

    /// Selects a range and scrolls it to the middle of the view if needed.
    pub fn select_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let (s, e) = (self.doc.read(cx).clip(range.start), self.doc.read(cx).clip(range.end));
        self.set_selections(vec![Selection { anchor: s, head: e }], 0, cx);
        self.reveal_primary = true;
    }

    pub fn set_search_highlights(&mut self, matches: Vec<Range<usize>>, active: Option<usize>, cx: &mut Context<Self>) {
        self.search_matches = matches;
        self.active_match = active;
        cx.notify();
    }

    /// Replaces many ranges in one undoable step (used by Replace All).
    pub fn replace_ranges(&mut self, edits: Vec<(Range<usize>, String)>, cx: &mut Context<Self>) {
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    /// The project's ESLint, which runs its JavaScript config: only in a
    /// trusted project.
    pub fn detect_external_linter(&mut self, cx: &App) {
        self.external = match (&self.doc.read(cx).path, self.doc.read(cx).language) {
            (Some(path), _) if !crate::trust::is_trusted(path) => None,
            (
                Some(path),
                crate::syntax::Language::JavaScript | crate::syntax::Language::TypeScript | crate::syntax::Language::Tsx,
            ) => lint::external::find_for(path),
            _ => None,
        };
        self.external_error = None;
    }

    /// Name of the project linter in use and its last error, if any.
    pub fn external_linter_status(&self) -> Option<(&'static str, Option<&str>)> {
        self.external
            .as_ref()
            .map(|l| (l.name, self.external_error.as_deref()))
    }

    fn run_external_linter(&mut self, cx: &mut Context<Self>) {
        let (Some(linter), Some(path)) = (self.external.clone(), self.doc.read(cx).path.clone()) else {
            return;
        };
        let rope = self.doc.read(cx).rope().clone();
        let version = self.doc.read(cx).version();
        self.external_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { linter.run(&path, &rope) })
                .await;
            this.update(cx, |editor, cx| {
                let was_ok = editor.external_error.is_none();
                match result {
                    Ok(diagnostics) => {
                        editor.external_error = None;
                        if editor.doc.read(cx).version() == version {
                            editor.doc.as_mut(cx).set_external_diagnostics(diagnostics);
                        }
                    }
                    Err(err) => {
                        editor.external_error = Some(err);
                        editor.doc.as_mut(cx).set_external_diagnostics(Vec::new());
                    }
                }
                // Built-in rules fill in whenever the external linter isn't working.
                if was_ok != editor.external_error.is_none() {
                    editor.linted_version = None;
                    editor.schedule_lint(Duration::ZERO, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn selections(&self) -> &[Selection] {
        &self.selections
    }

    pub fn primary_selection(&self) -> Selection {
        self.selections[self.primary]
    }

    /// 1-based (line, column) of the primary cursor, column in characters.
    pub fn cursor_position(&self, cx: &App) -> (usize, usize) {
        let head = self.primary_selection().head;
        let row = self.doc.read(cx).row_of(head);
        let start = self.doc.read(cx).line_start(row);
        let col = self.doc.read(cx).slice(start..head).chars().count();
        (row + 1, col + 1)
    }

    pub fn selected_chars(&self, cx: &App) -> usize {
        self.selections
            .iter()
            .map(|s| {
                let r = self.doc.read(cx).rope();
                r.byte_to_char(s.range().end) - r.byte_to_char(s.range().start)
            })
            .sum()
    }

    pub fn indent_label(&self) -> String {
        if self.indent_unit == "\t" {
            "Tabs".into()
        } else {
            format!("Spaces: {}", self.indent_unit.len())
        }
    }

    /// Moves the cursor to a byte offset and scrolls it into view.
    pub fn go_to(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let offset = self.doc.read(cx).clip(offset);
        self.set_selections(vec![Selection::cursor(offset)], 0, cx);
        self.center_on_next_frame = true;
        window.focus(&self.focus_handle);
    }

    /// Sets the text that changes are measured against (HEAD) and the staged
    /// text, or clears them for files outside a repository or not yet committed.
    pub fn set_diff_base(&mut self, git_file: Option<GitFile>, base: Option<String>, index: Option<String>, cx: &mut Context<Self>) {
        if self.git_file != git_file {
            self.blame = None;
            self.blame_request = None;
        }
        self.git_file = git_file;
        self.diff_base = base.map(Arc::from);
        self.index_base = index.map(Arc::from);
        self.schedule_diff(Duration::ZERO, cx);
    }

    fn schedule_diff(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(base) = self.diff_base.clone() else {
            if !self.hunks.is_empty() {
                self.hunks = Arc::new(Vec::new());
                self.open_hunk = None;
                cx.notify();
            }
            return;
        };
        let rope = self.doc.read(cx).rope().clone();
        let version = self.doc.read(cx).version();
        let index = self.index_base.clone();
        self.diff_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let (hunks, staged) = cx
                .background_executor()
                .spawn(async move {
                    let text = rope.to_string();
                    let hunks = git::diff_hunks(&base, &text);
                    // A change is staged when nothing differs from the index there.
                    let unstaged = index.map(|index| git::diff_hunks(&index, &text));
                    let staged: Vec<bool> = hunks
                        .iter()
                        .map(|h| unstaged.as_ref().is_some_and(|u| !u.iter().any(|u| git::rows_touch(&u.new_rows, &h.new_rows))))
                        .collect();
                    (hunks, staged)
                })
                .await;
            this.update(cx, |editor, cx| {
                if editor.doc.read(cx).version() == version {
                    editor.hunks = Arc::new(hunks);
                    editor.hunk_staged = Arc::new(staged);
                    if editor.open_hunk.is_some_and(|i| i >= editor.hunks.len()) {
                        editor.open_hunk = None;
                    }
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Re-reads the file if it changed on disk and has no unsaved edits
    /// (e.g. after a checkout, pull or discard).
    pub fn reload_if_changed(&mut self, cx: &mut Context<Self>) {
        // An unloaded tab reads the file as it is when it's shown.
        if self.doc.read(cx).is_dirty() || !self.is_loaded() {
            return;
        }
        let reloaded = self.doc.as_mut(cx).reload_from_disk();
        if let Ok(true) = reloaded {
            self.after_reload(cx);
        }
    }

    /// Reads the file again as `encoding`; unsaved edits are dropped.
    pub fn reload_with_encoding(&mut self, encoding: crate::encoding::TextEncoding, cx: &mut Context<Self>) -> std::io::Result<()> {
        self.doc.as_mut(cx).reload_with_encoding(encoding)?;
        self.after_reload(cx);
        Ok(())
    }

    fn after_reload(&mut self, cx: &mut Context<Self>) {
        let selections = self.clamp_selections(self.selections.clone(), cx);
        self.set_selections(selections, 0, cx);
        self.autoscroll = false;
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
        self.schedule_diff(Duration::ZERO, cx);
        self.edited(cx);
        cx.notify();
    }

    /// Re-reads the settings and `.editorconfig` for this file: tab size,
    /// indentation, soft wrap and what saving does.
    pub fn apply_settings(&mut self, cx: &mut Context<Self>) {
        // Applied when an unloaded tab is read.
        if !self.is_loaded() {
            return;
        }
        let fs = Settings::get(cx).for_file(self.doc.read(cx).language, self.doc.read(cx).path.as_deref());
        // Settings reloads come for every editor; most files resolve the same.
        self.sync_vim(cx);
        if self.applied_settings.as_ref() == Some(&fs) {
            return;
        }
        self.applied_settings = Some(fs.clone());
        self.file_settings = fs.clone();
        self.tab_size = fs.tab_size;
        let configured = if fs.hard_tabs { "\t".to_string() } else { " ".repeat(fs.tab_size) };
        // A file's own indentation wins over settings, but not over .editorconfig.
        self.indent_unit = if fs.indent_is_explicit { configured } else { detect_indent(self.doc.read(cx)).unwrap_or(configured) };
        if self.mode == EditorMode::Full {
            // Wrapping a huge file means measuring every line.
            self.soft_wrap = fs.soft_wrap && !self.doc.read(cx).large;
        }
        self.wrap = None;
        self.schedule_inlay_hints(cx);
        cx.notify();
    }

    /// `trim_trailing_whitespace` and `insert_final_newline`. Lines with a
    /// caret and a caret at the very end are left alone, so auto-save never
    /// eats a space or moves the caret mid-typing.
    fn apply_save_actions(&mut self, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full {
            return;
        }
        let FileSettings { trim_trailing_whitespace, insert_final_newline, .. } = self.file_settings;
        let caret_rows: Vec<usize> = self.selections.iter().map(|s| self.doc.read(cx).row_of(s.head)).collect();
        let mut edits = Vec::new();
        if trim_trailing_whitespace {
            for row in 0..self.doc.read(cx).line_count() {
                if caret_rows.contains(&row) {
                    continue;
                }
                let text = self.doc.read(cx).line_text(row);
                let kept = text.trim_end_matches([' ', '\t']).len();
                if kept < text.len() {
                    let start = self.doc.read(cx).line_start(row);
                    edits.push((start + kept..start + text.len(), String::new()));
                }
            }
        }
        let len = self.doc.read(cx).len();
        if insert_final_newline
            && len > 0
            && self.doc.read(cx).rope().byte(len - 1) != b'\n'
            && !self.selections.iter().any(|s| s.head == len)
        {
            edits.push((len..len, "\n".to_string()));
        }
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    pub fn save(&mut self, cx: &mut Context<Self>) -> std::io::Result<()> {
        // A restored tab saved before it was shown (Save All).
        self.load(cx)?;
        self.apply_save_actions(cx);
        self.doc.as_mut(cx).save()?;
        self.lsp_did_save(cx);
        self.doc.as_mut(cx).finalize_transaction();
        self.schedule_lint(Duration::ZERO, cx);
        self.run_external_linter(cx);
        cx.notify();
        Ok(())
    }

    /// Extensions changed: the file may have a language now, or a grammar
    /// for it. True when it was parsed again.
    pub fn refresh_language(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(path) = self.doc.read(cx).path.clone().filter(|_| self.is_loaded()) else { return false };
        let language = crate::syntax::Language::from_path(&path);
        if language == self.doc.read(cx).language && language.extension().is_none() {
            return false;
        }
        self.doc.as_mut(cx).set_language(language);
        self.highlight_cache = None;
        self.apply_settings(cx);
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
        cx.notify();
        true
    }

    /// The project's trust changed: ESLint may run now, or must stop.
    pub fn trust_changed(&mut self, cx: &mut Context<Self>) {
        if !self.is_loaded() {
            return;
        }
        self.detect_external_linter(cx);
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
    }

    pub fn set_path(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) {
        // The language and settings come with the text.
        if !self.is_loaded() {
            self.doc.as_mut(cx).path = Some(path);
            return;
        }
        self.doc.as_mut(cx).set_path(path);
        self.apply_settings(cx);
        self.detect_external_linter(cx);
        self.linted_version = None;
        self.schedule_lint(Duration::ZERO, cx);
    }

    // ---- Selection management ----------------------------------------------

    fn set_selections(&mut self, mut selections: Vec<Selection>, primary: usize, cx: &mut Context<Self>) {
        let primary_sel = selections[primary.min(selections.len() - 1)];
        selections.sort_by_key(|s| s.range().start);
        // Merge overlapping selections.
        let mut merged: Vec<Selection> = Vec::with_capacity(selections.len());
        for sel in selections {
            if let Some(last) = merged.last_mut() {
                let lr = last.range();
                let sr = sel.range();
                if sr.start < lr.end || (sr.start == lr.end && (sel.is_empty() && last.is_empty())) {
                    let start = lr.start.min(sr.start);
                    let end = lr.end.max(sr.end);
                    *last = if last.reversed() {
                        Selection { anchor: end, head: start }
                    } else {
                        Selection { anchor: start, head: end }
                    };
                    continue;
                }
            }
            merged.push(sel);
        }
        self.primary = merged
            .iter()
            .position(|s| s.range().start <= primary_sel.range().start && primary_sel.range().end <= s.range().end)
            .unwrap_or(merged.len() - 1);
        self.selections = merged;
        if !std::mem::take(&mut self.expanding) {
            self.expand_stack.clear();
        }
        self.marked_range = None;
        self.autoscroll = true;
        self.cursor_visible = true;
        self.blink_epoch += 1;
        self.blink_task = None;
        self.unfold_at_carets(cx);
        self.schedule_symbol_highlights(cx);
        self.lsp_selection_changed(cx);
        cx.notify();
    }

    fn move_selections(
        &mut self,
        extend: bool,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&Self, &Selection, &App) -> usize,
    ) {
        let selections = self
            .selections
            .iter()
            .map(|s| {
                let head = f(self, s, cx);
                if extend {
                    Selection { anchor: s.anchor, head }
                } else {
                    Selection::cursor(head)
                }
            })
            .collect();
        self.goal_x = None;
        self.set_selections(selections, self.primary, cx);
    }

    // ---- Editing -----------------------------------------------------------

    /// Applies one edit per selection. The closure returns the replaced range,
    /// the new text, and where the cursor lands within the new text (`None`
    /// selects the whole inserted text).
    fn edit_selections(
        &mut self,
        kind: EditKind,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&Document, &Selection) -> (Range<usize>, String, Option<usize>),
    ) {
        let before = self.selections.clone();
        let mut edits = Vec::with_capacity(before.len());
        let mut placements = Vec::with_capacity(before.len());
        for sel in &before {
            let (range, text, cursor) = f(self.doc.read(cx), sel);
            placements.push(cursor);
            edits.push((range, text));
        }
        let new_ranges = self.doc.as_mut(cx).edit(edits, &before, kind);
        let selections = new_ranges
            .into_iter()
            .zip(placements)
            .map(|(r, place)| match place {
                Some(off) => Selection::cursor(r.start + off),
                None => Selection { anchor: r.start, head: r.end },
            })
            .collect();
        self.goal_x = None;
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    /// Applies arbitrary edits and then remaps all selections through them.
    fn edit_and_remap(
        &mut self,
        mut edits: Vec<(Range<usize>, String)>,
        kind: EditKind,
        cx: &mut Context<Self>,
    ) {
        edits.sort_by_key(|e| e.0.start);
        edits.dedup_by(|a, b| a.0 == b.0);
        let map = |offset: usize, edits: &[(Range<usize>, String)]| {
            let mut delta: isize = 0;
            for (r, t) in edits {
                if r.start > offset {
                    break;
                }
                if r.is_empty() || r.end <= offset {
                    // Insertions at or before the offset, and replacements
                    // entirely before it, shift it.
                    delta += t.len() as isize - r.len() as isize;
                } else {
                    // Offset inside a replaced range: clamp into the new text.
                    let start = (r.start as isize + delta) as usize;
                    return start + t.len().min(offset - r.start);
                }
            }
            (offset as isize + delta).max(0) as usize
        };
        let before = self.selections.clone();
        let selections: Vec<Selection> = before
            .iter()
            .map(|s| Selection { anchor: map(s.anchor, &edits), head: map(s.head, &edits) })
            .collect();
        self.doc.as_mut(cx).edit(edits, &before, kind);
        self.goal_x = None;
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    fn after_edit(&mut self, cx: &mut Context<Self>) {
        self.doc.as_mut(cx).set_selections_after(&self.selections);
        self.schedule_inlay_hints(cx);
        self.schedule_lint(Duration::from_millis(150), cx);
        self.schedule_diff(Duration::from_millis(120), cx);
        self.edited(cx);
        cx.notify();
    }

    fn insert(&mut self, text: &str, cx: &mut Context<Self>) {
        let kind = if text.contains(char::is_whitespace) || text.len() > 1 {
            EditKind::Other
        } else {
            EditKind::Typing
        };
        let typed = text.to_string();
        self.edit_selections(kind, cx, |doc, sel| {
            let range = sel.range();
            let mut chars = typed.chars();
            let single = match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            };
            if let Some(c) = single {
                let next = doc.char_at(range.end);
                // Type over an auto-inserted closing character.
                if sel.is_empty() && matches!(c, ')' | ']' | '}' | '"' | '\'' | '`') && next == Some(c) {
                    return (range.end..range.end + 1, c.to_string(), Some(1));
                }
                if let Some(close) = closing_pair(c) {
                    if !sel.is_empty() {
                        // Wrap the selection.
                        let inner = doc.slice(range.clone());
                        let len = inner.len();
                        return (range, format!("{c}{inner}{close}"), Some(len + 2));
                    }
                    let prev = doc.char_before(range.start);
                    let next_ok = next.is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';' | '>'));
                    let is_quote = matches!(c, '"' | '\'' | '`');
                    let prev_ok = !is_quote || prev.is_none_or(|p| !p.is_alphanumeric() && p != c);
                    if next_ok && prev_ok {
                        return (range, format!("{c}{close}"), Some(1));
                    }
                }
            }
            let len = typed.len();
            (range, typed.clone(), Some(len))
        });
        self.after_typing(text, cx);
    }

    fn schedule_lint(&mut self, delay: Duration, cx: &mut Context<Self>) {
        if !self.doc.read(cx).language.is_lintable() {
            self.doc.as_mut(cx).set_builtin_diagnostics(Vec::new());
            return;
        }
        let syntax_only = self.external.is_some() && self.external_error.is_none();
        let version = self.doc.read(cx).version();
        if self.linted_version == Some(version) {
            return;
        }
        let rope = self.doc.read(cx).rope().clone();
        let language = self.doc.read(cx).language;
        let path = self.doc.read(cx).path.clone();
        self.lint_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let diagnostics = cx
                .background_executor()
                .spawn(async move { lint::lint(language, &rope.to_string(), path.as_deref(), syntax_only) })
                .await;
            this.update(cx, |editor, cx| {
                if editor.doc.read(cx).version() == version {
                    let mut diagnostics = diagnostics;
                    // The language server reports syntax errors itself.
                    if editor.lsp.active {
                        diagnostics.retain(|d| d.rule != "syntax");
                    }
                    editor.doc.as_mut(cx).set_builtin_diagnostics(diagnostics);
                    editor.linted_version = Some(version);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn restart_blink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let epoch = self.blink_epoch;
        self.blink_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(530)).await;
                let keep_going = this
                    .update_in(cx, |editor, window, cx| {
                        if editor.blink_epoch != epoch {
                            return false;
                        }
                        if editor.focus_handle.is_focused(window) && window.is_window_active() {
                            editor.cursor_visible = !editor.cursor_visible;
                            cx.notify();
                        } else if !editor.cursor_visible {
                            editor.cursor_visible = true;
                            cx.notify();
                        }
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        }));
    }

    // ---- Geometry ---------------------------------------------------------

    fn editor_font(&self, cx: &App) -> Font {
        let theme = Theme::get(cx);
        match self.mode {
            EditorMode::Full => font(theme.mono_font.clone()),
            EditorMode::SingleLine | EditorMode::MultiLine => font(theme.ui_font.clone()),
        }
    }

    fn font_size(&self, cx: &App) -> Pixels {
        let theme = Theme::get(cx);
        match self.mode {
            EditorMode::Full => px(theme.editor_font_size),
            EditorMode::SingleLine | EditorMode::MultiLine => px(theme.ui_font_size + 0.5),
        }
    }

    fn line_height(&self, cx: &App) -> Pixels {
        match self.mode {
            EditorMode::Full => px(Theme::get(cx).editor_line_height),
            EditorMode::SingleLine | EditorMode::MultiLine => px(20.),
        }
    }

    /// `row` as displayed: tabs expanded, inlay hints in place. Wrapped lines
    /// get no hints (the wrap map is measured without them).
    fn display_line(&self, row: usize, cx: &App) -> DisplayLine {
        let text = self.doc.read(cx).line_text(row);
        let hints = self.doc.read(cx).inlay_hints();
        if !self.file_settings.inlay_hints || hints.is_empty() || !self.line_breaks(row).is_empty() {
            return DisplayLine::new(&text, self.tab_size);
        }
        let start = self.doc.read(cx).line_start(row);
        let end = start + text.len();
        let first = hints.partition_point(|h| h.offset < start);
        let inlays: Vec<(usize, &str, bool, bool)> = hints[first..]
            .iter()
            .take_while(|h| h.offset <= end)
            .map(|h| (h.offset - start, h.label.as_str(), h.pad_left, h.pad_right))
            .collect();
        DisplayLine::with_inlays(&text, self.tab_size, &inlays)
    }

    /// Shapes the part of `row` shown on display segment `seg`.
    fn shape_segment(&self, row: usize, seg: usize, window: &mut Window, cx: &App) -> RowShape {
        let theme = Theme::get(cx);
        let dl = self.display_line(row, cx);
        let breaks = self.line_breaks(row);
        let (range, indent) = wrap::segment(breaks, seg.min(breaks.len()), dl.text.len());
        let last = seg >= breaks.len();
        let text = &dl.text[range.clone()];
        let run = TextRun {
            len: text.len(),
            font: self.editor_font(cx),
            color: theme.text.into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if text.is_empty() { vec![] } else { vec![run] };
        let line = window
            .text_system()
            .shape_line(SharedString::from(text.to_string()), self.font_size(cx), &runs, None);
        let indent = self.char_width() * indent as f32;
        RowShape { line, dl, range, indent, last }
    }

    fn char_width(&self) -> Pixels {
        self.layout.as_ref().map_or(px(8.), |l| l.char_width)
    }

    /// The x of `offset` within its display row (wrap indent included).
    fn x_for_offset(&self, offset: usize, window: &mut Window, cx: &App) -> Pixels {
        let row = self.doc.read(cx).row_of(offset);
        let col = self.display_line(row, cx).to_display(offset - self.doc.read(cx).line_start(row));
        let seg = wrap::segment_of(self.line_breaks(row), col);
        let shape = self.shape_segment(row, seg, window, cx);
        shape.indent + shape.line.x_for_index(col - shape.range.start)
    }

    fn offset_for_display_row_x(&self, drow: usize, x: Pixels, window: &mut Window, cx: &App) -> usize {
        let (row, seg) = self.line_of_display_row(drow, cx);
        let shape = self.shape_segment(row, seg, window, cx);
        let mut col = shape.range.start + shape.line.closest_index_for_x((x - shape.indent).max(px(0.)));
        // The end of a wrapped row is the start of the next one; stay on this row.
        if !shape.last && col >= shape.range.end && shape.range.end > shape.range.start {
            col = shape.dl.text[..shape.range.end].char_indices().last().map_or(col, |(i, _)| i);
        }
        self.doc.read(cx).line_start(row) + shape.dl.to_buffer(col)
    }

    fn offset_for_position(&self, position: Point<Pixels>, window: &mut Window, cx: &App) -> usize {
        let Some(layout) = self.layout.clone() else {
            return 0;
        };
        let y = position.y - layout.bounds.top() + self.scroll.y;
        if y < px(0.) {
            return 0;
        }
        let drow = (y / layout.line_height).floor() as usize;
        if drow >= self.display_row_count(cx) {
            return self.doc.read(cx).len();
        }
        let x = position.x - layout.text_left + self.scroll.x;
        self.offset_for_display_row_x(drow, x.max(px(0.)), window, cx)
    }

    fn vertical_target(&self, sel: &Selection, rows: isize, goal: Pixels, window: &mut Window, cx: &App) -> usize {
        let drow = self.display_row_of(sel.head, cx) as isize + rows;
        if drow < 0 {
            0
        } else if drow as usize >= self.display_row_count(cx) {
            self.doc.read(cx).len()
        } else {
            self.offset_for_display_row_x(drow as usize, goal, window, cx)
        }
    }

    // ---- Soft wrap -------------------------------------------------------------

    fn toggle_markdown_preview(&mut self, _: &ToggleMarkdownPreview, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode == EditorMode::Full && self.doc.read(cx).language == crate::syntax::Language::Markdown {
            self.md_preview = !self.md_preview;
            cx.notify();
        }
    }

    /// The preview beside a Markdown file, parsed again when the text changes.
    fn render_markdown_preview(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let path = self.doc.read(cx).path.clone();
        let rope = self.doc.read(cx).rope().clone();
        let job = move || crate::markdown_preview::parse(&rope.to_string(), path.as_deref().and_then(std::path::Path::parent));
        self.md_blocks.schedule((self.doc.read(cx).version(), self.doc.read(cx).path.clone()), job, |e| &mut e.md_blocks, cx);
        let blocks = self.md_blocks.last();
        let theme = Theme::get(cx).clone();
        let zoom = self.md_zoom;
        let step = |id: &'static str, icon: crate::assets::Icon, delta: f32, cx: &mut Context<Self>| {
            div()
                .id(id)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .on_click(cx.listener(move |e, _, _, cx| e.zoom_preview(delta, cx)))
                .child(crate::ui::icon_el(icon, px(12.), theme.text_muted.into()))
        };
        // − 100% + at the top right; the percentage resets.
        let zoom_bar = div()
            .absolute()
            .top(px(6.))
            .right(px(10.))
            .flex()
            .items_center()
            .p(px(2.))
            .rounded(px(7.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.elevated)
            .text_size(px(11.5))
            .text_color(theme.text_muted)
            .child(step("md-zoom-out", crate::assets::Icon::Minus, -0.1, cx))
            .child(
                div()
                    .id("md-zoom-reset")
                    .min_w(px(40.))
                    .flex()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .tooltip(|_, cx| crate::ui::tooltip_view("Reset Zoom", cx))
                    .on_click(cx.listener(|e, _, _, cx| e.zoom_preview(1. - e.md_zoom, cx)))
                    .child(format!("{}%", (zoom * 100.).round())),
            )
            .child(step("md-zoom-in", crate::assets::Icon::Plus, 0.1, cx));
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .id("markdown-preview")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.md_scroll)
                    .bg(theme.background)
                    .px_6()
                    .py_5()
                    // ⌘/Ctrl + wheel zooms, like a browser.
                    .on_scroll_wheel(cx.listener(|e, event: &gpui::ScrollWheelEvent, _, cx| {
                        if event.modifiers.secondary() {
                            cx.stop_propagation();
                            e.md_zoom_rest += f32::from(event.delta.pixel_delta(px(20.)).y);
                            // About one wheel notch per step.
                            while e.md_zoom_rest.abs() >= 20. {
                                let up = e.md_zoom_rest > 0.;
                                e.md_zoom_rest -= if up { 20. } else { -20. };
                                e.zoom_preview(if up { 0.1 } else { -0.1 }, cx);
                            }
                        }
                    }))
                    .child(crate::markdown_preview::render(&blocks, crate::markdown_preview::Draw { zoom }, cx)),
            )
            .child(zoom_bar)
    }

    fn zoom_preview(&mut self, delta: f32, cx: &mut Context<Self>) {
        let zoom = ((self.md_zoom + delta) * 10.).round() / 10.;
        self.md_zoom = zoom.clamp(0.5, 3.);
        cx.notify();
    }

    /// For this editor, until settings change (like Toggle Soft Wrap).
    fn toggle_inlay_hints(&mut self, _: &ToggleInlayHints, _: &mut Window, cx: &mut Context<Self>) {
        self.file_settings.inlay_hints = !self.file_settings.inlay_hints;
        self.schedule_inlay_hints(cx);
        cx.notify();
    }

    fn toggle_soft_wrap(&mut self, _: &ToggleSoftWrap, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_chromeless() {
            return;
        }
        self.soft_wrap = !self.soft_wrap;
        self.wrap = None;
        self.scroll.x = px(0.);
        self.reveal_primary = true;
        cx.notify();
    }

    /// Recomputes line breaks when the text, width or font changed. Called
    /// before anything maps between offsets and display rows.
    fn sync_wrap(&mut self, window: &mut Window, cx: &App) {
        let wrap = self.soft_wrap && !self.is_single_line();
        // Without wrapping, folds or long lines, display rows are buffer lines.
        if !wrap && self.doc.read(cx).folds().is_empty() && !self.doc.read(cx).has_long_lines() {
            self.wrap = None;
            return;
        }
        let Some(layout) = self.layout.as_ref() else { return };
        let font = self.editor_font(cx);
        let key = WrapKey {
            width: layout.wrap_width,
            font_family: font.family.clone(),
            font_size: self.font_size(cx),
            tab_size: self.tab_size,
            wrap,
        };
        let (char_width, version, folds, rope) = (layout.char_width, self.doc.read(cx).version(), self.doc.read(cx).folds(), self.doc.read(cx).rope());
        // Edits re-measure only their lines; a new width, font or tab size
        // measures them all.
        if let Some(map) = self.wrap.as_mut().filter(|map| map.key == key) {
            let edits = self.doc.read(cx).line_edits_since(map.version);
            let caught_up = match edits {
                Some(edits) => map.update(edits, version, folds, rope, window.text_system(), font.clone(), char_width),
                None => false,
            };
            if caught_up {
                return;
            }
        }
        self.wrap = Some(WrapMap::new(key, version, folds, rope, window.text_system(), font, char_width));
    }

    /// Both brackets of the pairs around carets (a few, so many cursors stay
    /// cheap). Cached: in a big array or object finding a partner is a walk,
    /// and a caret blink draws again.
    fn bracket_pairs(&mut self, cx: &App) -> Arc<Vec<Range<usize>>> {
        let heads: Vec<usize> = self.selections.iter().filter(|s| s.is_empty()).take(8).map(|s| s.head).collect();
        let key = (self.doc.read(cx).tree_version(), heads);
        if let Some((k, pairs)) = &self.bracket_cache
            && *k == key
        {
            return pairs.clone();
        }
        let syntax = self.doc.read(cx).syntax();
        let pairs: Arc<Vec<_>> =
            Arc::new(key.1.iter().filter_map(|&head| syntax.bracket_pair(self.doc.read(cx).rope(), head)).flat_map(|(a, b)| [a, b]).collect());
        self.bracket_cache = Some((key, pairs.clone()));
        pairs
    }

    /// Highlights for display rows `first_drow..last_drow` (buffer lines
    /// `first_row..last_row`): folded lines and a long line's rows off
    /// screen are left out. Cached, since a caret blink draws again.
    fn visible_highlights(&mut self, first_drow: usize, last_drow: usize, first_row: usize, last_row: usize, cx: &App) -> Arc<Vec<(Range<usize>, crate::syntax::Highlight)>> {
        let mut ranges: Vec<Range<usize>> = Vec::new();
        for row in (first_row..last_row).filter(|&row| !self.row_hidden(row)) {
            let (start, end) = (self.doc.read(cx).line_start(row), self.doc.read(cx).line_end(row));
            match ranges.last_mut() {
                Some(r) if r.end + 1 >= start => r.end = end,
                _ => ranges.push(start..end),
            }
        }
        let (row, seg) = self.line_of_display_row(first_drow, cx);
        if let (Some(r), Some(b)) = (ranges.first_mut(), seg.checked_sub(1).and_then(|s| self.line_breaks(row).get(s))) {
            r.start = (self.doc.read(cx).line_start(row) + b.byte as usize).min(r.end);
        }
        let (row, seg) = self.line_of_display_row(last_drow.saturating_sub(1), cx);
        if let (Some(r), Some(b)) = (ranges.last_mut(), self.line_breaks(row).get(seg)) {
            r.end = (self.doc.read(cx).line_start(row) + b.byte as usize).max(r.start);
        }
        let key = (self.doc.read(cx).tree_version(), self.doc.read(cx).language, ranges);
        if let Some((k, highlights)) = &self.highlight_cache
            && *k == key
        {
            return highlights.clone();
        }
        let syntax = self.doc.read(cx).syntax();
        let highlights: Arc<Vec<_>> = Arc::new(key.2.iter().flat_map(|r| syntax.highlights(self.doc.read(cx).rope(), r.clone())).collect());
        self.highlight_cache = Some((key, highlights.clone()));
        highlights
    }

    fn display_row_count(&self, cx: &App) -> usize {
        self.wrap.as_ref().map_or(self.doc.read(cx).line_count(), WrapMap::row_count)
    }

    fn first_display_row(&self, row: usize) -> usize {
        self.wrap.as_ref().map_or(row, |w| w.first_display_row(row))
    }

    /// The buffer line and its wrap segment on display row `drow`.
    fn line_of_display_row(&self, drow: usize, cx: &App) -> (usize, usize) {
        match &self.wrap {
            Some(w) => w.line_of(drow),
            None => (drow.min(self.doc.read(cx).line_count().saturating_sub(1)), 0),
        }
    }

    fn line_breaks(&self, row: usize) -> &[Break] {
        self.wrap.as_ref().map_or(&[], |w| w.breaks(row))
    }

    /// The display row showing `offset` (on a wrap break, the later row).
    fn display_row_of(&self, offset: usize, cx: &App) -> usize {
        let row = self.doc.read(cx).row_of(offset);
        let breaks = self.line_breaks(row);
        if breaks.is_empty() {
            return self.first_display_row(row);
        }
        let col = DisplayLine::new(&self.doc.read(cx).line_text(row), self.tab_size).to_display(offset - self.doc.read(cx).line_start(row));
        self.first_display_row(row) + wrap::segment_of(breaks, col)
    }

    fn move_vertically(&mut self, rows: isize, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_wrap(window, cx);
        let goals: Vec<Pixels> = self
            .selections
            .iter()
            .map(|s| match self.goal_x {
                Some(g) if self.selections.len() == 1 => g,
                _ => self.x_for_offset(s.head, window, cx),
            })
            .collect();
        let selections: Vec<Selection> = self
            .selections
            .iter()
            .zip(&goals)
            .map(|(s, &goal)| {
                let start = if !extend && !s.is_empty() {
                    let r = s.range();
                    let at = if rows < 0 { r.start } else { r.end };
                    Selection::cursor(at)
                } else {
                    *s
                };
                let head = self.vertical_target(&start, rows, goal, window, cx);
                if extend {
                    Selection { anchor: s.anchor, head }
                } else {
                    Selection::cursor(head)
                }
            })
            .collect();
        let goal = goals[self.primary];
        self.set_selections(selections, self.primary, cx);
        self.goal_x = Some(goal);
    }

    // ---- Actions -------------------------------------------------------------

    fn move_left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| {
            if s.is_empty() { this.doc.read(cx).prev_char_boundary(s.head) } else { s.range().start }
        });
    }

    fn move_right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| {
            if s.is_empty() { this.doc.read(cx).next_char_boundary(s.head) } else { s.range().end }
        });
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.doc.read(cx).prev_char_boundary(s.head));
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.doc.read(cx).next_char_boundary(s.head));
    }

    fn move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Move(-1), cx) {
            return;
        }
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.move_vertically(-1, false, window, cx);
    }

    fn move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Move(1), cx) {
            return;
        }
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.move_vertically(1, false, window, cx);
    }

    fn select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(-1, true, window, cx);
    }

    fn select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertically(1, true, window, cx);
    }

    fn page_up(&mut self, _: &PageUp, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        let rows = self.layout.as_ref().map_or(20, |l| l.rows_per_page) as isize;
        if let Some(l) = &self.layout {
            self.scroll.y = (self.scroll.y - l.line_height * rows as f32).max(px(0.));
        }
        self.move_vertically(-rows, false, window, cx);
    }

    fn page_down(&mut self, _: &PageDown, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        let rows = self.layout.as_ref().map_or(20, |l| l.rows_per_page) as isize;
        if let Some(l) = &self.layout {
            self.scroll.y += l.line_height * rows as f32;
        }
        self.move_vertically(rows, false, window, cx);
    }

    fn move_word_left(&mut self, _: &MoveWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| word_left(this.doc.read(cx), s.head));
    }

    fn move_word_right(&mut self, _: &MoveWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| word_right(this.doc.read(cx), s.head));
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| word_left(this.doc.read(cx), s.head));
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| word_right(this.doc.read(cx), s.head));
    }

    fn smart_home(&self, offset: usize, cx: &App) -> usize {
        let row = self.doc.read(cx).row_of(offset);
        let start = self.doc.read(cx).line_start(row);
        let text = self.doc.read(cx).line_text(row);
        let indent = text.len() - text.trim_start().len();
        if offset == start + indent { start } else { start + indent }
    }

    fn move_line_start(&mut self, _: &MoveLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| this.smart_home(s.head, cx));
    }

    fn move_line_end(&mut self, _: &MoveLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(false, cx, |this, s, cx| this.doc.read(cx).line_end(this.doc.read(cx).row_of(s.head)));
    }

    fn select_line_start(&mut self, _: &SelectLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.smart_home(s.head, cx));
    }

    fn select_line_end(&mut self, _: &SelectLineEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selections(true, cx, |this, s, cx| this.doc.read(cx).line_end(this.doc.read(cx).row_of(s.head)));
    }

    fn move_doc_start(&mut self, _: &MoveDocStart, _: &mut Window, cx: &mut Context<Self>) {
        self.set_selections(vec![Selection::cursor(0)], 0, cx);
    }

    fn move_doc_end(&mut self, _: &MoveDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.set_selections(vec![Selection::cursor(self.doc.read(cx).len())], 0, cx);
    }

    fn select_doc_start(&mut self, _: &SelectDocStart, _: &mut Window, cx: &mut Context<Self>) {
        let anchor = self.primary_selection().anchor;
        self.set_selections(vec![Selection { anchor, head: 0 }], 0, cx);
    }

    fn select_doc_end(&mut self, _: &SelectDocEnd, _: &mut Window, cx: &mut Context<Self>) {
        let anchor = self.primary_selection().anchor;
        let head = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor, head }], 0, cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let len = self.doc.read(cx).len();
        self.set_selections(vec![Selection { anchor: 0, head: len }], 0, cx);
        self.autoscroll = false;
    }

    fn select_line(&mut self, _: &SelectLine, _: &mut Window, cx: &mut Context<Self>) {
        let selections = self
            .selections
            .iter()
            .map(|s| {
                let r = s.range();
                let start = self.doc.read(cx).line_start(self.doc.read(cx).row_of(r.start));
                let end_row = self.doc.read(cx).row_of(r.end);
                let end = (self.doc.read(cx).line_end(end_row) + 1).min(self.doc.read(cx).len());
                Selection { anchor: start, head: end }
            })
            .collect();
        self.set_selections(selections, self.primary, cx);
    }

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let unit = self.indent_unit.clone();
        self.edit_selections(EditKind::Deleting, cx, |doc, s| {
            if !s.is_empty() {
                return (s.range(), String::new(), Some(0));
            }
            let head = s.head;
            let row_start = doc.line_start(doc.row_of(head));
            let before = doc.slice(row_start..head);
            // Delete a whole indent level when in leading whitespace.
            if !before.is_empty() && unit != "\t" && before.chars().all(|c| c == ' ') {
                let n = (before.len() - 1) % unit.len() + 1;
                return (head - n..head, String::new(), Some(0));
            }
            // Delete an auto-inserted pair together.
            let prev = doc.char_before(head);
            if let (Some(p), Some(n)) = (prev, doc.char_at(head))
                && closing_pair(p) == Some(n)
            {
                return (head - p.len_utf8()..head + n.len_utf8(), String::new(), Some(0));
            }
            (doc.prev_char_boundary(head)..head, String::new(), Some(0))
        });
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Deleting, cx, |doc, s| {
            let r = if s.is_empty() { s.head..doc.next_char_boundary(s.head) } else { s.range() };
            (r, String::new(), Some(0))
        });
    }

    fn delete_word_left(&mut self, _: &DeleteWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = if s.is_empty() { word_left(doc, s.head)..s.head } else { s.range() };
            (r, String::new(), Some(0))
        });
    }

    fn delete_word_right(&mut self, _: &DeleteWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = if s.is_empty() { s.head..word_right(doc, s.head) } else { s.range() };
            (r, String::new(), Some(0))
        });
    }

    fn delete_to_line_start(&mut self, _: &DeleteToLineStart, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = if s.is_empty() {
                let start = doc.line_start(doc.row_of(s.head));
                if start == s.head { doc.prev_char_boundary(s.head)..s.head } else { start..s.head }
            } else {
                s.range()
            };
            (r, String::new(), Some(0))
        });
    }

    fn newline(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Accept { replace: false }, cx) {
            return;
        }
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        let unit = self.indent_unit.clone();
        let is_html = self.doc.read(cx).language == crate::syntax::Language::Html;
        // Python blocks and YAML mappings open with a trailing `:`.
        let colon_opens = matches!(
            self.doc.read(cx).language,
            crate::syntax::Language::Python | crate::syntax::Language::Yaml
        );
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let r = s.range();
            let row = doc.row_of(r.start);
            let line = doc.line_text(row);
            let line_start = doc.line_start(row);
            let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let before = doc.slice(line_start..r.start);
            let before_trim = before.trim_end();
            let prev = before_trim.chars().last();
            let next = doc.char_at(r.end);
            let opens_html_tag = is_html && prev == Some('>') && !before_trim.ends_with("/>") && {
                let tag_start = before_trim.rfind('<');
                tag_start.is_some_and(|i| {
                    let tag = &before_trim[i..];
                    !tag.starts_with("</") && !tag.starts_with("<!")
                })
            };
            // `def f():`, `if x:`, `steps:`
            let opens_colon_block = colon_opens && prev == Some(':') && !before.trim_start().starts_with('#');
            // An extension's indents.scm: an @indent node opens on this line.
            let opens_by_query = doc.syntax().indent_after(doc.rope(), r.start, row) == Some(true);
            let opens = matches!(prev, Some('{' | '[' | '(')) || opens_html_tag || opens_colon_block || opens_by_query;
            let closes_next = match prev {
                Some('{') => next == Some('}'),
                Some('[') => next == Some(']'),
                Some('(') => next == Some(')'),
                Some('>') => opens_html_tag && doc.slice(r.end..(r.end + 2).min(doc.len())) == "</",
                _ => false,
            };
            if opens && closes_next {
                let inner = format!("\n{indent}{unit}");
                let text = format!("{inner}\n{indent}");
                (r, text, Some(inner.len()))
            } else if opens {
                let text = format!("\n{indent}{unit}");
                let len = text.len();
                (r, text, Some(len))
            } else {
                let text = format!("\n{indent}");
                let len = text.len();
                (r, text, Some(len))
            }
        });
    }

    fn newline_below(&mut self, _: &NewlineBelow, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.move_selections(false, cx, |this, s, cx| this.doc.read(cx).line_end(this.doc.read(cx).row_of(s.head)));
        self.newline(&Newline, window, cx);
    }

    /// Rows covered by selections, as merged row ranges.
    fn selected_row_blocks(&self, cx: &App) -> Vec<Range<usize>> {
        let mut blocks: Vec<Range<usize>> = Vec::new();
        for s in &self.selections {
            let r = s.range();
            let start = self.doc.read(cx).row_of(r.start);
            let mut end = self.doc.read(cx).row_of(r.end);
            // A selection ending at column 0 doesn't include that row.
            if end > start && self.doc.read(cx).line_start(end) == r.end {
                end -= 1;
            }
            match blocks.last_mut() {
                Some(last) if start <= last.end => last.end = last.end.max(end + 1),
                _ => blocks.push(start..end + 1),
            }
        }
        blocks
    }

    fn indent(&mut self, _: &Indent, _: &mut Window, cx: &mut Context<Self>) {
        if self.completion_key(lsp::CompletionKey::Accept { replace: true }, cx) {
            return;
        }
        if self.is_chromeless() {
            cx.propagate();
            return;
        }
        if self.snippet_tab(1, cx) || self.expand_user_snippet(cx) {
            return;
        }
        if self.selections.iter().all(|s| s.is_empty()) {
            let unit = self.indent_unit.clone();
            let tab_size = self.tab_size;
            self.edit_selections(EditKind::Other, cx, |doc, s| {
                let text = if unit == "\t" {
                    "\t".to_string()
                } else {
                    let row_start = doc.line_start(doc.row_of(s.head));
                    let col = DisplayLine::new(&doc.slice(row_start..s.head), tab_size).text.chars().count();
                    " ".repeat(unit.len() - col % unit.len())
                };
                let len = text.len();
                (s.range(), text, Some(len))
            });
            return;
        }
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            for row in block {
                if !self.doc.read(cx).line_text(row).trim().is_empty() {
                    let at = self.doc.read(cx).line_start(row);
                    edits.push((at..at, self.indent_unit.clone()));
                }
            }
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn outdent(&mut self, _: &Outdent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_chromeless() {
            cx.propagate();
            return;
        }
        if self.snippet_tab(-1, cx) {
            return;
        }
        let mut edits = Vec::new();
        let width = if self.indent_unit == "\t" { self.tab_size } else { self.indent_unit.len() };
        for block in self.selected_row_blocks(cx) {
            for row in block {
                let text = self.doc.read(cx).line_text(row);
                let at = self.doc.read(cx).line_start(row);
                let n = if text.starts_with('\t') {
                    1
                } else {
                    text.chars().take(width).take_while(|c| *c == ' ').count()
                };
                if n > 0 {
                    edits.push((at..at + n, String::new()));
                }
            }
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn toggle_comment(&mut self, _: &ToggleComment, _: &mut Window, cx: &mut Context<Self>) {
        let language = self.doc.read(cx).language;
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            let rows: Vec<usize> = block.clone().filter(|&r| !self.doc.read(cx).line_text(r).trim().is_empty()).collect();
            if rows.is_empty() {
                continue;
            }
            match language.line_comment() {
                Some(prefix) => {
                    let token = prefix.trim_end();
                    let all_commented = rows.iter().all(|&r| self.doc.read(cx).line_text(r).trim_start().starts_with(token));
                    let min_indent = rows
                        .iter()
                        .map(|&r| {
                            let t = self.doc.read(cx).line_text(r);
                            t.len() - t.trim_start().len()
                        })
                        .min()
                        .unwrap_or(0);
                    for &r in &rows {
                        let t = self.doc.read(cx).line_text(r);
                        let start = self.doc.read(cx).line_start(r);
                        if all_commented {
                            let indent = t.len() - t.trim_start().len();
                            let len = if t[indent..].starts_with(prefix) { prefix.len() } else { token.len() };
                            edits.push((start + indent..start + indent + len, String::new()));
                        } else {
                            edits.push((start + min_indent..start + min_indent, prefix.to_string()));
                        }
                    }
                }
                // No line comments (HTML, CSS, Markdown): wrap each line in a block comment.
                None => {
                    let Some((open, close)) = language.block_comment() else {
                        continue;
                    };
                    let (open_token, close_token) = (open.trim_end(), close.trim_start());
                    let all_commented = rows.iter().all(|&r| {
                        let t = self.doc.read(cx).line_text(r);
                        let t = t.trim();
                        t.len() >= open_token.len() + close_token.len()
                            && t.starts_with(open_token)
                            && t.ends_with(close_token)
                    });
                    for &r in &rows {
                        let t = self.doc.read(cx).line_text(r);
                        let start = self.doc.read(cx).line_start(r);
                        let indent = t.len() - t.trim_start().len();
                        let end = start + t.trim_end().len();
                        if all_commented {
                            let body = t.trim();
                            let mut open_len = if body.starts_with(open) { open.len() } else { open_token.len() };
                            let mut close_len = if body.ends_with(close) { close.len() } else { close_token.len() };
                            // `/* */`: the padding spaces overlap, so remove only the tokens.
                            if open_len + close_len > body.len() {
                                (open_len, close_len) = (open_token.len(), close_token.len());
                            }
                            edits.push((end - close_len..end, String::new()));
                            edits.push((start + indent..start + indent + open_len, String::new()));
                        } else {
                            edits.push((end..end, close.into()));
                            edits.push((start + indent..start + indent, open.into()));
                        }
                    }
                }
            }
        }
        if !edits.is_empty() {
            self.edit_and_remap(edits, EditKind::Other, cx);
        }
    }

    fn duplicate_line(&mut self, _: &DuplicateLine, _: &mut Window, cx: &mut Context<Self>) {
        // Each block's text is inserted after it; cursors move onto the copy.
        let blocks = self.selected_row_blocks(cx);
        let mut edits = Vec::new();
        let mut shifts: Vec<(usize, usize)> = Vec::new(); // (block end, cumulative shift)
        let mut total = 0;
        for block in &blocks {
            let start = self.doc.read(cx).line_start(block.start);
            let end = self.doc.read(cx).line_end(block.end - 1);
            let text = format!("\n{}", self.doc.read(cx).slice(start..end));
            total += text.len();
            shifts.push((end, total));
            edits.push((end..end, text));
        }
        let before = self.selections.clone();
        let selections = before
            .iter()
            .map(|s| {
                let shift = shifts
                    .iter()
                    .find(|(end, _)| s.range().end <= *end)
                    .map_or(total, |(_, shift)| *shift);
                Selection { anchor: s.anchor + shift, head: s.head + shift }
            })
            .collect();
        self.doc.as_mut(cx).edit(edits, &before, EditKind::Other);
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    fn move_lines(&mut self, up: bool, cx: &mut Context<Self>) {
        let blocks = self.selected_row_blocks(cx);
        let last_row = self.doc.read(cx).line_count() - 1;
        if (up && blocks.first().is_some_and(|b| b.start == 0)) || (!up && blocks.last().is_some_and(|b| b.end > last_row)) {
            return;
        }
        let mut edits = Vec::new();
        let mut shifts: Vec<(Range<usize>, isize)> = Vec::new();
        for block in &blocks {
            let block_start = self.doc.read(cx).line_start(block.start);
            let block_end = self.doc.read(cx).line_end(block.end - 1);
            let block_text = self.doc.read(cx).slice(block_start..block_end);
            if up {
                let prev_start = self.doc.read(cx).line_start(block.start - 1);
                let prev_text = self.doc.read(cx).line_text(block.start - 1);
                edits.push((prev_start..block_end, format!("{block_text}\n{prev_text}")));
                shifts.push((block_start..block_end + 1, -(prev_text.len() as isize + 1)));
            } else {
                let next_end = self.doc.read(cx).line_end(block.end);
                let next_text = self.doc.read(cx).line_text(block.end);
                edits.push((block_start..next_end, format!("{next_text}\n{block_text}")));
                shifts.push((block_start..block_end + 1, next_text.len() as isize + 1));
            }
        }
        let before = self.selections.clone();
        let selections = before
            .iter()
            .map(|s| {
                let shift = shifts
                    .iter()
                    .find(|(r, _)| r.contains(&s.range().start))
                    .map_or(0, |(_, d)| *d);
                Selection {
                    anchor: (s.anchor as isize + shift) as usize,
                    head: (s.head as isize + shift) as usize,
                }
            })
            .collect();
        self.doc.as_mut(cx).edit(edits, &before, EditKind::Other);
        self.set_selections(selections, self.primary, cx);
        self.after_edit(cx);
    }

    fn move_line_up(&mut self, _: &MoveLineUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(true, cx);
    }

    fn move_line_down(&mut self, _: &MoveLineDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_lines(false, cx);
    }

    fn select_next_occurrence(&mut self, _: &SelectNextOccurrence, _: &mut Window, cx: &mut Context<Self>) {
        let primary = self.primary_selection();
        if primary.is_empty() {
            let selections = self
                .selections
                .iter()
                .map(|s| {
                    let r = word_range(self.doc.read(cx), s.head);
                    Selection { anchor: r.start, head: r.end }
                })
                .collect();
            self.set_selections(selections, self.primary, cx);
            return;
        }
        let needle = self.doc.read(cx).slice(primary.range());
        let haystack = self.doc.read(cx).rope().to_string();
        let from = self.selections.iter().map(|s| s.range().end).max().unwrap_or(0);
        let found = haystack[from..]
            .find(&needle)
            .map(|i| i + from)
            .or_else(|| haystack.find(&needle))
            .filter(|&i| !self.selections.iter().any(|s| s.range().start == i));
        if let Some(start) = found {
            let mut selections = self.selections.clone();
            selections.push(Selection { anchor: start, head: start + needle.len() });
            let primary = selections.len() - 1;
            self.set_selections(selections, primary, cx);
        }
    }

    fn add_cursor_vertical(&mut self, rows: isize, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_wrap(window, cx);
        let primary = self.primary_selection();
        let goal = self.goal_x.unwrap_or_else(|| self.x_for_offset(primary.head, window, cx));
        let edge = if rows < 0 {
            self.selections.iter().min_by_key(|s| s.head).copied()
        } else {
            self.selections.iter().max_by_key(|s| s.head).copied()
        }
        .unwrap_or(primary);
        let drow = self.display_row_of(edge.head, cx) as isize + rows;
        if drow < 0 || drow as usize >= self.display_row_count(cx) {
            return;
        }
        let head = self.offset_for_display_row_x(drow as usize, goal, window, cx);
        let mut selections = self.selections.clone();
        selections.push(Selection::cursor(head));
        let primary_ix = selections.len() - 1;
        self.set_selections(selections, primary_ix, cx);
        self.goal_x = Some(goal);
    }

    fn add_cursor_above(&mut self, _: &AddCursorAbove, window: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertical(-1, window, cx);
    }

    fn add_cursor_below(&mut self, _: &AddCursorBelow, window: &mut Window, cx: &mut Context<Self>) {
        self.add_cursor_vertical(1, window, cx);
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        // Escape leaves a snippet's stops (and still does the rest below).
        self.doc.as_mut(cx).snippet = None;
        if self.open_hunk.take().is_some() {
            cx.notify();
            return;
        }
        if self.dismiss_lsp_popovers(cx) {
            return;
        }
        let p = self.primary_selection();
        if self.is_single_line() || (self.selections.len() == 1 && p.is_empty()) {
            // Nothing to collapse: let the parent (find bar, modal) handle Escape.
            cx.propagate();
            return;
        }
        let sel = if self.selections.len() > 1 { p } else { Selection::cursor(p.head) };
        self.set_selections(vec![sel], 0, cx);
    }

    fn copy_text(&self, cx: &App) -> (String, bool) {
        if self.selections.iter().all(|s| s.is_empty()) {
            // Copy whole lines when nothing is selected.
            let mut out = String::new();
            for block in self.selected_row_blocks(cx) {
                let start = self.doc.read(cx).line_start(block.start);
                let end = self.doc.read(cx).line_end(block.end - 1);
                out.push_str(&self.doc.read(cx).slice(start..end));
                out.push('\n');
            }
            (out, true)
        } else {
            let parts: Vec<String> = self.selections.iter().map(|s| self.doc.read(cx).slice(s.range())).collect();
            (parts.join("\n"), false)
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let (text, _) = self.copy_text(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        let (text, whole_lines) = self.copy_text(cx);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if whole_lines {
            let mut edits = Vec::new();
            for block in self.selected_row_blocks(cx) {
                let start = self.doc.read(cx).line_start(block.start);
                let end = (self.doc.read(cx).line_end(block.end - 1) + 1).min(self.doc.read(cx).len());
                edits.push((start..end, String::new()));
            }
            self.edit_and_remap(edits, EditKind::Other, cx);
        } else {
            self.edit_selections(EditKind::Other, cx, |_, s| (s.range(), String::new(), Some(0)));
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let mut text = text.replace("\r\n", "\n");
        if self.is_single_line() {
            text = text.trim_end_matches('\n').replace('\n', " ");
        }
        let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        let n = self.selections.len();
        if n > 1 && lines.len() == n {
            let mut i = 0;
            self.edit_selections(EditKind::Other, cx, |_, s| {
                let t = lines[i].to_string();
                i += 1;
                let len = t.len();
                (s.range(), t, Some(len))
            });
        } else {
            let len = text.len();
            self.edit_selections(EditKind::Other, cx, |_, s| (s.range(), text.clone(), Some(len)));
        }
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        let undone = self.doc.as_mut(cx).undo();
        if let Some(selections) = undone {
            let selections = self.clamp_selections(selections, cx);
            self.set_selections(selections, 0, cx);
            self.schedule_lint(Duration::from_millis(150), cx);
            self.schedule_diff(Duration::from_millis(120), cx);
            self.edited(cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        let redone = self.doc.as_mut(cx).redo();
        if let Some(selections) = redone {
            let selections = self.clamp_selections(selections, cx);
            self.set_selections(selections, 0, cx);
            self.schedule_lint(Duration::from_millis(150), cx);
            self.schedule_diff(Duration::from_millis(120), cx);
            self.edited(cx);
        }
    }

    fn clamp_selections(&self, selections: Vec<Selection>, cx: &App) -> Vec<Selection> {
        let mut s: Vec<Selection> = selections
            .into_iter()
            .map(|s| Selection { anchor: self.doc.read(cx).clip(s.anchor), head: self.doc.read(cx).clip(s.head) })
            .collect();
        if s.is_empty() {
            s.push(Selection::cursor(0));
        }
        s
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    // ---- Mouse ---------------------------------------------------------------

    fn scrollbar_geometry(&self, cx: &App) -> Option<(Bounds<Pixels>, Bounds<Pixels>)> {
        let l = self.layout.as_ref()?;
        let viewport = l.bounds.size.height;
        let content = l.line_height * self.display_row_count(cx) as f32 + viewport - l.line_height;
        if content <= viewport {
            return None;
        }
        let track = Bounds::new(
            point(l.bounds.right() - px(SCROLLBAR_WIDTH), l.bounds.top()),
            size(px(SCROLLBAR_WIDTH), viewport),
        );
        let thumb_h = (viewport * (viewport / content)).max(px(28.));
        let max_scroll = content - viewport;
        let t = (self.scroll.y / max_scroll).clamp(0., 1.);
        let thumb = Bounds::new(
            point(track.left() + px(2.), track.top() + (viewport - thumb_h) * t),
            size(px(SCROLLBAR_WIDTH - 4.), thumb_h),
        );
        Some((track, thumb))
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        self.sync_wrap(window, cx);
        let Some(layout) = self.layout.clone() else { return };

        if let Some((track, thumb)) = self.scrollbar_geometry(cx)
            && track.contains(&event.position)
        {
            if thumb.contains(&event.position) {
                self.scrollbar_drag = Some(event.position.y - thumb.top());
            } else {
                self.scrollbar_drag = Some(thumb.size.height / 2.);
                self.drag_scrollbar_to(event.position.y, cx);
            }
            return;
        }

        // A pinned line jumps to its definition.
        if let Some(offset) = self.sticky_at(event.position) {
            self.set_selections(vec![Selection::cursor(offset)], 0, cx);
            self.center_on_next_frame = true;
            cx.notify();
            return;
        }

        if !self.is_chromeless() {
            // A ▶ runs its target.
            if event.position.x < layout.bounds.left() + px(5. + RUN_ICON + 4.) {
                let y = event.position.y - layout.bounds.top() + self.scroll.y;
                let (row, seg) = self.line_of_display_row((y / layout.line_height).floor().max(0.) as usize, cx);
                if seg == 0
                    && let Some(target) = self.run_target_at(row, cx)
                {
                    cx.emit(EditorRunEvent { target, debug: false, menu_at: Some(event.position) });
                    return;
                }
            }
            // A line number sets or clears a breakpoint (in files that can be debugged).
            if let Some(row) = self.breakpoint_row_at(event.position, &layout, cx) {
                self.toggle_breakpoint_at(row, cx);
                return;
            }
            // Accept Current / Incoming / Both on a conflict.
            if let Some(&(_, start, resolution)) = self.conflict_hits.iter().find(|(b, _, _)| b.contains(&event.position)) {
                self.resolve_conflict(start, resolution, cx);
                return;
            }
            // The `⋯` placeholder unfolds.
            if let Some(&(_, start)) = self.fold_pills.iter().find(|(b, _)| b.contains(&event.position)) {
                self.doc.as_mut(cx).unfold_where(|f| f.start == start);
                cx.notify();
                return;
            }
            // The fold arrow toggles its region.
            let arrow_right = layout.bounds.left() + layout.gutter_width - px(FOLD_ZONE_RIGHT);
            if event.position.x <= arrow_right + px(2.) && event.position.x >= arrow_right - px(FOLD_ICON + 4.) {
                let y = event.position.y - layout.bounds.top() + self.scroll.y;
                let (row, seg) = self.line_of_display_row((y / layout.line_height).floor().max(0.) as usize, cx);
                if seg == 0 && (self.fold_under(row, cx).is_some() || self.fold_regions(cx).iter().any(|r| r.start == row)) {
                    self.toggle_fold_at(row, cx);
                    return;
                }
            }
        }

        // Clicking a change marker in the gutter toggles its popover.
        let bar_x = layout.bounds.left() + layout.gutter_width - px(GIT_BAR_OFFSET);
        if !self.is_chromeless() && (event.position.x - bar_x).abs() < px(6.) {
            let y = event.position.y - layout.bounds.top() + self.scroll.y;
            let row_f = y / layout.line_height;
            if let Some(ix) = self.hunk_at(row_f, cx) {
                self.open_hunk = if self.open_hunk == Some(ix) { None } else { Some(ix) };
                cx.notify();
                return;
            }
        }
        self.open_hunk = None;
        self.dismiss_lsp_popovers(cx);

        let offset = self.offset_for_position(event.position, window, cx);
        let in_gutter = event.position.x < layout.bounds.left() + layout.gutter_width;
        // ⌘-click (Ctrl-click elsewhere): Go to Declaration.
        if event.modifiers.secondary() && !in_gutter && event.click_count == 1 && !self.is_chromeless() {
            self.set_selections(vec![Selection::cursor(offset)], 0, cx);
            self.navigate_from(offset, cx);
            return;
        }
        let (mode, unit) = if in_gutter || event.click_count >= 3 {
            (DragMode::Line, self.line_range(offset, cx))
        } else if event.click_count == 2 {
            (DragMode::Word, word_range(self.doc.read(cx), offset))
        } else {
            (DragMode::Char, offset..offset)
        };

        if event.modifiers.shift && event.modifiers.alt && !in_gutter && !self.is_single_line() {
            // Alt+Shift+click: a box from the caret to here (VS Code).
            let head = self.primary_selection().head;
            let from = (self.doc.read(cx).row_of(head), self.display_column_of(head, cx));
            self.drag = Some(Drag { mode: DragMode::Char, origin: head..head, column_from: Some(from) });
            self.select_box(from, event.position, cx);
        } else if event.modifiers.shift {
            let p = self.primary_selection();
            let head = if offset < p.anchor { unit.start } else { unit.end };
            let mut selections = self.selections.clone();
            selections[self.primary] = Selection { anchor: p.anchor, head };
            self.set_selections(selections, self.primary, cx);
            self.drag = Some(Drag { mode, origin: p.anchor..p.anchor, column_from: None });
        } else {
            let sel = Selection { anchor: unit.start, head: unit.end };
            if event.modifiers.alt {
                let mut selections = self.selections.clone();
                selections.push(sel);
                let ix = selections.len() - 1;
                self.set_selections(selections, ix, cx);
            } else {
                self.set_selections(vec![sel], 0, cx);
            }
            // Alt+drag selects a box (JetBrains); Alt+click alone adds a caret.
            let column_from = (event.modifiers.alt && mode == DragMode::Char && !in_gutter && !self.is_single_line())
                .then(|| (self.doc.read(cx).row_of(offset), self.column_at_x(event.position.x)));
            self.drag = Some(Drag { mode, origin: unit, column_from });
        }
        self.goal_x = None;
        self.autoscroll = false;
        self.restart_blink(window, cx);
    }

    /// The display column (tabs expanded) nearest to a window x.
    fn column_at_x(&self, x: Pixels) -> usize {
        let Some(layout) = self.layout.as_ref() else { return 0 };
        ((x - layout.text_left + self.scroll.x) / layout.char_width).round().max(0.) as usize
    }

    fn display_column_of(&self, offset: usize, cx: &App) -> usize {
        let row = self.doc.read(cx).row_of(offset);
        let start = self.doc.read(cx).line_start(row);
        DisplayLine::new(&self.doc.read(cx).line_text(row), self.tab_size).to_display(offset - start)
    }

    /// Column (box) selection: on every line from `from`'s row to the one
    /// under `to`, the same columns. Lines too short for a box that has width
    /// are skipped; a zero-width box puts a caret on each line.
    fn select_box(&mut self, from: (usize, usize), to: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(layout) = self.layout.as_ref() else { return };
        let y = to.y - layout.bounds.top() + self.scroll.y;
        let drow = ((y / layout.line_height).floor().max(0.) as usize).min(self.display_row_count(cx).saturating_sub(1));
        let row = self.line_of_display_row(drow, cx).0.min(self.doc.read(cx).line_count() - 1);
        let col = self.column_at_x(to.x);
        let (left, right) = (from.1.min(col), from.1.max(col));
        let rows: Vec<usize> = if row >= from.0 { (from.0..=row).collect() } else { (row..=from.0).rev().collect() };
        let mut selections = Vec::new();
        for r in rows.into_iter().filter(|&r| !self.row_hidden(r)) {
            let text = self.doc.read(cx).line_text(r);
            let dl = DisplayLine::new(&text, self.tab_size);
            if left > dl.text.len() && right > left {
                continue;
            }
            let start = self.doc.read(cx).line_start(r);
            let (a, b) = (start + dl.to_buffer(from.1), start + dl.to_buffer(col));
            selections.push(Selection { anchor: a, head: b });
        }
        if selections.is_empty() {
            let start = self.doc.read(cx).line_start(from.0);
            selections.push(Selection::cursor(start + DisplayLine::new(&self.doc.read(cx).line_text(from.0), self.tab_size).to_buffer(from.1)));
        }
        let primary = selections.len() - 1;
        self.set_selections(selections, primary, cx);
    }

    /// The row whose line number is at `position`, where clicks set breakpoints.
    fn breakpoint_row_at(&self, position: Point<Pixels>, layout: &LayoutSnapshot, cx: &App) -> Option<usize> {
        let number_right = layout.bounds.left() + layout.gutter_width - px(FOLD_ZONE_RIGHT + 18.);
        if !self.can_break(cx) || position.x < layout.bounds.left() + px(5. + RUN_ICON + 4.) || position.x > number_right + px(4.) {
            return None;
        }
        let y = position.y - layout.bounds.top() + self.scroll.y;
        let (row, seg) = self.line_of_display_row((y / layout.line_height).floor().max(0.) as usize, cx);
        (seg == 0 && row < self.doc.read(cx).line_count()).then_some(row)
    }

    /// Right-click: place the caret (unless clicking inside a selection, so
    /// Cut/Copy apply to it) and ask the workspace for the context menu. On
    /// a line number, the breakpoint menu instead.
    fn on_right_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != EditorMode::Full {
            return;
        }
        window.focus(&self.focus_handle);
        self.sync_wrap(window, cx);
        self.open_hunk = None;
        if let Some(layout) = self.layout.clone()
            && let Some(row) = self.breakpoint_row_at(event.position, &layout, cx)
        {
            cx.emit(EditorBreakpointMenu { row, position: event.position });
            return;
        }
        let offset = self.offset_for_position(event.position, window, cx);
        let inside = self.selections.iter().any(|s| !s.is_empty() && s.range().contains(&offset));
        if !inside {
            self.set_selections(vec![Selection::cursor(offset)], 0, cx);
            self.goal_x = None;
        }
        self.restart_blink(window, cx);
        cx.emit(EditorContextMenu(event.position));
    }

    pub fn soft_wrap(&self) -> bool {
        self.soft_wrap
    }

    /// Whether the caret is on a changed (git) line, for Rollback Lines.
    pub fn has_change_at_cursor(&self, cx: &App) -> bool {
        self.current_hunk(cx).is_some()
    }

    /// Whether the change at the caret is staged (`None`: no change there).
    pub fn change_at_cursor_staged(&self, cx: &App) -> Option<bool> {
        self.current_hunk(cx).map(|i| self.hunk_staged.get(i).copied().unwrap_or(false))
    }

    /// Whether the caret is in a merge conflict (as of the last frame).
    pub fn has_conflict_at_cursor(&self, cx: &App) -> bool {
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        self.conflicts.current(&self.doc.read(cx).version()).is_some_and(|c| c.iter().any(|c| c.rows().contains(&row)))
    }

    fn line_range(&self, offset: usize, cx: &App) -> Range<usize> {
        let row = self.doc.read(cx).row_of(offset);
        let start = self.doc.read(cx).line_start(row);
        let end = (self.doc.read(cx).line_end(row) + 1).min(self.doc.read(cx).len());
        start..end
    }

    fn drag_scrollbar_to(&mut self, y: Pixels, cx: &mut Context<Self>) {
        let (Some(grab), Some((track, thumb)), Some(l)) = (self.scrollbar_drag, self.scrollbar_geometry(cx), self.layout.as_ref()) else {
            return;
        };
        let viewport = l.bounds.size.height;
        let content = l.line_height * self.display_row_count(cx) as f32 + viewport - l.line_height;
        let range = track.size.height - thumb.size.height;
        if range <= px(0.) {
            return;
        }
        let t = ((y - grab - track.top()) / range).clamp(0., 1.);
        self.scroll.y = (content - viewport) * t;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            self.scrollbar_drag = None;
            let over_gutter = !self.is_chromeless()
                && self.layout.as_ref().is_some_and(|l| {
                    l.bounds.contains(&event.position) && event.position.x < l.bounds.left() + l.gutter_width
                });
            if over_gutter != self.gutter_hovered {
                self.gutter_hovered = over_gutter;
                cx.notify();
            }
            // The pinned lines aren't the code under them.
            if self.sticky_at(event.position).is_some() {
                self.pointer_left(window, cx);
            } else {
                self.hover_at(event.position, window, cx);
            }
            return;
        }
        if self.scrollbar_drag.is_some() {
            self.drag_scrollbar_to(event.position.y, cx);
            return;
        }
        let Some(drag) = self.drag.as_ref() else { return };
        let (mode, origin, column_from) = (drag.mode, drag.origin.clone(), drag.column_from);
        self.sync_wrap(window, cx);
        if let Some(from) = column_from {
            let row = self.doc.read(cx).row_of(self.offset_for_position(event.position, window, cx));
            // Still on the spot it started: an Alt+click, not a box yet.
            if (row, self.column_at_x(event.position.x)) != from {
                self.select_box(from, event.position, cx);
            }
            self.autoscroll = self.layout.as_ref().is_some_and(|l| !l.bounds.contains(&event.position));
            return;
        }
        let offset = self.offset_for_position(event.position, window, cx);
        let unit = match mode {
            DragMode::Char => offset..offset,
            DragMode::Word => word_range(self.doc.read(cx), offset),
            DragMode::Line => self.line_range(offset, cx),
        };
        let sel = if offset < origin.start {
            Selection { anchor: origin.end, head: unit.start }
        } else {
            Selection { anchor: origin.start, head: unit.end.max(origin.end) }
        };
        let mut selections = self.selections.clone();
        selections[self.primary] = sel;
        self.set_selections(selections, self.primary, cx);
        // Only scroll when the pointer leaves the viewport.
        self.autoscroll = self
            .layout
            .as_ref()
            .is_some_and(|l| !l.bounds.contains(&event.position));
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            self.vim_after_mouse(cx);
        }
        self.scrollbar_drag = None;
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let line_height = self.layout.as_ref().map_or(px(20.), |l| l.line_height);
        let mut delta = event.delta.pixel_delta(line_height);
        // Lock to the dominant axis to avoid diagonal jitter on trackpads.
        if delta.x.abs() > delta.y.abs() {
            delta.y = px(0.);
        } else {
            delta.x = px(0.);
        }
        self.scroll.y -= delta.y;
        self.scroll.x -= delta.x;
        self.clamp_scroll(cx);
        self.lsp.hover = None;
        cx.notify();
    }

    fn clamp_scroll(&mut self, cx: &App) {
        let Some(l) = self.layout.as_ref() else { return };
        let max_y = l.line_height * (self.display_row_count(cx).saturating_sub(1)) as f32;
        self.scroll.y = self.scroll.y.clamp(px(0.), max_y.max(px(0.)));
        if self.wrap.as_ref().is_some_and(|w| w.key.wrap) {
            self.scroll.x = px(0.);
            return;
        }
        let text_width = l.bounds.right() - l.text_left - px(SCROLLBAR_WIDTH);
        let max_x = (self.max_line_width + l.char_width * 4. - text_width).max(px(0.));
        self.scroll.x = self.scroll.x.clamp(px(0.), max_x);
    }

    // ---- Frame layout ------------------------------------------------------------

    fn layout_frame(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) -> FrameLayout {
        let theme = Theme::get(cx).clone();
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        // Text fields skip the code chrome; only one-line fields center vertically.
        let single = self.is_chromeless();
        let font_size = self.font_size(cx);
        let line_height = self.line_height(cx);
        let editor_font = self.editor_font(cx);
        // Single-line fields center their one row vertically.
        let top = if self.is_single_line() {
            bounds.top() + ((bounds.size.height - line_height) / 2.).max(px(0.))
        } else {
            bounds.top()
        };
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&editor_font);
        let char_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);

        let line_count = self.doc.read(cx).line_count();
        let digits = line_count.to_string().len().max(3);
        // Diagnostic dot, numbers, fold arrow, git bar.
        let gutter_width = if single { px(0.) } else { char_width * digits as f32 + px(58.) };
        let text_bounds = Bounds::from_corners(
            point(bounds.left() + gutter_width, bounds.top()),
            bounds.bottom_right(),
        );
        let text_left = text_bounds.left() + if single { px(0.) } else { px(TEXT_PADDING) };
        let rows_per_page = ((bounds.size.height / line_height).floor() as usize).saturating_sub(2).max(1);
        let wrap_width = (bounds.right() - text_left - if single { char_width } else { px(SCROLLBAR_WIDTH) + char_width })
            .max(char_width * 10.);

        self.layout = Some(LayoutSnapshot {
            bounds,
            text_left,
            line_height,
            char_width,
            gutter_width,
            rows_per_page,
            wrap_width,
        });
        self.sync_wrap(window, cx);
        let wrapping = self.wrap.as_ref().is_some_and(|w| w.key.wrap);

        // Keep the primary cursor visible after keyboard navigation.
        if std::mem::take(&mut self.reveal_primary) {
            let drow = self.display_row_of(self.primary_selection().head, cx) as f32;
            let visible = self.scroll.y..self.scroll.y + bounds.size.height - line_height;
            if !visible.contains(&(line_height * drow)) {
                self.center_on_next_frame = true;
            } else {
                self.autoscroll = true;
            }
        }
        if std::mem::take(&mut self.center_on_next_frame) && !single {
            let drow = self.display_row_of(self.primary_selection().head, cx);
            self.scroll.y = (line_height * drow as f32 - bounds.size.height / 3.).max(px(0.));
            self.scroll.x = px(0.);
        } else if std::mem::take(&mut self.autoscroll) {
            let head = self.primary_selection().head;
            let drow = self.display_row_of(head, cx);
            let margin = line_height * 3.;
            let top = line_height * drow as f32;
            let viewport = bounds.size.height;
            // Pinned lines cover the top; keep the caret below them.
            let top_margin = margin + line_height * self.sticky_rows.len() as f32;
            if top - top_margin < self.scroll.y {
                self.scroll.y = (top - top_margin).max(px(0.));
            } else if top + line_height + margin > self.scroll.y + viewport {
                self.scroll.y = top + line_height + margin - viewport;
            }
            if !wrapping {
                let x = self.x_for_offset(head, window, cx);
                let text_width = bounds.right() - text_left - if single { px(0.) } else { px(SCROLLBAR_WIDTH) };
                if x < self.scroll.x + char_width * 2. {
                    self.scroll.x = (x - char_width * 4.).max(px(0.));
                } else if x > self.scroll.x + text_width - char_width * 3. {
                    self.scroll.x = x - text_width + char_width * 6.;
                }
            }
        }
        self.clamp_scroll(cx);

        // Visible display rows, and the buffer lines they show.
        let display_rows = self.display_row_count(cx);
        let first_drow = (self.scroll.y / line_height).floor() as usize;
        let visible_rows = (bounds.size.height / line_height).ceil() as usize + 1;
        let last_drow = (first_drow + visible_rows).min(display_rows);
        let first_drow = first_drow.min(last_drow);
        let first_row = self.line_of_display_row(first_drow, cx).0;
        let last_row = if last_drow == 0 { 0 } else { (self.line_of_display_row(last_drow - 1, cx).0 + 1).min(line_count) };
        let range_start = self.doc.read(cx).line_start(first_row);
        let range_end = if last_row == 0 { 0 } else { self.doc.read(cx).line_end(last_row - 1) };
        self.fold_pills.clear();
        // Fold arrows show for folded headers, and for every foldable line
        // while the pointer is over the gutter.
        let fold_regions = (!single && (self.gutter_hovered || !self.doc.read(cx).folds().is_empty())).then(|| self.shown_fold_regions(cx));
        let run_rows: Vec<usize> =
            if single { Vec::new() } else { self.shown_run_targets(cx).iter().filter_map(|t| t.row).collect() };
        let conflicts = if single { Arc::new(Vec::new()) } else { self.shown_conflicts(cx) };
        let debug_row = self.doc.read(cx).debug_row();
        self.conflict_hits.clear();
        let highlights = self.visible_highlights(first_drow, last_drow, first_row, last_row, cx);
        // Bracket pairs around carets.
        let brackets: Vec<Range<usize>> = if focused && !single {
            self.bracket_pairs(cx).iter().filter(|r| (range_start..=range_end).contains(&r.start)).cloned().collect()
        } else {
            Vec::new()
        };
        // Sorted by start: none past the last visible line.
        let below = self.doc.read(cx).diagnostics.partition_point(|d| d.range.start <= range_end);
        let diagnostics: Vec<&lint::Diagnostic> =
            self.doc.read(cx).diagnostics[..below].iter().filter(|d| d.range.end >= range_start).collect();

        let breakpoints = if single { Vec::new() } else { self.doc.read(cx).breakpoints() };
        let head_rows: Vec<usize> = self
            .selections
            .iter()
            .filter(|s| s.is_empty())
            .map(|s| self.doc.read(cx).row_of(s.head))
            .collect();

        let mut frame = FrameLayout {
            text_bounds,
            line_height,
            ..Default::default()
        };
        // Rulers: full-height guides at fixed columns.
        if !single {
            for &col in &self.file_settings.rulers {
                let x = text_left - self.scroll.x + char_width * col as f32;
                if x >= text_bounds.left() && x <= text_bounds.right() {
                    frame.guides.push(fill(Bounds::new(point(x, text_bounds.top()), size(px(1.), text_bounds.size.height)), theme.indent_guide));
                }
            }
        }
        let show_whitespace = if single { ShowWhitespace::None } else { self.file_settings.show_whitespace };
        let whitespace_color = Hsla::from(theme.text_dim).alpha(0.55);
        let ui_font = font(theme.ui_font.clone());

        let tab_size = self.tab_size;
        // Guides at each indent level of the file's own style.
        let guide_step = if self.indent_unit == "\t" { tab_size } else { self.indent_unit.len().max(1) };
        // Indentation (in columns) for indent guides.
        let indents: Vec<Option<usize>> = (first_row..last_row)
            .map(|row| {
                let t = self.doc.read(cx).line_text(row);
                if t.trim().is_empty() {
                    None
                } else {
                    Some(DisplayLine::new(&t[..t.len() - t.trim_start().len()], tab_size).text.len())
                }
            })
            .collect();

        let mut hl_ix = 0;
        for row in first_row..last_row {
            if self.row_hidden(row) {
                continue;
            }
            let line_start = self.doc.read(cx).line_start(row);
            let text = self.doc.read(cx).line_text(row);
            let line_end = line_start + text.len();
            let dl = self.display_line(row, cx);

            // Build styled runs from highlight spans and diagnostics.
            while hl_ix < highlights.len() && highlights[hl_ix].0.end <= line_start {
                hl_ix += 1;
            }
            let mut cuts: Vec<usize> = vec![0, text.len()];
            for (r, _) in highlights[hl_ix..].iter().take_while(|(r, _)| r.start < line_end) {
                cuts.push(r.start.saturating_sub(line_start).min(text.len()));
                cuts.push(r.end.saturating_sub(line_start).min(text.len()));
            }
            let row_diags: Vec<&&lint::Diagnostic> = diagnostics
                .iter()
                .filter(|d| d.range.start <= line_end && d.range.end > line_start)
                .collect();
            for d in &row_diags {
                cuts.push(d.range.start.saturating_sub(line_start).min(text.len()));
                cuts.push(d.range.end.saturating_sub(line_start).min(text.len()));
            }
            cuts.sort_unstable();
            cuts.dedup();

            let mut runs = Vec::with_capacity(cuts.len());
            let mut h = hl_ix;
            for w in cuts.windows(2) {
                let (s, e) = (w[0], w[1]);
                let abs = line_start + s;
                while h < highlights.len() && highlights[h].0.end <= abs {
                    h += 1;
                }
                let kind = highlights
                    .get(h)
                    .filter(|(r, _)| r.start <= abs && abs < r.end)
                    .map(|(_, k)| *k)
                    .unwrap_or_default();
                let mut run_font = editor_font.clone();
                if kind.base() == crate::syntax::Highlight::Comment && theme.italic_comments {
                    run_font.style = FontStyle::Italic;
                }
                let severity = row_diags
                    .iter()
                    .filter(|d| d.range.start <= abs && abs < d.range.end)
                    .map(|d| d.severity)
                    .min();
                let len = dl.to_display(e) - dl.to_display(s);
                if len == 0 {
                    continue;
                }
                runs.push(TextRun {
                    len,
                    font: run_font,
                    color: theme.highlight_color(kind),
                    background_color: None,
                    underline: severity.map(|sev| UnderlineStyle {
                        thickness: px(1.),
                        color: Some(severity_color(&theme, sev)),
                        wavy: true,
                    }),
                    strikethrough: None,
                });
            }

            if !dl.inlays.is_empty() {
                runs = inlay_runs(runs, &dl, editor_font.clone(), theme.text_dim.into());
            }
            let is_cursor_row = head_rows.contains(&row);
            let indent = indents[row - first_row].unwrap_or_else(|| {
                let prev = indents[..row - first_row].iter().rev().find_map(|i| *i).unwrap_or(0);
                let next = indents[row - first_row..].iter().find_map(|i| *i).unwrap_or(0);
                prev.min(next)
            });
            let breaks = self.line_breaks(row).to_vec();
            let row_first_drow = self.first_display_row(row);
            let worst = row_diags.iter().filter(|d| self.doc.read(cx).row_of(d.range.start) == row).min_by_key(|d| d.severity);

            // Each wrapped segment of the line is its own display row.
            for seg in 0..=breaks.len() {
                let drow = row_first_drow + seg;
                if drow < first_drow || drow >= last_drow {
                    continue;
                }
                let (seg_range, wrap_indent) = wrap::segment(&breaks, seg, dl.text.len());
                let first_seg = seg == 0;
                let last_seg = seg == breaks.len();
                let seg_runs = if breaks.is_empty() { std::mem::take(&mut runs) } else { slice_runs(&runs, seg_range.clone()) };
                let shaped = text_system.shape_line(
                    SharedString::from(dl.text[seg_range.clone()].to_string()),
                    font_size,
                    &seg_runs,
                    None,
                );
                if !wrapping {
                    self.max_line_width = self.max_line_width.max(shaped.width);
                }
                let y = top + line_height * drow as f32 - self.scroll.y;
                let base_x = text_left - self.scroll.x;
                let origin = point(base_x + char_width * wrap_indent as f32, y);
                // x of a display column of the line, clamped to this segment.
                let x_of = |col: usize| shaped.x_for_index(col.clamp(seg_range.start, seg_range.end) - seg_range.start);
                // The part of a line-relative buffer range on this segment (in
                // display columns), and whether it runs on past the segment.
                let clip = |start: usize, end: usize, past_line_end: bool| -> Option<(usize, usize, bool)> {
                    let (ds, de) = (dl.to_display(start), dl.to_display(end));
                    if ds > seg_range.end || de < seg_range.start || (!last_seg && ds == seg_range.end) {
                        return None;
                    }
                    let (s, e) = (ds.max(seg_range.start), de.min(seg_range.end));
                    let continues = if last_seg { past_line_end } else { de >= seg_range.end };
                    (s < e || continues).then_some((s, e, continues))
                };

                // The line a debug session is paused on.
                if debug_row == Some(row) && !single {
                    frame.backgrounds.push(fill(
                        Bounds::from_corners(point(bounds.left(), y), point(bounds.right(), y + line_height)),
                        Hsla::from(theme.warning).alpha(0.16),
                    ));
                }

                // Current line highlight.
                if is_cursor_row && self.selections.len() == 1 && !single {
                    frame.backgrounds.push(fill(
                        Bounds::from_corners(point(bounds.left(), y), point(bounds.right(), y + line_height)),
                        theme.current_line,
                    ));
                }

                // Merge conflict regions: current side green, incoming blue,
                // markers stronger.
                if let Some(c) = conflicts.iter().find(|c| c.rows().contains(&row)) {
                    let marker = row == c.start || row == c.end || row + 1 == c.theirs.start || c.base.as_ref().is_some_and(|b| row + 1 == b.start);
                    let color: Hsla = if row == c.start || c.ours.contains(&row) {
                        theme.git_added.into()
                    } else if row == c.end || c.theirs.contains(&row) {
                        theme.git_modified.into()
                    } else {
                        theme.text_dim.into()
                    };
                    frame.backgrounds.push(fill(
                        Bounds::from_corners(point(bounds.left() + gutter_width, y), point(bounds.right(), y + line_height)),
                        color.alpha(if marker { 0.24 } else { 0.11 }),
                    ));
                }

                // Indent guides.
                let mut col = 0;
                while !single && col + guide_step <= indent.max(1) && col < indent {
                    let x = base_x + char_width * col as f32;
                    if x >= text_bounds.left() {
                        frame.guides.push(fill(
                            Bounds::new(point(x, y), size(px(1.), line_height)),
                            theme.indent_guide,
                        ));
                    }
                    col += guide_step;
                }

                // Inlay hints sit on a soft chip.
                for span in dl.inlays.iter().filter(|s| s.label.start >= seg_range.start && s.label.end <= seg_range.end) {
                    let bounds = Bounds::from_corners(
                        point(origin.x + x_of(span.label.start) - px(2.), y + px(2.)),
                        point(origin.x + x_of(span.label.end) + px(2.), y + line_height - px(2.)),
                    );
                    frame.search.push(fill(bounds, Hsla::from(theme.text_dim).alpha(0.13)).corner_radii(px(3.)));
                }

                // Spaces as dots and tabs as lines, where `show_whitespace` asks.
                if show_whitespace != ShowWhitespace::None {
                    let body_start = text.len() - text.trim_start().len();
                    let body_end = text.trim_end().len();
                    let selected: Vec<Range<usize>> = self
                        .selections
                        .iter()
                        .map(|s| s.range())
                        .filter(|r| !r.is_empty() && r.start <= line_end && r.end > line_start)
                        .collect();
                    // Only this row's part of the line (a long line has many rows).
                    let seg_text = dl.to_buffer(seg_range.start)..dl.to_buffer(seg_range.end).max(dl.to_buffer(seg_range.start));
                    let chars = text[seg_text.clone()].char_indices().map(|(i, c)| (seg_text.start + i, c));
                    for (i, c) in chars.filter(|(_, c)| *c == ' ' || *c == '\t') {
                        let shown = match show_whitespace {
                            ShowWhitespace::All => true,
                            ShowWhitespace::Boundary => i < body_start || i >= body_end,
                            ShowWhitespace::Trailing => i >= body_end,
                            ShowWhitespace::Selection => selected.iter().any(|r| r.contains(&(line_start + i))),
                            ShowWhitespace::None => false,
                        };
                        let (ds, de) = (dl.to_display(i), dl.to_display(i + 1));
                        if !shown || ds < seg_range.start || ds >= seg_range.end {
                            continue;
                        }
                        let (x0, x1) = (origin.x + x_of(ds), origin.x + x_of(de));
                        let mid = y + line_height / 2.;
                        let quad = if c == ' ' {
                            let d = (char_width * 0.16).max(px(1.5));
                            Bounds::new(point((x0 + x1) / 2. - d / 2., mid - d / 2.), size(d, d))
                        } else {
                            let inset = char_width * 0.3;
                            Bounds::from_corners(point(x0 + inset, mid), point(x1 - inset, mid + px(1.)))
                        };
                        frame.guides.push(fill(quad, whitespace_color));
                    }
                }

                // Search matches.
                let first_match = self.search_matches.partition_point(|m| m.end < line_start);
                for (ix, m) in self.search_matches.iter().enumerate().skip(first_match) {
                    if m.start > line_end {
                        break;
                    }
                    let Some((s, e, continues)) =
                        clip(m.start.max(line_start) - line_start, m.end.min(line_end) - line_start, m.end > line_end)
                    else {
                        continue;
                    };
                    let x0 = x_of(s);
                    let mut x1 = x_of(e);
                    if continues {
                        x1 += char_width * 0.6;
                    }
                    let bounds = Bounds::from_corners(point(origin.x + x0, y + px(1.)), point(origin.x + x1, y + line_height - px(1.)));
                    let quad = if Some(ix) == self.active_match {
                        gpui::quad(bounds, px(3.), theme.search_active, px(1.), Hsla::from(theme.search_active).alpha(0.9), gpui::BorderStyle::Solid)
                    } else {
                        fill(bounds, theme.search_match).corner_radii(px(3.))
                    };
                    frame.search.push(quad);
                }

                // Other uses of the symbol at the caret (from the language server).
                let symbols = self.current_symbol_highlights(cx);
                for r in symbols.iter().skip(symbols.partition_point(|r| r.end < line_start)) {
                    if r.start > line_end {
                        break;
                    }
                    let Some((s, e, _)) = clip(r.start.max(line_start) - line_start, r.end.min(line_end) - line_start, false) else {
                        continue;
                    };
                    let bounds = Bounds::from_corners(point(origin.x + x_of(s), y + px(1.)), point(origin.x + x_of(e), y + line_height - px(1.)));
                    frame.search.push(fill(bounds, theme.bracket_match).corner_radii(px(3.)));
                }

                // Matching brackets (painted with search highlights, under the text).
                for b in brackets.iter().filter(|b| b.start >= line_start && b.start < line_end) {
                    let col = dl.to_display(b.start - line_start);
                    if wrap::segment_of(&breaks, col) != seg {
                        continue;
                    }
                    let (x0, x1) = (x_of(col), x_of(dl.to_display(b.end.min(line_end) - line_start)));
                    frame.search.push(
                        fill(
                            Bounds::from_corners(point(origin.x + x0, y + px(1.)), point(origin.x + x1, y + line_height - px(1.))),
                            theme.bracket_match,
                        )
                        .corner_radii(px(2.)),
                    );
                }

                // Selections.
                for sel in &self.selections {
                    let r = sel.range();
                    if sel.is_empty() || r.end < line_start || r.start > line_end {
                        continue;
                    }
                    if r.end == line_start && r.start < line_start {
                        continue;
                    }
                    let Some((s, e, continues)) =
                        clip(r.start.max(line_start) - line_start, r.end.min(line_end) - line_start, r.end > line_end)
                    else {
                        continue;
                    };
                    let x0 = x_of(s);
                    let mut x1 = x_of(e);
                    if continues {
                        x1 += char_width * 0.6;
                    }
                    if x1 > x0 {
                        let color = if focused { theme.selection } else { theme.selection_inactive };
                        frame.selections.push(
                            fill(
                                Bounds::from_corners(point(origin.x + x0, y), point(origin.x + x1, y + line_height)),
                                color,
                            )
                            .corner_radii(px(3.)),
                        );
                    }
                }

                // Cursors (blocks in Vim's Normal and Visual modes).
                let block = self.vim_block_cursor();
                if focused && (self.cursor_visible || block) {
                    for sel in &self.selections {
                        let head = self.vim_cursor_of(sel);
                        if self.doc.read(cx).row_of(head) != row {
                            continue;
                        }
                        let col = dl.to_display(head - line_start);
                        if wrap::segment_of(&breaks, col) != seg {
                            continue;
                        }
                        let x = x_of(col);
                        let bounds = if block {
                            let next = dl.to_display((head - line_start + self.doc.read(cx).char_at(head).filter(|c| *c != '\n').map_or(0, char::len_utf8)).min(text.len()));
                            let width = if next > col { x_of(next) - x } else { char_width };
                            Bounds::new(point(origin.x + x, y + px(1.)), size(width, line_height - px(2.)))
                        } else {
                            Bounds::new(point(origin.x + x - px(1.), y + px(2.)), size(px(2.), line_height - px(4.)))
                        };
                        let color = if block { Hsla::from(theme.cursor).alpha(0.55) } else { theme.cursor.into() };
                        frame.cursors.push(fill(bounds, color).corner_radii(px(1.)));
                    }
                }

                if single {
                    if first_seg && self.doc.read(cx).len() == 0 && !self.placeholder.is_empty() {
                        let placeholder = text_system.shape_line(
                            self.placeholder.clone(),
                            font_size,
                            &[TextRun {
                                len: self.placeholder.len(),
                                font: editor_font.clone(),
                                color: theme.text_dim.into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        frame.lines.push((origin, placeholder));
                    }
                    frame.lines.push((origin, shaped));
                    continue;
                }

                // Gutter: line number and diagnostic dot, on the line's first row.
                if first_seg {
                    let number = SharedString::from((row + 1).to_string());
                    let number_color = if debug_row == Some(row) {
                        theme.warning
                    } else if is_cursor_row {
                        theme.line_number_active
                    } else {
                        theme.line_number
                    };
                    let number_line = text_system.shape_line(
                        number.clone(),
                        font_size,
                        &[TextRun {
                            len: number.len(),
                            font: editor_font.clone(),
                            color: number_color.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let nx = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT + 18.) - number_line.width;
                    if let Some(breakpoint) = breakpoints.iter().find(|b| b.row == row) {
                        // A breakpoint covers its line number, as in JetBrains:
                        // a dot, "=" in it with a condition, a diamond for a logpoint.
                        let d = (line_height * 0.62).min(px(13.));
                        let right = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT + 18.);
                        let mark = Bounds::new(point(right - d, y + (line_height - d) / 2.), size(d, d));
                        if breakpoint.log_message.is_some() {
                            let r = d * 0.6;
                            let c = mark.center();
                            let mut diamond = gpui::PathBuilder::fill();
                            diamond.move_to(point(c.x, c.y - r));
                            diamond.line_to(point(c.x + r, c.y));
                            diamond.line_to(point(c.x, c.y + r));
                            diamond.line_to(point(c.x - r, c.y));
                            diamond.close();
                            if let Ok(path) = diamond.build() {
                                frame.gutter_paths.push((path, theme.error.into()));
                            }
                        } else {
                            frame.gutter_marks.push(fill(mark, theme.error).corner_radii(d / 2.));
                            if breakpoint.condition.is_some() {
                                let (w, h) = (d * 0.5, px(1.5));
                                for dy in [-d * 0.13, d * 0.13] {
                                    let bar = Bounds::new(point(mark.center().x - w / 2., mark.center().y + dy - h / 2.), size(w, h));
                                    frame.gutter_marks.push(fill(bar, theme.background));
                                }
                            }
                        }
                    } else {
                        frame.numbers.push((point(nx, y), number_line));
                    }

                    let folded = self.fold_under(row, cx).is_some();
                    let foldable = !folded
                        && self.gutter_hovered
                        && fold_regions.as_ref().is_some_and(|r| r.binary_search_by_key(&row, |region| region.start).is_ok());
                    if folded || foldable {
                        let icon_size = px(FOLD_ICON);
                        let x = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT) - icon_size;
                        let (icon, color) =
                            if folded { (Icon::ChevronRight, theme.text_muted) } else { (Icon::ChevronDown, theme.text_dim) };
                        frame.fold_icons.push((
                            Bounds::new(point(x, y + (line_height - icon_size) / 2.), size(icon_size, icon_size)),
                            icon.path(),
                            color.into(),
                        ));
                    }

                    if run_rows.contains(&row) {
                        let icon_size = px(RUN_ICON);
                        frame.fold_icons.push((
                            Bounds::new(point(bounds.left() + px(5.), y + (line_height - icon_size) / 2.), size(icon_size, icon_size)),
                            Icon::Play.path(),
                            theme.success.into(),
                        ));
                    } else if let Some(worst) = worst {
                        let dot = px(6.);
                        frame.gutter_marks.push(
                            fill(
                                Bounds::new(point(bounds.left() + px(10.), y + (line_height - dot) / 2.), size(dot, dot)),
                                severity_color(&theme, worst.severity),
                            )
                            .corner_radii(dot / 2.),
                        );
                    }
                }

                // Git change bars.
                let bar_x = bounds.left() + gutter_width - px(GIT_BAR_OFFSET) - px(1.5);
                // Hunks are sorted and don't overlap: just the ones at this row.
                let first_hunk = self.hunks.partition_point(|h| h.new_rows.end < row);
                for (hix, hunk) in self.hunks.iter().enumerate().skip(first_hunk).take_while(|(_, h)| h.new_rows.start <= row) {
                    let color: Hsla = match hunk.kind {
                        HunkKind::Added => theme.git_added,
                        HunkKind::Modified => theme.git_modified,
                        HunkKind::Deleted => theme.git_deleted,
                    }
                    .into();
                    // Staged changes are drawn faded.
                    let color = if self.hunk_staged.get(hix).copied().unwrap_or(false) { color.alpha(0.4) } else { color };
                    if hunk.kind == HunkKind::Deleted {
                        if hunk.new_rows.start == row && first_seg {
                            frame.gutter_marks.push(
                                fill(Bounds::new(point(bar_x - px(1.5), y - px(3.)), size(px(6.), px(6.))), color)
                                    .corner_radii(px(3.)),
                            );
                        }
                    } else if hunk.new_rows.contains(&row) {
                        let first = row == hunk.new_rows.start && first_seg;
                        let last = row + 1 == hunk.new_rows.end && last_seg;
                        let top = if first { y + px(1.) } else { y };
                        let bottom = if last { y + line_height - px(1.) } else { y + line_height };
                        frame.gutter_marks.push(
                            fill(Bounds::from_corners(point(bar_x, top), point(bar_x + px(3.), bottom)), color)
                                .corner_radii(gpui::Corners {
                                    top_left: if first { px(1.5) } else { px(0.) },
                                    top_right: if first { px(1.5) } else { px(0.) },
                                    bottom_left: if last { px(1.5) } else { px(0.) },
                                    bottom_right: if last { px(1.5) } else { px(0.) },
                                }),
                        );
                    }
                }

                // Inline diagnostic or blame after the line's text, on its last row.
                if !last_seg {
                    frame.lines.push((origin, shaped));
                    continue;
                }
                // A folded header ends with a `⋯` placeholder (click to unfold).
                let mut text_end = origin.x + shaped.width;
                if let Some(c) = conflicts.iter().find(|c| c.start == row) {
                    // Accept Current · Accept Incoming · Accept Both
                    let mut x = text_end + char_width * 3.;
                    for resolution in [Resolution::Current, Resolution::Incoming, Resolution::Both] {
                        let label = resolution.label();
                        let line = text_system.shape_line(
                            SharedString::from(label),
                            px(theme.ui_font_size - 0.5),
                            &[TextRun {
                                len: label.len(),
                                font: ui_font.clone(),
                                color: theme.text_muted.into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        let pill = Bounds::new(point(x - px(5.), y + px(2.)), size(line.width + px(10.), line_height - px(4.)));
                        frame.inline_bgs.push(fill(pill, theme.hover).corner_radii(px(4.)));
                        x += line.width + px(16.);
                        frame.inline_texts.push((point(pill.left() + px(5.), y), line));
                        self.conflict_hits.push((pill, c.start, resolution));
                    }
                } else if let Some(fold) = self.fold_under(row, cx) {
                    let label = self.fold_placeholder(&fold, cx);
                    let pill_line = text_system.shape_line(
                        SharedString::from(label.clone()),
                        font_size,
                        &[TextRun {
                            len: label.len(),
                            font: editor_font.clone(),
                            color: theme.text_muted.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let x = text_end + char_width * 0.6;
                    let pill = Bounds::new(point(x - px(4.), y + px(2.)), size(pill_line.width + px(8.), line_height - px(4.)));
                    frame.inline_bgs.push(fill(pill, theme.active).corner_radii(px(4.)));
                    frame.inline_texts.push((point(x, y), pill_line));
                    self.fold_pills.push((pill, fold.start));
                    text_end = pill.right();
                }
                if let Some(worst) = worst {
                    let color = severity_color(&theme, worst.severity);
                    // Multi-line messages (rust-analyzer's) fit on one line here;
                    // hovering shows them in full.
                    let mut message =
                        worst.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
                    if message.len() > 160 {
                        message.truncate(message.floor_char_boundary(160));
                        message.push('…');
                    }
                    let count = row_diags.iter().filter(|d| self.doc.read(cx).row_of(d.range.start) == row).count();
                    if count > 1 {
                        message.push_str(&format!("  (+{})", count - 1));
                    }
                    let msg_size = px(theme.ui_font_size - 0.5);
                    let msg_line = text_system.shape_line(
                        SharedString::from(message.clone()),
                        msg_size,
                        &[TextRun {
                            len: message.len(),
                            font: ui_font.clone(),
                            color: color.alpha(0.9),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    let x = text_end + char_width * 3.;
                    let pad = px(8.);
                    let pill_h = line_height - px(4.);
                    frame.inline_bgs.push(
                        fill(
                            Bounds::new(point(x - pad, y + px(2.)), size(msg_line.width + pad * 2., pill_h)),
                            color.alpha(0.1),
                        )
                        .corner_radii(px(4.)),
                    );
                    frame.inline_texts.push((point(x, y), msg_line));
                } else if focused
                    && self.selections.len() == 1
                    && is_cursor_row
                    && let Some((blame_row, _, blame)) = self.blame.as_ref()
                    && *blame_row == row
                {
                    // Inline blame for the cursor line (only when no diagnostic is shown).
                    let who = match &self.git_file {
                        _ if blame.uncommitted => "You".to_string(),
                        Some(g) if g.user_name.as_deref() == Some(blame.author.as_str()) => "You".to_string(),
                        _ => blame.author.clone(),
                    };
                    let text = if blame.uncommitted {
                        "You • Uncommitted changes".to_string()
                    } else {
                        let mut summary = blame.summary.clone();
                        if summary.len() > 80 {
                            summary.truncate(summary.floor_char_boundary(80));
                            summary.push('…');
                        }
                        format!("{who}, {} • {summary}", git::relative_time(blame.time))
                    };
                    let line = text_system.shape_line(
                        SharedString::from(text.clone()),
                        px(theme.ui_font_size - 0.5),
                        &[TextRun {
                            len: text.len(),
                            font: ui_font.clone(),
                            color: Hsla::from(theme.text_dim).alpha(0.9),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }],
                        None,
                    );
                    frame.inline_texts.push((point(text_end + char_width * 4., y), line));
                }

                frame.lines.push((origin, shaped));
            }
        }

        if !single {
            self.request_blame(window, cx);
            let number_right = bounds.left() + gutter_width - px(FOLD_ZONE_RIGHT + 18.);
            self.layout_sticky(&mut frame, bounds, first_row, text_left, number_right, window, cx);
        }

        // Scrollbar with diagnostic and search markers.
        if let Some((track, thumb)) = self.scrollbar_geometry(cx).filter(|_| !single) {
            let hovered_color = if self.scrollbar_drag.is_some() { 0.28 } else { 0.16 };
            frame.scrollbar.push(fill(thumb, Hsla::from(theme.text).alpha(hovered_color)).corner_radii(px(4.)));
            let rows = display_rows.max(1) as f32;
            for d in &self.doc.read(cx).diagnostics {
                let row = self.first_display_row(self.doc.read(cx).row_of(d.range.start)) as f32;
                let y = track.top() + track.size.height * (row / rows);
                frame.scrollbar.push(fill(
                    Bounds::new(point(track.right() - px(5.), y), size(px(4.), px(2.))),
                    severity_color(&theme, d.severity),
                ));
            }
            for hunk in self.hunks.iter() {
                let color: Hsla = match hunk.kind {
                    HunkKind::Added => theme.git_added,
                    HunkKind::Modified => theme.git_modified,
                    HunkKind::Deleted => theme.git_deleted,
                }
                .into();
                let (d0, d1) = (self.first_display_row(hunk.new_rows.start), self.first_display_row(hunk.new_rows.end));
                let y0 = track.top() + track.size.height * (d0 as f32 / rows);
                let h = (track.size.height * (d1.saturating_sub(d0) as f32 / rows)).max(px(2.));
                frame.scrollbar.push(fill(Bounds::new(point(track.left(), y0), size(px(2.), h)), color));
            }
            let mut last_y = None;
            for m in &self.search_matches {
                let row = self.first_display_row(self.doc.read(cx).row_of(m.start)) as f32;
                let y = (track.top() + track.size.height * (row / rows)).round();
                if last_y == Some(y) {
                    continue;
                }
                last_y = Some(y);
                frame.scrollbar.push(fill(
                    Bounds::new(point(track.left() + px(3.), y), size(px(6.), px(2.))),
                    Hsla::from(theme.search_active).alpha(0.8),
                ));
            }
            for s in &self.selections {
                let row = self.first_display_row(self.doc.read(cx).row_of(s.head)) as f32;
                let y = track.top() + track.size.height * (row / rows);
                frame.scrollbar.push(fill(
                    Bounds::new(point(track.left() + px(1.), y), size(px(4.), px(2.))),
                    theme.accent,
                ));
            }
        }

        if self.blink_task.is_none() {
            self.restart_blink(window, cx);
        }
        frame
    }
}

fn severity_color(theme: &Theme, severity: Severity) -> Hsla {
    match severity {
        Severity::Error => theme.error,
        Severity::Warning => theme.warning,
        Severity::Info => theme.info,
    }
    .into()
}

fn closing_pair(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        '`' => Some('`'),
        _ => None,
    }
}

/// The indentation a file already uses, if it has indented lines.
fn detect_indent(doc: &Document) -> Option<String> {
    let (mut tabs, mut spaces2, mut spaces4) = (0, 0, 0);
    for row in 0..doc.line_count().min(500) {
        let t = doc.line_text(row);
        if t.starts_with('\t') {
            tabs += 1;
        } else {
            let n = t.len() - t.trim_start_matches(' ').len();
            if n > 0 && !t.trim().is_empty() {
                if n.is_multiple_of(4) {
                    spaces4 += 1;
                } else if n.is_multiple_of(2) {
                    spaces2 += 1;
                }
            }
        }
    }
    if tabs + spaces2 + spaces4 == 0 {
        None
    } else if tabs > spaces2 + spaces4 {
        Some("\t".into())
    } else if spaces2 > spaces4 / 2 && spaces2 > 0 {
        Some("  ".into())
    } else {
        Some("    ".into())
    }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum CharKind {
    Word,
    Space,
    Punct,
    Newline,
}

fn char_kind(c: char) -> CharKind {
    if c == '\n' {
        CharKind::Newline
    } else if c.is_whitespace() {
        CharKind::Space
    } else if c.is_alphanumeric() || c == '_' || c == '$' {
        CharKind::Word
    } else {
        CharKind::Punct
    }
}

fn word_left(doc: &Document, mut o: usize) -> usize {
    while let Some(c) = doc.char_before(o) {
        if char_kind(c) == CharKind::Space { o -= c.len_utf8() } else { break }
    }
    if let Some(c) = doc.char_before(o) {
        if c == '\n' {
            return o - 1;
        }
        let k = char_kind(c);
        while let Some(c) = doc.char_before(o) {
            if char_kind(c) == k { o -= c.len_utf8() } else { break }
        }
    }
    o
}

fn word_right(doc: &Document, mut o: usize) -> usize {
    while let Some(c) = doc.char_at(o) {
        if char_kind(c) == CharKind::Space { o += c.len_utf8() } else { break }
    }
    if let Some(c) = doc.char_at(o) {
        if c == '\n' {
            return o + 1;
        }
        let k = char_kind(c);
        while let Some(c) = doc.char_at(o) {
            if char_kind(c) == k { o += c.len_utf8() } else { break }
        }
    }
    o
}

fn word_range(doc: &Document, offset: usize) -> Range<usize> {
    let kind = match (doc.char_at(offset), doc.char_before(offset)) {
        (Some(c), _) if char_kind(c) == CharKind::Word => CharKind::Word,
        (_, Some(c)) if char_kind(c) == CharKind::Word => CharKind::Word,
        (Some(c), _) if c != '\n' => char_kind(c),
        _ => return offset..offset,
    };
    let mut start = offset;
    while let Some(c) = doc.char_before(start) {
        if char_kind(c) == kind { start -= c.len_utf8() } else { break }
    }
    let mut end = offset;
    while let Some(c) = doc.char_at(end) {
        if char_kind(c) == kind { end += c.len_utf8() } else { break }
    }
    start..end
}

/// One display row of a line: its slice of the display text, shaped.
struct RowShape {
    line: ShapedLine,
    dl: DisplayLine,
    range: Range<usize>,
    /// Wrap indent before the text.
    indent: Pixels,
    /// Whether this is the line's last row.
    last: bool,
}

/// The part of `runs` (covering a whole line) that falls in `range`.
fn slice_runs(runs: &[TextRun], range: Range<usize>) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut pos = 0;
    for run in runs {
        let (start, end) = (pos.max(range.start), (pos + run.len).min(range.end));
        pos += run.len;
        if start < end {
            out.push(TextRun { len: end - start, ..run.clone() });
        }
    }
    out
}

/// Runs for a line with inlay hints: the hints' text in `color`, plain, and
/// a run for hints after the last character (the text's runs end there).
fn inlay_runs(runs: Vec<TextRun>, dl: &DisplayLine, font: Font, color: Hsla) -> Vec<TextRun> {
    let mut runs = runs;
    let total: usize = runs.iter().map(|r| r.len).sum();
    if total < dl.text.len() {
        runs.push(TextRun { len: dl.text.len() - total, font, color, background_color: None, underline: None, strikethrough: None });
    }
    let in_hint = |at: usize| dl.inlays.iter().any(|s| s.whole.contains(&at));
    let mut cuts: Vec<usize> = dl.inlays.iter().flat_map(|s| [s.whole.start, s.whole.end]).collect();
    cuts.sort_unstable();
    let mut out = Vec::with_capacity(runs.len() + cuts.len());
    let mut pos = 0;
    for run in runs {
        let end = pos + run.len;
        let mut start = pos;
        for &cut in cuts.iter().filter(|&&c| c > pos && c < end).chain(std::iter::once(&end)) {
            let mut piece = TextRun { len: cut - start, ..run.clone() };
            if in_hint(start) {
                piece.color = color;
                piece.underline = None;
                piece.background_color = None;
            }
            out.push(piece);
            start = cut;
        }
        pos = end;
    }
    out
}

/// A buffer line prepared for display: tabs are expanded to spaces, with a
/// mapping between buffer columns and display columns (both in bytes).
pub struct DisplayLine {
    pub text: String,
    map: Option<Vec<usize>>,
    /// Inlay hints shown in `text`: the inserted text (padding included), the
    /// label within it, and the buffer column they sit before.
    pub inlays: Vec<InlaySpan>,
}

pub struct InlaySpan {
    pub whole: Range<usize>,
    pub label: Range<usize>,
    pub column: usize,
}

impl DisplayLine {
    pub fn new(line: &str, tab_size: usize) -> Self {
        Self::with_inlays(line, tab_size, &[])
    }

    /// With inlay hints (buffer column, label, pad left, pad right), sorted
    /// by column. A buffer column maps to the display column before its
    /// hints, so carets sit left of them.
    pub fn with_inlays(line: &str, tab_size: usize, inlays: &[(usize, &str, bool, bool)]) -> Self {
        if !line.contains('\t') && inlays.is_empty() {
            return Self { text: line.to_string(), map: None, inlays: Vec::new() };
        }
        let mut text = String::with_capacity(line.len() + 16);
        let mut map = Vec::with_capacity(line.len() + 1);
        let mut spans = Vec::new();
        let mut next = 0;
        let mut insert = |at: usize, text: &mut String| {
            while let Some(&(column, label, pad_left, pad_right)) = inlays.get(next).filter(|i| i.0 <= at) {
                let start = text.len();
                text.push_str(if pad_left { " " } else { "" });
                let label_start = text.len();
                text.push_str(label);
                let label_end = text.len();
                text.push_str(if pad_right { " " } else { "" });
                spans.push(InlaySpan { whole: start..text.len(), label: label_start..label_end, column });
                next += 1;
            }
        };
        let mut col = 0;
        for (i, ch) in line.char_indices() {
            let before = text.len();
            insert(i, &mut text);
            for _ in 0..ch.len_utf8() {
                map.push(before);
            }
            if ch == '\t' {
                let n = tab_size - col % tab_size;
                text.extend(std::iter::repeat_n(' ', n));
                col += n;
            } else {
                text.push(ch);
                col += 1;
            }
        }
        let before = text.len();
        insert(line.len(), &mut text);
        map.push(before);
        Self { text, map: Some(map), inlays: spans }
    }

    pub fn to_display(&self, col: usize) -> usize {
        match &self.map {
            None => col.min(self.text.len()),
            Some(m) => m[col.min(m.len() - 1)],
        }
    }

    pub fn to_buffer(&self, display: usize) -> usize {
        // On a hint: the column it sits before.
        if let Some(span) = self.inlays.iter().find(|s| s.whole.start < display && display <= s.whole.end) {
            return span.column;
        }
        match &self.map {
            None => display.min(self.text.len()),
            Some(m) => m.partition_point(|&d| d < display).min(m.len() - 1),
        }
    }
}

// ---- Git ------------------------------------------------------------------------

impl Editor {
    /// The change marker under a gutter position (display rows as a fraction, so
    /// deletion markers on a row boundary can be hit).
    fn hunk_at(&self, row: f32, cx: &App) -> Option<usize> {
        let deleted = self.hunks.iter().position(|h| {
            h.kind == HunkKind::Deleted && (row - self.first_display_row(h.new_rows.start) as f32).abs() < 0.35
        });
        deleted.or_else(|| {
            let r = self.line_of_display_row(row.floor().max(0.) as usize, cx).0;
            self.hunks.iter().position(|h| h.kind != HunkKind::Deleted && h.new_rows.contains(&r))
        })
    }

    /// The change at the cursor, preferring an open popover.
    fn current_hunk(&self, cx: &App) -> Option<usize> {
        self.open_hunk.or_else(|| {
            let row = self.doc.read(cx).row_of(self.primary_selection().head);
            self.hunks.iter().position(|h| {
                h.new_rows.contains(&row) || (h.kind == HunkKind::Deleted && (h.new_rows.start == row || h.new_rows.start == row + 1))
            })
        })
    }

    fn rollback_hunk(&mut self, _: &RollbackHunk, _: &mut Window, cx: &mut Context<Self>) {
        let Some(hunk) = self.current_hunk(cx).and_then(|i| self.hunks.get(i)).cloned() else {
            return;
        };
        let line_count = self.doc.read(cx).line_count();
        let start = self.doc.read(cx).line_start(hunk.new_rows.start.min(line_count));
        let end = if hunk.new_rows.end >= line_count { self.doc.read(cx).len() } else { self.doc.read(cx).line_start(hunk.new_rows.end) };
        let mut text = hunk.old_text.clone();
        // Keep the file's final-newline state when the hunk reaches the end.
        if end == self.doc.read(cx).len() && !self.doc.read(cx).rope().to_string().ends_with('\n') && text.ends_with('\n') {
            text.pop();
        }
        self.open_hunk = None;
        self.edit_and_remap(vec![(start..end, text)], EditKind::Other, cx);
    }

    fn stage_hunk(&mut self, _: &StageHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.stage_current_hunk(true, cx);
    }

    fn unstage_hunk(&mut self, _: &UnstageHunk, _: &mut Window, cx: &mut Context<Self>) {
        self.stage_current_hunk(false, cx);
    }

    /// Stages (or unstages) the change at the caret, as the buffer shows it,
    /// unsaved edits included.
    fn stage_current_hunk(&mut self, stage: bool, cx: &mut Context<Self>) {
        let (Some(hunk), Some(git_file)) = (self.current_hunk(cx).and_then(|i| self.hunks.get(i)).cloned(), self.git_file.clone()) else {
            cx.emit(EditorLspEvent::Message("No change at the caret".into()));
            return;
        };
        let (rel, text, rows) = (git_file.rel, self.text(cx), hunk.new_rows);
        let op = if stage {
            crate::git_panel::GitOp::StageLines { rel, text, rows }
        } else {
            crate::git_panel::GitOp::UnstageLines { rel, text: Some(text), rows }
        };
        cx.emit(EditorGitEvent(op));
    }

    fn jump_to_change(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.hunks.is_empty() {
            return;
        }
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        let starts: Vec<usize> = self.hunks.iter().map(|h| h.new_rows.start).collect();
        let target = if forward {
            starts.iter().copied().find(|&s| s > row).unwrap_or(starts[0])
        } else {
            starts.iter().rev().copied().find(|&s| s < row).unwrap_or(*starts.last().unwrap())
        };
        let offset = self.doc.read(cx).line_start(target.min(self.doc.read(cx).line_count() - 1));
        let ix = self.hunks.iter().position(|h| h.new_rows.start == target);
        if self.open_hunk.is_some() {
            self.open_hunk = ix;
        }
        self.select_range(offset..offset, cx);
    }

    fn next_change(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_change(true, cx);
    }

    fn previous_change(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_change(false, cx);
    }

    fn toggle_inline_blame(&mut self, _: &ToggleInlineBlame, _: &mut Window, cx: &mut Context<Self>) {
        let on = cx.try_global::<InlineBlame>().is_none_or(|b| b.0);
        cx.set_global(InlineBlame(!on));
        cx.refresh_windows();
    }

    /// Fetches blame for the cursor line after it settles for a moment.
    fn request_blame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let enabled = cx.try_global::<InlineBlame>().is_none_or(|b| b.0);
        let Some(git_file) = self.git_file.clone().filter(|_| enabled) else {
            return;
        };
        if !self.focus_handle.is_focused(window) || self.selections.len() != 1 || self.diff_base.is_none() {
            return;
        }
        let row = self.doc.read(cx).row_of(self.primary_selection().head);
        let version = self.doc.read(cx).version();
        if self.blame_request == Some((row, version)) {
            return;
        }
        self.blame_request = Some((row, version));
        let rope = self.doc.read(cx).rope().clone();
        self.blame_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(350)).await;
            let blame = cx
                .background_executor()
                .spawn(async move { git::blame_line(&git_file.root, &git_file.rel, row, &rope.to_string()) })
                .await;
            this.update(cx, |editor, cx| {
                if editor.blame_request == Some((row, version)) {
                    editor.blame = blame.map(|b| (row, version, b));
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn render_hunk_popover(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let ix = self.open_hunk?;
        let hunk = self.hunks.get(ix)?.clone();
        let layout = self.layout.as_ref()?;
        let theme = Theme::get(cx).clone();
        let anchor_row = self.first_display_row(if hunk.kind == HunkKind::Deleted { hunk.new_rows.start } else { hunk.new_rows.end });
        let top = layout.line_height * anchor_row as f32 - self.scroll.y + px(2.);
        let left = layout.gutter_width - px(4.);
        let width = (layout.bounds.size.width - left - px(28.)).min(px(720.)).max(px(240.));
        let staged = self.hunk_staged.get(ix).copied().unwrap_or(false);
        let (title, color): (String, Hsla) = match hunk.kind {
            HunkKind::Added => (format!("{} added line{}", hunk.new_rows.len(), plural(hunk.new_rows.len())), theme.git_added.into()),
            HunkKind::Modified => (format!("{} changed line{}", hunk.new_rows.len(), plural(hunk.new_rows.len())), theme.git_modified.into()),
            HunkKind::Deleted => (format!("{} deleted line{}", hunk.old_rows.len(), plural(hunk.old_rows.len())), theme.git_deleted.into()),
        };
        let button = |id: &'static str, icon: Icon, tip: &'static str| {
            div()
                .id(id)
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| crate::ui::tooltip_view(tip, cx))
                .child(crate::ui::icon_el(icon, px(14.), theme.text_muted.into()))
        };
        let old_lines: Vec<String> = hunk.old_text.lines().map(|l| l.replace('\t', "    ")).collect();
        Some(
            div()
                .id("hunk-popover")
                .absolute()
                .top(top)
                .left(left)
                .w(width)
                .flex()
                .flex_col()
                .bg(theme.elevated)
                .border_1()
                .border_color(Hsla::from(theme.border).alpha(0.85))
                .rounded(px(10.))
                .shadow(crate::ui::soft_shadow(&theme, 2))
                .overflow_hidden()
                .text_size(px(12.5))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(32.))
                        .px_2()
                        .border_b_1()
                        .border_color(theme.border_subtle)
                        .child(div().size(px(8.)).rounded_full().bg(color))
                        .child(div().pl_1().text_color(theme.text_muted).child(title))
                        .when(staged, |el| el.child(div().text_size(px(11.)).text_color(theme.text_dim).child("· Staged")))
                        .child(div().flex_1())
                        .child(if staged {
                            button("hunk-stage", Icon::Minus, "Unstage Lines").on_click(cx.listener(|e, _, _, cx| e.stage_current_hunk(false, cx)))
                        } else {
                            button("hunk-stage", Icon::Plus, "Stage Lines").on_click(cx.listener(|e, _, _, cx| e.stage_current_hunk(true, cx)))
                        })
                        .child(button("hunk-rollback", Icon::Undo, "Rollback Lines").on_click(cx.listener(|e, _, w, cx| e.rollback_hunk(&RollbackHunk, w, cx))))
                        .child(button("hunk-prev", Icon::ChevronUp, "Previous Change").on_click(cx.listener(|e, _, _, cx| e.jump_to_change(false, cx))))
                        .child(button("hunk-next", Icon::ChevronDown, "Next Change").on_click(cx.listener(|e, _, _, cx| e.jump_to_change(true, cx))))
                        .child(button("hunk-close", Icon::X, "Close (Esc)").on_click(cx.listener(|e, _, _, cx| {
                            e.open_hunk = None;
                            cx.notify();
                        }))),
                )
                .child(if old_lines.is_empty() {
                    div()
                        .px_3()
                        .py_2()
                        .text_color(theme.text_dim)
                        .child("New lines — nothing to show from HEAD")
                        .into_any_element()
                } else {
                    div()
                        .id("hunk-old-lines")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .py_1()
                        .font_family(theme.mono_font.clone())
                        .text_size(px(theme.editor_font_size - 1.))
                        .children(old_lines.into_iter().map(|line| {
                            div()
                                .flex()
                                .px_2()
                                .bg(Hsla::from(theme.git_deleted).alpha(0.1))
                                .child(div().w(px(16.)).flex_shrink_0().text_color(theme.git_deleted).child("−"))
                                .child(div().text_color(theme.text_muted).whitespace_nowrap().child(line))
                        }))
                        .into_any_element()
                })
                .into_any_element(),
        )
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ---- JetBrains-style editing actions ----------------------------------------

impl Editor {
    fn delete_line(&mut self, _: &DeleteLine, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            self.set_text("", cx);
            return;
        }
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            let mut start = self.doc.read(cx).line_start(block.start);
            let mut end = self.doc.read(cx).line_start(block.end);
            if block.end >= self.doc.read(cx).line_count() {
                // Last line: remove the newline before it instead.
                end = self.doc.read(cx).len();
                start = start.saturating_sub(1);
            }
            edits.push((start..end, String::new()));
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn extend_selection(&mut self, _: &ExtendSelection, _: &mut Window, cx: &mut Context<Self>) {
        let before = self.selections.clone();
        let len = self.doc.read(cx).len();
        let expanded: Vec<Selection> = before
            .iter()
            .map(|s| {
                let r = s.range();
                if r.is_empty() {
                    let w = word_range(self.doc.read(cx), r.start);
                    if !w.is_empty() {
                        return Selection { anchor: w.start, head: w.end };
                    }
                }
                let next = self.doc.read(cx).syntax().expand(r.clone()).unwrap_or(0..len);
                Selection { anchor: next.start, head: next.end }
            })
            .collect();
        if expanded == before {
            return;
        }
        self.expand_stack.push(before);
        self.expanding = true;
        self.set_selections(expanded, self.primary, cx);
    }

    fn shrink_selection(&mut self, _: &ShrinkSelection, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.expand_stack.pop() {
            self.expanding = true;
            let primary = self.primary.min(previous.len() - 1);
            self.set_selections(previous, primary, cx);
        }
    }

    fn unselect_occurrence(&mut self, _: &UnselectOccurrence, _: &mut Window, cx: &mut Context<Self>) {
        if self.selections.len() < 2 {
            return;
        }
        let mut selections = self.selections.clone();
        selections.remove(self.primary);
        let primary = self.primary.saturating_sub(1).min(selections.len() - 1);
        self.set_selections(selections, primary, cx);
    }

    fn select_all_occurrences(&mut self, _: &SelectAllOccurrences, _: &mut Window, cx: &mut Context<Self>) {
        let p = self.primary_selection();
        let range = if p.is_empty() { word_range(self.doc.read(cx), p.head) } else { p.range() };
        if range.is_empty() {
            return;
        }
        let needle = self.doc.read(cx).slice(range.clone());
        let haystack = self.doc.read(cx).rope().to_string();
        let selections: Vec<Selection> = haystack
            .match_indices(&needle)
            .map(|(i, _)| Selection { anchor: i, head: i + needle.len() })
            .collect();
        let primary = selections.iter().position(|s| s.anchor == range.start).unwrap_or(0);
        self.set_selections(selections, primary, cx);
    }

    fn toggle_case(&mut self, _: &ToggleCase, _: &mut Window, cx: &mut Context<Self>) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let range = if s.is_empty() { word_range(doc, s.head) } else { s.range() };
            let text = doc.slice(range.clone());
            let new = if text.chars().any(char::is_lowercase) {
                text.to_uppercase()
            } else {
                text.to_lowercase()
            };
            (range, new, None)
        });
    }

    fn join_lines(&mut self, _: &JoinLines, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            return;
        }
        let mut edits = Vec::new();
        for block in self.selected_row_blocks(cx) {
            let last = if block.len() == 1 { block.start + 1 } else { block.end - 1 };
            for row in block.start..last.min(self.doc.read(cx).line_count() - 1) {
                let current = self.doc.read(cx).line_text(row);
                let next = self.doc.read(cx).line_text(row + 1);
                let trimmed_current = current.trim_end();
                let next_trimmed = next.trim_start();
                let start = self.doc.read(cx).line_start(row) + trimmed_current.len();
                let end = self.doc.read(cx).line_start(row + 1) + (next.len() - next_trimmed.len());
                let glue = if next_trimmed.is_empty()
                    || trimmed_current.is_empty()
                    || next_trimmed.starts_with([')', ']', '}', ',', ';', '.'])
                    || trimmed_current.ends_with(['(', '[', '{'])
                {
                    ""
                } else {
                    " "
                };
                edits.push((start..end, glue.to_string()));
            }
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    fn newline_above(&mut self, _: &NewlineAbove, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_single_line() {
            cx.propagate();
            return;
        }
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let row = doc.row_of(s.head);
            let start = doc.line_start(row);
            let line = doc.line_text(row);
            let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
            let len = indent.len();
            (start..start, format!("{indent}\n"), Some(len))
        });
    }

    fn toggle_block_comment(&mut self, _: &ToggleBlockComment, _: &mut Window, cx: &mut Context<Self>) {
        let Some((open, close)) = self.doc.read(cx).language.block_comment() else {
            return;
        };
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let range = s.range();
            let text = doc.slice(range.clone());
            if s.is_empty() {
                return (range, format!("{open}{close}"), Some(open.len()));
            }
            let (o, c) = (open.trim_end(), close.trim_start());
            if text.starts_with(o) && text.ends_with(c) && text.len() >= o.len() + c.len() {
                let inner = &text[o.len()..text.len() - c.len()];
                let inner = inner.strip_prefix(' ').unwrap_or(inner);
                let inner = inner.strip_suffix(' ').unwrap_or(inner);
                (range, inner.to_string(), None)
            } else {
                (range, format!("{open}{text}{close}"), None)
            }
        });
    }
}

// ---- IME / platform text input ---------------------------------------------

impl Editor {
    fn to_utf16(&self, offset: usize, cx: &App) -> usize {
        let r = self.doc.read(cx).rope();
        r.char_to_utf16_cu(r.byte_to_char(offset.min(r.len_bytes())))
    }

    fn from_utf16(&self, offset: usize, cx: &App) -> usize {
        let r = self.doc.read(cx).rope();
        let ch = r.utf16_cu_to_char(offset.min(r.len_utf16_cu()));
        r.char_to_byte(ch)
    }

    fn range_from_utf16(&self, r: &Range<usize>, cx: &App) -> Range<usize> {
        self.from_utf16(r.start, cx)..self.from_utf16(r.end, cx)
    }

    fn range_to_utf16(&self, r: &Range<usize>, cx: &App) -> Range<usize> {
        self.to_utf16(r.start, cx)..self.to_utf16(r.end, cx)
    }
}

impl EntityInputHandler for Editor {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16, cx);
        actual_range.replace(self.range_to_utf16(&range, cx));
        Some(self.doc.read(cx).slice(range))
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, cx: &mut Context<Self>) -> Option<UTF16Selection> {
        let s = self.primary_selection();
        Some(UTF16Selection {
            range: self.range_to_utf16(&s.range(), cx),
            reversed: s.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range.as_ref().map(|r| self.range_to_utf16(r, cx))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let explicit = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r, cx))
            .or(self.marked_range.clone());
        match explicit {
            Some(range) => {
                // IME commit: replace the composed range only.
                let before = self.selections.clone();
                let new = self.doc.as_mut(cx).edit(vec![(range, text.to_string())], &before, EditKind::Other);
                let end = new[0].end;
                self.set_selections(vec![Selection::cursor(end)], 0, cx);
                self.after_edit(cx);
            }
            None => self.insert(text, cx),
        }
        self.marked_range = None;
        self.restart_blink(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r, cx))
            .or(self.marked_range.clone())
            .unwrap_or_else(|| self.primary_selection().range());
        let before = self.selections.clone();
        let new = self.doc.as_mut(cx).edit(vec![(range, new_text.to_string())], &before, EditKind::Other);
        let new_range = new[0].clone();
        let sel = match new_selected_range_utf16 {
            Some(r) => {
                let base = self.to_utf16(new_range.start, cx);
                let s = self.from_utf16(base + r.start, cx);
                let e = self.from_utf16(base + r.end, cx);
                Selection { anchor: s, head: e }
            }
            None => Selection::cursor(new_range.end),
        };
        self.set_selections(vec![sel], 0, cx);
        self.marked_range = (!new_text.is_empty()).then_some(new_range);
        self.after_edit(cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.sync_wrap(window, cx);
        let layout = self.layout.clone()?;
        let range = self.range_from_utf16(&range_utf16, cx);
        let drow = self.display_row_of(range.start, cx);
        let y = layout.bounds.top() + layout.line_height * drow as f32 - self.scroll.y;
        let x0 = self.x_for_offset(range.start, window, cx);
        let end = range.end.max(range.start);
        let x1 = if self.display_row_of(end, cx) == drow { self.x_for_offset(end, window, cx) } else { x0 };
        let left = layout.text_left - self.scroll.x;
        Some(Bounds::from_corners(
            point(left + x0, y),
            point(left + x1, y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_position(point, window, cx);
        Some(self.to_utf16(offset, cx))
    }
}

impl Focusable for Editor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(path) = self.doc.read(cx).image.clone() {
            return self.render_image(path, cx).into_any_element();
        }
        let popover = self.render_hunk_popover(cx);
        let lsp_popovers = self.render_lsp_popovers(window, cx);
        let markdown = self.mode == EditorMode::Full && self.doc.read(cx).language == crate::syntax::Language::Markdown;
        let preview = (markdown && self.md_preview).then(|| self.render_markdown_preview(cx).into_any_element());
        let preview_toggle = markdown.then(|| {
            let theme = Theme::get(cx);
            let on = self.md_preview;
            div()
                .id("markdown-preview-toggle")
                .absolute()
                .top(px(6.))
                .right(px(SCROLLBAR_WIDTH + 6.))
                .size(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover_fill(if on { Hsla::from(theme.accent_soft) } else { Hsla::from(theme.elevated) }, theme.hover)
                .tooltip(move |_, cx| crate::ui::tooltip_view(if on { "Hide Preview" } else { "Show Preview" }, cx))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|e, _, window, cx| e.toggle_markdown_preview(&ToggleMarkdownPreview, window, cx)))
                .child(crate::ui::icon_el(
                    crate::assets::Icon::SplitRight,
                    px(13.),
                    if on { theme.accent.into() } else { theme.text_muted.into() },
                ))
        });
        let theme = Theme::get(cx);
        let view = div()
            .id("editor")
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .map(|el| match self.mode {
                EditorMode::Full => el.size_full().bg(theme.background),
                EditorMode::SingleLine => el.w_full().h(px(20.)),
                EditorMode::MultiLine => el.size_full(),
            })
            .on_action(cx.listener(Self::move_left))
            .on_action(cx.listener(Self::move_right))
            .on_action(cx.listener(Self::move_up))
            .on_action(cx.listener(Self::move_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::move_word_left))
            .on_action(cx.listener(Self::move_word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::move_line_start))
            .on_action(cx.listener(Self::move_line_end))
            .on_action(cx.listener(Self::select_line_start))
            .on_action(cx.listener(Self::select_line_end))
            .on_action(cx.listener(Self::move_doc_start))
            .on_action(cx.listener(Self::move_doc_end))
            .on_action(cx.listener(Self::select_doc_start))
            .on_action(cx.listener(Self::select_doc_end))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_word_left))
            .on_action(cx.listener(Self::delete_word_right))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::newline_below))
            .on_action(cx.listener(Self::indent))
            .on_action(cx.listener(Self::outdent))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::select_line))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_action(cx.listener(Self::toggle_comment))
            .on_action(cx.listener(Self::duplicate_line))
            .on_action(cx.listener(Self::move_line_up))
            .on_action(cx.listener(Self::move_line_down))
            .on_action(cx.listener(Self::select_next_occurrence))
            .on_action(cx.listener(Self::add_cursor_above))
            .on_action(cx.listener(Self::add_cursor_below))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::delete_line))
            .on_action(cx.listener(Self::extend_selection))
            .on_action(cx.listener(Self::shrink_selection))
            .on_action(cx.listener(Self::unselect_occurrence))
            .on_action(cx.listener(Self::select_all_occurrences))
            .on_action(cx.listener(Self::toggle_case))
            .on_action(cx.listener(Self::toggle_inlay_hints))
            .on_action(cx.listener(Self::convert_to_upper_case))
            .on_action(cx.listener(Self::convert_to_lower_case))
            .on_action(cx.listener(Self::convert_to_title_case))
            .on_action(cx.listener(Self::convert_to_snake_case))
            .on_action(cx.listener(Self::convert_to_kebab_case))
            .on_action(cx.listener(Self::convert_to_camel_case))
            .on_action(cx.listener(Self::convert_to_pascal_case))
            .on_action(cx.listener(Self::sort_lines_ascending))
            .on_action(cx.listener(Self::sort_lines_descending))
            .on_action(cx.listener(Self::reverse_lines))
            .on_action(cx.listener(Self::remove_duplicate_lines))
            .on_action(cx.listener(Self::join_lines))
            .on_action(cx.listener(Self::newline_above))
            .on_action(cx.listener(Self::toggle_block_comment))
            .on_action(cx.listener(Self::rollback_hunk))
            .on_action(cx.listener(Self::stage_hunk))
            .on_action(cx.listener(Self::unstage_hunk))
            .on_action(cx.listener(Self::accept_current))
            .on_action(cx.listener(Self::accept_incoming))
            .on_action(cx.listener(Self::accept_both))
            .on_action(cx.listener(Self::next_change))
            .on_action(cx.listener(Self::previous_change))
            .on_action(cx.listener(Self::toggle_inline_blame))
            .on_action(cx.listener(Self::toggle_soft_wrap))
            .on_action(cx.listener(Self::toggle_markdown_preview))
            .on_action(cx.listener(Self::show_completions))
            .on_action(cx.listener(Self::quick_documentation))
            .on_action(cx.listener(Self::go_to_declaration))
            .on_action(cx.listener(Self::go_to_implementation))
            .on_action(cx.listener(Self::go_to_type_definition))
            .on_action(cx.listener(Self::find_usages))
            .on_action(cx.listener(Self::rename_symbol))
            .on_action(cx.listener(Self::show_context_actions))
            .on_action(cx.listener(Self::reformat_code))
            .on_action(cx.listener(Self::collapse_region))
            .on_action(cx.listener(Self::expand_region))
            .on_action(cx.listener(Self::collapse_all))
            .on_action(cx.listener(Self::expand_all))
            .on_action(cx.listener(Self::run_at_caret))
            .on_action(cx.listener(Self::debug_at_caret))
            .on_action(cx.listener(Self::toggle_breakpoint))
            .on_action(cx.listener(Self::submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_hover(cx.listener(|editor, hovered: &bool, window, cx| {
                if !*hovered {
                    editor.pointer_left(window, cx);
                }
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .relative()
            .child(EditorElement { editor: cx.entity() })
            .children(popover)
            .children(lsp_popovers)
            .children(preview_toggle);
        match preview {
            // Side by side: the text keeps its own events; the preview only scrolls and links.
            Some(preview) => div().size_full().flex().child(view.flex_1().min_w_0()).child(preview).into_any_element(),
            None => view.into_any_element(),
        }
    }
}

impl Editor {
    /// An image file: the picture, scaled down to fit, with its format,
    /// dimensions and size underneath. No text input reaches it.
    fn render_image(&mut self, path: std::path::PathBuf, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        let info = self.image_info.get_or_insert_with(|| crate::preview::ImageInfo::read(&path)).label();
        div()
            .id("image")
            .key_context("ImagePreview")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .p_6()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gpui::img(path).size_full().object_fit(gpui::ObjectFit::ScaleDown)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(30.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_t_1()
                    .border_color(theme.border_subtle)
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(info),
            )
    }
}

// ---- Element ---------------------------------------------------------------------

#[derive(Default)]
pub struct FrameLayout {
    text_bounds: Bounds<Pixels>,
    line_height: Pixels,
    backgrounds: Vec<PaintQuad>,
    guides: Vec<PaintQuad>,
    search: Vec<PaintQuad>,
    selections: Vec<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    cursors: Vec<PaintQuad>,
    numbers: Vec<(Point<Pixels>, ShapedLine)>,
    gutter_marks: Vec<PaintQuad>,
    /// Shapes that aren't rectangles (logpoints' diamonds).
    gutter_paths: Vec<(gpui::Path<Pixels>, Hsla)>,
    /// Fold arrows: bounds, icon and color.
    fold_icons: Vec<(Bounds<Pixels>, &'static str, Hsla)>,
    inline_bgs: Vec<PaintQuad>,
    inline_texts: Vec<(Point<Pixels>, ShapedLine)>,
    scrollbar: Vec<PaintQuad>,
    sticky_bgs: Vec<PaintQuad>,
    sticky_lines: Vec<(Point<Pixels>, ShapedLine)>,
    sticky_numbers: Vec<(Point<Pixels>, ShapedLine)>,
    hitbox: Option<Hitbox>,
}

struct EditorElement {
    editor: Entity<Editor>,
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
    type RequestLayoutState = ();
    type PrepaintState = FrameLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> FrameLayout {
        let mut frame = self
            .editor
            .update(cx, |editor, cx| editor.layout_frame(bounds, window, cx));
        frame.hitbox = Some(window.insert_hitbox(frame.text_bounds, HitboxBehavior::Normal));
        frame
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut FrameLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.editor.clone()), cx);
        if let Some(hitbox) = frame.hitbox.as_ref() {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }

        let line_height = frame.line_height;
        for q in frame.backgrounds.drain(..) {
            window.paint_quad(q);
        }
        window.with_content_mask(Some(ContentMask { bounds: frame.text_bounds }), |window| {
            for q in frame.guides.drain(..) {
                window.paint_quad(q);
            }
            for q in frame.search.drain(..) {
                window.paint_quad(q);
            }
            for q in frame.selections.drain(..) {
                window.paint_quad(q);
            }
            for q in frame.inline_bgs.drain(..) {
                window.paint_quad(q);
            }
            for (origin, line) in &frame.lines {
                line.paint(*origin, line_height, window, cx).ok();
            }
            for (origin, line) in &frame.inline_texts {
                line.paint(*origin, line_height, window, cx).ok();
            }
            for q in frame.cursors.drain(..) {
                window.paint_quad(q);
            }
        });
        for (origin, line) in &frame.numbers {
            line.paint(*origin, line_height, window, cx).ok();
        }
        for q in frame.gutter_marks.drain(..) {
            window.paint_quad(q);
        }
        for (path, color) in frame.gutter_paths.drain(..) {
            window.paint_path(path, color);
        }
        for (bounds, icon, color) in frame.fold_icons.drain(..) {
            window.paint_svg(bounds, icon.into(), gpui::TransformationMatrix::unit(), color, cx).ok();
        }
        // Sticky scroll's pinned lines, over everything but the scrollbar.
        for q in frame.sticky_bgs.drain(..) {
            window.paint_quad(q);
        }
        window.with_content_mask(Some(ContentMask { bounds: frame.text_bounds }), |window| {
            for (origin, line) in &frame.sticky_lines {
                line.paint(*origin, line_height, window, cx).ok();
            }
        });
        for (origin, line) in &frame.sticky_numbers {
            line.paint(*origin, line_height, window, cx).ok();
        }
        for q in frame.scrollbar.drain(..) {
            window.paint_quad(q);
        }
    }
}
