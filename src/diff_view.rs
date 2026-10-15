//! The diff of one file: a working-tree change (from the Commit panel) or a
//! file in a commit (from the Git log). Side by side or unified, with syntax
//! colors, changed words emphasized, and Stage / Unstage per change.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, IntoElement, Render,
    ScrollStrategy, SharedString, StyledText, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};
use ropey::Rope;

use crate::{
    assets::Icon,
    editor::{Cancel, NextChange, PreviousChange},
    git::{self, Change, CommitFile, DiffDocument, DiffLineKind, FileStatus},
    preview::{self, ImageInfo},
    git_panel::{GitOp, change_color},
    syntax::{Highlight, Language, Syntax},
    theme::Theme,
    ui::{HoverFill, icon_el, text_button, tooltip_view},
};

const ROW_HEIGHT: f32 = 20.;

pub enum DiffViewEvent {
    Close,
    OpenFile(PathBuf),
    Run(GitOp),
    /// The user switched between side by side and unified.
    Layout { side_by_side: bool },
}

/// What is being compared.
#[derive(Clone)]
pub enum DiffSource {
    /// Staged: HEAD → index. Unstaged: index → working tree (or the open
    /// editor's text, so unsaved edits show).
    Working { file: FileStatus, staged: bool, buffer: Option<String> },
    /// A commit's change to a file: first parent → commit.
    Commit { sha: String, summary: String, file: CommitFile },
}

/// Syntax spans per line, in tab-expanded byte offsets.
type LineSpans = Vec<Vec<(Range<usize>, Highlight)>>;

/// The two versions of an image: a temporary copy of each and its details.
struct ImageSides {
    before: Option<(PathBuf, ImageInfo)>,
    after: Option<(PathBuf, ImageInfo)>,
}

struct Loaded {
    /// Binary content has no meaningful text diff.
    binary: bool,
    /// Images are compared as pictures.
    images: Option<ImageSides>,
    doc: DiffDocument,
    old_spans: LineSpans,
    new_spans: LineSpans,
    /// The compared "after" text (what Stage Lines stages from).
    current: Arc<str>,
}

pub struct DiffView {
    repo: PathBuf,
    source: DiffSource,
    side_by_side: bool,
    /// Show every line instead of collapsing unchanged runs.
    expanded: bool,
    loaded: Option<Arc<Loaded>>,
    /// The change last jumped to with F7 / ⇧F7.
    current_change: Option<usize>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
}

impl EventEmitter<DiffViewEvent> for DiffView {}

impl DiffView {
    pub fn new(repo: PathBuf, source: DiffSource, side_by_side: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let mut view = Self {
            repo,
            source,
            side_by_side,
            expanded: false,
            loaded: None,
            current_change: None,
            focus_handle,
            scroll: UniformListScrollHandle::new(),
        };
        view.reload(cx);
        view
    }

