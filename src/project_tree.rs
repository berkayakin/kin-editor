//! The project's file tree (the sidebar's Files view): which folders are
//! open, the rows they make, drag & drop between them and from Finder. File
//! operations are the workspace's: the tree reports what was asked
//! (`TreeEvent`) and the workspace tells it what changed (expand, remap,
//! reveal) and how rows look (Git changes, the open file, a menu's target).

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use gpui::{
    Context, DragMoveEvent, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, MouseButton, MouseDownEvent, Pixels, Point, Render,
    SharedString, UniformListScrollHandle, Window, div, prelude::*, px, uniform_list,
};

use crate::{
    assets::Icon,
    git::Change,
    git_panel::change_color,
    theme::Theme,
    ui::{DragLabel, HoverFill, file_icon, folder_icon, icon_el, selected_edge},
    workspace::{RedoFileChange, UndoFileChange},
};

const ROW_HEIGHT: f32 = 26.0;

#[derive(Clone)]
struct TreeEntry {
    path: PathBuf,
    name: SharedString,
    depth: usize,
    is_dir: bool,
}

/// A tree row being dragged.
#[derive(Clone)]
pub struct DraggedEntry {
    pub path: PathBuf,
    pub name: SharedString,
}

pub enum TreeEvent {
    /// A file was clicked.
    Open(PathBuf),
    /// A row (or empty space: the project folder) was right-clicked.
    Menu { path: PathBuf, is_dir: bool, position: Point<Pixels> },
    /// A row was dropped into a folder.
    Move { from: PathBuf, into: PathBuf },
    /// Files from Finder were dropped into a folder.
    CopyIn { paths: Vec<PathBuf>, into: PathBuf },
    /// ⌘Z / ⇧⌘Z with the tree focused: undo or redo a file operation.
    History { redo: bool },
    /// A folder opened or closed (the session keeps which).
    Toggled,
}

impl EventEmitter<TreeEvent> for ProjectTree {}

pub struct ProjectTree {
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    entries: Vec<TreeEntry>,
    scroll: UniformListScrollHandle,
    focus: FocusHandle,
    /// While dragging over the tree: the folder a drop would go into.
    drop_target: Option<PathBuf>,
    /// Change per file (and "contains changes" per folder).
    git: Arc<HashMap<PathBuf, Change>>,
    /// What Git ignores: files, and folders with everything in them. Dimmed.
    ignored: Arc<HashSet<PathBuf>>,
    /// The open file, marked.
    active: Option<PathBuf>,
    /// The row a context menu is open for, outlined.
    menu_target: Option<PathBuf>,
}