    /// Recomputes the diff (after staging, or to expand unchanged lines).
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let (repo, source) = (self.repo.clone(), self.source.clone());
        let context = if self.expanded { usize::MAX } else { 3 };
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    if let Some(images) = load_images(&repo, &source) {
                        return Loaded {
                            binary: true,
                            images: Some(images),
                            doc: DiffDocument::default(),
                            old_spans: Vec::new(),
                            new_spans: Vec::new(),
                            current: Arc::from(""),
                        };
                    }
                    let (base, current, path) = match &source {
                        DiffSource::Working { file, staged: true, .. } => (
                            git::head_text(&repo, &file.rel).unwrap_or_default(),
                            git::index_text(&repo, &file.rel).unwrap_or_default(),
                            file.path.clone(),
                        ),
                        DiffSource::Working { file, staged: false, buffer } => {
                            let base = if file.unstaged == Some(Change::Untracked) {
                                String::new()
                            } else {
                                git::index_text(&repo, &file.rel).unwrap_or_default()
                            };
                            // Read bytes, not UTF-8 text, so binary files are detected rather than empty.
                            let read = || std::fs::read(&file.path).ok().map(|b| String::from_utf8_lossy(&b).into_owned());
                            let current = buffer.clone().or_else(read).unwrap_or_default();
                            (base, current, file.path.clone())
                        }
                        DiffSource::Commit { sha, file, .. } => {
                            let (base, current) = git::commit_file_texts(&repo, sha, file);
                            (base, current, repo.join(&file.rel))
                        }
                    };
                    if git::is_binary(&base) || git::is_binary(&current) {
                        return Loaded {
                            binary: true,
                            images: None,
                            doc: DiffDocument::default(),
                            old_spans: Vec::new(),
                            new_spans: Vec::new(),
                            current: current.into(),
                        };
                    }
                    let language = Language::from_path(&path);
                    Loaded {
                        binary: false,
                        images: None,
                        doc: git::diff_document(&base, &current, context),
                        old_spans: line_spans(language, &base),
                        new_spans: line_spans(language, &current),
                        current: current.into(),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.loaded = Some(Arc::new(loaded));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The file this diff is of, for its tab: the path and the path shown.
    pub fn tab_file(&self) -> (PathBuf, String) {
        let (path, rel, _) = self.file_label();
        (path, rel)
    }

    /// Closes the diff the way its own × does (back to where it came from).
    pub fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(DiffViewEvent::Close);
    }

    fn file_label(&self) -> (PathBuf, String, Change) {
        match &self.source {
            DiffSource::Working { file, staged, .. } => {
                let change = if *staged { file.staged } else { file.unstaged }.unwrap_or(Change::Modified);
                (file.path.clone(), file.rel.clone(), change)
            }
            DiffSource::Commit { file, .. } => (self.repo.join(&file.rel), file.rel.clone(), file.change),
        }
    }

    /// The Stage / Unstage operation for a change, if this diff can do that.
    fn change_op(&self, ix: usize) -> Option<(GitOp, &'static str)> {
        let DiffSource::Working { file, staged, .. } = &self.source else { return None };
        let loaded = self.loaded.as_ref()?;
        let change = loaded.doc.changes.get(ix)?;
        let rel = file.rel.clone();
        Some(if *staged {
            // The index is the "after" side here.
            (GitOp::UnstageLines { rel, text: None, rows: change.new_rows.clone() }, "Unstage")
        } else {
            (GitOp::StageLines { rel, text: loaded.current.to_string(), rows: change.new_rows.clone() }, "Stage")
        })
    }

    fn rows_len(&self) -> usize {
        self.loaded.as_ref().map_or(0, |l| if self.side_by_side { l.doc.split.len() } else { l.doc.unified.len() })
    }

    /// Row of the first line of change `ix` in the current layout.
    fn row_of_change(&self, ix: usize) -> Option<usize> {
        let loaded = self.loaded.as_ref()?;
        if self.side_by_side {
            loaded.doc.split.iter().position(|r| r.change == Some((ix, true)))
        } else {
            loaded.doc.unified.iter().position(|l| l.change == Some((ix, true)))
        }
    }

    fn jump(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(count) = self.loaded.as_ref().map(|l| l.doc.changes.len()).filter(|n| *n > 0) else { return };
        let next = match (self.current_change, forward) {
            (None, true) => 0,
            (None, false) => count - 1,
            (Some(i), true) => (i + 1) % count,
            (Some(i), false) => (i + count - 1) % count,
        };
        self.current_change = Some(next);
        if let Some(row) = self.row_of_change(next) {
            self.scroll.scroll_to_item(row.saturating_sub(3), ScrollStrategy::Top);
        }
        cx.notify();
    }

    fn on_next(&mut self, _: &NextChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(true, cx);
    }

    fn on_previous(&mut self, _: &PreviousChange, _: &mut Window, cx: &mut Context<Self>) {
        self.jump(false, cx);
    }

    fn on_escape(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DiffViewEvent::Close);
    }

    fn set_side_by_side(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.side_by_side != on {
            self.side_by_side = on;
            self.scroll = UniformListScrollHandle::new();
            cx.emit(DiffViewEvent::Layout { side_by_side: on });
            cx.notify();
        }
    }
}

/// For an image file, both versions as temporary files (`None` for other files).
fn load_images(repo: &Path, source: &DiffSource) -> Option<ImageSides> {
    let (path, before, after) = match source {
        DiffSource::Working { file, staged: true, .. } => (
            file.path.clone(),
            git::blob_bytes(repo, &format!("HEAD:{}", file.rel)),
            git::blob_bytes(repo, &format!(":{}", file.rel)),
        ),
        DiffSource::Working { file, staged: false, .. } => {
            let before = if file.unstaged == Some(Change::Untracked) { None } else { git::blob_bytes(repo, &format!(":{}", file.rel)) };
            (file.path.clone(), before, std::fs::read(&file.path).ok())
        }
        DiffSource::Commit { sha, file, .. } => {
            let old_rel = file.old_rel.as_deref().unwrap_or(&file.rel);
            let before = (file.change != Change::Added).then(|| git::blob_bytes(repo, &format!("{sha}^:{old_rel}"))).flatten();
            let after = (file.change != Change::Deleted).then(|| git::blob_bytes(repo, &format!("{sha}:{}", file.rel))).flatten();
            (repo.join(&file.rel), before, after)
        }
    };
    if !preview::is_image(&path) {
        return None;
    }
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let side = |bytes: Option<Vec<u8>>| {
        let copy = preview::temp_copy(&bytes?, &extension)?;
        let mut info = ImageInfo::read(&copy);
        // The copy is named by its hash; show the real file's format.
        info.format = ImageInfo::read(&path).format;
        Some((copy, info))
    };
    Some(ImageSides { before: side(before), after: side(after) })
}

/// Syntax spans per line of `text`, in the tab-expanded offsets the diff uses.
fn line_spans(language: Language, text: &str) -> LineSpans {
    // Very large files aren't worth highlighting here.
    if text.len() > 2_000_000 {
        return Vec::new();
    }
    let rope = Rope::from_str(text);
    let syntax = Syntax::new(language, &rope);
    let spans = syntax.highlights(&rope, 0..text.len());
    let mut out = Vec::new();
    let mut ix = 0;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end = start + line.len();
        let mut line_spans = Vec::new();
        while ix < spans.len() && spans[ix].0.end <= start {
            ix += 1;
        }
        for (range, kind) in spans[ix..].iter().take_while(|(r, _)| r.start < end) {
            let (s, e) = (range.start.max(start) - start, range.end.min(end) - start);
            if s < e {
                line_spans.push((expanded_offset(line, s)..expanded_offset(line, e), *kind));
            }
        }
        out.push(line_spans);
        start = end;
    }
    out
}

/// Where byte `offset` of `line` lands once tabs become four spaces.
fn expanded_offset(line: &str, offset: usize) -> usize {
    offset + 3 * line.as_bytes()[..offset.min(line.len())].iter().filter(|b| **b == b'\t').count()
}