impl ProjectTree {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            root: None,
            expanded: HashSet::new(),
            entries: Vec::new(),
            scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            drop_target: None,
            git: Arc::default(),
            ignored: Arc::default(),
            active: None,
            menu_target: None,
        }
    }

    /// A new project, with the folders that were open in it last time.
    pub fn set_root(&mut self, root: Option<PathBuf>, expanded: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        self.expanded = root.iter().cloned().chain(expanded).collect();
        self.root = root;
        self.rebuild(cx);
    }

    /// The open folders.
    pub fn expanded(&self) -> &HashSet<PathBuf> {
        &self.expanded
    }

    pub fn is_expanded(&self, dir: &Path) -> bool {
        self.expanded.contains(dir)
    }

    /// Lists the open folders again (the disk changed).
    pub fn rebuild(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();
        if let Some(root) = self.root.clone() {
            self.push_children(&root, 0);
        }
        cx.notify();
    }

    fn push_children(&mut self, dir: &Path, depth: usize) {
        for (path, is_dir, cycle, name) in children(dir) {
            let expanded = is_dir && !cycle && self.expanded.contains(&path);
            self.entries.push(TreeEntry { path: path.clone(), name: name.into(), depth, is_dir });
            if expanded {
                self.push_children(&path, depth + 1);
            }
        }
    }

    /// Opens a folder (listed on the next `rebuild`).
    pub fn expand(&mut self, dir: PathBuf) {
        self.expanded.insert(dir);
    }

    /// Closes every folder but the project's.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        self.expanded.retain(|p| Some(p) == root.as_ref());
        self.rebuild(cx);
    }

    /// `path` is gone: so are its open folders.
    pub fn forget(&mut self, path: &Path) {
        self.expanded.retain(|p| !p.starts_with(path));
    }

    /// Open folders follow a move or rename.
    pub fn remap(&mut self, remap: impl Fn(&Path) -> Option<PathBuf>) {
        self.expanded = self.expanded.iter().map(|p| remap(p).unwrap_or_else(|| p.clone())).collect();
    }

    /// Opens the folders down to `path` (listed on the next `rebuild`).
    pub fn reveal(&mut self, path: &Path) {
        let Some(root) = self.root.clone() else { return };
        for dir in path.ancestors().skip(1) {
            if !dir.starts_with(&root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
        }
    }

    /// Opens the folders down to `path` and scrolls its row into the middle.
    pub fn reveal_and_scroll(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.reveal(path);
        self.rebuild(cx);
        if let Some(ix) = self.entries.iter().position(|e| e.path == path) {
            self.scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
        }
    }

    pub fn has_drop_target(&self) -> bool {
        self.drop_target.is_some()
    }

    pub fn set_drop_target(&mut self, target: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.drop_target != target {
            self.drop_target = target;
            cx.notify();
        }
    }

    /// How rows look: Git changes, what Git ignores, the open file, a context
    /// menu's row.
    pub fn decorate(
        &mut self,
        git: Arc<HashMap<PathBuf, Change>>,
        ignored: Arc<HashSet<PathBuf>>,
        active: Option<PathBuf>,
        menu_target: Option<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if Arc::ptr_eq(&self.git, &git) && Arc::ptr_eq(&self.ignored, &ignored) && self.active == active && self.menu_target == menu_target {
            return;
        }
        (self.git, self.ignored, self.active, self.menu_target) = (git, ignored, active, menu_target);
        cx.notify();
    }

    /// Whether Git ignores `path`: itself, or a folder it's in.
    fn is_ignored(&self, path: &Path) -> bool {
        !self.ignored.is_empty() && path.ancestors().any(|p| self.ignored.contains(p))
    }

    fn click(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix).cloned() else { return };
        // Never open a folder as a file (opening a folder makes it the project).
        if entry.is_dir || entry.path.is_dir() {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.rebuild(cx);
            cx.emit(TreeEvent::Toggled);
        } else {
            cx.emit(TreeEvent::Open(entry.path));
        }
    }
}

impl Focusable for ProjectTree {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ProjectTree {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let drop_on_root = self.drop_target.is_some() && self.drop_target == self.root;
        let list = uniform_list(
            "file-tree",
            self.entries.len(),
            cx.processor(move |tree, range: std::ops::Range<usize>, _window, cx| {
                let theme = Theme::get(cx);
                range
                    .map(|ix| {
                        let e = &tree.entries[ix];
                        let selected = tree.active.as_deref() == Some(e.path.as_path());
                        let change = tree.git.get(&e.path).copied();
                        // Ignored by Git: dimmed, like VS Code and Zed.
                        let name_color: Hsla = match change {
                            Some(c) => change_color(theme, c),
                            None if tree.is_ignored(&e.path) => theme.text_dim.into(),
                            None if selected => theme.text.into(),
                            None => theme.text_muted.into(),
                        };
                        let is_open = e.is_dir && tree.expanded.contains(&e.path);
                        let icon = if e.is_dir { folder_icon(is_open, px(16.), theme) } else { file_icon(&e.path, px(16.), theme) };
                        // Dropping on a file moves into the folder it's in.
                        let drop_dir = if e.is_dir { e.path.clone() } else { e.path.parent().map(Path::to_path_buf).unwrap_or_default() };
                        let dragged = DraggedEntry { path: e.path.clone(), name: e.name.clone() };
                        let is_target = e.is_dir && tree.drop_target.as_deref() == Some(e.path.as_path());
                        div().w_full().px_1().child(
                            div()
                                .id(ix)
                                .relative()
                                .w_full()
                                .flex()
                                .items_center()
                                .h(px(ROW_HEIGHT))
                                .pl(px(8. + e.depth as f32 * 14.))
                                .pr_2()
                                .gap_1p5()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_color(if selected { theme.text } else { theme.text_muted })
                                // A lit edge marks the open file.
                                .when(selected, |el| el.bg(theme.accent_soft).child(selected_edge(theme)))
                                .row_hover(theme.hover)
                                .on_click(cx.listener(move |tree, _, _, cx| tree.click(ix, cx)))
                                .on_drag(dragged, |entry, _, _, cx| cx.new(|_| DragLabel(entry.name.clone())))
                                // While dragging, the folder a drop goes into lights up.
                                .when(is_target, |el| el.bg(theme.accent_soft).border_1().border_color(Hsla::from(theme.accent).alpha(0.7)))
                                .on_drag_move(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |tree, ev: &DragMoveEvent<DraggedEntry>, _, cx| {
                                        if ev.bounds.contains(&ev.event.position) {
                                            tree.set_drop_target(Some(dir.clone()), cx);
                                        }
                                    }
                                }))
                                .on_drag_move(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |tree, ev: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                                        if ev.bounds.contains(&ev.event.position) {
                                            tree.set_drop_target(Some(dir.clone()), cx);
                                        }
                                    }
                                }))
                                .on_drop(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |tree, dragged: &DraggedEntry, _, cx| {
                                        cx.stop_propagation();
                                        tree.set_drop_target(None, cx);
                                        cx.emit(TreeEvent::Move { from: dragged.path.clone(), into: dir.clone() });
                                    }
                                }))
                                .on_drop(cx.listener(move |tree, paths: &gpui::ExternalPaths, _, cx| {
                                    cx.stop_propagation();
                                    tree.set_drop_target(None, cx);
                                    cx.emit(TreeEvent::CopyIn { paths: paths.paths().to_vec(), into: drop_dir.clone() });
                                }))
                                .when(tree.menu_target.as_deref() == Some(e.path.as_path()), |el| {
                                    el.border_1().border_color(Hsla::from(theme.accent).alpha(0.6))
                                })
                                .on_mouse_down(MouseButton::Right, {
                                    let (path, is_dir) = (e.path.clone(), e.is_dir);
                                    cx.listener(move |_, ev: &MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        cx.emit(TreeEvent::Menu { path: path.clone(), is_dir, position: ev.position });
                                    })
                                })
                                .child(div().w(px(14.)).flex_shrink_0().when(e.is_dir, |el| {
                                    el.child(icon_el(if is_open { Icon::ChevronDown } else { Icon::ChevronRight }, px(13.), theme.text_dim.into()))
                                }))
                                .child(icon)
                                .child(div().flex_1().min_w_0().truncate().text_color(name_color).child(e.name.clone()))
                                .when_some(change.filter(|_| !e.is_dir), |el, c| {
                                    el.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_size(px(11.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(change_color(theme, c))
                                            .child(c.letter()),
                                    )
                                }),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(self.scroll.clone())
        .size_full()
        // Over the tree but not on a row, a drop goes to the project folder;
        // rows (told later) narrow it to theirs.
        .on_drag_move(cx.listener(|tree, ev: &DragMoveEvent<DraggedEntry>, _, cx| {
            let target = ev.bounds.contains(&ev.event.position).then(|| tree.root.clone()).flatten();
            tree.set_drop_target(target, cx);
        }))
        .on_drag_move(cx.listener(|tree, ev: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
            let target = ev.bounds.contains(&ev.event.position).then(|| tree.root.clone()).flatten();
            tree.set_drop_target(target, cx);
        }))
        // The project folder itself has no row: outline the tree instead.
        .when(drop_on_root, |el| el.border_1().border_color(Hsla::from(theme.accent).alpha(0.7)).rounded(px(6.)))
        .on_drop(cx.listener(|tree, dragged: &DraggedEntry, _, cx| {
            tree.set_drop_target(None, cx);
            if let Some(root) = tree.root.clone() {
                cx.emit(TreeEvent::Move { from: dragged.path.clone(), into: root });
            }
        }))
        .on_drop(cx.listener(|tree, paths: &gpui::ExternalPaths, _, cx| {
            cx.stop_propagation();
            tree.set_drop_target(None, cx);
            if let Some(root) = tree.root.clone() {
                cx.emit(TreeEvent::CopyIn { paths: paths.paths().to_vec(), into: root });
            }
        }))
        // Puts the first row's text level with the tab labels beside it
        // (GPUI applies a uniform list's own padding twice, so this is 2px).
        .pt(px(1.))
        .pb_2()
        .track_focus(&self.focus)
        .key_context("FileTree")
        .on_action(cx.listener(|_, _: &UndoFileChange, _, cx| cx.emit(TreeEvent::History { redo: false })))
        .on_action(cx.listener(|_, _: &RedoFileChange, _, cx| cx.emit(TreeEvent::History { redo: true })))
        // Right-clicking empty space targets the project root.
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|tree, ev: &MouseDownEvent, _, cx| {
                if let Some(root) = tree.root.clone() {
                    cx.emit(TreeEvent::Menu { path: root, is_dir: true, position: ev.position });
                }
            }),
        );
        div().relative().size_full().child(list).child(crate::ui::list_scrollbar("file-tree-scrollbar", &self.scroll, cx))
    }
}