/// A line with syntax colors and changed words on a stronger background.
fn styled_line(text: &str, spans: Option<&Vec<(Range<usize>, Highlight)>>, emphasis: &[Range<usize>], emphasis_bg: Hsla, theme: &Theme) -> StyledText {
    let mut cuts = vec![0, text.len()];
    let spans: &[(Range<usize>, Highlight)] = spans.map_or(&[], |s| s.as_slice());
    for (r, _) in spans {
        cuts.extend([r.start.min(text.len()), r.end.min(text.len())]);
    }
    for r in emphasis {
        cuts.extend([r.start.min(text.len()), r.end.min(text.len())]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let highlights = cuts.windows(2).filter(|w| w[0] < w[1] && text.is_char_boundary(w[0]) && text.is_char_boundary(w[1])).map(|w| {
        let kind = spans.iter().find(|(r, _)| r.start <= w[0] && w[0] < r.end).map_or(Highlight::None, |(_, k)| *k);
        let emphasized = emphasis.iter().any(|r| r.start <= w[0] && w[0] < r.end);
        (
            w[0]..w[1],
            HighlightStyle {
                color: Some(theme.highlight_color(kind)),
                background_color: emphasized.then_some(emphasis_bg),
                ..Default::default()
            },
        )
    });
    StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights.collect::<Vec<_>>())
}

impl Focusable for DiffView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let (path, rel, change) = self.file_label();
        let file_icon = crate::ui::file_icon(&path, px(16.), &theme);
        let (added, removed) = self.loaded.as_ref().map_or((0, 0), |l| (l.doc.added, l.doc.removed));
        // Line counts and layouts mean nothing for a binary file.
        let binary = self.loaded.as_ref().is_some_and(|l| l.binary);
        let deleted = change == Change::Deleted;
        let icon_button = |id: &'static str, icon: Icon, tip: &'static str| {
            div()
                .id(id)
                .size(px(28.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded(px(8.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .child(icon_el(icon, px(15.), theme.text_muted.into()))
        };
        let badge = match &self.source {
            DiffSource::Working { staged, .. } => format!("{} · {}", change.label(), if *staged { "Staged" } else { "Unstaged" }),
            DiffSource::Commit { sha, summary, .. } => format!("{} · {}", &sha[..sha.len().min(8)], summary),
        };
        // Side by side / Unified switch.
        let segment = |id: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .px_2()
                .h(px(22.))
                .flex()
                .items_center()
                .rounded(px(7.))
                .text_size(px(12.))
                .cursor_pointer()
                .when(on, |el| crate::ui::segment_on(el, &theme).text_color(theme.text))
                .when(!on, |el| el.text_color(theme.text_muted).hover(|s| s.text_color(theme.text)))
                .child(label)
        };

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .h(px(46.))
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(icon_el(Icon::GitCompare, px(15.), theme.text_muted.into()))
            .child(file_icon)
            .child(div().flex_shrink_0().font_weight(FontWeight::SEMIBOLD).child(rel.clone()))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .px_1p5()
                    .rounded(px(4.))
                    .bg(change_color(&theme, change).alpha(0.15))
                    .text_size(px(11.))
                    .text_color(change_color(&theme, change))
                    .child(badge),
            )
            .when(!binary, |el| {
                el.child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.git_added).child(format!("+{added}")))
                    .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.git_deleted).child(format!("−{removed}")))
            })
            .child(div().flex_1())
            .when(!binary, |el| el.child(
                crate::ui::dock(&theme)
                    .child(segment("diff-split", "Side by Side", self.side_by_side).on_click(cx.listener(|this, _, _, cx| this.set_side_by_side(true, cx))))
                    .child(segment("diff-unified", "Unified", !self.side_by_side).on_click(cx.listener(|this, _, _, cx| this.set_side_by_side(false, cx)))),
            ))
            .when_some(
                match &self.source {
                    DiffSource::Working { file, staged, .. } => Some((file.clone(), *staged)),
                    DiffSource::Commit { .. } => None,
                },
                |el, (file, staged)| {
                    let rel = file.rel.clone();
                    el.when(!staged, |el| {
                        el.child(
                            text_button("diff-rollback", "Rollback", false, cx)
                                .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::Run(GitOp::Discard(vec![file.clone()]))))),
                        )
                    })
                    .child(text_button("diff-stage", if staged { "Unstage File" } else { "Stage File" }, true, cx).on_click(cx.listener(
                        move |_, _, _, cx| {
                            let op = if staged { GitOp::Unstage(vec![rel.clone()]) } else { GitOp::Stage(vec![rel.clone()]) };
                            cx.emit(DiffViewEvent::Run(op));
                        },
                    )))
                },
            )
            .when(!deleted, |el| {
                let path = path.clone();
                el.child(icon_button("diff-open", Icon::File, "Open File").on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::OpenFile(path.clone())))))
            })
            .child(icon_button("diff-close", Icon::X, "Close (Esc)").on_click(cx.listener(|_, _, _, cx| cx.emit(DiffViewEvent::Close))));

        let rows = self.rows_len();
        let body = match self.loaded.clone() {
            None => div().flex_1().p_4().text_color(theme.text_dim).child("Loading diff…").into_any_element(),
            Some(loaded) if loaded.images.is_some() => self.render_images(loaded.images.as_ref().unwrap(), &theme).into_any_element(),
            Some(loaded) if loaded.binary => div()
                .flex_1()
                .p_4()
                .text_color(theme.text_dim)
                .child("Binary file: there's no text diff to show")
                .into_any_element(),
            Some(loaded) if loaded.doc.changes.is_empty() => {
                div().flex_1().p_4().text_color(theme.text_dim).child("No differences").into_any_element()
            }
            Some(loaded) => {
                let side_by_side = self.side_by_side;
                let current_change = self.current_change;
                uniform_list("diff-rows", rows, {
                    cx.processor(move |this, range: Range<usize>, _window, cx| {
                        let theme = Theme::get(cx).clone();
                        range.map(|ix| this.render_row(&loaded, ix, side_by_side, current_change, &theme, cx)).collect()
                    })
                })
                .track_scroll(self.scroll.clone())
                .flex_1()
                .w_full()
                .pt_1()
                .pb(px(10.))
                .bg(theme.background)
                .font_family(theme.mono_font.clone())
                .text_size(px(theme.editor_font_size - 1.))
                .into_any_element()
            }
        };

        div()
            .flex()
            .flex_col()
            .bg(theme.elevated)
            // It sits in an island (see `Workspace::render_diff_overlay`): follow
            // the island's rounded corners inside its inset.
            .rounded(px(crate::ui::ISLAND_RADIUS - crate::ui::ISLAND_INSET))
            .key_context("DiffView")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_previous))
            // Fills the editor area (the workspace places it there).
            .size_full()
            .child(header)
            .child(body)
    }
}