/// A folder's entries, folders first: (path, is a folder, is a link back to
/// a folder it's in, name).
pub fn children(dir: &Path) -> Vec<(PathBuf, bool, bool, String)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut here: Option<Option<PathBuf>> = None;
    let mut children: Vec<(PathBuf, bool, bool, String)> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), ".git" | ".DS_Store" | "target") {
                return None;
            }
            let file_type = e.file_type().ok()?;
            let path = e.path();
            // A link to a folder is a folder; one to a folder around it
            // would expand forever, so it doesn't.
            let (is_dir, cycle) = if file_type.is_symlink() {
                let target = std::fs::canonicalize(&path).ok().filter(|t| t.is_dir());
                let here = here.get_or_insert_with(|| std::fs::canonicalize(dir).ok());
                let cycle = target.as_ref().zip(here.as_ref()).is_some_and(|(t, h)| h.starts_with(t));
                (target.is_some(), cycle)
            } else {
                (file_type.is_dir(), false)
            };
            Some((path, is_dir, cycle, name))
        })
        .collect();
    children.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.3.to_lowercase().cmp(&b.3.to_lowercase())));
    children
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, TestAppContext};

    use super::*;

    #[gpui::test]
    fn what_git_ignores_is_dimmed_with_everything_in_it(cx: &mut TestAppContext) {
        let tree = cx.new(ProjectTree::new);
        let ignored: Arc<HashSet<PathBuf>> = Arc::new(["/p/target", "/p/run.log"].map(PathBuf::from).into_iter().collect());
        tree.update(cx, |t, cx| t.decorate(Arc::default(), ignored, None, None, cx));
        tree.read_with(cx, |t, _| {
            assert!(t.is_ignored(Path::new("/p/target")));
            assert!(t.is_ignored(Path::new("/p/target/debug/a.o")));
            assert!(t.is_ignored(Path::new("/p/run.log")));
            assert!(!t.is_ignored(Path::new("/p/src/main.rs")));
            // A name that only starts like an ignored folder's.
            assert!(!t.is_ignored(Path::new("/p/target-notes.md")));
        });
    }
}