impl DiffView {
    /// Before and after, side by side, scaled down to fit.
    fn render_images(&self, sides: &ImageSides, theme: &Theme) -> impl IntoElement {
        let side = |title: &'static str, version: &Option<(PathBuf, ImageInfo)>, missing: &'static str| {
            let column = div().flex_1().min_w_0().h_full().flex().flex_col().gap_2().p_3();
            let header = div()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(12.))
                .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.text_muted).child(title))
                .when_some(version.as_ref(), |el, (_, info)| el.child(div().text_color(theme.text_dim).child(info.label())));
            let picture = match version {
                Some((path, _)) => div()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .rounded(px(8.))
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gpui::img(path.clone()).size_full().object_fit(gpui::ObjectFit::ScaleDown)),
                None => div()
                    .flex_1()
                    .min_h_0()
                    .rounded(px(8.))
                    .bg(theme.background)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(12.))
                    .text_color(theme.text_dim)
                    .child(missing),
            };
            column.child(header).child(picture)
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(side("Before", &sides.before, "No previous version (new file)"))
            .child(div().w(px(1.)).h_full().bg(theme.border_subtle))
            .child(side("After", &sides.after, "Deleted"))
    }

    fn render_row(&self, loaded: &Loaded, ix: usize, side_by_side: bool, current_change: Option<usize>, theme: &Theme, cx: &mut Context<Self>) -> gpui::AnyElement {
        let added_bg = Hsla::from(theme.git_added).alpha(0.1);
        let removed_bg = Hsla::from(theme.git_deleted).alpha(0.1);
        let added_em = Hsla::from(theme.git_added).alpha(0.3);
        let removed_em = Hsla::from(theme.git_deleted).alpha(0.3);
        let number = |n: Option<usize>| {
            div()
                .w(px(44.))
                .pr_2()
                .flex_shrink_0()
                .flex()
                .justify_end()
                .text_color(theme.line_number)
                .child(n.map(|n| n.to_string()).unwrap_or_default())
        };
        let row = div().id(ix).relative().w_full().flex().h(px(ROW_HEIGHT));

        let (kind, label, change) = if side_by_side {
            let r = &loaded.doc.split[ix];
            (r.kind, r.label.clone(), r.change)
        } else {
            let l = &loaded.doc.unified[ix];
            (l.kind, l.text.clone(), l.change)
        };
        if kind == DiffLineKind::Skipped {
            return row
                .justify_center()
                .items_center()
                .bg(theme.panel)
                .text_color(theme.text_dim)
                .font_family(theme.ui_font.clone())
                .text_size(px(11.5))
                .cursor_pointer()
                .hover(|s| s.text_color(theme.text_muted))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.expanded = true;
                    this.reload(cx);
                }))
                .child(format!("⋯ {label} (click to show all)"))
                .into_any_element();
        }

        let row = if side_by_side {
            let r = &loaded.doc.split[ix];
            let half = |side: Option<&git::DiffSide>, spans: &LineSpans, bg: Hsla, em: Hsla, changed: bool| {
                let cell = div().flex_1().min_w_0().h_full().flex().overflow_hidden();
                match side {
                    Some(s) => cell
                        .when(changed, |el| el.bg(bg))
                        .child(number(Some(s.no)))
                        .child(div().whitespace_nowrap().child(styled_line(&s.text, spans.get(s.no - 1), &s.emphasis, em, theme))),
                    // The other side has no line here.
                    None => cell.bg(theme.panel),
                }
            };
            let changed = r.kind != DiffLineKind::Context;
            row.child(half(r.left.as_ref(), &loaded.old_spans, removed_bg, removed_em, changed))
                .child(div().w(px(1.)).h_full().flex_shrink_0().bg(theme.border))
                .child(half(r.right.as_ref(), &loaded.new_spans, added_bg, added_em, changed))
        } else {
            let l = &loaded.doc.unified[ix];
            let (bg, sign, sign_color, em, spans): (Option<Hsla>, &str, Hsla, Hsla, Option<&Vec<_>>) = match l.kind {
                DiffLineKind::Added => (Some(added_bg), "+", theme.git_added.into(), added_em, l.new_no.and_then(|n| loaded.new_spans.get(n - 1))),
                DiffLineKind::Removed => (Some(removed_bg), "−", theme.git_deleted.into(), removed_em, l.old_no.and_then(|n| loaded.old_spans.get(n - 1))),
                _ => (None, " ", theme.text_dim.into(), added_em, l.new_no.and_then(|n| loaded.new_spans.get(n - 1))),
            };
            row.when_some(bg, |el, bg| el.bg(bg))
                .child(number(l.old_no))
                .child(number(l.new_no))
                .child(div().w(px(16.)).flex_shrink_0().text_color(sign_color).child(sign.to_string()))
                .child(div().whitespace_nowrap().child(styled_line(&l.text, spans, &l.emphasis, em, theme)))
        };

        // The first row of a change: a marker for F7, and Stage / Unstage.
        let row = match change {
            Some((c, true)) => {
                let row = row.when(current_change == Some(c), |el| el.border_t_1().border_color(theme.accent));
                match self.change_op(c) {
                    Some((op, label)) => row.child(
                        div()
                            .id(("change-op", c))
                            .absolute()
                            .right(px(8.))
                            .top(px(1.))
                            .h(px(18.))
                            .px_2()
                            .flex()
                            .items_center()
                            .rounded(px(4.))
                            .border_1()
                            .border_color(theme.border)
                            .font_family(theme.ui_font.clone())
                            .text_size(px(11.))
                            .text_color(theme.text_muted)
                            .cursor_pointer()
                            .hover_fill(theme.elevated, theme.hover)
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(DiffViewEvent::Run(op.clone()))))
                            .child(label),
                    ),
                    None => row,
                }
            }
            _ => row,
        };
        row.into_any_element()
    }
}
