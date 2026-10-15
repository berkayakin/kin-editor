//! The window's root view: title bar, file tree, tabs, problems panel and
//! status bar around the active editor.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, Context, DragMoveEvent, Entity, EntityId, FocusHandle, Focusable, FontWeight, Hsla,
    CursorStyle, KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PathPromptOptions, Pixels, PromptLevel, Render,
    SharedString, Stateful, Subscription, Task, TitlebarOptions, UniformListScrollHandle, Window,
    WindowBounds, WindowControlArea, WindowHandle, WindowOptions, actions, div, img, point,
    prelude::*, px, relative, rgb, size, uniform_list,
};

use crate::{
    assets::{self, Icon},
    document::{Breakpoint, Document},
    branch_picker::{BranchPicker, BranchPickerEvent},
    clone_dialog::{self, CloneDialog, CloneDialogEvent},
    context_menu::{ContextMenu, ContextMenuEvent, MenuEntry},
    input_dialog::{InputDialog, InputDialogEvent},
    encoding::TextEncoding,
    watcher::{self, FsChange, GitPath, Watcher},
    onboarding::{Onboarding, OnboardingEvent},
    diff_view::{DiffSource, DiffView, DiffViewEvent},
    push_dialog::{PushDialog, PushDialogEvent},
    git_log::{GitLog, GitLogEvent},
    debugger::{DebugEvent, DebugSession, launch},
    extensions,
    editor::{
        self, Cancel, Editor, EditorBreakpointMenu, EditorBreakpointsChanged, EditorContextMenu, EditorEvent, EditorGitEvent, EditorLspEvent,
        EditorRunEvent, GitFile,
    },
    git::{self, Change, FileStatus, RepoStatus},
    git_panel::{GitOp, GitPanel, GitPanelEvent, change_color},
    find_bar::{FindBar, FindBarEvent},
    keymap,
    lint::Severity,
    lsp::{
        LspEvent, LspStatus, LspStore, Server,
        protocol::{self, CodeAction, Encoding, Location, Position, TextEdit},
    },
    picker::{ActionEntry, GoToLine as GoToLineModal, GoToLineEvent, LocationRow, Picker, PickerEvent, PickerMode},
    project_search::{ProjectSearch, ProjectSearchEvent},
    search::{SearchQuery, openable_files},
    state::{FileBreakpoints, PaneLayout, ProjectSession, SessionState, TabSession, WindowPlacement},
    syntax::Language,
    settings::{self, AutoSave, Settings, StartupProblems},
    run::{self, DebugSpec, RunTarget},
    terminal::{RunState, TerminalEvent, TerminalView},
    theme::{self, Theme},
    preferences::{self, Preferences, PreferencesEvent},
    theme_picker::{ThemePicker, ThemePickerEvent},
    ui::{binding_label, icon_button, icon_el, kbd, switch, tooltip_view},
    updater,
};

actions!(
    workspace,
    [
        NewFile,
        NewWindow,
        CloseWindow,
        MinimizeWindow,
        ZoomWindow,
        OpenFile,
        OpenFolder,
        OpenFolderInThisWindow,
        RecentProjects,
        Save,
        SaveAs,
        CloseTab,
        NextTab,
        PrevTab,
        ToggleSidebar,
        ToggleProblems,
        ToggleTerminal,
        ToggleGitLog,
        ShowFileHistory,
        NewTerminal,
        CloseTerminal,
        SplitRight,
        SplitDown,
        NextPane,
        RunConfiguration,
        Rerun,
        StopRun,
        DebugConfiguration,
        AttachToProcess,
        AddConditionalBreakpoint,
        AddLogpoint,
        OpenExtensions,
        ProjectTrust,
        UndoFileChange,
        RedoFileChange,
        /// Vim's `:` command line.
        VimCommandLine,
        ReloadExtensions,
        Resume,
        PauseProgram,
        StepOver,
        StepInto,
        StepOut,
        ToggleDebugPanel,
        CloseOtherTabs,
        CloseAllTabs,
        TogglePinTab,
        Quit,
        Find,
        Replace,
        FindNext,
        FindPrevious,
        FindInFiles,
        ReplaceInFiles,
        SearchEverywhere,
        GoToFile,
        RecentFiles,
        FindAction,
        GoToLine,
        FileStructure,
        OpenSettings,
        OpenSettingsFile,
        OpenKeymap,
        RestartLanguageServer,
        IncreaseFontSize,
        DecreaseFontSize,
        ResetFontSize,
        SelectTheme,
        OpenThemesFolder,
        OpenSnippetsFolder,
        ReloadThemes,
        ShowCommit,
        ToggleCommit,
        GitPush,
        GitPull,
        GitFetch,
        GitBranches,
        GitClone,
        GitInit,
        OpenRemote,
        ToggleAutoSave,
        CheckForUpdates,
        WelcomeSetup,
        GoToSymbol,
        SplitTerminal,
        SelectOpenedFile,
        InstallShellCommand,
    ]
);

/// How many folders the welcome screen remembers.
const MAX_RECENT_PROJECTS: usize = 10;

/// Where a folder opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenIn {
    /// Replace this window's project (asking about unsaved edits first).
    ThisWindow,
    NewWindow,
    /// This window if it has no project yet, otherwise a new one (like Zed).
    Auto,
}

/// Why the workspace is being closed down; decides what happens once
/// unsaved edits are dealt with.
#[derive(Clone)]
enum CloseIntent {
    CloseWindow,
    Quit,
    /// Close everything and open this folder in the same window.
    ReplaceProject(PathBuf),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SidebarMode {
    Files,
    Commit,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Success,
    /// Neutral news ("No usages found").
    Info,
    Error,
}

struct Toast {
    id: usize,
    kind: ToastKind,
    message: SharedString,
    detail: Option<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    /// The Problems panel's top edge.
    Problems,
    /// The file tree's right edge.
    Sidebar,
    /// The divider after pane `n`.
    Pane(usize),
}

#[derive(Clone, Copy)]
struct Resize {
    edge: Edge,
    /// Pointer position along the drag axis when the drag started.
    start: Pixels,
    /// Panel size when the drag started.
    size: Pixels,
}

/// Something the file-tree context menu can do.
#[derive(Clone)]
enum TreeAction {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Open(PathBuf),
    Rename(PathBuf),
    Duplicate(PathBuf),
    Delete(PathBuf),
    CopyPath(PathBuf),
    CopyRelativePath(PathBuf),
    Reveal(PathBuf),
    FindInFolder(PathBuf),
    CollapseAll,
    Git(GitOp),
    ShowDiff(FileStatus, bool),
    AddToGitignore(String),
    UndoFileChange,
    RedoFileChange,
}

/// What an open input dialog will do with the name it collects.
enum PendingInput {
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    /// Rename a symbol through its language server.
    RenameSymbol { server: Arc<Server>, path: PathBuf, position: Position, current: String },
    /// A Vim `:` command.
    VimCommand,
}

/// A group of tabs with its own tab bar; panes sit side by side or stacked.
struct Pane {
    tabs: Vec<Entity<Editor>>,
    active: usize,
}

impl Pane {
    fn new() -> Self {
        Self { tabs: Vec::new(), active: 0 }
    }
}

/// Terminal tabs shown together in the bottom panel; splitting the terminal
/// adds a group beside it, like a pane.
struct TerminalGroup {
    tabs: Vec<Entity<TerminalView>>,
    active: usize,
}

impl TerminalGroup {
    fn new(terminal: Entity<TerminalView>) -> Self {
        Self { tabs: vec![terminal], active: 0 }
    }
}

/// Where `open_terminal` puts a new terminal.
#[derive(Clone, Copy)]
enum TerminalPlace {
    /// A tab in a group: at `ix` (clamped), or after its tabs.
    Tab { group: usize, ix: Option<usize> },
    /// A group of its own right after this one (a split).
    GroupAfter(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneAxis {
    /// Side by side.
    Row,
    /// Stacked.
    Column,
}

/// A tab being dragged to a new place or pane.
#[derive(Clone)]
struct DraggedTab {
    editor: Entity<Editor>,
    name: SharedString,
}

/// A file tree entry being dragged onto a folder.
#[derive(Clone)]
struct DraggedEntry {
    path: PathBuf,
    name: SharedString,
}

/// What the drag shows under the pointer.
struct DraggedTabView(SharedString);

impl Render for DraggedTabView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        popover(theme)
            .px_3()
            .py_1()
            .rounded(px(14.))
            .text_size(px(theme.ui_font_size))
            .text_color(theme.text)
            .child(self.0.clone())
    }
}

/// Tab context menu commands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabAction {
    Close,
    CloseOthers,
    CloseAll,
    CloseToTheRight,
    TogglePin,
    SplitRight,
    SplitDown,
    SelectInTree,
}

/// What a context menu item does.
enum MenuAction {
    Tree(TreeAction),
    /// An action dispatched to the focused editor.
    Run(Box<dyn gpui::Action>),
    /// A language server's fix or refactoring.
    CodeAction(Arc<Server>, CodeAction),
    Tab(TabAction, Entity<Editor>),
    /// Status bar: save with these line endings (true: CRLF).
    LineEndings(bool, Entity<Editor>),
    Encoding(TextEncoding, Entity<Editor>),
    /// A gutter ▶'s Run (false) or Debug (true).
    Launch(RunTarget, bool),
    /// A line number's breakpoint menu.
    Breakpoint(BreakpointEdit, Entity<Editor>, usize),
    /// Status bar: the project's Python venv (None: detect).
    Venv(Option<crate::python::Choice>),
    /// Status bar: pick a venv folder.
    ChooseVenv,
}

/// What the breakpoint menu does with a row's breakpoint.
#[derive(Clone, Copy, PartialEq)]
enum BreakpointEdit {
    Toggle,
    Condition,
    LogMessage,
}

impl Clone for MenuAction {
    fn clone(&self) -> Self {
        match self {
            MenuAction::Tree(action) => MenuAction::Tree(action.clone()),
            MenuAction::Run(action) => MenuAction::Run(action.boxed_clone()),
            MenuAction::CodeAction(server, action) => MenuAction::CodeAction(server.clone(), action.clone()),
            MenuAction::Tab(action, editor) => MenuAction::Tab(*action, editor.clone()),
            MenuAction::LineEndings(crlf, editor) => MenuAction::LineEndings(*crlf, editor.clone()),
            MenuAction::Encoding(encoding, editor) => MenuAction::Encoding(*encoding, editor.clone()),
            MenuAction::Launch(target, debug) => MenuAction::Launch(target.clone(), *debug),
            MenuAction::Breakpoint(edit, editor, row) => MenuAction::Breakpoint(*edit, editor.clone(), *row),
            MenuAction::Venv(choice) => MenuAction::Venv(choice.clone()),
            MenuAction::ChooseVenv => MenuAction::ChooseVenv,
        }
    }
}

struct OpenContextMenu {
    menu: Entity<ContextMenu>,
    position: gpui::Point<Pixels>,
    actions: Vec<Option<MenuAction>>,
    /// The tree entry that was right-clicked (highlighted while open).
    target: Option<PathBuf>,
    _subscription: Subscription,
}

enum Modal {
    Picker(Entity<Picker>),
    GoToLine(Entity<GoToLineModal>),
    ProjectSearch(Entity<ProjectSearch>),
    Theme(Entity<ThemePicker>),
    Diff(Entity<DiffView>),
    Branches(Entity<BranchPicker>),
    Input(Entity<InputDialog>),
    Clone(Entity<CloneDialog>),
    Preferences(Entity<Preferences>),
    Push(Entity<PushDialog>),
    Trust(Entity<crate::trust_dialog::TrustDialog>),
}

/// How long two Shift taps may be apart to count as a double Shift.
const DOUBLE_SHIFT_WINDOW: Duration = Duration::from_millis(400);

const TITLEBAR_HEIGHT: f32 = 40.0;
const STATUSBAR_HEIGHT: f32 = 30.0;
const TAB_HEIGHT: f32 = 38.0;
const TREE_ROW_HEIGHT: f32 = 26.0;


#[derive(Clone)]
struct TreeEntry {
    path: PathBuf,
    name: SharedString,
    depth: usize,
    is_dir: bool,
}

pub struct Workspace {
    focus_handle: FocusHandle,
    /// The file tree's, so ⌘Z there undoes file operations.
    tree_focus: FocusHandle,
    file_history: crate::file_history::FileHistory,
    root: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    entries: Vec<TreeEntry>,
    tree_scroll: UniformListScrollHandle,
    /// Every open editor, in any pane (for saving, searching, sessions).
    tabs: Vec<Entity<Editor>>,
    panes: Vec<Pane>,
    active_pane: usize,
    pane_axis: PaneAxis,
    /// Each pane's share of the space, summing to 1.
    pane_sizes: Vec<f32>,
    /// Bounds of the pane area last frame, for divider drags.
    panes_bounds: std::rc::Rc<std::cell::Cell<Bounds<Pixels>>>,
    /// The two shares around the divider being dragged.
    pane_drag_pair: f32,
    /// Pinned tabs stay first and survive Close Others / Close All.
    pinned: HashSet<EntityId>,
    show_sidebar: bool,
    show_problems: bool,
    /// Height of the bottom panel (Problems or Terminal; one shows at a time).
    problems_height: Pixels,
    show_terminal: bool,
    /// Terminal tabs in groups side by side, each with its own tab strip
    /// (one group unless the terminal is split). Groups are never empty.
    terminal_groups: Vec<TerminalGroup>,
    /// The group that has (or last had) the keyboard.
    active_group: usize,
    /// While dragging over the tree: the folder a drop would go into.
    tree_drop: Option<PathBuf>,

    /// For Rerun.
    last_run: Option<RunTarget>,
    /// The debug session (running or finished) whose panel shows in the bottom area.
    debug: Option<(Entity<DebugSession>, Subscription)>,
    show_debug: bool,
    last_debug: Option<RunTarget>,
    /// Where the debug session is paused (file, 1-based line), to mark it in that file's editor.
    debug_location: Option<(PathBuf, u32)>,
    /// Breakpoints of files that aren't open; open files keep theirs in their documents.
    breakpoints: HashMap<PathBuf, Vec<Breakpoint>>,
    /// Errors and warnings the last Run printed: its name and what was found.
    run_problems: Option<(String, Vec<run::OutputProblem>)>,
    /// The status bar's venv for a folder (looked up once, not every frame).
    venv_shown: std::cell::RefCell<Option<(PathBuf, Option<PathBuf>)>>,
    sidebar_width: Pixels,
    /// The panel edge being dragged, if any.
    resizing: Option<Resize>,
    window_title: String,
    find_bar: Entity<FindBar>,
    show_find: bool,
    modal: Option<Modal>,
    modal_subscription: Option<Subscription>,
    /// All project files, refreshed in the background.
    file_index: Arc<Vec<PathBuf>>,
    index_task: Option<Task<()>>,
    /// Most recently used files, newest first.
    recent: Vec<PathBuf>,
    actions: Arc<Vec<ActionEntry>>,
    shift_down: bool,
    last_shift_tap: Option<Instant>,
    sidebar_mode: SidebarMode,
    git_panel: Entity<GitPanel>,
    git_log: Entity<GitLog>,
    /// The Push dialog a diff was opened from; it comes back when the diff closes.
    diff_return: Option<Entity<PushDialog>>,
    /// The bottom panel shows the Git log (instead of Problems or the terminal).
    show_git_log: bool,
    /// Working-tree root of the project's Git repository.
    repo: Option<PathBuf>,
    git_status: Arc<RepoStatus>,
    /// Change per file (and "contains changes" per folder) for the file tree.
    git_paths: Arc<HashMap<PathBuf, Change>>,
    git_user: Option<String>,
    /// URL of the repository's `origin` (or first) remote.
    git_remote: Option<String>,
    last_commit: Option<git::CommitInfo>,
    /// Project folders opened before, newest first.
    recent_projects: Vec<PathBuf>,
    git_busy: Option<&'static str>,
    status_task: Option<Task<()>>,
    git_op_task: Option<Task<()>>,
    toasts: Vec<Toast>,
    next_toast: usize,
    context_menu: Option<OpenContextMenu>,
    pending_input: Option<PendingInput>,
    auto_save: AutoSave,
    auto_save_delay: Duration,
    /// Pending `after_delay` saves, restarted by every edit.
    auto_save_tasks: HashMap<EntityId, Task<()>>,
    /// Tabs whose auto-save failure was already reported, to avoid repeats.
    auto_save_warned: HashSet<EntityId>,
    /// The user chose to discard unsaved changes, so closing goes ahead.
    close_confirmed: bool,
    /// Debounced write of the project session (see `save_session`).
    session_task: Option<Task<()>>,
    /// Debounced write of the window's place on screen.
    placement_task: Option<Task<()>>,
    /// Watches the project folder; `watch_task` applies what it reports.
    watcher: Option<Watcher>,
    watch_task: Option<Task<()>>,
    /// The project's .gitignore, to skip Git refreshes for ignored churn.
    ignore: Option<ignore::gitignore::Gitignore>,
    /// A Git refresh was asked for while one was running.
    git_refresh_again: bool,
    /// The running Go to Symbol request; replaced as the query changes.
    symbol_task: Option<Task<()>>,
    /// Welcome Setup, shown in place of everything below the title bar.
    onboarding: Option<(Entity<Onboarding>, Subscription)>,
    /// Language servers for this window's files.
    lsp: Entity<LspStore>,
    /// The Preferences page shown last, to reopen there.
    preferences_page: preferences::Page,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);
        let find_bar = cx.new(FindBar::new);
        let find_subscription = cx.subscribe_in(&find_bar, window, |ws, _, event: &FindBarEvent, window, cx| match event {
            FindBarEvent::Dismissed => {
                ws.show_find = false;
                ws.focus_active(window, cx);
                cx.notify();
            }
        });
        let state = SessionState::load();
        let git_panel = cx.new(GitPanel::new);
        let git_subscription = cx.subscribe_in(&git_panel, window, |ws, _, event: &GitPanelEvent, window, cx| match event {
            GitPanelEvent::Run(op) => ws.run_git(op.clone(), window, cx),
            GitPanelEvent::OpenDiff { file, staged } => ws.open_diff(file.clone(), *staged, window, cx),
            GitPanelEvent::OpenFile(path) => ws.open_path(path.clone(), window, cx),
        });
        let git_log = cx.new(GitLog::new);
        let log_subscription = cx.subscribe_in(&git_log, window, |ws, _, event: &GitLogEvent, window, cx| match event {
            GitLogEvent::OpenDiff(source) => ws.show_diff(source.clone(), window, cx),
            GitLogEvent::Close => {
                ws.show_git_log = false;
                ws.focus_active(window, cx);
                ws.save_state();
                cx.notify();
            }
        });
        // Keep Git state fresh: when the window regains focus, and every few seconds.
        // Leaving the window saves edits, like VS Code and Zed.
        let activation = cx.observe_window_activation(window, |ws, window, cx| {
            if window.is_window_active() {
                ws.check_config(cx);
                ws.refresh_git(cx);
                ws.reload_changed_files(cx);
                // Other windows may have opened projects meanwhile.
                ws.recent_projects = SessionState::load().recent_projects;
                cx.notify();
            } else if ws.auto_save != AutoSave::Off {
                ws.auto_save_all(cx);
            }
        });
        // Last chance to write edits when the app quits some other way
        // (Dock menu, logout).
        let quit = cx.on_app_quit(|ws, cx| {
            if ws.auto_save != AutoSave::Off {
                ws.auto_save_all(cx);
            }
            if let Err(err) = ws.save_session(cx) {
                eprintln!("couldn't save the session: {err}");
            }
            ws.lsp.update(cx, |s, _| s.shutdown_all());
            if let Some((session, _)) = ws.debug.take() {
                session.update(cx, |s, cx| s.stop(cx));
            }
            async {}
        });
        let this = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            this.update(cx, |ws, cx| ws.prepare_to_close(CloseIntent::CloseWindow, window, cx)).unwrap_or(true)
        });
        let lsp = cx.new(|_| LspStore::new());
        let lsp_subscription = cx.subscribe_in(&lsp, window, |ws, _, event: &LspEvent, window, cx| match event {
            LspEvent::ApplyEdit { key, request_id, encoding, edits } => {
                let applied = ws.apply_workspace_edit(edits.clone(), *encoding, window, cx);
                let (key, id) = (key.clone(), request_id.clone());
                ws.lsp.read(cx).respond(&key, id, serde_json::json!({ "applied": applied }));
            }
            LspEvent::Message(message) => ws.toast(ToastKind::Error, message.clone(), None, cx),
        });
        let lsp_observer = cx.observe(&lsp, |_, _, cx| cx.notify());
        let settings = Settings::get(cx).clone();
        // Settings changed (saved settings.json): re-apply everywhere.
        let settings_observer = cx.observe_global::<Settings>(|ws, cx| {
            let settings = Settings::get(cx).clone();
            ws.auto_save = settings.auto_save.unwrap_or_default();
            ws.auto_save_delay = Duration::from_millis(settings.auto_save_delay.unwrap_or(1000).max(50));
            for tab in ws.tabs.clone() {
                tab.update(cx, |e, cx| e.apply_settings(cx));
            }
            cx.notify();
        });
        // An update finished downloading: show Restart to Update.
        let updater_observer = cx.observe_global::<updater::Updater>(|_, cx| cx.notify());
        // Extensions reloaded, or a grammar finished building.
        let extensions_observer = cx.observe_global::<extensions::ExtensionsChanged>(|ws, cx| ws.extensions_changed(cx));
        // A folder was trusted or stopped being trusted (here or in another window).
        let trust_observer = cx.observe_global::<crate::trust::TrustChanged>(|ws, cx| ws.trust_changed(cx));
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(4)).await;
                let alive = this
                    .update_in(cx, |ws, window, cx| {
                        if window.is_window_active() {
                            ws.refresh_git(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
        let mut this = Self {
            focus_handle,
            tree_focus: cx.focus_handle(),
            file_history: Default::default(),
            root: None,
            expanded: HashSet::new(),
            entries: Vec::new(),
            tree_scroll: UniformListScrollHandle::new(),
            tabs: Vec::new(),
            panes: vec![Pane::new()],
            active_pane: 0,
            pane_axis: PaneAxis::Row,
            pane_sizes: vec![1.0],
            panes_bounds: Default::default(),
            pane_drag_pair: 0.,
            pinned: HashSet::new(),
            // Without a project there's nothing to browse; opening one shows
            // the sidebar as it was left (see `set_root`).
            show_sidebar: false,
            show_problems: state.show_problems.unwrap_or(false) && !state.show_terminal.unwrap_or(false),
            // Closed unless it was open when the window last closed.
            show_terminal: state.show_terminal.unwrap_or(false),
            show_git_log: state.show_git_log.unwrap_or(false) && !state.show_terminal.unwrap_or(false) && !state.show_problems.unwrap_or(false),
            terminal_groups: Vec::new(),
            active_group: 0,
            tree_drop: None,

            last_run: None,
            debug: None,
            show_debug: false,
            last_debug: None,
            debug_location: None,
            breakpoints: HashMap::new(),
            run_problems: None,
            venv_shown: Default::default(),
            problems_height: px(state.problems_height.unwrap_or(200.).clamp(90., 4000.)),
            sidebar_width: px(state.sidebar_width.unwrap_or(250.).clamp(160., 4000.)),
            resizing: None,
            window_title: String::new(),
            find_bar,
            show_find: false,
            modal: None,
            modal_subscription: None,
            file_index: Arc::new(Vec::new()),
            index_task: None,
            recent: Vec::new(),
            actions: Arc::new(keymap::action_entries()),
            shift_down: false,
            last_shift_tap: None,
            sidebar_mode: if state.sidebar_mode.as_deref() == Some("commit") { SidebarMode::Commit } else { SidebarMode::Files },
            git_panel,
            git_log,
            diff_return: None,
            repo: None,
            git_status: Arc::new(RepoStatus::default()),
            git_paths: Arc::new(HashMap::new()),
            git_user: None,
            git_remote: None,
            last_commit: None,
            recent_projects: state.recent_projects.clone(),
            git_busy: None,
            status_task: None,
            git_op_task: None,
            toasts: Vec::new(),
            next_toast: 0,
            context_menu: None,
            pending_input: None,
            auto_save: settings.auto_save.unwrap_or_default(),
            auto_save_delay: Duration::from_millis(settings.auto_save_delay.unwrap_or(1000).max(50)),
            auto_save_tasks: HashMap::new(),
            auto_save_warned: HashSet::new(),
            close_confirmed: false,
            session_task: None,
            placement_task: None,
            onboarding: None,
            watcher: None,
            watch_task: None,
            ignore: None,
            git_refresh_again: false,
            symbol_task: None,
            lsp,
            preferences_page: preferences::Page::Appearance,
            _subscriptions: vec![
                find_subscription,
                git_subscription,
                log_subscription,
                activation,
                quit,
                settings_observer,
                updater_observer,
                extensions_observer,
                trust_observer,
                lsp_subscription,
                lsp_observer,
                cx.observe_window_bounds(window, |ws, window, cx| ws.remember_placement(window, cx)),
            ],
        };
        // Problems with settings.json / keymap.json found at startup.
        if let Some(problems) = cx.try_global::<StartupProblems>().map(|p| p.0.clone()) {
            cx.set_global(StartupProblems::default());
            this.report_config_problems(problems, cx);
        }
        this
    }

    fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((onboarding, _)) = &self.onboarding {
            window.focus(&onboarding.focus_handle(cx));
            return;
        }
        match self.active_editor() {
            Some(editor) => window.focus(&editor.focus_handle(cx)),
            None => window.focus(&self.focus_handle),
        }
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = path.canonicalize().unwrap_or(path);
        if path.is_dir() {
            self.set_root(path, window, cx);
            return;
        }
        if let Some(editor) = self.tabs.iter().find(|t| t.read(cx).doc.path.as_deref() == Some(path.as_path())).cloned() {
            self.activate_editor(&editor, window, cx);
            return;
        }
        match Document::open(&path) {
            Ok(doc) => {
                if doc.large {
                    let detail = "Highlighting, linting and language servers are off for files this big.";
                    self.toast(ToastKind::Info, "Opened as plain text", Some(detail.into()), cx);
                }
                let editor = cx.new(|cx| Editor::new(doc, cx));
                self.add_tab(editor, window, cx);
            }
            Err(err) => {
                let message = format!("Couldn't open {}", path.display());
                let detail = err.to_string();
                let _ = window.prompt(PromptLevel::Critical, &message, Some(&detail), &["OK"], cx);
            }
        }
    }

    /// Opens a file at a 1-based line and column (from terminal output).
    fn open_at(&mut self, path: PathBuf, line: Option<u32>, column: Option<u32>, window: &mut Window, cx: &mut Context<Self>) {
        let path = path.canonicalize().unwrap_or(path);
        self.open_path(path.clone(), window, cx);
        let (Some(line), Some(editor)) = (line, self.tab_for(&path, cx)) else { return };
        editor.update(cx, |e, cx| {
            let row = (line as usize).saturating_sub(1).min(e.doc.line_count().saturating_sub(1));
            let start = e.doc.line_start(row);
            let text = e.doc.line_text(row);
            let col = column.map_or(0, |c| c as usize).saturating_sub(1);
            let offset = start + text.char_indices().nth(col).map_or(text.len(), |(i, _)| i);
            e.go_to(offset, window, cx);
        });
        if let Some(editor) = self.tab_for(&path, cx) {
            window.focus(&editor.focus_handle(cx));
        }
    }

    fn add_tab(&mut self, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        self._subscriptions.push(cx.observe(&editor, |_, _, cx| cx.notify()));
        self._subscriptions.push(cx.subscribe(&editor, |ws, editor, _: &EditorEvent, cx| {
            ws.schedule_auto_save(&editor, cx);
            ws.schedule_session_save(cx);
        }));
        self._subscriptions.push(cx.subscribe_in(&editor, window, |ws, editor, event: &EditorContextMenu, window, cx| {
            ws.open_editor_menu(editor.clone(), event.0, window, cx);
        }));
        self._subscriptions.push(cx.subscribe_in(&editor, window, |ws, _, event: &EditorLspEvent, window, cx| {
            ws.on_editor_lsp_event(event, window, cx);
        }));
        self._subscriptions.push(cx.subscribe_in(&editor, window, |ws, _, event: &EditorRunEvent, window, cx| {
            ws.on_run_event(event, window, cx);
        }));
        self._subscriptions.push(cx.subscribe(&editor, |ws, editor, _: &EditorBreakpointsChanged, cx| {
            ws.breakpoints_changed(&editor, cx);
        }));
        self._subscriptions.push(cx.subscribe_in(&editor, window, |ws, editor, event: &EditorBreakpointMenu, window, cx| {
            ws.open_breakpoint_menu(editor.clone(), event.row, event.position, window, cx);
        }));
        // Breakpoints it had while closed, and the paused line if it's here.
        let path = editor.read(cx).doc.path.clone();
        if let Some(path) = path {
            if let Some(breakpoints) = self.breakpoints.remove(&path) {
                editor.update(cx, |e, cx| e.set_breakpoints(&breakpoints, cx));
            }
            if let Some((at, line)) = self.debug_location.clone().filter(|(at, _)| *at == path) {
                let _ = at;
                editor.update(cx, |e, cx| e.set_debug_line(Some(line.saturating_sub(1) as usize), cx));
            }
        }
        self._subscriptions.push(cx.subscribe_in(&editor, window, |ws, _, event: &EditorGitEvent, window, cx| {
            ws.run_git(event.0.clone(), window, cx);
        }));
        self.lsp.update(cx, |s, cx| s.register(&editor, cx));
        if self.repo.is_none() && self.root.is_none() {
            // A lone file: use the repository it lives in, if any.
            if let Some(dir) = editor.read(cx).doc.path.as_deref().and_then(Path::parent) {
                self.detect_repo(dir.to_path_buf(), cx);
            }
        } else {
            self.load_diff_base(&editor, cx);
        }
        self.tabs.push(editor.clone());
        let pane = &mut self.panes[self.active_pane];
        // After the current tab, but never among pinned ones.
        let pinned = pane.tabs.iter().filter(|t| self.pinned.contains(&t.entity_id())).count();
        let at = if pane.tabs.is_empty() { 0 } else { (pane.active + 1).max(pinned).min(pane.tabs.len()) };
        pane.tabs.insert(at, editor);
        self.activate_in(self.active_pane, at, window, cx);
    }

    /// Shows an open editor in its pane and focuses it.
    fn activate_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((pane, ix)) = self.locate(editor) {
            self.activate_in(pane, ix, window, cx);
        }
    }

    /// The pane holding `editor` and its index there.
    fn locate(&self, editor: &Entity<Editor>) -> Option<(usize, usize)> {
        self.panes.iter().enumerate().find_map(|(p, pane)| pane.tabs.iter().position(|t| t == editor).map(|ix| (p, ix)))
    }

    fn activate_in(&mut self, pane: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.panes.get(pane).and_then(|p| p.tabs.get(ix)).cloned() {
            if let Some(previous) = self.active_editor().cloned()
                && previous != editor
                && matches!(self.auto_save, AutoSave::AfterDelay | AutoSave::OnFocusChange)
            {
                self.auto_save(&previous, cx);
            }
            // A diff over the editor area would hide the file being shown.
            if matches!(self.modal, Some(Modal::Diff(_))) {
                self.modal = None;
                self.modal_subscription = None;
                self.diff_return = None;
            }
            self.active_pane = pane;
            self.panes[pane].active = ix;
            window.focus(&editor.focus_handle(cx));
            if let Some(path) = editor.read(cx).doc.path.clone() {
                self.recent.retain(|p| p != &path);
                self.recent.insert(0, path);
                self.recent.truncate(50);
            }
            if self.show_find {
                self.find_bar.update(cx, |f, cx| f.set_target(Some(editor), cx));
            }
            self.schedule_session_save(cx);
        }
        cx.notify();
    }

    fn active_editor(&self) -> Option<&Entity<Editor>> {
        let pane = self.panes.get(self.active_pane)?;
        pane.tabs.get(pane.active)
    }

    // ---- File tree --------------------------------------------------------------

    pub fn set_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.update_recent_projects(|list| {
            list.retain(|p| p != &root);
            list.insert(0, root.clone());
        });
        self.expanded.clear();
        self.expanded.insert(root.clone());
        // The folders that were open last time, if they still exist.
        self.breakpoints.clear();
        self.run_problems = None;
        let session = ProjectSession::load(&root);
        if let Some(session) = &session {
            self.expanded.extend(session.expanded.iter().map(|p| root.join(p)).filter(|p| p.is_dir()));
            self.breakpoints = session.breakpoints.iter().map(|b| (root.join(&b.path), b.breakpoints.clone())).collect();
        }
        // Before any tab's language server starts.
        let venv = session.and_then(|s| s.python_venv).map(|saved| crate::python::Choice::from_saved(&saved));
        crate::python::set_choice(&root, venv);
        self.venv_shown.take();
        self.root = Some(root);
        self.show_sidebar = SessionState::load().show_sidebar.unwrap_or(true);
        self.rebuild_tree();
        self.refresh_file_index(cx);
        self.start_watching(cx);
        if let Some(root) = self.root.clone() {
            self.detect_repo(root, cx);
        }
        self.save_state();
        let root = self.root.clone();
        self.lsp.update(cx, |s, _| s.set_root(root));
        if self.tabs.is_empty() {
            self.restore_session(window, cx);
        }
        if self.restricted(cx) && self.modal.is_none() {
            self.open_trust_dialog(window, cx);
        }
        cx.notify();
    }

    /// Reopens the folder from the previous session, if it still exists.
    pub fn restore_last_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = SessionState::load().last_root.filter(|r| r.is_dir()) else {
            return;
        };
        self.set_root(root, window, cx);
    }

    // ---- Project session --------------------------------------------------------

    /// Writes the project's tabs, carets and unsaved text for next time.
    fn save_session(&self, cx: &App) -> std::io::Result<()> {
        let Some(root) = self.root.clone() else { return Ok(()) };
        let mut tabs = Vec::new();
        let mut active = 0;
        let mut pane_active = Vec::new();
        for (pane_ix, pane) in self.panes.iter().enumerate() {
            let mut active_in_pane = 0;
            let mut count = 0;
            for (ix, tab) in pane.tabs.iter().enumerate() {
                let e = tab.read(cx);
                let dirty = e.doc.is_dirty();
                // An untitled buffer without edits has nothing to bring back.
                if e.doc.path.is_none() && !dirty {
                    continue;
                }
                if ix == pane.active {
                    active_in_pane = count;
                    if pane_ix == self.active_pane {
                        active = tabs.len();
                    }
                }
                count += 1;
                tabs.push(TabSession {
                    path: e.doc.path.clone(),
                    cursor: e.primary_selection().head,
                    unsaved: dirty.then(|| e.doc.rope().to_string()),
                    pane: pane_ix,
                    pinned: self.pinned.contains(&tab.entity_id()),
                    folds: e.folded_rows(),
                    encoding: (e.doc.encoding != TextEncoding::UTF8).then(|| e.doc.encoding.key()),
                });
            }
            pane_active.push(active_in_pane);
        }
        let layout = (self.panes.len() > 1).then(|| PaneLayout {
            column: self.pane_axis == PaneAxis::Column,
            sizes: self.pane_sizes.clone(),
            active: pane_active,
        });
        let mut expanded: Vec<PathBuf> =
            self.expanded.iter().filter_map(|p| p.strip_prefix(&root).ok()).filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).collect();
        expanded.sort();
        let mut breakpoints: Vec<FileBreakpoints> = self
            .all_breakpoints(cx)
            .into_iter()
            .filter_map(|(path, breakpoints)| Some(FileBreakpoints { path: path.strip_prefix(&root).ok()?.to_path_buf(), breakpoints }))
            .collect();
        breakpoints.sort_by(|a, b| a.path.cmp(&b.path));
        let python_venv = crate::python::choice(&root).map(|c| c.to_saved());
        ProjectSession { root, tabs, active, layout, expanded, breakpoints, python_venv }.save()
    }

    fn schedule_session_save(&mut self, cx: &mut Context<Self>) {
        if self.root.is_none() {
            return;
        }
        self.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_millis(800)).await;
            this.update(cx, |ws, cx| {
                if let Err(err) = ws.save_session(cx) {
                    eprintln!("couldn't save the session: {err}");
                }
            })
            .ok();
        }));
    }

    /// Reopens the tabs the project had last time, with their unsaved edits.
    fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.root.as_deref().and_then(ProjectSession::load) else {
            return;
        };
        for tab in session.tabs {
            let doc = match tab.path.as_deref().map(Document::open) {
                Some(Ok(mut doc)) => {
                    // An encoding picked last time, rather than the guess.
                    if let Some(encoding) = tab.encoding.as_deref().and_then(TextEncoding::from_key)
                        && encoding != doc.encoding
                    {
                        let _ = doc.reload_with_encoding(encoding);
                    }
                    doc
                }
                // The file is gone and there's nothing unsaved to keep.
                Some(Err(_)) if tab.unsaved.is_none() => continue,
                // Untitled, or a deleted file with edits: keep the text.
                _ => Document::empty(),
            };
            let editor = cx.new(|cx| Editor::new(doc, cx));
            if let Some(text) = tab.unsaved {
                // An edit on top of the file, so undo gets back to the saved text.
                editor.update(cx, |e, cx| {
                    let len = e.doc.len();
                    e.replace_ranges(vec![(0..len, text)], cx);
                });
            }
            // Into the pane it was in, creating panes as needed.
            while self.panes.len() <= tab.pane.min(7) {
                self.panes.push(Pane::new());
                self.pane_sizes.push(0.);
            }
            self.active_pane = tab.pane.min(7);
            if tab.pinned {
                self.pinned.insert(editor.entity_id());
            }
            self.add_tab(editor.clone(), window, cx);
            editor.update(cx, |e, cx| {
                e.restore_folds(&tab.folds, cx);
                e.go_to(tab.cursor.min(e.doc.len()), window, cx);
            });
        }
        // Panes left empty (their files are gone) aren't worth keeping.
        self.drop_empty_panes();
        if let Some(layout) = session.layout.filter(|l| l.sizes.len() == self.panes.len()) {
            self.pane_axis = if layout.column { PaneAxis::Column } else { PaneAxis::Row };
            self.pane_sizes = layout.sizes;
            for (pane, active) in self.panes.iter_mut().zip(layout.active) {
                pane.active = active.min(pane.tabs.len().saturating_sub(1));
            }
        } else {
            self.pane_sizes = vec![1. / self.panes.len() as f32; self.panes.len()];
        }
        if let Some(editor) = self.tabs_in_order().get(session.active).cloned() {
            self.activate_editor(&editor, window, cx);
        }
    }

    /// Tabs in pane order, as the session lists them.
    fn tabs_in_order(&self) -> Vec<Entity<Editor>> {
        self.panes.iter().flat_map(|p| p.tabs.iter().cloned()).collect()
    }

    /// Removes empty panes (keeping at least one) and rebalances sizes.
    fn drop_empty_panes(&mut self) {
        let mut ix = 0;
        while ix < self.panes.len() {
            if self.panes.len() > 1 && self.panes[ix].tabs.is_empty() {
                self.remove_pane(ix);
            } else {
                ix += 1;
            }
        }
    }

    fn remove_pane(&mut self, ix: usize) {
        self.panes.remove(ix);
        let freed = if ix < self.pane_sizes.len() { self.pane_sizes.remove(ix) } else { 0. };
        // The neighbor takes the space.
        if let Some(size) = self.pane_sizes.get_mut(ix.saturating_sub(1).min(self.panes.len().saturating_sub(1))) {
            *size += freed;
        }
        if self.panes.len() == 1 {
            self.pane_sizes = vec![1.0];
        }
        if self.active_pane >= ix && self.active_pane > 0 {
            self.active_pane -= 1;
        }
        self.active_pane = self.active_pane.min(self.panes.len() - 1);
    }

    /// Changes the recent projects list shared by all windows. It's re-read
    /// from disk first so one window doesn't drop another's entries.
    fn update_recent_projects(&mut self, f: impl FnOnce(&mut Vec<PathBuf>)) {
        let mut state = SessionState::load();
        f(&mut state.recent_projects);
        state.recent_projects.truncate(MAX_RECENT_PROJECTS);
        self.recent_projects = state.recent_projects.clone();
        if let Err(err) = state.save() {
            eprintln!("couldn't save recent projects: {err}");
        }
    }

    /// Remembers layout and the open folder for the next launch.
    fn save_state(&self) {
        let saved = SessionState::load();
        let state = SessionState {
            // Opening a lone file shouldn't forget the remembered project.
            last_root: self.root.clone().or(saved.last_root),
            sidebar_width: Some(self.sidebar_width.into()),
            problems_height: Some(self.problems_height.into()),
            // The sidebar's visibility is remembered for projects only; it's
            // always closed without one.
            show_sidebar: if self.root.is_some() { Some(self.show_sidebar) } else { saved.show_sidebar },
            show_problems: Some(self.show_problems),
            show_terminal: Some(self.show_terminal),
            sidebar_mode: Some(match self.sidebar_mode {
                SidebarMode::Files => "files".into(),
                SidebarMode::Commit => "commit".into(),
            }),
            show_git_log: Some(self.show_git_log),
            // Shared with other windows, so keep what's on disk.
            font_zoom: saved.font_zoom,
            diff_side_by_side: saved.diff_side_by_side,
            recent_projects: saved.recent_projects,
            trusted_folders: saved.trusted_folders,
            window: saved.window,
            welcome_done: saved.welcome_done,
            other: saved.other,
        };
        if let Err(err) = state.save() {
            eprintln!("couldn't save window state: {err}");
        }
    }

    /// Saves where the window is (after moves and resizes settle), so the
    /// next launch opens it there.
    fn remember_placement(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (bounds, fullscreen) = match window.window_bounds() {
            WindowBounds::Fullscreen(bounds) => (bounds, true),
            WindowBounds::Windowed(bounds) | WindowBounds::Maximized(bounds) => (bounds, false),
        };
        let placement = WindowPlacement {
            display: window.display(cx).and_then(|d| d.uuid().ok()).map(|u| u.to_string()),
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
            fullscreen,
        };
        self.placement_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(Duration::from_millis(400)).await;
            let mut state = SessionState::load();
            state.window = Some(placement);
            if let Err(err) = state.save() {
                eprintln!("couldn't save window state: {err}");
            }
        }));
    }

    /// Shows Welcome Setup (theme, keymap, editor basics) over the window.
    pub fn show_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let onboarding = cx.new(Onboarding::new);
        let subscription = cx.subscribe_in(&onboarding, window, |ws, _, event: &OnboardingEvent, window, cx| match event {
            OnboardingEvent::Done => ws.finish_onboarding(window, cx),
            OnboardingEvent::OpenSettings => {
                ws.finish_onboarding(window, cx);
                ws.open_settings(&OpenSettings, window, cx);
            }
            OnboardingEvent::BrowseThemes => ws.select_theme(&SelectTheme, window, cx),
            OnboardingEvent::Problems(problems) => ws.report_config_problems(problems.clone(), cx),
        });
        window.focus(&onboarding.focus_handle(cx));
        self.onboarding = Some((onboarding, subscription));
        cx.notify();
    }

    fn welcome_setup(&mut self, _: &WelcomeSetup, window: &mut Window, cx: &mut Context<Self>) {
        self.show_onboarding(window, cx);
    }

    fn finish_onboarding(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.onboarding = None;
        SessionState::set_onboarded();
        self.focus_active(window, cx);
        cx.notify();
    }

    // ---- Watching the disk --------------------------------------------------------

    /// Watches the project folder so changes made outside Kin show up right away.
    fn start_watching(&mut self, cx: &mut Context<Self>) {
        self.watcher = None;
        self.watch_task = None;
        let Some(root) = self.root.clone() else { return };
        self.ignore = project_ignore(&root);
        let (watcher, mut changes) = match Watcher::new(&root) {
            Ok(watching) => watching,
            Err(err) => {
                eprintln!("couldn't watch {}: {err}", root.display());
                return;
            }
        };
        self.watcher = Some(watcher);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            use futures::StreamExt as _;
            while let Some(mut batch) = changes.next().await {
                // A burst (a checkout, npm install) becomes one update.
                cx.background_executor().timer(Duration::from_millis(150)).await;
                while let Ok(more) = changes.try_recv() {
                    batch.extend(more);
                }
                if this.update(cx, |ws, cx| ws.disk_changed(batch, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Updates what a batch of changes on disk affects: the tree (only for
    /// folders it shows), Go to File, Git status and open files.
    fn disk_changed(&mut self, changes: Vec<FsChange>, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        // A venv may have been created or removed.
        self.venv_shown.take();
        let (mut tree, mut index, mut git) = (false, false, false);
        let mut paths = HashSet::new();
        for change in changes {
            match watcher::git_path(&change.path) {
                GitPath::Metadata => git = true,
                GitPath::Internal => {}
                GitPath::WorkTree => {
                    if change.structural && change.path.parent().is_some_and(|p| self.expanded.contains(p)) {
                        tree = true;
                    }
                    if watcher::in_heavy_folder(&root, &change.path) || !change.path.starts_with(&root) {
                        continue;
                    }
                    if change.path.file_name().is_some_and(|n| n == ".gitignore") {
                        self.ignore = project_ignore(&root);
                    }
                    let ignored = |path: &Path| {
                        path.starts_with(&root)
                            && self.ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(path, false).is_ignore())
                    };
                    // Go to File lists ignored files, but not inside ignored folders.
                    index |= change.structural && !change.path.parent().is_some_and(ignored);
                    git |= !ignored(&change.path);
                    paths.insert(change.path);
                }
            }
        }
        if tree {
            self.rebuild_tree();
            cx.notify();
        }
        if index {
            self.refresh_file_index(cx);
        }
        if git {
            self.refresh_git(cx);
        }
        for editor in self.tabs.clone() {
            if editor.read(cx).doc.path.as_ref().is_some_and(|p| paths.contains(p)) {
                editor.update(cx, |e, cx| e.reload_if_changed(cx));
            }
        }
    }

    /// Rescans project files in the background (for Go to File).
    fn refresh_file_index(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else {
            return;
        };
        self.index_task = Some(cx.spawn(async move |this, cx| {
            let files = cx.background_executor().spawn(async move { openable_files(&root) }).await;
            this.update(cx, |ws, cx| {
                ws.file_index = Arc::new(files);
                if let Some(Modal::Picker(picker)) = &ws.modal {
                    let files = ws.file_index.clone();
                    picker.update(cx, |p, cx| p.set_files(files, cx));
                }
            })
            .ok();
        }));
    }

    fn rebuild_tree(&mut self) {
        self.entries.clear();
        if let Some(root) = self.root.clone() {
            self.push_children(&root, 0);
        }
    }

    fn push_children(&mut self, dir: &Path, depth: usize) {
        let Ok(read) = std::fs::read_dir(dir) else { return };
        let mut children: Vec<(PathBuf, bool, String)> = read
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if matches!(name.as_str(), ".git" | ".DS_Store" | "target") {
                    return None;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Some((e.path(), is_dir, name))
            })
            .collect();
        children.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.to_lowercase().cmp(&b.2.to_lowercase())));
        for (path, is_dir, name) in children {
            let expanded = is_dir && self.expanded.contains(&path);
            self.entries.push(TreeEntry {
                path: path.clone(),
                name: name.into(),
                depth,
                is_dir,
            });
            if expanded {
                self.push_children(&path, depth + 1);
            }
        }
    }

    fn click_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix).cloned() else { return };
        if entry.is_dir {
            if !self.expanded.remove(&entry.path) {
                self.expanded.insert(entry.path);
            }
            self.rebuild_tree();
            self.schedule_session_save(cx);
            cx.notify();
        } else {
            self.open_path(entry.path, window, cx);
        }
    }

    // ---- Actions ------------------------------------------------------------------

    fn new_file(&mut self, _: &NewFile, window: &mut Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| Editor::new(Document::empty(), cx));
        self.add_tab(editor, window, cx);
    }

    fn open_file(&mut self, _: &OpenFile, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    for p in paths {
                        ws.open_path(p, window, cx);
                    }
                })
                .ok();
            }
        })
        .detach();
    }

    fn open_folder(&mut self, _: &OpenFolder, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_for_folder(OpenIn::Auto, window, cx);
    }

    fn open_folder_in_this_window(&mut self, _: &OpenFolderInThisWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.prompt_for_folder(OpenIn::ThisWindow, window, cx);
    }

    fn prompt_for_folder(&mut self, target: OpenIn, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = rx.await
                && let Some(path) = paths.pop()
            {
                this.update_in(cx, |ws, window, cx| ws.open_project(path, target, window, cx)).ok();
            }
        })
        .detach();
    }

    fn new_window(&mut self, _: &NewWindow, window: &mut Window, cx: &mut Context<Self>) {
        open_window(Some(window.bounds()), cx, |_, _, _| {});
    }

    fn minimize_window(&mut self, _: &MinimizeWindow, window: &mut Window, _: &mut Context<Self>) {
        window.minimize_window();
    }

    fn zoom_window(&mut self, _: &ZoomWindow, window: &mut Window, _: &mut Context<Self>) {
        window.zoom_window();
    }

    fn close_window(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        if self.prepare_to_close(CloseIntent::CloseWindow, window, cx) {
            window.remove_window();
        }
    }

    /// Opens a project folder. A folder that's already open in another
    /// window brings that window to the front instead.
    fn open_project(&mut self, path: PathBuf, target: OpenIn, window: &mut Window, cx: &mut Context<Self>) {
        let path = path.canonicalize().unwrap_or(path);
        if !path.is_dir() {
            self.toast(ToastKind::Error, "Folder not found", Some(clone_dialog::display_path(&path)), cx);
            self.update_recent_projects(|list| list.retain(|p| p != &path));
            cx.notify();
            return;
        }
        if self.root.as_deref() == Some(path.as_path()) {
            return;
        }
        if let Some(other) = window_with_project(&path, cx) {
            other.update(cx, |_, window, _| window.activate_window()).ok();
            return;
        }
        match target {
            OpenIn::Auto if self.root.is_none() => self.set_root(path, window, cx),
            OpenIn::ThisWindow => self.replace_project(path, window, cx),
            OpenIn::NewWindow | OpenIn::Auto => {
                open_window(Some(window.bounds()), cx, move |ws, window, cx| ws.set_root(path, window, cx));
            }
        }
    }

    /// Swaps this window's project for another one, closing its tabs once
    /// unsaved edits are saved or discarded. Without a project yet, lone
    /// files stay open.
    fn replace_project(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.root.is_none() {
            self.set_root(path, window, cx);
        } else if self.prepare_to_close(CloseIntent::ReplaceProject(path.clone()), window, cx) {
            self.finish_close(CloseIntent::ReplaceProject(path), window, cx);
        }
    }

    /// Clears everything tied to the open project.
    fn close_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.context_menu = None;
        self.pending_input = None;
        self.show_find = false;
        self.find_bar.update(cx, |f, cx| f.set_target(None, cx));
        self.tabs.clear();
        self.panes = vec![Pane::new()];
        self.active_pane = 0;
        self.pane_sizes = vec![1.0];
        self.pinned.clear();
        self.session_task = None;
        self.terminal_groups.clear();
        self.active_group = 0;
        self.show_terminal = false;
        if let Some((session, _)) = self.debug.take() {
            session.update(cx, |s, cx| s.stop(cx));
        }
        self.show_debug = false;
        self.debug_location = None;
        self.breakpoints.clear();
        self.lsp.update(cx, |s, _| {
            s.shutdown_all();
            s.set_root(None);
        });
        self.auto_save_tasks.clear();
        self.auto_save_warned.clear();
        self.recent.clear();
        self.file_index = Arc::new(Vec::new());
        self.index_task = None;
        self.watcher = None;
        self.watch_task = None;
        self.root = None;
        self.show_sidebar = false;
        self.repo = None;
        self.git_remote = None;
        self.last_commit = None;
        self.git_status = Arc::new(RepoStatus::default());
        self.git_paths = Arc::new(HashMap::new());
        window.focus(&self.focus_handle);
        cx.notify();
    }

    fn save(&mut self, _: &Save, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        if editor.read(cx).doc.path.is_none() {
            self.save_as(&SaveAs, window, cx);
            return;
        }
        // Format on save: only for an explicit Save, like VS Code, so
        // auto-save never reformats mid-typing.
        if editor.read(cx).format_on_save() {
            let task = editor.update(cx, |e, cx| e.format(false, cx));
            cx.spawn_in(window, async move |this, cx| {
                let result = task.await;
                this.update_in(cx, |ws, window, cx| {
                    if let Err(err) = result {
                        ws.toast(ToastKind::Info, "Saved without formatting", Some(err), cx);
                    }
                    ws.save_editor(&editor, window, cx);
                })
                .ok();
            })
            .detach();
            return;
        }
        self.save_editor(&editor, window, cx);
    }

    fn save_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        match editor.update(cx, |e, cx| e.save(cx)) {
            Ok(()) => {
                self.auto_save_warned.remove(&editor.entity_id());
                self.check_config(cx);
            }
            Err(err) => {
                let _ = window.prompt(PromptLevel::Critical, "Couldn't save file", Some(&err.to_string()), &["OK"], cx);
            }
        }
        self.refresh_git(cx);
    }

    // ---- Settings and keymap files -----------------------------------------------------

    /// Applies settings.json / keymap.json if they changed, reporting problems.
    fn check_config(&mut self, cx: &mut Context<Self>) {
        let problems = settings::reload_if_changed(cx);
        self.report_config_problems(problems, cx);
    }

    fn report_config_problems(&mut self, problems: Vec<String>, cx: &mut Context<Self>) {
        const SHOWN: usize = 3;
        for problem in problems.iter().take(SHOWN) {
            let (file, detail) = problem.split_once(": ").unwrap_or(("settings", problem));
            self.toast(ToastKind::Error, format!("Problem in {file}"), Some(detail.to_string()), cx);
        }
        if problems.len() > SHOWN {
            self.toast(ToastKind::Error, format!("{} more problems in your settings", problems.len() - SHOWN), None, cx);
        }
    }

    /// Opens a config file in a tab, creating it from `template` first.
    fn open_config_file(&mut self, path: Option<PathBuf>, template: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = path else { return };
        if !path.exists() {
            let written = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, template));
            if let Err(err) = written {
                let _ = window.prompt(PromptLevel::Warning, "Couldn't create the file", Some(&err.to_string()), &["OK"], cx);
                return;
            }
        }
        self.open_path(path, window, cx);
    }

    // ---- Language servers -----------------------------------------------------------

    fn on_editor_lsp_event(&mut self, event: &EditorLspEvent, window: &mut Window, cx: &mut Context<Self>) {
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
            EditorLspEvent::CodeActions { server, actions, position } => {
                let entries = actions
                    .iter()
                    .map(|a| if a.preferred { MenuEntry::item(a.title.clone(), Icon::Check) } else { MenuEntry::text(a.title.clone()) })
                    .collect();
                let menu_actions = actions.iter().map(|a| Some(MenuAction::CodeAction(server.clone(), a.clone()))).collect();
                self.show_context_menu(entries, menu_actions, *position, None, window, cx);
            }
            EditorLspEvent::Message(message) => self.toast(ToastKind::Info, message.clone(), None, cx),
        }
    }

    /// The open tab for `path`, comparing real paths (servers may report
    /// `/tmp/…` for `/private/tmp/…`).
    fn tab_for(&self, path: &Path, cx: &App) -> Option<Entity<Editor>> {
        let wanted = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.tabs
            .iter()
            .find(|t| {
                t.read(cx).doc.path.as_deref().is_some_and(|p| p == path || p.canonicalize().is_ok_and(|p| p == wanted))
            })
            .cloned()
    }

    fn open_location(&mut self, location: Location, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        self.open_path(location.path.clone(), window, cx);
        if let Some(editor) = self.tab_for(&location.path, cx) {
            editor.update(cx, |e, cx| {
                let offset = protocol::position_to_offset(e.doc.rope(), location.range.start, encoding);
                e.go_to(offset, window, cx);
            });
        }
    }

    /// Lists usages (or several declarations) to pick from.
    fn show_locations(&mut self, title: SharedString, locations: Vec<Location>, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) {
        let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut rows = Vec::with_capacity(locations.len());
        for location in locations {
            let line = location.range.start.line as usize;
            let lines = files.entry(location.path.clone()).or_insert_with(|| match self.tab_for(&location.path, cx) {
                Some(editor) => editor.read(cx).doc.rope().to_string().lines().map(str::to_string).collect(),
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
    fn apply_workspace_edit(&mut self, edits: Vec<(PathBuf, Vec<TextEdit>)>, encoding: Encoding, window: &mut Window, cx: &mut Context<Self>) -> bool {
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
                let resolved = protocol::resolve_edits(e.doc.rope(), &file_edits, encoding);
                e.replace_ranges(resolved, cx);
            });
        }
        // Opening files switched tabs; go back to where the user was.
        if let Some(active) = active {
            self.activate_editor(&active, window, cx);
        }
        applied
    }

    fn rename_symbol(&mut self, server: Arc<Server>, path: PathBuf, position: Position, new_name: String, window: &mut Window, cx: &mut Context<Self>) {
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
    fn apply_code_action(&mut self, server: Arc<Server>, action: serde_json::Value, window: &mut Window, cx: &mut Context<Self>) {
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
    fn execute_command(&mut self, server: Arc<Server>, command: serde_json::Value, cx: &mut Context<Self>) {
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

    fn restart_language_server(&mut self, _: &RestartLanguageServer, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_editor().and_then(|e| e.read(cx).doc.path.clone()) else { return };
        self.lsp.update(cx, |s, cx| s.restart(&path, cx));
    }

    fn open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
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
    fn open_extensions(&mut self, _: &OpenExtensions, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        self.preferences_page = preferences::Page::Extensions;
        self.open_settings(&OpenSettings, window, cx);
    }

    fn reload_extensions(&mut self, _: &ReloadExtensions, _: &mut Window, cx: &mut Context<Self>) {
        extensions::reload(cx);
        self.toast(ToastKind::Success, "Extensions reloaded", None, cx);
    }

    fn open_settings_file(&mut self, _: &OpenSettingsFile, window: &mut Window, cx: &mut Context<Self>) {
        self.open_config_file(Settings::path(), settings::SETTINGS_TEMPLATE, window, cx);
    }

    fn open_keymap(&mut self, _: &OpenKeymap, window: &mut Window, cx: &mut Context<Self>) {
        self.open_config_file(keymap::keymap_path(), keymap::KEYMAP_TEMPLATE, window, cx);
    }

    // ---- Auto-save -------------------------------------------------------------------

    /// Restarts the idle timer for an edited tab in `after_delay` mode.
    fn schedule_auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        if self.auto_save != AutoSave::AfterDelay || !editor.read(cx).doc.is_dirty() {
            return;
        }
        let delay = self.auto_save_delay;
        let target = editor.downgrade();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |ws, cx| {
                if let Some(editor) = target.upgrade() {
                    ws.auto_save(&editor, cx);
                }
            })
            .ok();
        });
        self.auto_save_tasks.insert(editor.entity_id(), task);
    }

    /// Writes a tab's edits to disk if it has a file and unsaved changes.
    /// Files changed or deleted by another program are left alone (and
    /// reported once) so auto-save never clobbers them. Returns whether the
    /// tab is saved afterwards.
    fn auto_save(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> bool {
        let id = editor.entity_id();
        self.auto_save_tasks.remove(&id);
        let doc = &editor.read(cx).doc;
        if !doc.is_dirty() {
            return true;
        }
        if doc.path.is_none() {
            return false;
        }
        if doc.changed_on_disk() {
            let name = doc.file_name();
            if self.auto_save_warned.insert(id) {
                self.toast(
                    ToastKind::Error,
                    format!("{name} changed on disk"),
                    Some("Auto-save is paused for this file so it doesn't overwrite the other change. Save it manually to keep your version.".into()),
                    cx,
                );
            }
            return false;
        }
        match editor.update(cx, |e, cx| e.save(cx)) {
            Ok(()) => {
                self.auto_save_warned.remove(&id);
                self.refresh_git(cx);
                self.check_config(cx);
                true
            }
            Err(err) => {
                let name = editor.read(cx).doc.file_name();
                if self.auto_save_warned.insert(id) {
                    self.toast(ToastKind::Error, format!("Couldn't auto-save {name}"), Some(err.to_string()), cx);
                }
                false
            }
        }
    }

    fn auto_save_all(&mut self, cx: &mut Context<Self>) {
        for editor in self.tabs.clone() {
            self.auto_save(&editor, cx);
        }
    }

    fn toggle_auto_save(&mut self, _: &ToggleAutoSave, window: &mut Window, cx: &mut Context<Self>) {
        self.auto_save = if self.auto_save == AutoSave::Off { AutoSave::AfterDelay } else { AutoSave::Off };
        if let Err(err) = Settings::set("auto_save", self.auto_save.key().into()) {
            let _ = window.prompt(PromptLevel::Warning, "Couldn't save the auto-save choice", Some(&err.to_string()), &["OK"], cx);
        }
        self.check_config(cx);
        if self.auto_save == AutoSave::Off {
            self.auto_save_tasks.clear();
            self.toast(ToastKind::Success, "Auto-save off", None, cx);
        } else {
            self.auto_save_all(cx);
            self.toast(ToastKind::Success, "Auto-save on", None, cx);
        }
    }

    fn check_for_updates(&mut self, _: &CheckForUpdates, _: &mut Window, cx: &mut Context<Self>) {
        let check = updater::check(cx);
        cx.spawn(async move |this, cx| {
            let result = check.await;
            this.update(cx, |ws, cx| match result {
                Ok(updater::Check::UpToDate) => {
                    ws.toast(ToastKind::Success, format!("Kin {} is up to date", env!("CARGO_PKG_VERSION")), None, cx)
                }
                Ok(updater::Check::Ready(version)) => ws.toast(
                    ToastKind::Success,
                    format!("Kin {version} is ready"),
                    Some("Restart to Update in the status bar installs it.".into()),
                    cx,
                ),
                Err(err) => ws.toast(ToastKind::Error, "Couldn't check for updates", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    fn restart_to_update(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = updater::restart(cx) {
            self.toast(ToastKind::Error, "Couldn't install the update", Some(err), cx);
        }
    }

    /// Saves what it can before the window closes, the app quits or the
    /// project is replaced. A project's unsaved edits go into its session, so
    /// that always proceeds. Without a project, returns false (and asks the
    /// user) when edits would still be lost: untitled files, files that
    /// couldn't be saved, or any edits with auto-save off.
    fn prepare_to_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.auto_save != AutoSave::Off {
            self.auto_save_all(cx);
        }
        // A project keeps unsaved edits in its session and brings them back
        // next time ("hot exit"), so there's nothing to ask.
        if self.root.is_some() {
            self.session_task = None;
            match self.save_session(cx) {
                Ok(()) => return true,
                Err(err) => eprintln!("couldn't save the session: {err}"),
            }
        }
        if self.close_confirmed && !matches!(intent, CloseIntent::ReplaceProject(_)) {
            return true;
        }
        let unsaved: Vec<String> = self
            .tabs
            .iter()
            .map(|t| &t.read(cx).doc)
            .filter(|d| d.is_dirty())
            .map(|d| d.file_name())
            .collect();
        if unsaved.is_empty() {
            return true;
        }
        let message = match unsaved.as_slice() {
            [one] => format!("Do you want to save the changes you made to {one}?"),
            many => format!("Do you want to save the changes you made to {} files?", many.len()),
        };
        let rx = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Your changes will be lost if you don't save them."),
            &["Save All", "Don't Save", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = rx.await else { return };
            this.update_in(cx, |ws, window, cx| match answer {
                0 => {
                    for editor in ws.tabs.clone() {
                        if editor.read(cx).doc.is_dirty() && editor.read(cx).doc.path.is_some() {
                            if let Err(err) = editor.update(cx, |e, cx| e.save(cx)) {
                                let name = editor.read(cx).doc.file_name();
                                ws.toast(ToastKind::Error, format!("Couldn't save {name}"), Some(err.to_string()), cx);
                            }
                        }
                    }
                    // Untitled files need a name first; show the first one.
                    if let Some(dirty) = ws.tabs.iter().find(|t| t.read(cx).doc.is_dirty()).cloned() {
                        ws.activate_editor(&dirty, window, cx);
                        if dirty.read(cx).doc.path.is_none() {
                            ws.save_as(&SaveAs, window, cx);
                        }
                    } else {
                        ws.finish_close(intent, window, cx);
                    }
                }
                1 => ws.finish_close(intent, window, cx),
                // Windows that already agreed to quit must ask again next time.
                _ if matches!(intent, CloseIntent::Quit) => cx.defer(cancel_quit),
                _ => {}
            })
            .ok();
        })
        .detach();
        false
    }

    fn finish_close(&mut self, intent: CloseIntent, window: &mut Window, cx: &mut Context<Self>) {
        match intent {
            CloseIntent::CloseWindow => {
                self.close_confirmed = true;
                window.remove_window();
            }
            CloseIntent::Quit => {
                self.close_confirmed = true;
                // Go on to the next window with unsaved edits.
                cx.defer(quit);
            }
            CloseIntent::ReplaceProject(path) => {
                self.close_project(window, cx);
                self.set_root(path, window, cx);
            }
        }
    }

    fn save_as(&mut self, _: &SaveAs, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        let current = editor.read(cx).doc.path.clone();
        let dir = current
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .or_else(|| self.root.clone())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let name = editor.read(cx).doc.file_name();
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                this.update_in(cx, |ws, window, cx| {
                    let result = editor.update(cx, |e, cx| {
                        e.set_path(path, cx);
                        e.save(cx)
                    });
                    if let Err(err) = result {
                        let _ = window.prompt(PromptLevel::Critical, "Couldn't save file", Some(&err.to_string()), &["OK"], cx);
                    }
                    // A new name can mean a new language and server.
                    ws.lsp.update(cx, |s, cx| s.register(&editor, cx));
                    ws.rebuild_tree();
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.close_editor(&editor, window, cx);
        }
    }

    fn close_editor(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let editor = editor.clone();
        if self.auto_save != AutoSave::Off {
            self.auto_save(&editor, cx);
        }
        if editor.read(cx).doc.is_dirty() {
            let name = editor.read(cx).doc.file_name();
            let rx = window.prompt(
                PromptLevel::Warning,
                &format!("Do you want to save the changes you made to {name}?"),
                Some("Your changes will be lost if you don't save them."),
                &["Save", "Don't Save", "Cancel"],
                cx,
            );
            cx.spawn_in(window, async move |this, cx| {
                let Ok(answer) = rx.await else { return };
                this.update_in(cx, |ws, window, cx| match answer {
                    0 => {
                        if editor.read(cx).doc.path.is_some() && editor.update(cx, |e, cx| e.save(cx)).is_ok() {
                            ws.remove_tab(&editor, window, cx);
                        } else {
                            ws.activate_editor(&editor, window, cx);
                            ws.save_as(&SaveAs, window, cx);
                        }
                    }
                    1 => ws.remove_tab(&editor, window, cx),
                    _ => {}
                })
                .ok();
            })
            .detach();
        } else {
            self.remove_tab(&editor, window, cx);
        }
    }

    fn remove_tab(&mut self, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tabs.iter().position(|t| t == editor) else { return };
        self.tabs.remove(ix);
        self.pinned.remove(&editor.entity_id());
        let was_active_pane = self.locate(editor).is_some_and(|(p, _)| p == self.active_pane);
        if let Some(path) = editor.read(cx).doc.path.clone() {
            self.lsp.update(cx, |s, _| s.did_close(&path));
            let breakpoints = editor.read(cx).breakpoints();
            if !breakpoints.is_empty() {
                self.breakpoints.insert(path, breakpoints);
            }
        }
        self.schedule_session_save(cx);
        self.auto_save_tasks.remove(&editor.entity_id());
        self.auto_save_warned.remove(&editor.entity_id());
        if let Some((pane_ix, tab_ix)) = self.locate(editor) {
            let pane = &mut self.panes[pane_ix];
            pane.tabs.remove(tab_ix);
            if tab_ix < pane.active || pane.active >= pane.tabs.len() {
                pane.active = pane.active.saturating_sub(1);
            }
            // A pane closes with its last tab (unless it's the only one).
            if pane.tabs.is_empty() && self.panes.len() > 1 {
                self.remove_pane(pane_ix);
            }
        }
        if self.tabs.is_empty() {
            self.show_find = false;
            self.find_bar.update(cx, |f, cx| f.set_target(None, cx));
            window.focus(&self.focus_handle);
        } else if was_active_pane || self.active_editor().is_none() {
            let pane = self.active_pane;
            match self.panes[pane].tabs.is_empty() {
                true => window.focus(&self.focus_handle),
                false => self.activate_in(pane, self.panes[pane].active, window, cx),
            }
        }
        cx.notify();
    }

    fn next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let pane = &self.panes[self.active_pane];
        if !pane.tabs.is_empty() {
            let ix = (pane.active + 1) % pane.tabs.len();
            self.activate_in(self.active_pane, ix, window, cx);
        }
    }

    fn prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        let pane = &self.panes[self.active_pane];
        if !pane.tabs.is_empty() {
            let ix = (pane.active + pane.tabs.len() - 1) % pane.tabs.len();
            self.activate_in(self.active_pane, ix, window, cx);
        }
    }

    // ---- Panes and tab management ---------------------------------------------------------

    fn split_right(&mut self, _: &SplitRight, window: &mut Window, cx: &mut Context<Self>) {
        self.split(PaneAxis::Row, window, cx);
    }

    fn split_down(&mut self, _: &SplitDown, window: &mut Window, cx: &mut Context<Self>) {
        self.split(PaneAxis::Column, window, cx);
    }

    /// Split and Move: a new pane after the current one takes the current
    /// tab (or starts empty when that's the pane's only tab). A file has one
    /// editor, so it isn't duplicated.
    fn split(&mut self, axis: PaneAxis, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() >= 6 {
            return;
        }
        // One direction at a time: splitting the other way re-flows the panes.
        self.pane_axis = axis;
        let source = self.active_pane;
        let moving = (self.panes[source].tabs.len() > 1).then(|| self.active_editor().cloned()).flatten();
        let at = source + 1;
        self.panes.insert(at, Pane::new());
        // The new pane takes half of the current one's space.
        let half = self.pane_sizes[source] / 2.;
        self.pane_sizes[source] = half;
        self.pane_sizes.insert(at, half);
        self.active_pane = at;
        match moving {
            Some(editor) => self.move_tab(&editor, at, None, window, cx),
            None => window.focus(&self.focus_handle),
        }
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Moves a tab to `pane` (before index `before`, or at the end).
    fn move_tab(&mut self, editor: &Entity<Editor>, pane: usize, before: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((from, ix)) = self.locate(editor) else { return };
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
        target.tabs.insert(at, editor.clone());
        self.keep_pinned_first(pane);
        let target_pane = pane;
        // The source pane closes if that was its last tab.
        let editor = editor.clone();
        if from != target_pane && self.panes[from].tabs.is_empty() && self.panes.len() > 1 {
            self.remove_pane(from);
        }
        self.activate_editor(&editor, window, cx);
        self.schedule_session_save(cx);
    }

    fn next_pane(&mut self, _: &NextPane, window: &mut Window, cx: &mut Context<Self>) {
        if self.panes.len() < 2 {
            return;
        }
        let next = (self.active_pane + 1) % self.panes.len();
        self.active_pane = next;
        match self.panes[next].tabs.get(self.panes[next].active).cloned() {
            Some(editor) => self.activate_editor(&editor, window, cx),
            None => window.focus(&self.focus_handle),
        }
        cx.notify();
    }

    /// Pinned tabs first, in their order.
    fn keep_pinned_first(&mut self, pane: usize) {
        let active = self.panes[pane].tabs.get(self.panes[pane].active).cloned();
        let pinned = &self.pinned;
        self.panes[pane].tabs.sort_by_key(|t| !pinned.contains(&t.entity_id()));
        if let Some(active) = active {
            self.panes[pane].active = self.panes[pane].tabs.iter().position(|t| t == &active).unwrap_or(0);
        }
    }

    fn toggle_pin_tab(&mut self, _: &TogglePinTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.run_tab_action(TabAction::TogglePin, &editor, window, cx);
        }
    }

    fn close_other_tabs(&mut self, _: &CloseOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.run_tab_action(TabAction::CloseOthers, &editor, window, cx);
        }
    }

    fn close_all_tabs(&mut self, _: &CloseAllTabs, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.active_editor().cloned() {
            self.run_tab_action(TabAction::CloseAll, &editor, window, cx);
        }
    }

    fn run_tab_action(&mut self, action: TabAction, editor: &Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((pane, ix)) = self.locate(editor) else { return };
        let others = |ws: &Self, keep: &dyn Fn(usize, &Entity<Editor>) -> bool| -> Vec<Entity<Editor>> {
            ws.panes[pane]
                .tabs
                .iter()
                .enumerate()
                .filter(|(i, t)| !keep(*i, t) && !ws.pinned.contains(&t.entity_id()))
                .map(|(_, t)| t.clone())
                .collect()
        };
        let to_close = match action {
            TabAction::Close => vec![editor.clone()],
            TabAction::CloseOthers => others(self, &|_, t| t == editor),
            TabAction::CloseAll => others(self, &|_, _| false),
            TabAction::CloseToTheRight => others(self, &|i, _| i <= ix),
            TabAction::TogglePin => {
                let id = editor.entity_id();
                if !self.pinned.remove(&id) {
                    self.pinned.insert(id);
                }
                self.keep_pinned_first(pane);
                self.schedule_session_save(cx);
                cx.notify();
                return;
            }
            TabAction::SelectInTree => {
                self.activate_editor(editor, window, cx);
                self.select_opened_file(&SelectOpenedFile, window, cx);
                return;
            }
            TabAction::SplitRight | TabAction::SplitDown => {
                self.activate_editor(editor, window, cx);
                let axis = if action == TabAction::SplitRight { PaneAxis::Row } else { PaneAxis::Column };
                self.split(axis, window, cx);
                return;
            }
        };
        for editor in to_close {
            self.close_editor(&editor, window, cx);
        }
    }

    fn open_tab_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let pinned = self.pinned.contains(&editor.entity_id());
        let path = editor.read(cx).doc.path.clone();
        let mut entries = Vec::new();
        let mut actions = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        let tab = |action: TabAction| Some(MenuAction::Tab(action, editor.clone()));
        add(MenuEntry::item("Close", Icon::X).shortcut(binding_label(&CloseTab, window)), tab(TabAction::Close));
        add(MenuEntry::text("Close Others"), tab(TabAction::CloseOthers));
        add(MenuEntry::text("Close All"), tab(TabAction::CloseAll));
        add(MenuEntry::text("Close Tabs to the Right"), tab(TabAction::CloseToTheRight));
        add(MenuEntry::Separator, None);
        add(MenuEntry::item(if pinned { "Unpin Tab" } else { "Pin Tab" }, Icon::Pin), tab(TabAction::TogglePin));
        add(MenuEntry::Separator, None);
        add(
            MenuEntry::item("Split and Move Right", Icon::SplitRight).shortcut(binding_label(&SplitRight, window)),
            tab(TabAction::SplitRight),
        );
        add(
            MenuEntry::item("Split and Move Down", Icon::SplitDown).shortcut(binding_label(&SplitDown, window)),
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

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
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
        self.save_state();
        cx.notify();
    }

    /// Shows or hides the sidebar, keeping the view it has.
    fn set_sidebar_visible(&mut self, visible: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = visible;
        // A hidden Commit view's message box must not keep focus (see `toggle_sidebar`).
        if !visible && self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        if visible && self.sidebar_mode == SidebarMode::Commit {
            self.refresh_git(cx);
        }
        self.save_state();
        cx.notify();
    }

    /// The sidebar switch's Files segment.
    fn show_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_mode == SidebarMode::Files {
            return;
        }
        self.sidebar_mode = SidebarMode::Files;
        if self.git_panel.focus_handle(cx).contains_focused(window, cx) {
            self.focus_active(window, cx);
        }
        self.save_state();
        cx.notify();
    }

    fn show_commit(&mut self, _: &ShowCommit, window: &mut Window, cx: &mut Context<Self>) {
        self.show_sidebar = true;
        self.sidebar_mode = SidebarMode::Commit;
        self.refresh_git(cx);
        self.git_panel.read(cx).focus_message(window, cx);
        self.save_state();
        cx.notify();
    }

    fn toggle_commit(&mut self, _: &ToggleCommit, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_sidebar && self.sidebar_mode == SidebarMode::Commit {
            self.show_sidebar = false;
            self.focus_active(window, cx);
            self.save_state();
            cx.notify();
        } else {
            self.show_commit(&ShowCommit, window, cx);
        }
    }

    // ---- Git -------------------------------------------------------------------------

    fn detect_repo(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let project = self.root.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move {
                    if !git::is_available() {
                        return None;
                    }
                    let repo = git::discover(&dir)?;
                    let user = git::user_name(&repo);
                    let remote = git::remote_url(&repo);
                    Some((repo, user, remote))
                })
                .await;
            this.update(cx, |ws, cx| {
                let (repo, user, remote) = found.map_or((None, None, None), |(r, u, o)| (Some(r), u, o));
                ws.repo = repo.clone();
                ws.git_user = user;
                ws.git_remote = remote;
                ws.last_commit = None;
                ws.git_status = Arc::new(RepoStatus::default());
                ws.git_paths = Arc::new(HashMap::new());
                // A repository above the project folder: watch its .git too.
                if let (Some(repo), Some(root), Some(watcher)) = (&repo, &ws.root, ws.watcher.as_mut())
                    && !repo.starts_with(root)
                    && let Err(err) = watcher.add(&repo.join(".git"))
                {
                    eprintln!("couldn't watch {}: {err}", repo.display());
                }
                ws.git_log.update(cx, |l, cx| l.set_repo(repo.clone(), cx));
                ws.git_panel.update(cx, |p, cx| {
                    p.set_repo(repo, project, cx);
                    p.set_status(Arc::new(RepoStatus::default()), cx);
                });
                ws.refresh_git(cx);
                for editor in ws.tabs.clone() {
                    ws.load_diff_base(&editor, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-reads `git status` in the background.
    fn refresh_git(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        if self.status_task.is_some() {
            // Changes made while git was reading must not be missed.
            self.git_refresh_again = true;
            return;
        }
        self.status_task = Some(cx.spawn(async move |this, cx| {
            let repo2 = repo.clone();
            let (result, last_commit) = cx
                .background_executor()
                .spawn(async move { (git::status(&repo2), git::last_commit(&repo2)) })
                .await;
            this.update(cx, |ws, cx| {
                ws.status_task = None;
                if std::mem::take(&mut ws.git_refresh_again) {
                    ws.refresh_git(cx);
                }
                if ws.last_commit != last_commit {
                    ws.last_commit = last_commit;
                    cx.notify();
                }
                let Ok(status) = result else { return };
                if *ws.git_status == status {
                    return;
                }
                let moved = (&ws.git_status.head_oid, &ws.git_status.branch, ws.git_status.ahead, ws.git_status.behind)
                    != (&status.head_oid, &status.branch, status.ahead, status.behind);
                if moved && ws.show_git_log {
                    ws.git_log.update(cx, |l, cx| l.reload(cx));
                }
                ws.git_paths = Arc::new(status.path_map(&repo));
                ws.git_status = Arc::new(status);
                let status = ws.git_status.clone();
                ws.git_panel.update(cx, |p, cx| p.set_status(status, cx));
                // HEAD or the index may have moved (commits, staging, checkouts).
                for editor in ws.tabs.clone() {
                    ws.load_diff_base(&editor, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Loads the file's HEAD text so the editor can show changed lines.
    fn load_diff_base(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let Some(path) = editor.read(cx).doc.path.clone() else { return };
        // Images have no lines to mark.
        if editor.read(cx).doc.image.is_some() {
            return;
        }
        let Ok(rel) = path.strip_prefix(&repo) else {
            editor.update(cx, |e, cx| e.set_diff_base(None, None, None, cx));
            return;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let git_file = GitFile { root: repo.clone(), rel: rel.clone(), user_name: self.git_user.clone() };
        let editor = editor.downgrade();
        cx.spawn(async move |_, cx| {
            let (base, index) = cx
                .background_executor()
                .spawn(async move { (git::head_text(&repo, &rel), git::index_text(&repo, &rel)) })
                .await;
            editor.update(cx, |e, cx| e.set_diff_base(Some(git_file), base, index, cx)).ok();
        })
        .detach();
    }

    /// Picks up on-disk changes (checkout, pull, rollback) in unmodified tabs.
    fn reload_changed_files(&mut self, cx: &mut Context<Self>) {
        for editor in self.tabs.clone() {
            editor.update(cx, |e, cx| e.reload_if_changed(cx));
        }
        self.rebuild_tree();
    }

    fn run_git(&mut self, op: GitOp, window: &mut Window, cx: &mut Context<Self>) {
        // Push asks first, like JetBrains: the commits to push, then Push or Cancel.
        if matches!(op, GitOp::Push) {
            self.show_push(window, cx);
            return;
        }
        // Marking a file resolved while it still has conflict markers is
        // usually a mistake, so ask.
        if let (GitOp::Stage(rels), Some(repo)) = (&op, &self.repo) {
            let unresolved: Vec<&String> = rels
                .iter()
                .filter(|rel| self.git_status.conflicted().any(|f| &f.rel == *rel))
                .filter(|rel| std::fs::read_to_string(repo.join(rel)).is_ok_and(|t| !crate::conflict::parse(&t).is_empty()))
                .collect();
            if !unresolved.is_empty() {
                let message = match unresolved.as_slice() {
                    [one] => format!("{one} still has conflict markers"),
                    many => format!("{} files still have conflict markers", many.len()),
                };
                let rx = window.prompt(PromptLevel::Warning, &message, Some("Mark as resolved anyway?"), &["Mark as Resolved", "Cancel"], cx);
                cx.spawn_in(window, async move |this, cx| {
                    if rx.await == Ok(0) {
                        this.update(cx, |ws, cx| ws.execute_git(op, cx)).ok();
                    }
                })
                .detach();
                return;
            }
        }
        if let GitOp::Discard(files) = &op {
            // Rolling back can't be undone, so ask first.
            let message = match files.as_slice() {
                [one] => format!("Roll back changes to {}?", one.rel),
                many => format!("Roll back changes to {} files?", many.len()),
            };
            let untracked = files.iter().any(|f| f.unstaged == Some(Change::Untracked));
            let detail = if untracked {
                "Untracked files will be deleted. This can't be undone."
            } else {
                "Local changes will be lost. This can't be undone."
            };
            let rx = window.prompt(PromptLevel::Warning, &message, Some(detail), &["Roll Back", "Cancel"], cx);
            cx.spawn_in(window, async move |this, cx| {
                if rx.await == Ok(0) {
                    this.update(cx, |ws, cx| ws.execute_git(op, cx)).ok();
                }
            })
            .detach();
            return;
        }
        self.execute_git(op, cx);
    }

    fn execute_git(&mut self, op: GitOp, cx: &mut Context<Self>) {
        let dir = match (&op, &self.repo, &self.root) {
            (GitOp::Init, _, Some(root)) => root.clone(),
            (_, Some(repo), _) => repo.clone(),
            _ => return,
        };
        if self.git_busy.is_some() {
            self.toast(ToastKind::Error, "Another Git operation is still running", None, cx);
            return;
        }
        let label = op.busy_label();
        self.git_busy = Some(label);
        self.git_panel.update(cx, |p, cx| p.set_busy(Some(label.into()), cx));
        let status = self.git_status.clone();
        let op2 = op.clone();
        self.git_op_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { perform_git(&dir, op2, &status) }).await;
            this.update(cx, |ws, cx| {
                ws.git_busy = None;
                ws.git_panel.update(cx, |p, cx| p.set_busy(None, cx));
                match result {
                    Ok(message) => {
                        if let Some(message) = message {
                            // A merge or rebase that stopped on conflicts isn't done yet.
                            let kind = if message.contains("conflicts") { ToastKind::Info } else { ToastKind::Success };
                            ws.toast(kind, message, None, cx);
                        }
                        match &op {
                            GitOp::Commit { .. } => ws.git_panel.update(cx, |p, cx| p.committed(cx)),
                            GitOp::Init => {
                                if let Some(root) = ws.root.clone() {
                                    ws.detect_repo(root, cx);
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(err) => {
                        let title = match &op {
                            GitOp::Push | GitOp::PushNow { .. } | GitOp::Commit { push: true, .. } => "Push failed",
                            GitOp::Pull => "Update failed",
                            GitOp::Fetch => "Fetch failed",
                            GitOp::Commit { .. } => "Commit failed",
                            GitOp::Checkout(_) | GitOp::CreateBranch(_) => "Checkout failed",
                            _ => "Git operation failed",
                        };
                        ws.toast(ToastKind::Error, title, Some(err), cx);
                    }
                }
                if matches!(
                    op,
                    GitOp::Discard(_) | GitOp::Pull | GitOp::Checkout(_) | GitOp::Merge(_) | GitOp::Rebase(_) | GitOp::Abort(_) | GitOp::Continue(_)
                ) {
                    ws.reload_changed_files(cx);
                }
                if ws.show_git_log
                    && matches!(
                        op,
                        GitOp::Commit { .. }
                            | GitOp::Fetch
                            | GitOp::Pull
                            | GitOp::Push
                            | GitOp::PushNow { .. }
                            | GitOp::Checkout(_)
                            | GitOp::CreateBranch(_)
                            | GitOp::Merge(_)
                            | GitOp::Rebase(_)
                            | GitOp::Abort(_)
                            | GitOp::Continue(_)
                    )
                {
                    ws.git_log.update(cx, |l, cx| l.reload(cx));
                }
                // Staging part of a file may leave its status as it was.
                if matches!(op, GitOp::StageLines { .. } | GitOp::UnstageLines { .. }) {
                    for editor in ws.tabs.clone() {
                        ws.load_diff_base(&editor, cx);
                    }
                }
                if let Some(Modal::Diff(view)) = &ws.modal {
                    view.update(cx, |v, cx| v.reload(cx));
                }
                ws.status_task = None;
                ws.refresh_git(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn toast(&mut self, kind: ToastKind, message: impl Into<SharedString>, detail: Option<String>, cx: &mut Context<Self>) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, kind, message: message.into(), detail: detail.map(Into::into) });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        let seconds = if kind == ToastKind::Error { 9 } else { 4 };
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(seconds)).await;
            this.update(cx, |ws, cx| {
                ws.toasts.retain(|t| t.id != id);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn git_push(&mut self, _: &GitPush, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Push, window, cx);
    }

    fn git_pull(&mut self, _: &GitPull, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Pull, window, cx);
    }

    fn git_fetch(&mut self, _: &GitFetch, window: &mut Window, cx: &mut Context<Self>) {
        self.run_git(GitOp::Fetch, window, cx);
    }

    fn git_branches(&mut self, _: &GitBranches, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        let picker = cx.new(|cx| BranchPicker::new(repo, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&picker, window, |ws, _, event: &BranchPickerEvent, window, cx| match event {
            BranchPickerEvent::Run(op) => {
                ws.close_modal(window, cx);
                ws.run_git(op.clone(), window, cx);
            }
            BranchPickerEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Branches(picker));
        cx.notify();
    }

    fn git_clone(&mut self, _: &GitClone, window: &mut Window, cx: &mut Context<Self>) {
        if !git::is_available() {
            self.toast(ToastKind::Error, "Git isn't installed", Some("Install Git and make sure it's on your PATH.".into()), cx);
            return;
        }
        self.close_modal(window, cx);
        let parent = clone_dialog::default_parent(self.root.as_deref());
        let dialog = cx.new(|cx| CloneDialog::new(parent, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, |ws, _, event: &CloneDialogEvent, window, cx| match event {
            CloneDialogEvent::Cloned(path) => {
                ws.close_modal(window, cx);
                ws.toast(ToastKind::Success, format!("Cloned {}", folder_name(path)), Some(clone_dialog::display_path(path)), cx);
                ws.open_project(path.clone(), OpenIn::Auto, window, cx);
            }
            CloneDialogEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Clone(dialog));
        cx.notify();
    }

    fn git_init(&mut self, _: &GitInit, window: &mut Window, cx: &mut Context<Self>) {
        match (&self.root, &self.repo) {
            (None, _) => self.toast(ToastKind::Error, "Open a folder first", None, cx),
            (Some(_), Some(_)) => self.toast(ToastKind::Error, "This folder is already in a Git repository", None, cx),
            (Some(_), None) => self.run_git(GitOp::Init, window, cx),
        }
    }

    fn open_remote(&mut self, _: &OpenRemote, _: &mut Window, cx: &mut Context<Self>) {
        match self.git_remote.as_deref().and_then(git::web_url) {
            Some(url) => cx.open_url(&url),
            None => self.toast(ToastKind::Error, "No browsable remote", Some("This repository has no HTTPS or SSH remote.".into()), cx),
        }
    }

    fn remove_recent_project(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.update_recent_projects(|list| list.retain(|p| p != path));
        cx.notify();
    }

    /// Opens a recent project here, or in a new window when `new_window`
    /// (⌘/Ctrl held).
    fn open_recent_project(&mut self, path: PathBuf, new_window: bool, window: &mut Window, cx: &mut Context<Self>) {
        let target = if new_window { OpenIn::NewWindow } else { OpenIn::ThisWindow };
        self.open_project(path, target, window, cx);
    }

    fn recent_projects(&mut self, _: &RecentProjects, window: &mut Window, cx: &mut Context<Self>) {
        self.recent_projects = SessionState::load().recent_projects;
        self.open_picker(PickerMode::Projects, window, cx);
    }

    fn open_diff(&mut self, file: FileStatus, staged: bool, window: &mut Window, cx: &mut Context<Self>) {
        let buffer = self
            .tabs
            .iter()
            .find(|t| t.read(cx).doc.path.as_deref() == Some(file.path.as_path()))
            .map(|t| t.read(cx).text());
        self.show_diff(DiffSource::Working { file, staged, buffer }, window, cx);
    }

    fn show_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            self.toast(ToastKind::Error, "Not a Git repository", None, cx);
            return;
        };
        self.close_modal(window, cx);
        self.diff_return = None;
        let dialog = cx.new(|cx| PushDialog::new(repo, window, cx));
        self.open_push_modal(dialog, window, cx);
    }

    fn open_push_modal(&mut self, dialog: Entity<PushDialog>, window: &mut Window, cx: &mut Context<Self>) {
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, |ws, dialog, event: &PushDialogEvent, window, cx| match event {
            PushDialogEvent::Push { force } => {
                ws.close_modal(window, cx);
                ws.run_git(GitOp::PushNow { force: *force }, window, cx);
            }
            PushDialogEvent::OpenDiff(source) => {
                ws.diff_return = Some(dialog.clone());
                ws.show_diff(source.clone(), window, cx);
            }
            PushDialogEvent::Dismissed => ws.close_modal(window, cx),
        }));
        dialog.read(cx).focus(window);
        self.modal = Some(Modal::Push(dialog));
        cx.notify();
    }

    fn show_diff(&mut self, source: DiffSource, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        self.close_modal(window, cx);
        let side_by_side = SessionState::load().diff_side_by_side.unwrap_or(true);
        let view = cx.new(|cx| DiffView::new(repo, source, side_by_side, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&view, window, |ws, _, event: &DiffViewEvent, window, cx| match event {
            DiffViewEvent::Close => {
                ws.close_modal(window, cx);
                // Back to the Push dialog the diff came from.
                if let Some(dialog) = ws.diff_return.take() {
                    ws.open_push_modal(dialog, window, cx);
                }
            }
            DiffViewEvent::OpenFile(path) => {
                ws.diff_return = None;
                ws.close_modal(window, cx);
                ws.open_path(path.clone(), window, cx);
            }
            DiffViewEvent::Run(op) => {
                ws.diff_return = None;
                // Staging a change keeps the diff open (it reloads afterwards).
                if !matches!(op, GitOp::StageLines { .. } | GitOp::UnstageLines { .. }) {
                    ws.close_modal(window, cx);
                }
                ws.run_git(op.clone(), window, cx);
            }
            DiffViewEvent::Layout { side_by_side } => {
                // App-wide, like the font zoom.
                let mut state = SessionState::load();
                state.diff_side_by_side = Some(*side_by_side);
                state.save().ok();
            }
        }));
        self.modal = Some(Modal::Diff(view));
        cx.notify();
    }

    fn toggle_problems(&mut self, _: &ToggleProblems, window: &mut Window, cx: &mut Context<Self>) {
        self.show_problems = !self.show_problems;
        if self.show_problems {
            self.show_git_log = false;
            self.show_debug = false;
        }
        if self.show_problems && self.show_terminal {
            self.show_terminal = false;
            self.focus_active(window, cx);
        }
        self.save_state();
        cx.notify();
    }

    // ---- Terminal -------------------------------------------------------------------

    /// ⌥F12: opens the terminal, focuses it, or (when focused) hides it.
    fn toggle_terminal(&mut self, _: &ToggleTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let focused = self.all_terminals().any(|t| t.focus_handle(cx).contains_focused(window, cx));
        if self.show_terminal && focused {
            self.show_terminal = false;
            self.focus_active(window, cx);
        } else {
            self.show_terminal = true;
            self.show_problems = false;
            self.show_git_log = false;
            self.show_debug = false;
            if self.terminal_groups.is_empty() {
                self.new_terminal(&NewTerminal, window, cx);
            } else {
                self.focus_terminal(window, cx);
            }
        }
        self.save_state();
        cx.notify();
    }

    /// Git log in the bottom panel (⌘9 / Alt+9); again hides it.
    fn toggle_git_log(&mut self, _: &ToggleGitLog, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_git_log {
            self.show_git_log = false;
            self.focus_active(window, cx);
        } else {
            self.git_log.update(cx, |l, cx| l.show_path(None, cx));
            self.open_git_log(window, cx);
        }
        self.save_state();
        cx.notify();
    }

    /// The active file's commits (Git → Show History).
    fn show_file_history(&mut self, _: &ShowFileHistory, window: &mut Window, cx: &mut Context<Self>) {
        let rel = self.active_editor().and_then(|e| {
            let path = e.read(cx).doc.path.clone()?;
            Some(path.strip_prefix(self.repo.as_ref()?).ok()?.to_string_lossy().replace('\\', "/"))
        });
        let Some(rel) = rel else {
            self.toast(ToastKind::Info, "This file isn't in a Git repository", None, cx);
            return;
        };
        self.git_log.update(cx, |l, cx| l.show_path(Some(rel), cx));
        self.open_git_log(window, cx);
        self.save_state();
        cx.notify();
    }

    fn open_git_log(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.repo.is_none() {
            self.toast(ToastKind::Info, "Not a Git repository", None, cx);
            return;
        }
        self.show_git_log = true;
        self.show_problems = false;
        self.show_terminal = false;
        self.show_debug = false;
        self.git_log.update(cx, |l, cx| l.reload(cx));
        self.git_log.read(cx).focus_filter(window, cx);
    }

    fn render_git_log_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        island(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(self.git_log.clone())
            .child(self.resize_handle(Edge::Problems, cx))
    }

    /// After `open_window`'s setup: start the shell for a terminal panel that
    /// was open last time, now that the project (its folder) is known.
    fn window_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.show_terminal && self.terminal_groups.is_empty() {
            let focused = self.active_editor().cloned();
            self.new_terminal(&NewTerminal, window, cx);
            // Keep the editor focused, as it was before quitting.
            if let Some(editor) = focused {
                window.focus(&editor.focus_handle(cx));
            }
        }
    }

    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.active_terminal() {
            window.focus(&terminal.focus_handle(cx));
        }
    }

    /// The current tab of the current group.
    fn active_terminal(&self) -> Option<&Entity<TerminalView>> {
        let group = self.terminal_groups.get(self.active_group)?;
        group.tabs.get(group.active)
    }

    fn all_terminals(&self) -> impl Iterator<Item = &Entity<TerminalView>> {
        self.terminal_groups.iter().flat_map(|g| g.tabs.iter())
    }

    /// (group, tab) of a terminal.
    fn terminal_position(&self, terminal: &Entity<TerminalView>) -> Option<(usize, usize)> {
        self.terminal_groups.iter().enumerate().find_map(|(g, group)| group.tabs.iter().position(|t| t == terminal).map(|ix| (g, ix)))
    }

    /// Where a new shell starts: the project folder (or the current file's).
    fn terminal_cwd(&self, cx: &App) -> PathBuf {
        self.root
            .clone()
            .or_else(|| self.active_editor().and_then(|e| e.read(cx).doc.path.as_deref()?.parent().map(Path::to_path_buf)))
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Starts a shell in a new tab of the current group.
    fn new_terminal(&mut self, _: &NewTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.terminal_cwd(cx);
        self.open_terminal(cwd, None, TerminalPlace::Tab { group: self.active_group, ix: None }, true, window, cx);
    }

    /// Makes a tab current, and its group the one with the keyboard.
    fn activate_terminal(&mut self, group: usize, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(g) = self.terminal_groups.get_mut(group) else { return };
        g.active = ix.min(g.tabs.len().saturating_sub(1));
        self.active_group = group;
        self.focus_terminal(window, cx);
        cx.notify();
    }

    /// A Run tab finished: what its output reports goes to Problems (a run
    /// without problems clears the last one's).
    fn collect_run_problems(&mut self, terminal: &Entity<TerminalView>, cx: &mut Context<Self>) {
        let Some((target, _)) = terminal.read(cx).run().cloned() else { return };
        let output = terminal.read(cx).output_text();
        let task = cx.spawn(async move |ws, cx| {
            let cwd = target.cwd.clone();
            let problems = cx.background_executor().spawn(async move { run::output_problems(&output, &cwd) }).await;
            ws.update(cx, |ws, cx| {
                ws.run_problems = (!problems.is_empty()).then(|| (target.name, problems));
                cx.notify();
            })
            .ok();
        });
        task.detach();
        cx.notify();
    }

    /// Open files may have a language now, or one with a grammar.
    /// The folder trust decides about: the project, else the active file's.
    fn trust_folder(&self, cx: &App) -> Option<PathBuf> {
        self.root.clone().or_else(|| self.active_editor()?.read(cx).doc.path.as_deref()?.parent().map(Path::to_path_buf))
    }

    fn restricted(&self, cx: &App) -> bool {
        self.trust_folder(cx).is_some_and(|f| !crate::trust::is_trusted(&f))
    }

    fn project_trust(&mut self, _: &ProjectTrust, window: &mut Window, cx: &mut Context<Self>) {
        self.open_trust_dialog(window, cx);
    }

    fn open_trust_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(folder) = self.trust_folder(cx) else {
            self.toast(ToastKind::Info, "Open a folder to trust it", None, cx);
            return;
        };
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| crate::trust_dialog::TrustDialog::new(folder, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, |ws, _, event: &crate::trust_dialog::TrustDialogEvent, window, cx| {
            use crate::trust_dialog::TrustDialogEvent;
            match event {
                TrustDialogEvent::Trust(folder) => {
                    ws.close_modal(window, cx);
                    crate::trust::trust(folder, cx);
                }
                TrustDialogEvent::Untrust(folder) => {
                    ws.close_modal(window, cx);
                    crate::trust::untrust(folder, cx);
                }
                TrustDialogEvent::Dismissed => ws.close_modal(window, cx),
            }
        }));
        self.modal = Some(Modal::Trust(dialog));
        cx.notify();
    }

    /// Starts what trust allows, or stops it.
    fn trust_changed(&mut self, cx: &mut Context<Self>) {
        if self.restricted(cx) {
            self.lsp.update(cx, |s, _| s.shutdown_all());
        }
        for editor in self.tabs.clone() {
            editor.update(cx, |e, cx| e.trust_changed(cx));
            // Starts its server when trusted now.
            self.lsp.update(cx, |s, cx| s.register(&editor, cx));
        }
        self.refresh_git(cx);
        cx.notify();
    }

    fn extensions_changed(&mut self, cx: &mut Context<Self>) {
        for editor in self.tabs.clone() {
            if editor.update(cx, |e, cx| e.refresh_language(cx)) {
                self.lsp.update(cx, |s, cx| s.register(&editor, cx));
            }
        }
        if let Some(message) = cx.global::<extensions::ExtensionsChanged>().message.clone() {
            self.toast(ToastKind::Info, message, None, cx);
        }
        cx.notify();
    }

    /// The last Run's problems, without the ones the active file shows
    /// already (its language server reports the same compiler errors).
    fn run_problems_shown(&self, cx: &App) -> Vec<&run::OutputProblem> {
        let Some((_, problems)) = &self.run_problems else { return Vec::new() };
        let doc = self.active_editor().map(|e| &e.read(cx).doc);
        let known = |p: &run::OutputProblem| {
            doc.is_some_and(|doc| {
                doc.path.as_deref() == Some(p.path.as_path())
                    && doc.diagnostics.iter().any(|d| d.severity == p.severity && doc.row_of(d.range.start) + 1 == p.line as usize)
            })
        };
        problems.iter().filter(|p| !known(p)).collect()
    }

    /// Adds a terminal where `place` says. A Run tab stays open when its
    /// program ends, showing how it went.
    fn open_terminal(
        &mut self,
        cwd: PathBuf,
        run: Option<RunTarget>,
        place: TerminalPlace,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match TerminalView::create(cwd, run, cx) {
            Ok(terminal) => {
                self._subscriptions.push(cx.subscribe_in(&terminal, window, |ws, terminal, event: &TerminalEvent, window, cx| match event {
                    TerminalEvent::TitleChanged => cx.notify(),
                    TerminalEvent::Exited if terminal.read(cx).run().is_some() => ws.collect_run_problems(terminal, cx),
                    TerminalEvent::Exited => ws.remove_terminal(terminal, window, cx),
                    TerminalEvent::OpenFile { path, line, column } => ws.open_at(path.clone(), *line, *column, window, cx),
                }));
                let (group, ix) = match place {
                    TerminalPlace::Tab { group, ix } if group < self.terminal_groups.len() => {
                        let tabs = &mut self.terminal_groups[group].tabs;
                        let ix = ix.unwrap_or(tabs.len()).min(tabs.len());
                        tabs.insert(ix, terminal);
                        (group, ix)
                    }
                    TerminalPlace::GroupAfter(group) if group < self.terminal_groups.len() => {
                        self.terminal_groups.insert(group + 1, TerminalGroup::new(terminal));
                        (group + 1, 0)
                    }
                    // No group to put it in yet.
                    _ => {
                        self.terminal_groups.push(TerminalGroup::new(terminal));
                        (self.terminal_groups.len() - 1, 0)
                    }
                };
                self.terminal_groups[group].active = ix;
                self.active_group = group;
                self.show_terminal = true;
                self.show_problems = false;
                self.show_git_log = false;
                self.show_debug = false;
                if focus {
                    self.focus_terminal(window, cx);
                }
                self.save_state();
            }
            Err(err) => {
                // A Run tab being replaced may have left its group empty.
                self.terminal_groups.retain(|g| !g.tabs.is_empty());
                self.active_group = self.active_group.min(self.terminal_groups.len().saturating_sub(1));
                // Don't retry on every frame.
                self.show_terminal = !self.terminal_groups.is_empty();
                self.toast(ToastKind::Error, "Couldn't start the terminal", Some(err.to_string()), cx);
            }
        }
        cx.notify();
    }

    // ---- Running ----------------------------------------------------------------------

    /// Runs a configuration in a Run tab, replacing the previous run of it.
    /// The editor keeps focus, like JetBrains' Run window.
    fn run_target(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        // Unsaved edits would make the run disagree with what's on screen.
        for editor in self.tabs.clone() {
            if editor.read(cx).doc.is_dirty() && editor.read(cx).doc.path.is_some() {
                editor.update(cx, |e, cx| e.save(cx)).ok();
            }
        }
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
    fn rerun(&mut self, _: &Rerun, window: &mut Window, cx: &mut Context<Self>) {
        match self.last_run.clone() {
            Some(target) => self.run_target(target, window, cx),
            None => self.run_configuration(&RunConfiguration, window, cx),
        }
    }

    /// Stop (⌘F2 / Ctrl+F2): interrupts the newest running program.
    fn stop_run(&mut self, _: &StopRun, _: &mut Window, cx: &mut Context<Self>) {
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

    // ---- Debugging --------------------------------------------------------------------

    /// A gutter ▶ (Run or Debug, as a menu when both work), or Run / Debug at caret.
    fn on_run_event(&mut self, event: &EditorRunEvent, window: &mut Window, cx: &mut Context<Self>) {
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
    fn debug_target(&mut self, target: RunTarget, window: &mut Window, cx: &mut Context<Self>) {
        if target.debug.is_none() {
            self.toast(ToastKind::Info, "This can't be debugged", Some("Rust, Go, Python and JavaScript tests and programs can be".into()), cx);
            return;
        }
        // What's debugged is what's on screen.
        for editor in self.tabs.clone() {
            if editor.read(cx).doc.is_dirty() && editor.read(cx).doc.path.is_some() {
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

    fn on_debug_event(&mut self, event: &DebugEvent, window: &mut Window, cx: &mut Context<Self>) {
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

    fn clear_debug_location(&mut self, cx: &mut Context<Self>) {
        if let Some((path, _)) = self.debug_location.take()
            && let Some(editor) = self.tab_for(&path, cx)
        {
            editor.update(cx, |e, cx| e.set_debug_line(None, cx));
        }
    }

    /// Every breakpoint: open files' (which move with edits) and the rest.
    fn all_breakpoints(&self, cx: &App) -> HashMap<PathBuf, Vec<Breakpoint>> {
        let mut all = self.breakpoints.clone();
        for editor in &self.tabs {
            let e = editor.read(cx);
            if let Some(path) = e.doc.path.clone() {
                let breakpoints = e.breakpoints();
                if breakpoints.is_empty() {
                    all.remove(&path);
                } else {
                    all.insert(path, breakpoints);
                }
            }
        }
        all
    }

    fn breakpoints_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let (path, breakpoints) = {
            let e = editor.read(cx);
            (e.doc.path.clone(), e.breakpoints())
        };
        if let (Some(path), Some((session, _))) = (path, &self.debug) {
            session.update(cx, |s, _| s.set_breakpoints(path, breakpoints));
        }
        self.schedule_session_save(cx);
    }

    /// Right-click on a line number: add or remove its breakpoint, or give it
    /// a condition or a log message.
    fn open_breakpoint_menu(&mut self, editor: Entity<Editor>, row: usize, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let existing = editor.read(cx).breakpoint(row).is_some();
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
    fn add_conditional_breakpoint(&mut self, _: &AddConditionalBreakpoint, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_breakpoint_at_caret(BreakpointEdit::Condition, window, cx);
    }

    /// Add Logpoint…: on the caret's line.
    fn add_logpoint(&mut self, _: &AddLogpoint, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_breakpoint_at_caret(BreakpointEdit::LogMessage, window, cx);
    }

    fn edit_breakpoint_at_caret(&mut self, edit: BreakpointEdit, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        if !editor.read(cx).can_break() {
            self.toast(ToastKind::Info, "Breakpoints work in Rust, Go, Python, JavaScript and TypeScript files", None, cx);
            return;
        }
        let row = editor.read(cx).caret_row();
        self.edit_breakpoint(edit, editor, row, window, cx);
    }

    /// Asks for a breakpoint's condition or log message; confirming adds the
    /// breakpoint if the line has none, and an empty text clears the option.
    fn edit_breakpoint(&mut self, edit: BreakpointEdit, editor: Entity<Editor>, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let current = editor.read(cx).breakpoint(row).unwrap_or(Breakpoint::at(row));
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
                let mut breakpoint = editor.read(cx).breakpoint(row).unwrap_or(Breakpoint::at(row));
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

    fn debug_session(&self) -> Option<Entity<DebugSession>> {
        self.debug.as_ref().map(|(s, _)| s.clone())
    }

    /// Resume (⌥⌘R / F9; F5 in VS Code and Zed): continue, or with nothing
    /// to continue, debug the last configuration again (else at the caret).
    fn resume(&mut self, _: &Resume, window: &mut Window, cx: &mut Context<Self>) {
        match self.debug_session() {
            Some(session) if session.read(cx).is_paused() => session.update(cx, |s, cx| s.resume(cx)),
            Some(session) if session.read(cx).is_active() => {}
            _ => match self.last_debug.clone() {
                Some(target) => self.debug_target(target, window, cx),
                None => window.dispatch_action(Box::new(editor::DebugAtCaret), cx),
            },
        }
    }

    fn pause_program(&mut self, _: &PauseProgram, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.pause(cx));
        }
    }

    fn step_over(&mut self, _: &StepOver, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_over(cx));
        }
    }

    fn step_into(&mut self, _: &StepInto, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_into(cx));
        }
    }

    fn step_out(&mut self, _: &StepOut, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.debug_session() {
            session.update(cx, |s, cx| s.step_out(cx));
        }
    }

    fn toggle_debug_panel(&mut self, _: &ToggleDebugPanel, window: &mut Window, cx: &mut Context<Self>) {
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

    fn render_debug_panel(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (session, _) = self.debug.as_ref()?;
        let theme = Theme::get(cx).clone();
        Some(
            island(&theme)
                .h(self.problems_height)
                .flex_shrink_0()
                .child(session.clone())
                .child(self.resize_handle(Edge::Problems, cx)),
        )
    }

    /// Run… (⌃⌥R / Alt+Shift+F10): the project's configurations and the current file.
    fn run_configuration(&mut self, _: &RunConfiguration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(targets) = self.configurations(self.last_run.clone(), cx) else { return };
        self.close_modal(window, cx);
        self.open_picker(PickerMode::Run, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modal {
            picker.update(cx, |p, cx| p.set_run_targets(targets, cx));
        }
    }

    /// Debug… (⌃⌥D / Alt+Shift+F9): the configurations that can be debugged.
    fn debug_configuration(&mut self, _: &DebugConfiguration, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut targets) = self.configurations(self.last_debug.clone(), cx) else { return };
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
    fn attach_to_process(&mut self, _: &AttachToProcess, window: &mut Window, cx: &mut Context<Self>) {
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
    fn configurations(&mut self, last: Option<RunTarget>, cx: &mut Context<Self>) -> Option<Vec<RunTarget>> {
        let root = self
            .root
            .clone()
            .or_else(|| self.active_editor().and_then(|e| e.read(cx).doc.path.as_deref()?.parent().map(Path::to_path_buf)));
        let Some(root) = root else {
            self.toast(ToastKind::Info, "Open a folder or a file to run something", None, cx);
            return None;
        };
        let current = self.active_editor().and_then(|e| {
            let doc = &e.read(cx).doc;
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
        targets.extend(run::project_targets(&root, current.as_ref().map(|(p, l)| (p.as_path(), *l))));
        // Then the current file's tests and mains, by name.
        if let Some(editor) = self.active_editor() {
            let file = editor.update(cx, |e, _| e.run_targets());
            targets.extend(file.iter().filter(|t| !targets.iter().any(|o| o.command == t.command)).cloned().collect::<Vec<_>>());
        }
        // The last one comes first, so Enter repeats it.
        if let Some(last) = last {
            targets.retain(|t| t.command != last.command || t.cwd != last.cwd);
            targets.insert(0, last);
        }
        if targets.is_empty() {
            self.toast(ToastKind::Info, "Nothing to run here", Some("Add run_configurations to settings.json".into()), cx);
            return None;
        }
        Some(targets)
    }

    fn close_terminal(&mut self, _: &CloseTerminal, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(terminal) = self.active_terminal().cloned() {
            self.remove_terminal(&terminal, window, cx);
        }
    }

    /// Opens a new shell in a group of its own beside the current group.
    fn split_terminal(&mut self, _: &SplitTerminal, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.terminal_cwd(cx);
        let place = if self.terminal_groups.is_empty() {
            TerminalPlace::Tab { group: 0, ix: None }
        } else {
            TerminalPlace::GroupAfter(self.active_group)
        };
        self.open_terminal(cwd, None, place, true, window, cx);
    }

    fn remove_terminal(&mut self, terminal: &Entity<TerminalView>, window: &mut Window, cx: &mut Context<Self>) {
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
        cx.notify();
    }

    /// The bottom panel's terminals: each group is an island with its own
    /// tab strip, side by side like split panes.
    fn render_terminal_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // A panel reopened at launch gets its shell in `window_ready`; until
        // then it's one empty group.
        let count = self.terminal_groups.len().max(1);
        div()
            .relative()
            .flex()
            .gap(px(GAP))
            .h(self.problems_height)
            .flex_shrink_0()
            .children((0..count).map(|g| self.render_terminal_group(g, count, cx)))
            .child(self.resize_handle(Edge::Problems, cx))
    }

    fn render_terminal_group(&self, g: usize, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let group = self.terminal_groups.get(g);
        let current = g == self.active_group;
        let tabs: Vec<AnyElement> = group
            .map(|group| {
                group
                    .tabs
                    .iter()
                    .enumerate()
                    .map(|(ix, terminal)| {
                        let active = ix == group.active;
                        let hover_group = SharedString::from(format!("terminal-tab-{g}-{ix}"));
                        let entity = terminal.clone();
                        div()
                            .id(SharedString::from(format!("terminal-tab-{g}-{ix}")))
                            .group(hover_group.clone())
                            .relative()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .h(px(28.))
                            .min_w(px(64.))
                            .max_w(px(220.))
                            .pl_2p5()
                            .pr_1()
                            .rounded(px(8.))
                            .border_1()
                            .border_color(gpui::transparent_black())
                            .cursor_pointer()
                            .text_color(if active && current { theme.text } else { theme.text_muted })
                            // Like editor tabs: the current one raised, lit in the group with the keyboard.
                            .when(active && current, |el| el.bg(theme.elevated).border_color(theme.border_subtle).child(tab_glow(&theme)))
                            .when(active && !current, |el| el.bg(theme.hover))
                            .when(!active, |el| el.hover(|s| s.bg(theme.hover)))
                            .on_click(cx.listener(move |ws, _, window, cx| ws.activate_terminal(g, ix, window, cx)))
                            .child(match terminal.read(cx).run().map(|(_, state)| *state) {
                                // A Run tab shows how its program is doing.
                                Some(RunState::Running) => icon_el(Icon::Play, px(12.), theme.success.into()),
                                Some(RunState::Exited(Some(0))) => icon_el(Icon::CircleCheck, px(12.), theme.success.into()),
                                Some(RunState::Exited(_)) => icon_el(Icon::CircleX, px(12.), theme.error.into()),
                                None => icon_el(Icon::Terminal, px(12.), theme.text_muted.into()),
                            })
                            .child(div().flex_1().min_w_0().truncate().child(terminal.read(cx).title()))
                            .child(
                                div()
                                    .id(SharedString::from(format!("close-terminal-{g}-{ix}")))
                                    .size(px(18.))
                                    .flex()
                                    .flex_shrink_0()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.))
                                    .when(!active, |el| el.invisible())
                                    .group_hover(hover_group, |s| s.visible())
                                    .hover(|s| s.bg(theme.active))
                                    .on_click(cx.listener(move |ws, _, window, cx| {
                                        cx.stop_propagation();
                                        ws.remove_terminal(&entity, window, cx);
                                    }))
                                    .child(icon_el(Icon::X, px(12.), theme.text_muted.into())),
                            )
                            .into_any_element()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let header = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .h(px(TAB_HEIGHT - ISLAND_INSET))
            .pb(px(ISLAND_INSET))
            .flex_shrink_0()
            .when(g == 0, |el| {
                el.child(
                    div()
                        .px_1p5()
                        .mr_1()
                        .flex_shrink_0()
                        .text_size(px(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_dim)
                        .child("TERMINAL"),
                )
            })
            .child(
                div()
                    .id(("terminal-tabs", g))
                    .flex()
                    .items_center()
                    .gap(px(2.))
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tabs),
            )
            .child(
                self.icon_button("new-terminal", Icon::Plus, false, cx, move |ws, _, window, cx| {
                    ws.active_group = g;
                    ws.new_terminal(&NewTerminal, window, cx);
                })
                .id(("new-terminal", g))
                .flex_shrink_0()
                .tooltip(|window, cx| status_tooltip("New Terminal", &NewTerminal, window, cx)),
            )
            .child(div().flex_1())
            .child(
                self.icon_button("split-terminal", Icon::SplitRight, false, cx, move |ws, _, window, cx| {
                    ws.active_group = g;
                    ws.split_terminal(&SplitTerminal, window, cx);
                })
                .id(("split-terminal", g))
                .flex_shrink_0()
                .tooltip(|window, cx| status_tooltip("Split Terminal", &SplitTerminal, window, cx)),
            )
            // The panel's own close button, on the group at its right end.
            .when(g + 1 == count, |el| {
                el.child(
                    self.icon_button("hide-terminal", Icon::X, false, cx, |ws, _, window, cx| {
                        ws.show_terminal = false;
                        ws.focus_active(window, cx);
                        ws.save_state();
                        cx.notify();
                    })
                    .flex_shrink_0()
                    .tooltip(|window, cx| status_tooltip("Hide Terminal", &ToggleTerminal, window, cx)),
                )
            });
        island(&theme)
            .id(("terminal-group", g))
            .flex_1()
            .min_w_0()
            // With splits, the group that has the keyboard is outlined, like a pane.
            .when(count > 1 && current, |el| el.border_color(Hsla::from(theme.accent).alpha(0.35)))
            // Clicking into a group makes it the current one.
            .capture_any_mouse_down(cx.listener(move |ws, _, _, cx| {
                if ws.active_group != g && g < ws.terminal_groups.len() {
                    ws.active_group = g;
                    cx.notify();
                }
            }))
            .child(header)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .children(group.and_then(|group| group.tabs.get(group.active)).cloned().map(|t| div().flex_1().min_w_0().child(t))),
            )
            .into_any_element()
    }

    // ---- Search & navigation ---------------------------------------------------

    fn find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find_bar(false, window, cx);
    }

    fn replace(&mut self, _: &Replace, window: &mut Window, cx: &mut Context<Self>) {
        self.show_find_bar(true, window, cx);
    }

    fn show_find_bar(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        self.close_modal(window, cx);
        self.show_find = true;
        self.find_bar.update(cx, |f, cx| f.show(editor, replace, window, cx));
        cx.notify();
    }

    fn find_next(&mut self, _: &FindNext, window: &mut Window, cx: &mut Context<Self>) {
        self.find_step(true, window, cx);
    }

    fn find_previous(&mut self, _: &FindPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.find_step(false, window, cx);
    }

    fn find_step(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        if !self.find_bar.read(cx).has_query(cx) {
            self.show_find_bar(false, window, cx);
            return;
        }
        self.find_bar.update(cx, |f, cx| {
            f.set_target(Some(editor), cx);
            if forward { f.select_next(cx) } else { f.select_prev(cx) }
        });
    }

    fn cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.show_find {
            self.find_bar.update(cx, |f, cx| f.dismiss(cx));
        } else {
            cx.propagate();
        }
    }

    fn close_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Text typed into Preferences a moment ago still gets saved.
        if let Some(Modal::Preferences(preferences)) = &self.modal {
            let preferences = preferences.clone();
            self.preferences_page = preferences.read(cx).page();
            preferences.update(cx, |p, cx| p.flush(cx));
        }
        // Leaving the theme picker without confirming undoes the preview.
        if let Some(Modal::Theme(picker)) = &self.modal {
            let picker = picker.read(cx);
            if !picker.confirmed {
                let original = picker.original.clone();
                theme::apply_theme(&original, cx);
            }
        }
        if self.modal.take().is_some() {
            self.modal_subscription = None;
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn open_picker(&mut self, mode: PickerMode, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Modal::Picker(p)) = &self.modal
            && p.read(cx).mode() == mode
        {
            // Pressing the shortcut again closes it, like JetBrains.
            self.close_modal(window, cx);
            return;
        }
        if mode != PickerMode::Structure {
            self.refresh_file_index(cx);
        }
        let files = if self.root.is_some() {
            self.file_index.clone()
        } else {
            Arc::new(self.tabs.iter().filter_map(|t| t.read(cx).doc.path.clone()).collect())
        };
        let recent = if mode == PickerMode::Projects { self.recent_projects.clone() } else { self.recent.clone() };
        let (root, actions) = (self.root.clone(), self.actions.clone());
        let picker = cx.new(|cx| Picker::new(mode, root, files, recent, actions, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&picker, window, |ws, _, event: &PickerEvent, window, cx| match event {
            PickerEvent::OpenFile(path) => {
                ws.close_modal(window, cx);
                ws.open_path(path.clone(), window, cx);
            }
            PickerEvent::OpenProject { path, new_window } => {
                ws.close_modal(window, cx);
                ws.open_recent_project(path.clone(), *new_window, window, cx);
            }
            PickerEvent::RunAction(action) => {
                ws.close_modal(window, cx);
                window.dispatch_action(action.boxed_clone(), cx);
            }
            PickerEvent::Run(target) => {
                ws.close_modal(window, cx);
                ws.run_target(target.clone(), window, cx);
            }
            PickerEvent::Debug(target) => {
                ws.close_modal(window, cx);
                ws.debug_target(target.clone(), window, cx);
            }
            PickerEvent::OpenLocation { location, encoding } => {
                ws.close_modal(window, cx);
                ws.open_location(location.clone(), *encoding, window, cx);
            }
            PickerEvent::GoTo(offset) => {
                ws.close_modal(window, cx);
                if let Some(editor) = ws.active_editor().cloned() {
                    editor.update(cx, |e, cx| e.go_to(*offset, window, cx));
                }
            }
            PickerEvent::SymbolQuery(query) => ws.query_symbols(query.clone(), cx),
            PickerEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Picker(picker));
        cx.notify();
    }

    fn select_theme(&mut self, _: &SelectTheme, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::Theme(_))) {
            self.close_modal(window, cx);
            return;
        }
        self.close_modal(window, cx);
        let picker = cx.new(|cx| ThemePicker::new(window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&picker, window, |ws, _, event: &ThemePickerEvent, window, cx| match event {
            ThemePickerEvent::Confirmed(name) => {
                if let Err(err) = Settings::set("theme", name.as_str().into()) {
                    let _ = window.prompt(PromptLevel::Warning, "Couldn't save the theme choice", Some(&err.to_string()), &["OK"], cx);
                }
                ws.check_config(cx);
                ws.close_modal(window, cx);
            }
            ThemePickerEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::Theme(picker));
        cx.notify();
    }

    fn open_themes_folder(&mut self, _: &OpenThemesFolder, window: &mut Window, cx: &mut Context<Self>) {
        match theme::ensure_themes_dir() {
            Ok(dir) => cx.open_url(&format!("file://{}", dir.display())),
            Err(err) => {
                let _ = window.prompt(PromptLevel::Warning, "Couldn't create the themes folder", Some(&err.to_string()), &["OK"], cx);
            }
        }
    }

    fn open_snippets_folder(&mut self, _: &OpenSnippetsFolder, window: &mut Window, cx: &mut Context<Self>) {
        match crate::snippets::ensure_snippets_dir() {
            Ok(dir) => cx.open_url(&format!("file://{}", dir.display())),
            Err(err) => {
                let _ = window.prompt(PromptLevel::Warning, "Couldn't create the snippets folder", Some(&err.to_string()), &["OK"], cx);
            }
        }
    }

    fn reload_themes(&mut self, _: &ReloadThemes, _: &mut Window, cx: &mut Context<Self>) {
        theme::reload_themes(cx);
    }

    fn search_everywhere(&mut self, _: &SearchEverywhere, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Everywhere, window, cx);
    }

    fn go_to_file(&mut self, _: &GoToFile, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Files, window, cx);
    }

    fn recent_files(&mut self, _: &RecentFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Recent, window, cx);
    }

    fn find_action(&mut self, _: &FindAction, window: &mut Window, cx: &mut Context<Self>) {
        self.open_picker(PickerMode::Actions, window, cx);
    }

    /// Go to Symbol: symbols across the project, from the running language servers.
    fn go_to_symbol(&mut self, _: &GoToSymbol, window: &mut Window, cx: &mut Context<Self>) {
        let servers = self.lsp.read(cx).running_servers();
        if !servers.iter().any(|s| s.capability("workspaceSymbolProvider")) {
            self.toast(ToastKind::Info, "No language server to ask", Some("Go to Symbol uses the project's language servers. Open one of its source files first.".into()), cx);
            return;
        }
        self.open_picker(PickerMode::Symbols, window, cx);
    }

    fn query_symbols(&mut self, query: String, cx: &mut Context<Self>) {
        let query = query.trim().to_string();
        let servers: Vec<_> = self.lsp.read(cx).running_servers().into_iter().filter(|s| s.capability("workspaceSymbolProvider")).collect();
        if query.is_empty() || servers.is_empty() {
            self.symbol_task = None;
            self.set_symbol_rows(Vec::new(), cx);
            return;
        }
        self.symbol_task = Some(cx.spawn(async move |this, cx| {
            // Typing fast sends one request, not one per key.
            cx.background_executor().timer(Duration::from_millis(120)).await;
            let requests = servers.iter().map(|server| server.request("workspace/symbol", serde_json::json!({ "query": query })));
            let results = futures::future::join_all(requests).await;
            this.update(cx, |ws, cx| {
                let mut rows = Vec::new();
                for (server, result) in servers.iter().zip(results) {
                    for (name, container, location) in result.map(|v| protocol::workspace_symbols(&v)).unwrap_or_default() {
                        let preview = match container {
                            Some(container) => format!("{name}  ·  {container}"),
                            None => name,
                        };
                        let file = ws.location_label(&location.path);
                        let line = location.range.start.line as usize + 1;
                        rows.push(LocationRow { location, file, line, preview, encoding: server.encoding });
                    }
                }
                ws.set_symbol_rows(rows, cx);
            })
            .ok();
        }));
    }

    fn set_symbol_rows(&mut self, rows: Vec<LocationRow>, cx: &mut Context<Self>) {
        if let Some(Modal::Picker(picker)) = &self.modal
            && picker.read(cx).mode() == PickerMode::Symbols
        {
            picker.update(cx, |p, cx| p.set_locations(None, rows, cx));
        }
    }

    fn file_structure(&mut self, _: &FileStructure, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        let (symbols, cursor) = {
            let e = editor.read(cx);
            let symbols: Vec<_> = e
                .doc
                .syntax()
                .symbols(e.doc.rope())
                .into_iter()
                .map(|s| {
                    let line = e.doc.row_of(s.offset) + 1;
                    (s, line)
                })
                .collect();
            (symbols, e.primary_selection().head)
        };
        self.open_picker(PickerMode::Structure, window, cx);
        if let Some(Modal::Picker(picker)) = &self.modal
            && picker.read(cx).mode() == PickerMode::Structure
        {
            picker.update(cx, |p, cx| p.set_symbols(symbols, cursor, cx));
        }
    }

    fn go_to_line(&mut self, _: &GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_editor().cloned() else { return };
        let (line_count, current) = {
            let e = editor.read(cx);
            (e.doc.line_count(), e.cursor_position())
        };
        let modal = cx.new(|cx| GoToLineModal::new(line_count, current, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&modal, window, move |ws, _, event: &GoToLineEvent, window, cx| match event {
            GoToLineEvent::Confirm { line, column } => {
                ws.close_modal(window, cx);
                let (line, column) = (*line, *column);
                editor.update(cx, |e, cx| {
                    let row = line - 1;
                    let start = e.doc.line_start(row);
                    let text = e.doc.line_text(row);
                    let col = text.char_indices().nth(column - 1).map_or(text.len(), |(b, _)| b);
                    e.go_to(start + col, window, cx);
                });
            }
            GoToLineEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::GoToLine(modal));
        cx.notify();
    }

    /// Current text of every open document, so searches see unsaved edits.
    fn open_buffers(&self, cx: &App) -> HashMap<PathBuf, Arc<str>> {
        self.tabs
            .iter()
            .filter_map(|t| {
                let doc = &t.read(cx).doc;
                Some((doc.path.clone()?, Arc::from(doc.rope().to_string())))
            })
            .collect()
    }

    fn find_in_files(&mut self, _: &FindInFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search(false, window, cx);
    }

    fn replace_in_files(&mut self, _: &ReplaceInFiles, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search(true, window, cx);
    }

    fn open_project_search(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open_project_search_in(None, replace, window, cx);
    }

    /// Find in Files, optionally limited to one folder.
    fn open_project_search_in(&mut self, scope: Option<PathBuf>, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        let root = scope
            .or_else(|| self.root.clone())
            .or_else(|| {
                let path = self.active_editor()?.read(cx).doc.path.clone()?;
                path.parent().map(Path::to_path_buf)
            })
            .or_else(|| std::env::current_dir().ok());
        let Some(root) = root else { return };
        let initial = self.active_editor().and_then(|e| {
            let e = e.read(cx);
            let s = e.primary_selection();
            let text = e.doc.slice(s.range());
            (!s.is_empty() && !text.contains('\n')).then_some(text)
        });
        let overrides = self.open_buffers(cx);
        let search = cx.new(|cx| ProjectSearch::new(root, initial, replace, overrides, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&search, window, |ws, _, event: &ProjectSearchEvent, window, cx| match event {
            ProjectSearchEvent::Open { path, range } => {
                let range = range.clone();
                ws.close_modal(window, cx);
                ws.open_path(path.clone(), window, cx);
                if let Some(editor) = ws.active_editor() {
                    editor.update(cx, |e, cx| e.select_range(range, cx));
                }
            }
            ProjectSearchEvent::ReplaceAll { query, replacement, files } => {
                ws.confirm_replace_in_files(query.clone(), replacement.clone(), files.clone(), window, cx);
            }
            ProjectSearchEvent::Dismissed => ws.close_modal(window, cx),
        }));
        self.modal = Some(Modal::ProjectSearch(search));
        cx.notify();
    }

    fn confirm_replace_in_files(
        &mut self,
        query: SearchQuery,
        replacement: String,
        files: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if files.is_empty() {
            return;
        }
        let message = format!(
            "Replace all occurrences in {} file{}?",
            files.len(),
            if files.len() == 1 { "" } else { "s" }
        );
        let rx = window.prompt(
            PromptLevel::Warning,
            &message,
            Some("Changes to open files can be undone. Other files are rewritten on disk."),
            &["Replace All", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if rx.await != Ok(0) {
                return;
            }
            this.update_in(cx, |ws, window, cx| {
                let mut failures = Vec::new();
                for path in &files {
                    if let Some(editor) = ws.tabs.iter().find(|t| t.read(cx).doc.path.as_deref() == Some(path.as_path())) {
                        editor.update(cx, |e, cx| {
                            let text = e.text();
                            let edits = query
                                .find_all(&text)
                                .into_iter()
                                .map(|r| (r.clone(), query.replacement_for(&text, r, &replacement)))
                                .collect();
                            e.replace_ranges(edits, cx);
                        });
                    } else {
                        let result = std::fs::read_to_string(path).and_then(|text| {
                            let (new, _) = query.replace_all(&text, &replacement);
                            std::fs::write(path, new)
                        });
                        if let Err(err) = result {
                            failures.push(format!("{}: {err}", path.display()));
                        }
                    }
                }
                if !failures.is_empty() {
                    let _ = window.prompt(PromptLevel::Critical, "Some files couldn't be changed", Some(&failures.join("\n")), &["OK"], cx);
                }
                let overrides = ws.open_buffers(cx);
                if let Some(Modal::ProjectSearch(search)) = &ws.modal {
                    search.update(cx, |s, cx| s.refresh(overrides, cx));
                }
            })
            .ok();
        })
        .detach();
    }

    /// Detects a double tap of Shift (Search Everywhere).
    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = event.modifiers;
        if m.shift && !m.control && !m.alt && !m.platform && !m.function {
            self.shift_down = true;
            return;
        }
        if !m.modified() && std::mem::take(&mut self.shift_down) {
            let now = Instant::now();
            if self.last_shift_tap.is_some_and(|t| now.duration_since(t) < DOUBLE_SHIFT_WINDOW) {
                self.last_shift_tap = None;
                self.open_picker(PickerMode::Everywhere, window, cx);
            } else {
                self.last_shift_tap = Some(now);
            }
            return;
        }
        self.shift_down = false;
        self.last_shift_tap = None;
    }

    /// Any real key press cancels a pending double-Shift.
    fn on_any_key_down(&mut self, _: &KeyDownEvent, _: &mut Window, _: &mut Context<Self>) {
        self.shift_down = false;
        self.last_shift_tap = None;
    }

    // ---- File tree context menu --------------------------------------------------

    fn open_tree_menu(&mut self, path: PathBuf, is_dir: bool, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<TreeAction>| {
            entries.push(entry);
            actions.push(action.map(MenuAction::Tree));
        };
        let dir = if is_dir { path.clone() } else { path.parent().map(Path::to_path_buf).unwrap_or_else(|| path.clone()) };
        let is_root = self.root.as_deref() == Some(path.as_path());

        let undo = self.file_history.undo_label().map(|l| format!("Undo {l}"));
        let redo = self.file_history.redo_label().map(|l| format!("Redo {l}"));
        let binding = |action: &dyn gpui::Action| crate::ui::binding_label(action, window);
        if let Some(label) = undo {
            add(MenuEntry::item(label, Icon::Undo).shortcut(binding(&UndoFileChange)), Some(TreeAction::UndoFileChange));
        }
        if let Some(label) = redo {
            add(MenuEntry::item(label, Icon::Redo).shortcut(binding(&RedoFileChange)), Some(TreeAction::RedoFileChange));
        }
        if self.file_history.undo_label().is_some() || self.file_history.redo_label().is_some() {
            add(MenuEntry::Separator, None);
        }
        add(MenuEntry::item("New File…", Icon::FilePlus), Some(TreeAction::NewFile(dir.clone())));
        add(MenuEntry::item("New Folder…", Icon::FolderPlus), Some(TreeAction::NewFolder(dir.clone())));
        add(MenuEntry::Separator, None);
        if !is_dir {
            add(MenuEntry::item("Open", Icon::File), Some(TreeAction::Open(path.clone())));
        }
        if !is_root {
            add(MenuEntry::item("Rename…", Icon::Pencil), Some(TreeAction::Rename(path.clone())));
        }
        if !is_dir {
            add(MenuEntry::item("Duplicate", Icon::Copy), Some(TreeAction::Duplicate(path.clone())));
        }
        add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(TreeAction::CopyPath(path.clone())));
        if !is_root {
            add(MenuEntry::item("Copy Relative Path", Icon::Clipboard), Some(TreeAction::CopyRelativePath(path.clone())));
        }
        add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(TreeAction::Reveal(path.clone())));
        if is_dir {
            add(MenuEntry::item("Find in Folder…", Icon::TextSearch), Some(TreeAction::FindInFolder(path.clone())));
            add(MenuEntry::item("Collapse All", Icon::ChevronsDownUp), Some(TreeAction::CollapseAll));
        }

        // Git actions for this file, or every changed file inside this folder.
        if let Some(repo) = self.repo.clone().filter(|r| path.starts_with(r)) {
            let files: Vec<FileStatus> = self
                .git_status
                .files
                .iter()
                .filter(|f| if is_dir { f.path.starts_with(&path) } else { f.path == path })
                .cloned()
                .collect();
            let unstaged: Vec<FileStatus> = files.iter().filter(|f| f.unstaged.is_some()).cloned().collect();
            let staged: Vec<FileStatus> = files.iter().filter(|f| f.staged.is_some()).cloned().collect();
            let rel = path.strip_prefix(&repo).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
            let suffix = if is_dir { " All in Folder" } else { "" };
            let mut git_entries: Vec<(MenuEntry, TreeAction)> = Vec::new();
            if !unstaged.is_empty() {
                git_entries.push((
                    MenuEntry::item(format!("Stage{suffix}"), Icon::Plus),
                    TreeAction::Git(GitOp::Stage(unstaged.iter().map(|f| f.rel.clone()).collect())),
                ));
            }
            if !staged.is_empty() {
                git_entries.push((
                    MenuEntry::item(format!("Unstage{suffix}"), Icon::Minus),
                    TreeAction::Git(GitOp::Unstage(staged.iter().map(|f| f.rel.clone()).collect())),
                ));
            }
            if !is_dir && let Some(f) = files.first() {
                git_entries.push((MenuEntry::item("Show Diff", Icon::GitCompare), TreeAction::ShowDiff(f.clone(), f.unstaged.is_none())));
            }
            if !unstaged.is_empty() {
                git_entries.push((
                    MenuEntry::item(format!("Rollback{suffix}…"), Icon::Undo).danger(),
                    TreeAction::Git(GitOp::Discard(unstaged)),
                ));
            }
            if !rel.is_empty() && !rel.starts_with(".git") {
                git_entries.push((MenuEntry::item("Add to .gitignore", Icon::EyeOff), TreeAction::AddToGitignore(rel)));
            }
            if !git_entries.is_empty() {
                add(MenuEntry::Separator, None);
                add(MenuEntry::Header("Git".into()), None);
                for (entry, action) in git_entries {
                    add(entry, Some(action));
                }
            }
        }

        if !is_root {
            add(MenuEntry::Separator, None);
            add(MenuEntry::item(if cfg!(windows) { "Move to Recycle Bin" } else { "Move to Trash" }, Icon::Trash).danger(), Some(TreeAction::Delete(path.clone())));
        }

        self.show_context_menu(entries, actions, position, Some(path), window, cx);
    }

    fn show_context_menu(
        &mut self,
        entries: Vec<MenuEntry>,
        actions: Vec<Option<MenuAction>>,
        position: gpui::Point<Pixels>,
        target: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| ContextMenu::new(entries, window, cx));
        let subscription = cx.subscribe_in(&menu, window, |ws, _, event: &ContextMenuEvent, window, cx| match event {
            ContextMenuEvent::Chose(ix) => {
                let action = ws.context_menu.as_ref().and_then(|m| m.actions.get(*ix).cloned().flatten());
                // Closing refocuses the editor, so editor actions reach it.
                ws.close_context_menu(window, cx);
                match action {
                    Some(MenuAction::Tree(action)) => ws.run_tree_action(action, window, cx),
                    Some(MenuAction::Run(action)) => window.dispatch_action(action, cx),
                    Some(MenuAction::CodeAction(server, action)) => ws.apply_code_action(server, action.raw, window, cx),
                    Some(MenuAction::Tab(action, editor)) => ws.run_tab_action(action, &editor, window, cx),
                    Some(MenuAction::LineEndings(crlf, editor)) => editor.update(cx, |e, cx| {
                        e.doc.set_crlf(crlf);
                        cx.emit(EditorEvent::Edited);
                        cx.notify();
                    }),
                    Some(MenuAction::Encoding(encoding, editor)) => ws.choose_encoding(encoding, editor, window, cx),
                    Some(MenuAction::Launch(target, true)) => ws.debug_target(target, window, cx),
                    Some(MenuAction::Launch(target, false)) => ws.run_target(target, window, cx),
                    Some(MenuAction::Breakpoint(BreakpointEdit::Toggle, editor, row)) => editor.update(cx, |e, cx| e.toggle_breakpoint_at(row, cx)),
                    Some(MenuAction::Breakpoint(edit, editor, row)) => ws.edit_breakpoint(edit, editor, row, window, cx),
                    Some(MenuAction::Venv(choice)) => ws.set_venv(choice, cx),
                    Some(MenuAction::ChooseVenv) => ws.choose_venv_folder(window, cx),
                    None => {}
                }
            }
            ContextMenuEvent::Dismissed => ws.close_context_menu(window, cx),
        });
        self.context_menu = Some(OpenContextMenu { menu, position, actions, target, _subscription: subscription });
        cx.notify();
    }

    // ---- Line endings and encoding (status bar) ------------------------------------

    fn open_line_endings_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let crlf = editor.read(cx).doc.uses_crlf;
        let choice = |label: &'static str, on: bool| if on { MenuEntry::item(label, Icon::Check) } else { MenuEntry::text(label) };
        let entries = vec![
            MenuEntry::Header("Line endings".into()),
            choice("LF (macOS, Linux)", !crlf),
            choice("CRLF (Windows)", crlf),
        ];
        let actions = vec![None, Some(MenuAction::LineEndings(false, editor.clone())), Some(MenuAction::LineEndings(true, editor))];
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    fn open_encoding_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let current = editor.read(cx).doc.encoding;
        let mut entries = vec![MenuEntry::Header("Encoding".into())];
        let mut actions = vec![None];
        for encoding in crate::encoding::choices() {
            entries.push(match encoding == current {
                true => MenuEntry::item(encoding.menu_label(), Icon::Check),
                false => MenuEntry::text(encoding.menu_label()),
            });
            actions.push(Some(MenuAction::Encoding(encoding, editor.clone())));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// The status bar's venv for the active file's folder.
    fn shown_venv(&self, dir: &Path) -> Option<PathBuf> {
        if let Some((cached_dir, venv)) = self.venv_shown.borrow().as_ref()
            && cached_dir == dir
        {
            return venv.clone();
        }
        let venv = crate::python::venv(dir);
        self.venv_shown.replace(Some((dir.to_path_buf(), venv.clone())));
        venv
    }

    fn open_venv_menu(&mut self, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        use crate::python::{self, Choice};
        let Some(root) = self.root.clone() else { return };
        let current = python::choice(&root);
        let check = |on: bool, label: String| if on { MenuEntry::item(label, Icon::Check) } else { MenuEntry::text(label) };
        let detected = self.active_editor().and_then(|e| e.read(cx).doc.path.as_deref()?.parent().and_then(python::detect));
        let auto = match &detected {
            Some(venv) => format!("Auto-detect ({})", python::label(venv, Some(&root))),
            None => "Auto-detect (none found)".into(),
        };
        let mut entries = vec![MenuEntry::Header("Python Environment".into()), check(current.is_none(), auto)];
        let mut actions = vec![None, Some(MenuAction::Venv(None))];
        let mut venvs = python::candidates(&root);
        // A folder picked outside the project stays listed.
        if let Some(Choice::Venv(path)) = &current
            && !venvs.contains(path)
        {
            venvs.push(path.clone());
        }
        for venv in venvs {
            let on = current.as_ref() == Some(&Choice::Venv(venv.clone()));
            entries.push(check(on, python::label(&venv, Some(&root))));
            actions.push(Some(MenuAction::Venv(Some(Choice::Venv(venv)))));
        }
        entries.push(check(current == Some(Choice::Off), "None".into()));
        actions.push(Some(MenuAction::Venv(Some(Choice::Off))));
        entries.push(MenuEntry::Separator);
        actions.push(None);
        entries.push(MenuEntry::text("Choose Folder…"));
        actions.push(Some(MenuAction::ChooseVenv));
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    /// Saves the project's venv choice. New terminals and Run tabs use it;
    /// the active file's language server restarts with it.
    fn set_venv(&mut self, choice: Option<crate::python::Choice>, cx: &mut Context<Self>) {
        let Some(root) = self.root.clone() else { return };
        crate::python::set_choice(&root, choice);
        self.venv_shown.take();
        self.schedule_session_save(cx);
        if let Some(path) = self.active_editor().and_then(|e| e.read(cx).doc.path.clone()) {
            self.lsp.update(cx, |s, cx| s.restart(&path, cx));
        }
        cx.notify();
    }

    fn choose_venv_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Use Venv".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = rx.await else { return };
            let Some(path) = paths.pop() else { return };
            this.update(cx, |ws, cx| {
                if path.join("pyvenv.cfg").is_file() {
                    ws.set_venv(Some(crate::python::Choice::Venv(path)), cx);
                } else {
                    let detail = format!("{} has no pyvenv.cfg. Pick the folder python -m venv created.", path.display());
                    ws.toast(ToastKind::Error, "Not a virtual environment", Some(detail), cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Like JetBrains: reload the file as the new encoding (its text was read
    /// wrong) or convert it (the next save writes the new encoding).
    fn choose_encoding(&mut self, encoding: TextEncoding, editor: Entity<Editor>, window: &mut Window, cx: &mut Context<Self>) {
        let (current, dirty, saved) = {
            let doc = &editor.read(cx).doc;
            (doc.encoding, doc.is_dirty(), doc.path.is_some())
        };
        if encoding == current {
            return;
        }
        let convert = move |editor: &Entity<Editor>, cx: &mut Context<Self>| {
            editor.update(cx, |e, cx| {
                e.doc.convert_to(encoding);
                cx.emit(EditorEvent::Edited);
                cx.notify();
            })
        };
        if !saved {
            convert(&editor, cx);
            return;
        }
        let label = encoding.label();
        let message = format!("Reload or convert to {label}?");
        let detail = if dirty {
            format!("Convert saves the text as {label}. Reloading isn't possible with unsaved changes.")
        } else {
            format!("Reload reads the file again as {label}, for text that looks wrong. Convert keeps the text and saves it as {label}.")
        };
        let buttons: &[&str] = if dirty { &["Convert", "Cancel"] } else { &["Reload", "Convert", "Cancel"] };
        let answer = window.prompt(PromptLevel::Info, &message, Some(&detail), buttons, cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(choice) = answer.await else { return };
            let reload = !dirty && choice == 0;
            let convert_choice = if dirty { 0 } else { 1 };
            this.update(cx, |ws, cx| {
                if reload {
                    if let Err(err) = editor.update(cx, |e, cx| e.reload_with_encoding(encoding, cx)) {
                        ws.toast(ToastKind::Error, "Couldn't reload the file", Some(err.to_string()), cx);
                    }
                } else if choice == convert_choice {
                    convert(&editor, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- Editor context menu -----------------------------------------------------

    fn open_editor_menu(&mut self, editor: Entity<Editor>, position: gpui::Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.close_modal(window, cx);
        let e = editor.read(cx);
        let path = e.doc.path.clone();
        let has_change = e.has_change_at_cursor();
        let change_staged = e.change_at_cursor_staged();
        let in_conflict = e.has_conflict_at_cursor();
        let soft_wrap = e.soft_wrap();
        let mut entries: Vec<MenuEntry> = Vec::new();
        let mut actions: Vec<Option<MenuAction>> = Vec::new();
        let mut add = |entry: MenuEntry, action: Option<MenuAction>| {
            entries.push(entry);
            actions.push(action);
        };
        // An editor action with its shortcut hint.
        let run = |entry: MenuEntry, action: Box<dyn gpui::Action>| {
            (entry.shortcut(binding_label(&*action, window)), Some(MenuAction::Run(action)))
        };
        // Code intelligence first, as in VS Code and JetBrains.
        for (label, action) in [
            ("Go to Declaration", Box::new(editor::GoToDeclaration) as Box<dyn gpui::Action>),
            ("Go to Implementation", Box::new(editor::GoToImplementation)),
            ("Find Usages", Box::new(editor::FindUsages)),
            ("Rename…", Box::new(editor::RenameSymbol)),
            ("Show Context Actions", Box::new(editor::ShowContextActions)),
            ("Reformat Code", Box::new(editor::ReformatCode)),
        ] {
            let (entry, action) = run(MenuEntry::text(label), action);
            add(entry, action);
        }
        add(MenuEntry::Separator, None);
        let (entry, action) = run(MenuEntry::text("Cut"), Box::new(editor::Cut));
        add(entry, action);
        let (entry, action) = run(MenuEntry::item("Copy", Icon::Copy), Box::new(editor::Copy));
        add(entry, action);
        let (entry, action) = run(MenuEntry::item("Paste", Icon::Clipboard), Box::new(editor::Paste));
        add(entry, action);
        add(MenuEntry::Separator, None);
        for (label, action) in [
            ("Comment with Line Comment", Box::new(editor::ToggleComment) as Box<dyn gpui::Action>),
            ("Duplicate Line or Selection", Box::new(editor::DuplicateLine)),
            ("Select All Occurrences", Box::new(editor::SelectAllOccurrences)),
            ("Extend Selection", Box::new(editor::ExtendSelection)),
        ] {
            let (entry, action) = run(MenuEntry::text(label), action);
            add(entry, action);
        }

        // Git, when the file is in the repository.
        if let (Some(path), Some(repo)) = (path.as_ref(), self.repo.as_ref())
            && path.starts_with(repo)
        {
            add(MenuEntry::Separator, None);
            add(MenuEntry::Header("Git".into()), None);
            if in_conflict {
                for (label, action) in [
                    ("Accept Current Change", Box::new(editor::AcceptCurrent) as Box<dyn gpui::Action>),
                    ("Accept Incoming Change", Box::new(editor::AcceptIncoming)),
                    ("Accept Both Changes", Box::new(editor::AcceptBoth)),
                ] {
                    let (entry, action) = run(MenuEntry::text(label), action);
                    add(entry, action);
                }
            }
            if has_change {
                let (entry, action) = match change_staged {
                    Some(true) => run(MenuEntry::item("Unstage Lines", Icon::Minus), Box::new(editor::UnstageHunk)),
                    _ => run(MenuEntry::item("Stage Lines", Icon::Plus), Box::new(editor::StageHunk)),
                };
                add(entry, action);
                let (entry, action) = run(MenuEntry::item("Rollback Lines", Icon::Undo), Box::new(editor::RollbackHunk));
                add(entry, action);
            }
            if let Some(f) = self.git_status.files.iter().find(|f| &f.path == path) {
                add(
                    MenuEntry::item("Show Diff", Icon::GitCompare),
                    Some(MenuAction::Tree(TreeAction::ShowDiff(f.clone(), f.unstaged.is_none()))),
                );
            }
            let (entry, action) = run(MenuEntry::item("Show History", Icon::History), Box::new(ShowFileHistory));
            add(entry, action);
            let (entry, action) = run(MenuEntry::item("Toggle Inline Git Blame", Icon::GitCommit), Box::new(editor::ToggleInlineBlame));
            add(entry, action);
        }

        add(MenuEntry::Separator, None);
        let wrap_entry = if soft_wrap { MenuEntry::item("Soft-Wrap", Icon::Check) } else { MenuEntry::text("Soft-Wrap") };
        let (entry, action) = run(wrap_entry, Box::new(editor::ToggleSoftWrap));
        add(entry, action);
        if let Some(path) = path {
            add(MenuEntry::Separator, None);
            add(MenuEntry::item("Copy Path", Icon::Clipboard), Some(MenuAction::Tree(TreeAction::CopyPath(path.clone()))));
            if self.root.as_ref().is_some_and(|r| path.starts_with(r)) {
                add(
                    MenuEntry::item("Copy Relative Path", Icon::Clipboard),
                    Some(MenuAction::Tree(TreeAction::CopyRelativePath(path.clone()))),
                );
            }
            add(MenuEntry::item(reveal_label(), Icon::FolderOpen), Some(MenuAction::Tree(TreeAction::Reveal(path))));
        }
        self.show_context_menu(entries, actions, position, None, window, cx);
    }

    fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            self.focus_active(window, cx);
            cx.notify();
        }
    }

    fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.context_menu.as_ref()?;
        Some(
            div()
                .absolute()
                .inset_0()
                // While a menu is open, what's behind it doesn't react to the
                // pointer (no editor hovers); clicking anywhere else closes it.
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|ws, _, window, cx| ws.close_context_menu(window, cx)))
                .on_mouse_down(MouseButton::Right, cx.listener(|ws, _, window, cx| ws.close_context_menu(window, cx)))
                .child(gpui::deferred(
                    gpui::anchored()
                        .position(open.position)
                        .snap_to_window_with_margin(px(8.))
                        .child(
                            div()
                                .occlude()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                                .child(open.menu.clone()),
                        ),
                ))
                .into_any_element(),
        )
    }

    fn run_tree_action(&mut self, action: TreeAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            TreeAction::NewFile(dir) => self.ask_for_name(PendingInput::NewFile(dir), window, cx),
            TreeAction::NewFolder(dir) => self.ask_for_name(PendingInput::NewFolder(dir), window, cx),
            TreeAction::Rename(path) => self.ask_for_name(PendingInput::Rename(path), window, cx),
            TreeAction::Open(path) => self.open_path(path, window, cx),
            TreeAction::Duplicate(path) => self.duplicate_file(path, window, cx),
            TreeAction::Delete(path) => self.confirm_trash(path, window, cx),
            TreeAction::CopyPath(path) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.display().to_string()));
            }
            TreeAction::CopyRelativePath(path) => {
                let base = self.root.clone().unwrap_or_default();
                let rel = path.strip_prefix(&base).unwrap_or(&path).to_string_lossy().into_owned();
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel));
            }
            TreeAction::Reveal(path) => cx.reveal_path(&path),
            TreeAction::FindInFolder(dir) => self.open_project_search_in(Some(dir), false, window, cx),
            TreeAction::CollapseAll => {
                self.expanded.retain(|p| Some(p) == self.root.as_ref());
                self.rebuild_tree();
                self.schedule_session_save(cx);
                cx.notify();
            }
            TreeAction::Git(op) => self.run_git(op, window, cx),
            TreeAction::ShowDiff(file, staged) => self.open_diff(file, staged, window, cx),
            TreeAction::AddToGitignore(rel) => self.add_to_gitignore(rel, cx),
            TreeAction::UndoFileChange => self.step_file_history(false, window, cx),
            TreeAction::RedoFileChange => self.step_file_history(true, window, cx),
        }
    }

    fn ask_for_name(&mut self, pending: PendingInput, window: &mut Window, cx: &mut Context<Self>) {
        let (title, icon, hint, initial, select) = match &pending {
            PendingInput::NewFile(dir) => (
                "New File",
                Icon::FilePlus,
                format!("In {} · use / to create folders too", self.display_rel(dir)),
                String::new(),
                None,
            ),
            PendingInput::NewFolder(dir) => ("New Folder", Icon::FolderPlus, format!("In {}", self.display_rel(dir)), String::new(), None),
            PendingInput::Rename(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                // Preselect the name without its extension, like JetBrains.
                let stem = if path.is_dir() { name.len() } else { name.rfind('.').filter(|&i| i > 0).unwrap_or(name.len()) };
                ("Rename", Icon::Pencil, format!("Rename {name}"), name.clone(), Some(0..stem))
            }
            PendingInput::RenameSymbol { current, .. } => {
                ("Rename", Icon::Pencil, format!("Rename {current} and its usages"), current.clone(), Some(0..current.len()))
            }
            PendingInput::VimCommand => (":", Icon::Terminal, "w, q, wq, x, or a line number".to_string(), String::new(), None),
        };
        self.close_modal(window, cx);
        let dialog = cx.new(|cx| InputDialog::new(title, icon, hint, &initial, select, window, cx));
        self.modal_subscription = Some(cx.subscribe_in(&dialog, window, |ws, dialog, event: &InputDialogEvent, window, cx| match event {
            InputDialogEvent::Confirm(name) => match ws.apply_name(name, window, cx) {
                Ok(()) => {
                    let in_tree = matches!(ws.pending_input, Some(PendingInput::Rename(_) | PendingInput::NewFolder(_)));
                    ws.pending_input = None;
                    ws.close_modal(window, cx);
                    // So ⌘Z undoes it.
                    if in_tree {
                        window.focus(&ws.tree_focus);
                    }
                }
                Err(err) => dialog.update(cx, |d, cx| d.set_error(err, cx)),
            },
            InputDialogEvent::Dismissed => {
                ws.pending_input = None;
                ws.close_modal(window, cx);
            }
        }));
        self.pending_input = Some(pending);
        self.modal = Some(Modal::Input(dialog));
        cx.notify();
    }

    fn display_rel(&self, path: &Path) -> String {
        match self.root.as_deref().and_then(|r| path.strip_prefix(r).ok()) {
            Some(rel) if rel.as_os_str().is_empty() => self.root.as_deref().map(folder_name).unwrap_or_default(),
            Some(rel) => rel.display().to_string(),
            None => path.display().to_string(),
        }
    }

    /// A location's file for usage and symbol lists: relative to the project,
    /// or, outside it (a library's sources), just its folder and name.
    fn location_label(&self, path: &Path) -> String {
        if self.root.as_deref().is_some_and(|r| path.starts_with(r)) {
            return self.display_rel(path);
        }
        let mut parts: Vec<_> = path.iter().rev().take(2).map(|p| p.to_string_lossy()).collect();
        parts.reverse();
        format!("…/{}", parts.join("/"))
    }

    fn vim_command_line(&mut self, _: &VimCommandLine, window: &mut Window, cx: &mut Context<Self>) {
        self.ask_for_name(PendingInput::VimCommand, window, cx);
    }

    /// Runs a Vim `:` command: write, quit, both, or go to a line.
    fn run_vim_command(&mut self, command: &str, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        let command = command.trim().trim_start_matches(':');
        if let Ok(line) = command.parse::<usize>() {
            let editor = self.active_editor().cloned().ok_or("No file is open")?;
            editor.update(cx, |e, cx| {
                let row = line.saturating_sub(1).min(e.doc.line_count().saturating_sub(1));
                let text = e.doc.line_text(row);
                let offset = e.doc.line_start(row) + (text.len() - text.trim_start().len());
                e.go_to(offset, window, cx);
            });
            return Ok(());
        }
        match command {
            "w" | "write" => self.save(&Save, window, cx),
            "q" | "q!" | "quit" | "close" => self.close_tab(&CloseTab, window, cx),
            "wq" | "x" | "wq!" | "x!" => {
                self.save(&Save, window, cx);
                self.close_tab(&CloseTab, window, cx);
            }
            "wa" | "wall" => {
                for editor in self.tabs.clone() {
                    if editor.read(cx).doc.is_dirty() && editor.read(cx).doc.path.is_some() {
                        editor.update(cx, |e, cx| e.save(cx)).ok();
                    }
                }
            }
            "qa" | "qa!" | "qall" => cx.defer(quit),
            "" => {}
            other => return Err(format!("Not a command Kin knows: {other}")),
        }
        Ok(())
    }

    /// Creates or renames according to the open input dialog.
    fn apply_name(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) -> Result<(), String> {
        if matches!(self.pending_input, Some(PendingInput::VimCommand)) {
            let command = name.to_string();
            self.pending_input = None;
            self.close_modal(window, cx);
            if let Err(err) = self.run_vim_command(&command, window, cx) {
                self.toast(ToastKind::Error, err, None, cx);
            }
            return Ok(());
        }
        if let Some(PendingInput::RenameSymbol { server, path, position, current }) = self.pending_input.as_ref() {
            let new_name = name.trim();
            if new_name.is_empty() {
                return Err("Enter a name".into());
            }
            if new_name != current {
                self.rename_symbol(server.clone(), path.clone(), *position, new_name.to_string(), window, cx);
            }
            return Ok(());
        }
        let name = name.trim_matches('/');
        if name.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err("That isn't a valid name".into());
        }
        match self.pending_input.as_ref().ok_or("Nothing to do")? {
            PendingInput::NewFile(dir) => {
                let path = dir.join(name);
                if path.exists() {
                    return Err(format!("{name} already exists"));
                }
                let created = crate::file_history::topmost_new(&path);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(|e| e.to_string())?;
                self.file_history.record(format!("New File “{name}”"), crate::file_history::Change::Created(created));
                self.reveal_in_tree(&path);
                self.after_fs_change(cx);
                self.open_path(path, window, cx);
            }
            PendingInput::NewFolder(dir) => {
                let path = dir.join(name);
                if path.exists() {
                    return Err(format!("{name} already exists"));
                }
                let created = crate::file_history::topmost_new(&path);
                std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
                self.file_history.record(format!("New Folder “{name}”"), crate::file_history::Change::Created(created));
                self.reveal_in_tree(&path);
                self.expanded.insert(path);
                self.after_fs_change(cx);
            }
            PendingInput::Rename(old) => {
                let old = old.clone();
                let new = old.parent().map(|p| p.join(name)).ok_or("Can't rename this")?;
                if new == old {
                    return Ok(());
                }
                if new.exists() {
                    return Err(format!("{name} already exists"));
                }
                // Folders the new name adds go away again on undo.
                let new_dirs = new.parent().map(crate::file_history::topmost_new).filter(|d| !d.exists());
                if let Some(parent) = new.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                }
                std::fs::rename(&old, &new).map_err(|e| e.to_string())?;
                let old_name = old.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let renamed = crate::file_history::Change::Renamed { from: old.clone(), to: new.clone() };
                let change = match new_dirs {
                    Some(dirs) => crate::file_history::Change::Batch(vec![crate::file_history::Change::Created(dirs), renamed]),
                    None => renamed,
                };
                self.file_history.record(format!("Rename “{old_name}”"), change);
                self.moved(&old, &new, cx);
                self.reveal_in_tree(&new);
                self.after_fs_change(cx);
            }
            // Handled before the name checks above.
            PendingInput::RenameSymbol { .. } | PendingInput::VimCommand => {}
        }
        Ok(())
    }

    /// Undoes (or redoes) the last file operation. Moving files with unsaved
    /// edits to the Trash asks first.
    fn step_file_history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        let label = if redo { self.file_history.redo_label() } else { self.file_history.undo_label() };
        let Some(label) = label.map(str::to_string) else { return };
        let mut trashes = Vec::new();
        if let Some(change) = self.file_history.next(redo) {
            change.visit(&mut |c| {
                if let crate::file_history::Change::Created(p) | crate::file_history::Change::Restored(p) = c {
                    trashes.push(p.clone());
                }
            });
        }
        let dirty = self.tabs.iter().any(|t| {
            let doc = &t.read(cx).doc;
            doc.is_dirty() && doc.path.as_deref().is_some_and(|p| trashes.iter().any(|t| p.starts_with(t)))
        });
        if !dirty {
            self.apply_file_history(redo, window, cx);
            return;
        }
        let verb = if redo { "Redo" } else { "Undo" };
        let trash_name = if cfg!(windows) { "Recycle Bin" } else { "Trash" };
        let detail = format!("This moves files with unsaved changes to the {trash_name}; the changes will be lost.");
        let rx = window.prompt(PromptLevel::Warning, &format!("{verb} {label}?"), Some(&detail), &[verb, "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if rx.await == Ok(0) {
                this.update_in(cx, |ws, window, cx| ws.apply_file_history(redo, window, cx)).ok();
            }
        })
        .detach();
    }

    fn apply_file_history(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        use crate::file_history::Change;
        let Some((label, result)) = self.file_history.step(redo) else { return };
        match result {
            Ok(done) => done.visit(&mut |change| match change {
                Change::Renamed { from, to } => {
                    self.moved(from, to, cx);
                    self.reveal_in_tree(to);
                }
                Change::Trashed(trashed) => {
                    let path = &trashed.original;
                    for editor in self.tabs.clone() {
                        if editor.read(cx).doc.path.as_deref().is_some_and(|p| p.starts_with(path)) {
                            self.remove_tab(&editor, window, cx);
                        }
                    }
                    self.expanded.retain(|p| !p.starts_with(path));
                    self.recent.retain(|p| !p.starts_with(path));
                }
                Change::Created(path) | Change::Restored(path) => self.reveal_in_tree(path),
                Change::Batch(_) => {}
            }),
            Err(err) => {
                let verb = if redo { "redo" } else { "undo" };
                self.toast(ToastKind::Error, format!("Couldn't {verb} {label}"), Some(err.to_string()), cx);
            }
        }
        window.focus(&self.tree_focus);
        self.after_fs_change(cx);
        self.schedule_session_save(cx);
    }

    /// Keeps open tabs, expanded folders and recents pointing at moved files.
    fn moved(&mut self, old: &Path, new: &Path, cx: &mut Context<Self>) {
        let remap = |p: &Path| p.strip_prefix(old).ok().map(|rest| if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(rest) });
        for editor in self.tabs.clone() {
            let path = editor.read(cx).doc.path.clone();
            if let Some(moved) = path.as_deref().and_then(remap) {
                editor.update(cx, |e, cx| e.set_path(moved, cx));
                self.load_diff_base(&editor, cx);
            }
        }
        self.expanded = self.expanded.iter().map(|p| remap(p).unwrap_or_else(|| p.clone())).collect();
        for p in &mut self.recent {
            if let Some(m) = remap(p) {
                *p = m;
            }
        }
    }

    /// Moves a dragged file or folder into `dir`, keeping open tabs on it.
    fn move_entry(&mut self, from: PathBuf, dir: PathBuf, cx: &mut Context<Self>) {
        let Some(name) = from.file_name() else { return };
        let to = dir.join(name);
        if from.parent() == Some(dir.as_path()) || to == from {
            return;
        }
        let problem = if dir.starts_with(&from) {
            Some("A folder can't go inside itself".to_string())
        } else if to.exists() {
            Some(format!("{} already has {}", folder_name(&dir), name.to_string_lossy()))
        } else {
            std::fs::rename(&from, &to).err().map(|e| e.to_string())
        };
        if let Some(problem) = problem {
            self.toast(ToastKind::Error, "Couldn't move", Some(problem), cx);
            return;
        }
        let label = format!("Move “{}”", name.to_string_lossy());
        self.file_history.record(label, crate::file_history::Change::Renamed { from: from.clone(), to: to.clone() });
        self.moved(&from, &to, cx);
        self.expanded.insert(dir);
        self.reveal_in_tree(&to);
        self.after_fs_change(cx);
        self.schedule_session_save(cx);
    }

    fn set_tree_drop(&mut self, target: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.tree_drop != target {
            self.tree_drop = target;
            cx.notify();
        }
    }


    /// Finder files dropped on the tree: copied into `dir` (under a free
    /// name if taken); then, like JetBrains, an offer to add them to Git.
    fn drop_into_tree(&mut self, paths: Vec<PathBuf>, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.tree_drop = None;
        let mut added = Vec::new();
        let mut problems = Vec::new();
        for source in paths {
            let name = source.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if dir.starts_with(&source) {
                problems.push(format!("{name} can't go inside itself"));
                continue;
            }
            match copy_into(&source, &dir) {
                Ok(path) => added.push(path),
                Err(err) => problems.push(format!("{name}: {err}")),
            }
        }
        if !problems.is_empty() {
            self.toast(ToastKind::Error, "Couldn't copy everything", Some(problems.join("\n")), cx);
        }
        let Some(first) = added.first().cloned() else { return };
        let label = match added.as_slice() {
            [one] => format!("Copy “{}”", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
            many => format!("Copy {} Items", many.len()),
        };
        let created = added.iter().cloned().map(crate::file_history::Change::Created).collect();
        self.file_history.record(label, crate::file_history::Change::Batch(created));
        window.focus(&self.tree_focus);
        self.expanded.insert(dir.clone());
        self.reveal_in_tree(&first);
        self.after_fs_change(cx);
        self.schedule_session_save(cx);
        let Some(repo) = self.repo.clone().filter(|r| added.iter().all(|p| p.starts_with(r))) else { return };
        let ignored: Vec<bool> = added.iter().map(|p| self.ignore.as_ref().is_some_and(|i| i.matched_path_or_any_parents(p, p.is_dir()).is_ignore())).collect();
        let added: Vec<PathBuf> = added.into_iter().zip(ignored).filter(|(_, ignored)| !ignored).map(|(p, _)| p).collect();
        if added.is_empty() {
            return;
        }
        let names: Vec<String> = added.iter().filter_map(|p| p.strip_prefix(&repo).ok()).map(|p| p.display().to_string()).collect();
        let message = if names.len() == 1 { "Add the file to Git?".to_string() } else { format!("Add {} files to Git?", names.len()) };
        let detail = names.iter().take(6).cloned().collect::<Vec<_>>().join("\n");
        let answer = window.prompt(PromptLevel::Info, &message, Some(&detail), &["Add", "Don't Add"], cx);
        cx.spawn(async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let mut args = vec!["add".to_string(), "--".to_string()];
            args.extend(names);
            let result = cx
                .background_executor()
                .spawn({
                    let repo = repo.clone();
                    async move { git::run(&repo, &args.iter().map(String::as_str).collect::<Vec<_>>()) }
                })
                .await;
            this.update(cx, |ws, cx| {
                if let Err(err) = result {
                    ws.toast(ToastKind::Error, "Couldn't add to Git", Some(err), cx);
                }
                ws.refresh_git(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Files and folders dropped from Finder: folders open as projects,
    /// files in this window.
    fn open_dropped(&mut self, paths: &[PathBuf], window: &mut Window, cx: &mut Context<Self>) {
        for path in paths {
            if path.is_dir() {
                self.open_project(path.clone(), OpenIn::Auto, window, cx);
            } else {
                self.open_path(path.clone(), window, cx);
            }
        }
    }

    /// Opens what another app asked Kin to open (Finder, the `kin` command).
    pub fn open_external(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        self.open_dropped(&paths, window, cx);
    }

    /// Installs the `kin` command (it may ask for an administrator password).
    fn install_shell_command(&mut self, _: &InstallShellCommand, _: &mut Window, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async { crate::cli::install() }).await;
            this.update(cx, |ws, cx| match result {
                Ok(()) => {
                    let detail = format!("{} opens files and folders here: kin . or kin file.rs", crate::cli::COMMAND_PATH);
                    ws.toast(ToastKind::Success, "Installed the kin command", Some(detail), cx);
                }
                Err(err) if err == "Cancelled" => {}
                Err(err) => ws.toast(ToastKind::Error, "Couldn't install the kin command", Some(err), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Shows the active file in the tree: opens its folders and scrolls to it.
    fn select_opened_file(&mut self, _: &SelectOpenedFile, _: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.active_editor().and_then(|e| e.read(cx).doc.path.clone()) else { return };
        if !self.root.as_deref().is_some_and(|r| path.starts_with(r)) {
            return;
        }
        self.show_sidebar = true;
        self.sidebar_mode = SidebarMode::Files;
        self.reveal_in_tree(&path);
        self.rebuild_tree();
        if let Some(ix) = self.entries.iter().position(|e| e.path == path) {
            self.tree_scroll.scroll_to_item(ix, gpui::ScrollStrategy::Center);
        }
        self.save_state();
        self.schedule_session_save(cx);
        cx.notify();
    }

    /// Expands the folders above `path` so it's visible in the tree.
    fn reveal_in_tree(&mut self, path: &Path) {
        let Some(root) = self.root.clone() else { return };
        for dir in path.ancestors().skip(1) {
            if !dir.starts_with(&root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
        }
    }

    fn after_fs_change(&mut self, cx: &mut Context<Self>) {
        self.rebuild_tree();
        self.refresh_file_index(cx);
        self.refresh_git(cx);
        cx.notify();
    }

    fn duplicate_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().map(|s| s.to_string_lossy().into_owned())) else {
            return;
        };
        let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let target = (1..100)
            .map(|n| dir.join(if n == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {n}{ext}") }))
            .find(|p| !p.exists());
        let Some(target) = target else { return };
        match std::fs::copy(&path, &target) {
            Ok(_) => {
                self.file_history.record(format!("Duplicate “{stem}{ext}”"), crate::file_history::Change::Created(target.clone()));
                self.after_fs_change(cx);
                // Offer to name the copy right away.
                self.ask_for_name(PendingInput::Rename(target), window, cx);
            }
            Err(err) => self.toast(ToastKind::Error, "Couldn't duplicate", Some(err.to_string()), cx),
        }
    }

    fn confirm_trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dirty = self.tabs.iter().any(|t| {
            let doc = &t.read(cx).doc;
            doc.is_dirty() && doc.path.as_deref().is_some_and(|p| p.starts_with(&path))
        });
        let mut detail = if path.is_dir() {
            "The folder and everything in it will be moved to the Trash.".to_string()
        } else {
            "You can restore it from the Trash.".to_string()
        };
        if dirty {
            detail.push_str(" Unsaved changes in open files will be lost.");
        }
        let trash_name = if cfg!(windows) { "Recycle Bin" } else { "Trash" };
        let confirm = format!("Move to {trash_name}");
        let rx = window.prompt(
            PromptLevel::Warning,
            &format!("Move “{name}” to the {trash_name}?"),
            Some(&detail),
            &[confirm.as_str(), "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if rx.await != Ok(0) {
                return;
            }
            this.update_in(cx, |ws, window, cx| match crate::file_history::move_to_trash(&path) {
                Ok(trashed) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    ws.file_history.record(format!("Delete “{name}”"), crate::file_history::Change::Trashed(trashed));
                    window.focus(&ws.tree_focus);
                    for editor in ws.tabs.clone() {
                        if editor.read(cx).doc.path.as_deref().is_some_and(|p| p.starts_with(&path)) {
                            ws.remove_tab(&editor, window, cx);
                        }
                    }
                    ws.expanded.retain(|p| !p.starts_with(&path));
                    ws.recent.retain(|p| !p.starts_with(&path));
                    ws.after_fs_change(cx);
                }
                Err(err) => ws.toast(ToastKind::Error, "Couldn't move to Trash", Some(err.to_string()), cx),
            })
            .ok();
        })
        .detach();
    }

    fn add_to_gitignore(&mut self, rel: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else { return };
        let file = repo.join(".gitignore");
        let entry = if repo.join(&rel).is_dir() { format!("/{rel}/") } else { format!("/{rel}") };
        let existing = std::fs::read_to_string(&file).unwrap_or_default();
        if existing.lines().any(|l| l.trim() == entry) {
            self.toast(ToastKind::Success, format!("{entry} is already ignored"), None, cx);
            return;
        }
        let mut text = existing;
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&entry);
        text.push('\n');
        match std::fs::write(&file, text) {
            Ok(()) => {
                self.toast(ToastKind::Success, format!("Added {entry} to .gitignore"), None, cx);
                self.reload_changed_files(cx);
                self.after_fs_change(cx);
            }
            Err(err) => self.toast(ToastKind::Error, "Couldn't update .gitignore", Some(err.to_string()), cx),
        }
    }

    fn render_modal(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let content: AnyElement = match self.modal.as_ref()? {
            Modal::Picker(p) => p.clone().into_any_element(),
            Modal::GoToLine(g) => g.clone().into_any_element(),
            Modal::ProjectSearch(s) => s.clone().into_any_element(),
            Modal::Theme(t) => t.clone().into_any_element(),
            // Drawn over the editor area by `render_panes`.
            Modal::Diff(_) => return None,
            Modal::Branches(b) => b.clone().into_any_element(),
            Modal::Input(i) => i.clone().into_any_element(),
            Modal::Clone(c) => c.clone().into_any_element(),
            Modal::Preferences(p) => p.clone().into_any_element(),
            Modal::Push(p) => p.clone().into_any_element(),
            Modal::Trust(t) => t.clone().into_any_element(),
        };
        let top = modal_top(window);
        // Deferred above the other deferred layers (resize handles), so they
        // neither show through a modal nor take its mouse events.
        Some(
            gpui::deferred(
                div()
                    .id("modal-layer")
                    .absolute()
                    .inset_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .pt(top)
                    .bg(gpui::black().opacity(0.18))
                    // Wheel and hover don't reach the editor behind a modal.
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|ws, _, window, cx| {
                            // A running clone keeps its dialog open; it has a Stop button.
                            if let Some(Modal::Clone(c)) = &ws.modal
                                && c.read(cx).is_busy()
                            {
                                return;
                            }
                            ws.close_modal(window, cx)
                        }),
                    )
                    .child(
                        div()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(content),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    // ---- Rendering --------------------------------------------------------------

    fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        div()
            .id("titlebar")
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .h(px(TITLEBAR_HEIGHT))
            .w_full()
            // On Windows a title bar click must reach no listener, or GPUI reports
            // it handled and the system's drag, double-click and window buttons
            // don't happen. Blocking the mouse keeps the workspace root from
            // taking focus on it.
            .when(cfg!(windows), |el| el.occlude())
            .when(mac, |el| {
                el.on_click(|event: &ClickEvent, window, _| {
                    if event.click_count() == 2 {
                        window.titlebar_double_click();
                    }
                })
            })
            // Drag areas (Windows) are the bar's empty parts, not the bar itself:
            // GPUI picks the first control area under the pointer, and a parent's
            // comes before its children's (the window buttons).
            // Room for the traffic lights, which macOS moves out of the window in full screen.
            .child(
                div()
                    .h_full()
                    .w(px(if mac && !window.is_fullscreen() { 86. } else { GAP + 4. }))
                    .flex_shrink_0()
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(self.root.is_some(), |el| el.child(self.render_project_chip(cx)))
                    .when(self.repo.is_some(), |el| el.child(self.render_branch_chip(cx)))
                    .when(self.restricted(cx), |el| el.child(self.render_restricted_chip(cx))),
            )
            .child(div().flex_1().h_full().window_control_area(WindowControlArea::Drag))
            .when(!mac, |el| el.child(self.render_window_controls(cx)))
            // Centered on the window, not on the space the chips leave.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(0.3))
                    .right(relative(0.3))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.render_command_capsule(cx)),
            )
    }

    /// The title bar's center: the current file, and a way into Search
    /// Everywhere (which it opens), like a command palette's field.
    fn render_command_capsule(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let file = self.active_editor().map(|e| {
            let doc = &e.read(cx).doc;
            (doc.path.clone(), doc.file_name(), doc.is_dirty())
        });
        let folder = file.as_ref().and_then(|(path, _, _)| path.as_deref()).and_then(|path| {
            let rel = self.display_rel(path);
            let (dir, _) = rel.rsplit_once(['/', '\\'])?;
            Some(format!("{dir}/"))
        });
        div()
            .id("command-capsule")
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .max_w(px(460.))
            .h(px(28.))
            .pl_2p5()
            .pr_1()
            .rounded(px(14.))
            .bg(theme.background)
            .border_1()
            .border_color(theme.border_subtle)
            .cursor_pointer()
            .text_size(px(12.5))
            .hover(|s| s.border_color(theme.border))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.search_everywhere(&SearchEverywhere, window, cx)))
            .child(icon_el(Icon::Search, px(13.), theme.text_dim.into()))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .justify_center()
                    .gap_1p5()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .map(|el| match file {
                        Some((path, name, dirty)) => el
                            .when_some(path.as_deref().map(|p| file_icon(p, px(13.), &theme)), |el, icon| el.child(icon))
                            .child(
                                div()
                                    .flex()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .when_some(folder, |el, f| el.child(div().flex_shrink().truncate().text_color(theme.text_dim).child(f)))
                                    .child(div().flex_shrink_0().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(name)),
                            )
                            .when(dirty, |el| el.child(div().flex_shrink_0().size(px(6.)).rounded_full().bg(theme.accent))),
                        None => el.text_color(theme.text_dim).child("Search Everywhere"),
                    }),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .py(px(1.))
                    .rounded(px(10.))
                    .bg(theme.hover)
                    .text_size(px(11.))
                    .text_color(theme.text_dim)
                    .child(double_shift_label()),
            )
    }

    /// The open project: a tinted monogram and its folder name. Clicking
    /// lists recent projects.
    fn render_project_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let name = self.root.as_deref().map(folder_name).unwrap_or_default();
        let initial = name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().to_string()).unwrap_or_default();
        let tint = monogram_color(&name, &theme);
        div()
            .id("project-chip")
            .flex()
            .items_center()
            .gap_2()
            .h(px(28.))
            .pl_1()
            .pr_2()
            .rounded(px(8.))
            .cursor_pointer()
            .text_size(px(12.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text)
            .hover(|st| st.bg(theme.hover))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.recent_projects(&RecentProjects, window, cx)))
            .tooltip(|window, cx| status_tooltip("Recent Projects…", &RecentProjects, window, cx))
            .child(
                div()
                    .size(px(20.))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .bg(gpui::linear_gradient(
                        135.,
                        gpui::linear_color_stop(tint, 0.),
                        gpui::linear_color_stop(tint.alpha(0.55), 1.),
                    ))
                    .text_size(px(11.))
                    .font_weight(FontWeight::BOLD)
                    .text_color(gpui::hsla(0., 0., 0., 0.78))
                    .child(initial),
            )
            .child(name)
    }

    /// The current branch beside the project name. Clicking opens Branches….
    fn render_branch_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let s = &self.git_status;
        let dirty = !s.files.is_empty();
        div()
            .id("branch-chip")
            .flex()
            .items_center()
            .gap_1p5()
            .h(px(24.))
            .max_w(px(240.))
            .px_2()
            .rounded(px(12.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(theme.text_muted)
            .bg(theme.hover)
            .hover(|st| st.bg(theme.active).text_color(theme.text))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.git_branches(&GitBranches, window, cx)))
            .tooltip(|window, cx| status_tooltip("Branches…", &GitBranches, window, cx))
            .child(icon_el(Icon::GitBranch, px(12.), theme.text_dim.into()))
            .child(div().truncate().child(self.branch_label()))
            // Uncommitted changes, like a shell prompt's marker.
            .when(dirty, |el| el.child(div().flex_shrink_0().size(px(5.)).rounded_full().bg(theme.git_modified)))
            .when(s.ahead > 0, |el| el.child(div().flex_shrink_0().text_color(theme.text_dim).child(format!("↑{}", s.ahead))))
            .when(s.behind > 0, |el| el.child(div().flex_shrink_0().text_color(theme.text_dim).child(format!("↓{}", s.behind))))
    }

    /// Shown while the project isn't trusted; opens the trust dialog.
    fn render_restricted_chip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        div()
            .id("restricted-chip")
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .h(px(22.))
            .px_1p5()
            .rounded(px(6.))
            .cursor_pointer()
            .text_size(px(12.))
            .text_color(theme.warning)
            .bg(Hsla::from(theme.warning).alpha(0.12))
            .hover(|st| st.bg(Hsla::from(theme.warning).alpha(0.2)))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|ws, _, window, cx| ws.open_trust_dialog(window, cx)))
            .tooltip(|_, cx| tooltip_view("Language servers, formatters and project tools are off until you trust this folder", cx))
            .child(icon_el(Icon::ShieldAlert, px(13.), theme.warning.into()))
            .child("Restricted Mode")
    }

    /// The checked-out branch, or what HEAD is when it isn't one.
    fn branch_label(&self) -> String {
        let s = &self.git_status;
        s.branch.clone().unwrap_or_else(|| match &s.head_oid {
            Some(oid) => format!("{} (detached)", &oid[..7.min(oid.len())]),
            None => "No commits yet".into(),
        })
    }

    fn render_toasts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.toasts.is_empty() {
            return None;
        }
        let theme = Theme::get(cx).clone();
        Some(
            div()
                .absolute()
                .right(px(16.))
                .bottom(px(STATUSBAR_HEIGHT + 12.))
                .flex()
                .flex_col()
                .gap_2()
                .w(px(360.))
                .children(self.toasts.iter().map(|t| {
                    let id = t.id;
                    let (icon, color): (Icon, Hsla) = match t.kind {
                        ToastKind::Success => (Icon::CircleCheck, theme.success.into()),
                        ToastKind::Info => (Icon::CircleAlert, theme.text_muted.into()),
                        ToastKind::Error => (Icon::CircleX, theme.error.into()),
                    };
                    div()
                        .id(("toast", id))
                        .flex()
                        .gap_2()
                        .p_3()
                        .rounded(px(12.))
                        .bg(theme.elevated)
                        .border_1()
                        .border_color(Hsla::from(theme.border).alpha(0.85))
                        .shadow(soft_shadow(&theme, 2))
                        .cursor_pointer()
                        .on_click(cx.listener(move |ws, _, _, cx| {
                            ws.toasts.retain(|t| t.id != id);
                            cx.notify();
                        }))
                        .child(div().pt(px(1.)).child(icon_el(icon, px(16.), color)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .min_w_0()
                                .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(t.message.clone()))
                                .when_some(t.detail.clone(), |el, d| {
                                    el.child(div().text_size(px(12.)).text_color(theme.text_muted).line_clamp(4).child(d))
                                }),
                        )
                }))
                .into_any_element(),
        )
    }

    fn render_window_controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        let button = |id: &'static str, glyph: &'static str, area: WindowControlArea, hover: Hsla| {
            div()
                .id(id)
                .w(px(46.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(10.))
                .text_color(theme.text_muted)
                .font_family("Segoe Fluent Icons")
                .hover(move |s| s.bg(hover))
                .window_control_area(area)
                .child(glyph)
        };
        div()
            .flex()
            .h_full()
            .child(button("min", "\u{E921}", WindowControlArea::Min, theme.hover.into()))
            .child(button("max", "\u{E922}", WindowControlArea::Max, theme.hover.into()))
            .child(button("close", "\u{E8BB}", WindowControlArea::Close, rgb(0xc42b1c).into()))
    }

    fn icon_button(
        &self,
        id: &'static str,
        icon: Icon,
        active: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<gpui::Div> {
        let theme = Theme::get(cx);
        let color = if active { theme.text } else { theme.text_dim };
        div()
            .id(id)
            .size(px(26.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .hover(|s| s.bg(theme.hover))
            .active(|s| s.bg(theme.active))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(on_click))
            .child(icon_el(icon, px(15.), color.into()))
    }

    /// The Commit view's header: branch (opens Branches…), fetch, pull, push.
    fn render_git_toolbar(&self, cx: &mut Context<Self>) -> [AnyElement; 4] {
        let theme = Theme::get(cx).clone();
        let s = &self.git_status;
        let branch = self.branch_label();
        let button = |id: &'static str, icon: Icon, tip: &'static str, count: u32, op: GitOp, cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .gap_0p5()
                .h(px(24.))
                .min_w(px(24.))
                .px_1()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(theme.text_muted)
                .hover(|st| st.bg(theme.hover))
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .on_click(cx.listener(move |ws, _, window, cx| ws.run_git(op.clone(), window, cx)))
                .child(icon_el(icon, px(14.), theme.text_muted.into()))
                .when(count > 0, |el| el.child(count.to_string()))
                .into_any_element()
        };
        [
            div()
                .id("sidebar-branch")
                .flex()
                .items_center()
                .gap_1p5()
                .flex_1()
                .min_w_0()
                .h(px(24.))
                .px_1p5()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(12.5))
                .hover(|st| st.bg(theme.hover))
                .tooltip(|window, cx| status_tooltip("Branches…", &GitBranches, window, cx))
                .on_click(cx.listener(|ws, _, window, cx| ws.git_branches(&GitBranches, window, cx)))
                .child(icon_el(Icon::GitBranch, px(14.), theme.text_dim.into()))
                .child(div().truncate().text_color(theme.text).child(branch))
                .into_any_element(),
            button("sidebar-fetch", Icon::Refresh, "Fetch", 0, GitOp::Fetch, cx),
            button("sidebar-pull", Icon::ArrowDown, "Update Project (Pull)", s.behind, GitOp::Pull, cx),
            button("sidebar-push", Icon::ArrowUp, "Push", s.ahead, GitOp::Push, cx),
        ]
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // Under the Files / Commit switch, the Commit view has a toolbar (its branch).
        let header = (self.sidebar_mode == SidebarMode::Commit && self.repo.is_some()).then(|| {
            div()
                .flex()
                .items_center()
                .gap_0p5()
                .h(px(30.))
                .px_1()
                .mb_1()
                .flex_shrink_0()
                .border_b_1()
                .border_color(theme.border_subtle)
                .children(self.render_git_toolbar(cx))
        });

        let body: AnyElement = if self.sidebar_mode == SidebarMode::Commit {
            div().flex_1().min_h_0().child(self.git_panel.clone()).into_any_element()
        } else if self.root.is_some() {
            let active_path = self.active_editor().and_then(|e| e.read(cx).doc.path.clone());
            let entries = self.entries.clone();
            let expanded = self.expanded.clone();
            let git_paths = self.git_paths.clone();
            let menu_target = self.context_menu.as_ref().and_then(|m| m.target.clone());
            let tree_drop = self.tree_drop.clone();
            let drop_on_root = tree_drop.is_some() && tree_drop == self.root;
            uniform_list(
                "file-tree",
                entries.len(),
                cx.processor(move |_ws, range: std::ops::Range<usize>, _window, cx| {
                    let theme = Theme::get(cx);
                    range
                        .map(|ix| {
                            let e = &entries[ix];
                            let selected = active_path.as_deref() == Some(e.path.as_path());
                            let change = git_paths.get(&e.path).copied();
                            let name_color: Hsla = match change {
                                Some(c) => change_color(theme, c),
                                None if selected => theme.text.into(),
                                None => theme.text_muted.into(),
                            };
                            let is_open = e.is_dir && expanded.contains(&e.path);
                            let icon = if e.is_dir { folder_icon(is_open, px(16.), theme) } else { file_icon(&e.path, px(16.), theme) };
                            // Dropping on a file moves into the folder it's in.
                            let drop_dir = if e.is_dir { e.path.clone() } else { e.path.parent().map(Path::to_path_buf).unwrap_or_default() };
                            let dragged = DraggedEntry { path: e.path.clone(), name: e.name.clone() };
                            let is_target = e.is_dir && tree_drop.as_deref() == Some(e.path.as_path());
                            div().w_full().px_1().child(div()
                                .id(ix)
                                .relative()
                                .w_full()
                                .flex()
                                .items_center()
                                .h(px(TREE_ROW_HEIGHT))
                                .pl(px(8. + e.depth as f32 * 14.))
                                .pr_2()
                                .gap_1p5()
                                .rounded(px(6.))
                                .cursor_pointer()
                                .text_color(if selected { theme.text } else { theme.text_muted })
                                // A lit edge marks the open file.
                                .when(selected, |el| el.bg(theme.accent_soft).child(selected_edge(theme)))
                                .when(!selected, |el| el.hover(|s| s.bg(theme.hover)))
                                .on_click(cx.listener(move |ws, _, window, cx| ws.click_entry(ix, window, cx)))
                                .on_drag(dragged, |entry, _, _, cx| cx.new(|_| DraggedTabView(entry.name.clone())))
                                // While dragging, the folder a drop goes into lights up (see `tree_drop`).
                                .when(is_target, |el| el.bg(theme.accent_soft).border_1().border_color(Hsla::from(theme.accent).alpha(0.7)))
                                .on_drag_move(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |ws, ev: &DragMoveEvent<DraggedEntry>, _, cx| {
                                        if ev.bounds.contains(&ev.event.position) {
                                            ws.set_tree_drop(Some(dir.clone()), cx);
                                        }
                                    }
                                }))
                                .on_drag_move(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |ws, ev: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                                        if ev.bounds.contains(&ev.event.position) {
                                            ws.set_tree_drop(Some(dir.clone()), cx);
                                        }
                                    }
                                }))
                                .on_drop(cx.listener({
                                    let dir = drop_dir.clone();
                                    move |ws, dragged: &DraggedEntry, _, cx| {
                                        cx.stop_propagation();
                                        ws.move_entry(dragged.path.clone(), dir.clone(), cx);
                                    }
                                }))
                                .on_drop(cx.listener(move |ws, paths: &gpui::ExternalPaths, window, cx| {
                                    cx.stop_propagation();
                                    ws.drop_into_tree(paths.paths().to_vec(), drop_dir.clone(), window, cx);
                                }))
                                .when(menu_target.as_deref() == Some(e.path.as_path()), |el| {
                                    el.border_1().border_color(Hsla::from(theme.accent).alpha(0.6))
                                })
                                .on_mouse_down(MouseButton::Right, {
                                    let path = e.path.clone();
                                    let is_dir = e.is_dir;
                                    cx.listener(move |ws, ev: &MouseDownEvent, window, cx| {
                                        cx.stop_propagation();
                                        ws.open_tree_menu(path.clone(), is_dir, ev.position, window, cx);
                                    })
                                })
                                .child(
                                    div()
                                        .w(px(14.))
                                        .flex_shrink_0()
                                        .when(e.is_dir, |el| {
                                            el.child(icon_el(
                                                if is_open { Icon::ChevronDown } else { Icon::ChevronRight },
                                                px(13.),
                                                theme.text_dim.into(),
                                            ))
                                        }),
                                )
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
                                }))
                        })
                        .collect()
                }),
            )
            .track_scroll(self.tree_scroll.clone())
            // Over the tree but not on a row, a drop goes to the project folder;
            // rows (told later) narrow it to theirs.
            .on_drag_move(cx.listener(|ws, ev: &DragMoveEvent<DraggedEntry>, _, cx| {
                let target = ev.bounds.contains(&ev.event.position).then(|| ws.root.clone()).flatten();
                ws.set_tree_drop(target, cx);
            }))
            .on_drag_move(cx.listener(|ws, ev: &DragMoveEvent<gpui::ExternalPaths>, _, cx| {
                let target = ev.bounds.contains(&ev.event.position).then(|| ws.root.clone()).flatten();
                ws.set_tree_drop(target, cx);
            }))
            // The project folder itself has no row: outline the tree instead.
            .when(drop_on_root, |el| el.border_1().border_color(Hsla::from(theme.accent).alpha(0.7)).rounded(px(6.)))
            .on_drop(cx.listener(|ws, dragged: &DraggedEntry, _, cx| {
                if let Some(root) = ws.root.clone() {
                    ws.move_entry(dragged.path.clone(), root, cx);
                }
            }))
            .on_drop(cx.listener(|ws, paths: &gpui::ExternalPaths, window, cx| {
                cx.stop_propagation();
                if let Some(root) = ws.root.clone() {
                    ws.drop_into_tree(paths.paths().to_vec(), root, window, cx);
                }
            }))
            .flex_1()
            // Puts the first row's text level with the tab labels beside it
            // (GPUI applies a uniform list's own padding twice, so this is 2px).
            .pt(px(1.))
            .pb_2()
            .track_focus(&self.tree_focus)
            .key_context("FileTree")
            .on_action(cx.listener(|ws, _: &UndoFileChange, window, cx| ws.step_file_history(false, window, cx)))
            .on_action(cx.listener(|ws, _: &RedoFileChange, window, cx| ws.step_file_history(true, window, cx)))
            // Right-clicking empty space targets the project root.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|ws, ev: &MouseDownEvent, window, cx| {
                    if let Some(root) = ws.root.clone() {
                        ws.open_tree_menu(root, true, ev.position, window, cx);
                    }
                }),
            )
            .into_any_element()
        } else {
            div()
                .flex()
                .flex_col()
                .gap_3()
                .p_4()
                .text_color(theme.text_muted)
                .child("No folder opened.")
                .child(self.primary_button("open-folder-side", Icon::FolderSearch, "Open Folder", cx, |ws, _, window, cx| {
                    ws.open_folder(&OpenFolder, window, cx)
                }))
                .child(self.primary_button("clone-side", Icon::Download, "Clone Repository", cx, |ws, _, window, cx| {
                    ws.git_clone(&GitClone, window, cx)
                }))
                .into_any_element()
        };

        island(&theme)
            .w(self.sidebar_width)
            .flex_shrink_0()
            .h_full()
            .child(self.render_sidebar_switch(cx))
            .children(header)
            .child(body)
            // Deferred so the grab area sits above the editor it overlaps.
            .child(gpui::deferred(self.resize_handle(Edge::Sidebar, cx)))
    }

    /// The sidebar's two views as a segmented switch across its top: a recessed
    /// track (the chrome's color) with the current view raised like the current
    /// tab beside it, and as tall as the tab bar so the two line up.
    fn render_sidebar_switch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let files_on = self.sidebar_mode == SidebarMode::Files;
        let changes = self.git_status.files.len();
        let segment = |id: &'static str, icon: Icon, label: &'static str, on: bool| {
            div()
                .id(id)
                .relative()
                .flex_1()
                .min_w_0()
                .h(px(24.))
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .px_2p5()
                .rounded(px(7.))
                .border_1()
                .border_color(gpui::transparent_black())
                .cursor_pointer()
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if on { theme.text } else { theme.text_muted })
                .when(on, |el| {
                    el.bg(theme.elevated)
                        .border_color(theme.border_subtle)
                        .shadow(soft_shadow(&theme, 1))
                        .child(tab_glow(&theme))
                })
                .when(!on, |el| el.hover(|s| s.bg(theme.hover).text_color(theme.text)))
                .child(icon_el(icon, px(14.), if on { theme.accent.into() } else { theme.text_dim.into() }))
                .child(div().truncate().child(label))
        };
        div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .h(px(TAB_HEIGHT - ISLAND_INSET))
            .pb(px(ISLAND_INSET))
            // A little room before the list starts.
            .mb(px(2.))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(px(10.))
                    .bg(theme.panel)
                    .border_1()
                    .border_color(Hsla::from(theme.border).alpha(0.6))
                    .child(
                        segment("sidebar-files", Icon::Folder, "Files", files_on)
                            .tooltip(|window, cx| status_tooltip("Files", &ToggleSidebar, window, cx))
                            .on_click(cx.listener(|ws, _, window, cx| ws.show_files(window, cx))),
                    )
                    .child(
                        segment("sidebar-commit", Icon::GitBranch, "Commit", !files_on)
                            // Uncommitted changes, so they show from the Files view too.
                            .when(changes > 0, |el| {
                                el.child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(11.5))
                                        .text_color(if files_on { theme.text_dim } else { theme.accent })
                                        .child(changes.to_string()),
                                )
                            })
                            .tooltip(|window, cx| status_tooltip("Commit", &ToggleCommit, window, cx))
                            .on_click(cx.listener(|ws, _, window, cx| ws.show_commit(&ShowCommit, window, cx))),
                    ),
            )
    }

    /// An invisible grab area filling the gap after an island; only the
    /// cursor shows it.
    fn resize_handle(&self, edge: Edge, cx: &mut Context<Self>) -> impl IntoElement {
        let id: gpui::SharedString = match edge {
            Edge::Problems => "resize-problems".into(),
            Edge::Sidebar => "resize-sidebar".into(),
            Edge::Pane(ix) => format!("resize-pane-{ix}").into(),
        };
        let handle = div().id(id).absolute();
        let handle = match edge {
            Edge::Problems => handle.top(px(-GAP - 1.)).left_0().right_0().h(px(GAP)).cursor(CursorStyle::ResizeUpDown),
            Edge::Sidebar => handle.top_0().bottom_0().right(px(-GAP - 1.)).w(px(GAP)).cursor(CursorStyle::ResizeLeftRight),
            Edge::Pane(_) if self.pane_axis == PaneAxis::Row => {
                handle.top_0().bottom_0().right(px(-GAP - 1.)).w(px(GAP)).cursor(CursorStyle::ResizeLeftRight)
            }
            Edge::Pane(_) => handle.left_0().right_0().bottom(px(-GAP - 1.)).h(px(GAP)).cursor(CursorStyle::ResizeUpDown),
        };
        handle.on_mouse_down(
            MouseButton::Left,
            cx.listener(move |ws, e: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                let (start, size) = match edge {
                    Edge::Problems => (e.position.y, ws.problems_height),
                    Edge::Sidebar => (e.position.x, ws.sidebar_width),
                    Edge::Pane(ix) => {
                        let bounds = ws.panes_bounds.get();
                        ws.pane_drag_pair = ws.pane_sizes[ix] + ws.pane_sizes.get(ix + 1).copied().unwrap_or(0.);
                        match ws.pane_axis {
                            PaneAxis::Row => (e.position.x, bounds.size.width * ws.pane_sizes[ix]),
                            PaneAxis::Column => (e.position.y, bounds.size.height * ws.pane_sizes[ix]),
                        }
                    }
                };
                ws.resizing = Some(Resize { edge, start, size });
                cx.notify();
            }),
        )
    }

    fn primary_button(
        &self,
        id: &'static str,
        icon: Icon,
        label: &'static str,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let theme = Theme::get(cx);
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(30.))
            .px_3()
            .rounded(px(15.))
            .bg(theme.accent_soft)
            .border_1()
            .border_color(Hsla::from(theme.accent).alpha(0.3))
            .text_color(theme.accent)
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(|s| s.bg(Hsla::from(theme.accent).alpha(0.24)))
            .on_click(cx.listener(on_click))
            .child(icon_el(icon, px(15.), theme.accent.into()))
            .child(label)
    }

    fn render_tab_bar(&self, pane_ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let pane = &self.panes[pane_ix];
        let focused_pane = pane_ix == self.active_pane;
        let accent: Hsla = theme.accent.into();
        div()
            .id(("tabs", pane_ix))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.))
            .flex_shrink_0()
            .h(px(TAB_HEIGHT - ISLAND_INSET))
            .w_full()
            .pb(px(ISLAND_INSET))
            .overflow_x_scroll()
            // Dropping on the bar's empty part moves the tab to the end.
            .on_drop(cx.listener(move |ws, dragged: &DraggedTab, window, cx| {
                ws.move_tab(&dragged.editor, pane_ix, None, window, cx);
            }))
            .children(pane.tabs.iter().enumerate().map(|(ix, editor)| {
                let e = editor.read(cx);
                let active = ix == pane.active;
                let pinned = self.pinned.contains(&editor.entity_id());
                let dirty = e.doc.is_dirty();
                let errors = e.doc.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
                let icon = file_icon(e.doc.path.as_deref().unwrap_or(Path::new("")), px(15.), &theme);
                let name = e.doc.file_name();
                let name_color: Hsla = if active && focused_pane { theme.text.into() } else { theme.text_muted.into() };
                let group = SharedString::from(format!("tab-{pane_ix}-{ix}"));
                let (for_close, for_menu, for_drag, for_unpin) = (editor.clone(), editor.clone(), editor.clone(), editor.clone());
                div()
                    .id(SharedString::from(format!("tab-{pane_ix}-{ix}")))
                    .group(group.clone())
                    .relative()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h(px(28.))
                    .pl_2p5()
                    .pr_1()
                    .flex_shrink_0()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .cursor_pointer()
                    // The current tab is raised; in the focused pane it's lit underneath.
                    .when(active && focused_pane, |el| {
                        el.bg(theme.elevated).border_color(theme.border_subtle).child(tab_glow(&theme))
                    })
                    .when(active && !focused_pane, |el| el.bg(theme.hover))
                    .when(!active, |el| el.hover(|s| s.bg(theme.hover)))
                    .on_click(cx.listener(move |ws, _, window, cx| ws.activate_in(pane_ix, ix, window, cx)))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |ws, _, window, cx| ws.close_editor(&for_close, window, cx)),
                    )
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |ws, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            ws.open_tab_menu(for_menu.clone(), event.position, window, cx);
                        }),
                    )
                    .on_drag(DraggedTab { editor: for_drag, name: name.clone().into() }, |tab, _, _, cx| {
                        cx.new(|_| DraggedTabView(tab.name.clone()))
                    })
                    // A drop lands before this tab; the accent edge shows where.
                    .drag_over::<DraggedTab>(move |style, _, _, _| style.border_l_2().border_color(accent))
                    .on_drop(cx.listener(move |ws, dragged: &DraggedTab, window, cx| {
                        cx.stop_propagation();
                        ws.move_tab(&dragged.editor, pane_ix, Some(ix), window, cx);
                    }))
                    .child(icon)
                    .child(div().text_color(name_color).when(active, |el| el.font_weight(FontWeight::MEDIUM)).child(name))
                    // Files with errors get a small dot rather than a recolored name.
                    .when(errors > 0, |el| el.child(div().size(px(6.)).rounded_full().bg(theme.error)))
                    .child(
                        div()
                            .id(SharedString::from(format!("close-{pane_ix}-{ix}")))
                            .size(px(18.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .hover(|s| s.bg(theme.active))
                            .on_click(cx.listener(move |ws, _, window, cx| {
                                cx.stop_propagation();
                                // A pinned tab's pin unpins it; otherwise this closes.
                                let action = if ws.pinned.contains(&for_unpin.entity_id()) { TabAction::TogglePin } else { TabAction::Close };
                                ws.run_tab_action(action, &for_unpin, window, cx);
                            }))
                            .map(|el| {
                                if pinned {
                                    el.child(icon_el(Icon::Pin, px(12.), theme.text_muted.into()))
                                } else if dirty {
                                    // A dot that turns into a close button on hover.
                                    el.child(
                                        div()
                                            .size(px(7.))
                                            .rounded_full()
                                            .bg(theme.accent)
                                            .group_hover(group.clone(), |s| s.invisible()),
                                    )
                                    .child(
                                        div()
                                            .absolute()
                                            .invisible()
                                            .group_hover(group.clone(), |s| s.visible())
                                            .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                                    )
                                } else {
                                    el.child(
                                        div()
                                            .when(!active, |d| d.invisible())
                                            .group_hover(group.clone(), |s| s.visible())
                                            .child(icon_el(Icon::X, px(13.), theme.text_muted.into())),
                                    )
                                }
                            }),
                    )
            }))
    }

    /// The editor area: panes side by side or stacked, with dividers.
    fn render_panes(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bounds = self.panes_bounds.clone();
        div()
            .relative()
            .flex()
            .gap(px(GAP))
            .size_full()
            .when(self.pane_axis == PaneAxis::Column, |el| el.flex_col())
            .child(
                gpui::canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .children((0..self.panes.len()).map(|ix| self.render_pane(ix, window, cx)))
            .when_some(self.diff_view(), |el, view| el.child(self.render_diff_overlay(view, cx)))
    }

    fn diff_view(&self) -> Option<Entity<DiffView>> {
        match &self.modal {
            Some(Modal::Diff(view)) => Some(view.clone()),
            _ => None,
        }
    }

    /// The Git diff covers only the editor area, so the sidebar (Commit view)
    /// and the bottom panels (Git log) stay usable beside it.
    fn render_diff_overlay(&self, view: Entity<DiffView>, cx: &App) -> impl IntoElement {
        gpui::deferred(
            island(Theme::get(cx))
                .id("diff-layer")
                .absolute()
                .inset_0()
                .occlude()
                .child(div().size_full().overflow_hidden().child(view)),
        )
        .with_priority(1)
    }

    fn render_pane(&self, pane_ix: usize, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let pane = &self.panes[pane_ix];
        let share = relative(self.pane_sizes.get(pane_ix).copied().unwrap_or(1.));
        let content: AnyElement = match pane.tabs.get(pane.active) {
            Some(editor) => editor.clone().into_any_element(),
            None if self.panes.len() == 1 => self.render_welcome(window, cx).into_any_element(),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_1()
                .bg(theme.background)
                .text_color(theme.text_dim)
                .child("No open files")
                .child(div().text_size(px(12.)).child("Open a file, or drag a tab here"))
                .into_any_element(),
        };
        let multiple = self.panes.len() > 1;
        let drop_group = SharedString::from(format!("pane-drop-{pane_ix}"));
        island(&theme)
            .min_w_0()
            .min_h_0()
            .map(|el| match self.pane_axis {
                PaneAxis::Row => el.h_full().w(share),
                PaneAxis::Column => el.w_full().h(share),
            })
            // With splits, the pane that has the keyboard is outlined.
            .when(multiple && pane_ix == self.active_pane, |el| el.border_color(Hsla::from(theme.accent).alpha(0.35)))
            .when(!pane.tabs.is_empty(), |el| el.child(self.render_tab_bar(pane_ix, cx)))
            .when_some(pane.tabs.get(pane.active).cloned(), |el, editor| el.children(self.render_breadcrumbs(&editor, cx)))
            .when(self.show_find && pane_ix == self.active_pane && !pane.tabs.is_empty(), |el| {
                el.child(self.find_bar.clone())
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    // Clicking into a pane makes it the current one.
                    .capture_any_mouse_down(cx.listener(move |ws, _, window, cx| {
                        if ws.active_pane != pane_ix {
                            let active = ws.panes[pane_ix].active;
                            ws.active_pane = pane_ix;
                            ws.activate_in(pane_ix, active, window, cx);
                        }
                    }))
                    .on_drop(cx.listener(move |ws, dragged: &DraggedTab, window, cx| {
                        ws.move_tab(&dragged.editor, pane_ix, None, window, cx);
                    }))
                    .child(content),
            )
            // Finder files dragged anywhere over the pane (tabs included) open in it.
            .group(drop_group.clone())
            .child(
                // Shown by GPUI while Finder files are over the pane, so there's no
                // state to go stale. Being the drop target also gives it the
                // hitbox GPUI needs to apply drag styles; hidden, it takes no events.
                div()
                    .invisible()
                    .group_drag_over::<gpui::ExternalPaths>(drop_group, |s| s.visible())
                    .on_drop(cx.listener(move |ws, paths: &gpui::ExternalPaths, window, cx| {
                        cx.stop_propagation();
                        ws.active_pane = pane_ix;
                        ws.open_dropped(paths.paths(), window, cx);
                    }))
                    .absolute()
                    .inset_0()
                    .p_2()
                    .child(
                    div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(10.))
                        .border_2()
                        .border_dashed()
                        .border_color(Hsla::from(theme.accent).alpha(0.8))
                        .bg(Hsla::from(theme.accent).alpha(0.08))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .px_3()
                                .py_1p5()
                                .rounded(px(8.))
                                .bg(theme.elevated)
                                .border_1()
                                .border_color(theme.border)
                                .shadow_lg()
                                .text_color(theme.text)
                                .child(icon_el(Icon::FileCode, px(15.), theme.accent.into()))
                                .child("Drop to open"),
                        ),
                    ),
            )
            .when(pane_ix + 1 < self.panes.len(), |el| el.child(self.resize_handle(Edge::Pane(pane_ix), cx)))
            .into_any_element()
    }

    /// Under the tabs: the file's folders and name, then the definitions
    /// around the caret; clicking one goes to it.
    fn render_breadcrumbs(&self, editor: &Entity<Editor>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (path, symbols) = editor.update(cx, |e, _| (e.doc.path.clone(), e.doc.image.is_none().then(|| e.breadcrumbs())));
        let path = path?;
        let theme = Theme::get(cx).clone();
        let inside = self.root.as_deref().is_some_and(|r| path.starts_with(r));
        let rel = self.display_rel(&path);
        let mut parts: Vec<String> = rel.split(['/', '\\']).map(str::to_string).collect();
        let file = parts.pop().unwrap_or_default();
        // A file outside the project: its folder is enough context.
        if !inside {
            parts = parts.pop().into_iter().collect();
        }
        let chevron = || icon_el(Icon::ChevronRight, px(11.), theme.text_dim.into());
        let mut row = div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .h(px(24.))
            .px_3()
            .overflow_hidden()
            .whitespace_nowrap()
            .bg(theme.background)
            .text_size(px(12.))
            .text_color(theme.text_dim);
        for part in parts {
            row = row.child(div().flex_shrink_0().child(part)).child(chevron());
        }
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .flex_shrink_0()
                .text_color(theme.text_muted)
                .child(file_icon(&path, px(13.), &theme))
                .child(file),
        );
        let symbols = symbols.unwrap_or_default();
        let last = symbols.len().saturating_sub(1);
        for (ix, (name, offset)) in symbols.into_iter().enumerate() {
            let editor = editor.clone();
            row = row.child(chevron()).child(
                div()
                    .id(("breadcrumb", ix))
                    .flex_shrink_0()
                    .px_1()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_color(if ix == last { theme.text } else { theme.text_muted })
                    .hover(|s| s.bg(theme.hover))
                    .on_click(move |_, window, cx| {
                        editor.update(cx, |e, cx| e.go_to(offset, window, cx));
                        window.focus(&editor.focus_handle(cx));
                    })
                    .child(name),
            );
        }
        Some(row.into_any_element())
    }

    fn render_welcome(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        // Lay out for the space the welcome screen actually gets.
        let sidebar = if self.show_sidebar { self.sidebar_width } else { px(0.) };
        let available = window.viewport_size().width - sidebar;
        let wide = available > px(820.);

        // ---- Header ----
        let header = div()
            .flex()
            .items_center()
            .gap_4()
            .child(img(assets::LOGO).size(px(56.)).rounded(px(14.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_2()
                            .child(div().font_family(assets::TITLE_FONT).text_size(px(32.)).line_height(px(36.)).text_color(theme.text).child("Kin"))
                            .child(
                                div()
                                    .mb(px(5.))
                                    .px_1p5()
                                    .rounded(px(5.))
                                    .bg(theme.hover)
                                    .text_size(px(11.))
                                    .text_color(theme.text_dim)
                                    .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                            ),
                    )
                    .child(div().text_color(theme.text_dim).child("A fast, GPU-rendered code editor")),
            )
            .child(
                div()
                    .id("w-theme")
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .h(px(28.))
                    .px_2p5()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(theme.border_subtle)
                    .text_size(px(12.))
                    .text_color(theme.text_muted)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover).text_color(theme.text))
                    .on_click(cx.listener(|ws, _, window, cx| ws.select_theme(&SelectTheme, window, cx)))
                    .tooltip(|window, cx| status_tooltip("Select Theme…", &SelectTheme, window, cx))
                    .child(icon_el(Icon::Palette, px(13.), theme.text_dim.into()))
                    .child(theme.name.clone()),
            );

        // ---- Start tiles ----
        let tile = |id: &'static str, icon: Icon, title: &'static str, detail: &'static str, keys: Option<String>| {
            div()
                .id(id)
                .flex()
                .flex_col()
                .gap_3()
                .p(px(14.))
                .rounded(px(12.))
                .bg(theme.panel)
                .border_1()
                .border_color(theme.border_subtle)
                .cursor_pointer()
                .hover(|s| s.bg(theme.hover).border_color(Hsla::from(theme.accent).alpha(0.45)))
                .active(|s| s.bg(theme.active))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .justify_between()
                        .child(
                            div()
                                .size(px(32.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(9.))
                                .bg(theme.accent_soft)
                                .child(icon_el(icon, px(16.), theme.accent.into())),
                        )
                        .when_some(keys, |el, k| el.child(kbd(k, cx))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(title))
                        .child(div().text_size(px(12.)).text_color(theme.text_dim).line_clamp(2).child(detail)),
                )
        };
        let tiles = div()
            .grid()
            .grid_cols(if wide { 4 } else { 2 })
            .gap_3()
            .child(
                tile("w-new-file", Icon::FilePlus, "New File", "Start from an empty buffer", binding_label(&NewFile, window))
                    .on_click(cx.listener(|ws, _, w, cx| ws.new_file(&NewFile, w, cx))),
            )
            .child(
                tile("w-open-file", Icon::FileCode, "Open File…", "Edit one or more files", binding_label(&OpenFile, window))
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_file(&OpenFile, w, cx))),
            )
            .child(
                tile("w-open-folder", Icon::FolderSearch, "Open Folder…", "Browse a project with its file tree", binding_label(&OpenFolder, window))
                    .on_click(cx.listener(|ws, _, w, cx| ws.open_folder(&OpenFolder, w, cx))),
            )
            .child(
                tile("w-clone", Icon::Download, "Clone Repository…", "From GitHub, GitLab or any Git URL", None)
                    .on_click(cx.listener(|ws, _, w, cx| ws.git_clone(&GitClone, w, cx))),
            );

        // ---- Recent projects (and this session's files) ----
        let mut recent = div().flex().flex_col().gap_2().child(section_label("Recent Projects", &theme));
        if self.recent_projects.is_empty() {
            recent = recent.child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1()
                    .py_6()
                    .rounded(px(12.))
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .child(icon_el(Icon::History, px(20.), theme.text_dim.into()))
                    .child(div().mt_1().text_color(theme.text_muted).child("No recent projects"))
                    .child(div().text_size(px(12.)).text_color(theme.text_dim).child("Folders you open or clone show up here")),
            );
        } else {
            let list = self.recent_projects.iter().enumerate().map(|(ix, path)| {
                let name = folder_name(path);
                let parent = path.parent().map(clone_dialog::display_path).unwrap_or_default();
                let current = self.root.as_deref() == Some(path.as_path());
                let missing = !path.exists();
                let group = SharedString::from(format!("recent-{ix}"));
                let color = monogram_color(&name, &theme);
                let open = path.clone();
                let remove = path.clone();
                div()
                    .id(("w-recent", ix))
                    .group(group.clone())
                    .flex()
                    .items_center()
                    .gap_3()
                    .h(px(46.))
                    .px_2()
                    .rounded(px(8.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover))
                    .when(missing, |el| el.opacity(0.55))
                    .on_click(cx.listener(move |ws, ev: &ClickEvent, window, cx| {
                        ws.open_recent_project(open.clone(), ev.modifiers().secondary(), window, cx)
                    }))
                    .child(
                        div()
                            .size(px(30.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.))
                            .bg(color.alpha(0.16))
                            .text_color(color)
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().truncate().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(name))
                                    .when(current, |el| {
                                        el.child(
                                            div()
                                                .px_1p5()
                                                .rounded(px(4.))
                                                .bg(theme.accent_soft)
                                                .text_size(px(10.5))
                                                .text_color(theme.accent)
                                                .child("Open"),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(theme.text_dim)
                                    .child(if missing { format!("{parent} · missing") } else { parent }),
                            ),
                    )
                    .child(
                        icon_button(("w-recent-x", ix), Icon::X, theme.text_dim.into(), cx)
                            .invisible()
                            .group_hover(group, |s| s.visible())
                            .tooltip(|_, cx| tooltip_view("Remove from Recent Projects", cx))
                            .on_click(cx.listener(move |ws, _, _, cx| {
                                cx.stop_propagation();
                                ws.remove_recent_project(&remove, cx);
                            })),
                    )
            });
            recent = recent.child(card(&theme).p_1().children(list));
        }
        let files: Vec<PathBuf> = self.recent.iter().filter(|p| p.is_file()).take(5).cloned().collect();
        if !files.is_empty() {
            recent = recent.child(div().h(px(8.))).child(section_label("Recent Files", &theme)).child(
                card(&theme).p_1().children(files.into_iter().enumerate().map(|(ix, path)| {
                    let icon = file_icon(&path, px(15.), &theme);
                    let rel = self.display_rel(&path);
                    let open = path.clone();
                    div()
                        .id(("w-file", ix))
                        .flex()
                        .items_center()
                        .gap_2()
                        .h(px(30.))
                        .px_2()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.hover))
                        .on_click(cx.listener(move |ws, _, window, cx| ws.open_path(open.clone(), window, cx)))
                        .child(icon)
                        .child(div().truncate().text_color(theme.text_muted).child(rel))
                })),
            );
        }

        // ---- Git ----
        let git = self.render_welcome_git(window, cx);

        // ---- Quick actions ----
        let double_shift = double_shift_label();
        let auto_save_on = self.auto_save != AutoSave::Off;
        let quick = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(section_label("Quick Actions", &theme))
            .child(
                card(&theme)
                    .p_1()
                    .child(
                        welcome_row("w-q-search", Icon::Search, "Search Everywhere", Some(double_shift.into()), &theme, cx)
                            .on_click(cx.listener(|ws, _, w, cx| ws.search_everywhere(&SearchEverywhere, w, cx))),
                    )
                    .child(
                        welcome_row("w-q-action", Icon::Command, "Find Action…", binding_label(&FindAction, window), &theme, cx)
                            .on_click(cx.listener(|ws, _, w, cx| ws.find_action(&FindAction, w, cx))),
                    )
                    .when(self.root.is_some(), |el| {
                        el.child(
                            welcome_row("w-q-file", Icon::FileText, "Go to File…", binding_label(&GoToFile, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.go_to_file(&GoToFile, w, cx))),
                        )
                        .child(
                            welcome_row("w-q-find", Icon::TextSearch, "Find in Files…", binding_label(&FindInFiles, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.find_in_files(&FindInFiles, w, cx))),
                        )
                    })
                    .child(
                        welcome_row("w-q-theme", Icon::Palette, "Select Theme…", binding_label(&SelectTheme, window), &theme, cx)
                            .on_click(cx.listener(|ws, _, w, cx| ws.select_theme(&SelectTheme, w, cx))),
                    )
                    .child(
                        welcome_row("w-q-autosave", Icon::Save, "Auto Save", None, &theme, cx)
                            .child(switch(auto_save_on, &theme))
                            .on_click(cx.listener(|ws, _, w, cx| ws.toggle_auto_save(&ToggleAutoSave, w, cx))),
                    ),
            );

        let left = div().flex().flex_col().gap_5().flex_1().min_w_0().child(recent).child(quick);
        let right = div()
            .flex()
            .flex_col()
            .gap_5()
            .when(wide, |el| el.w(px(320.)).flex_shrink_0())
            .child(git);

        let tip = div()
            .flex()
            .items_center()
            .justify_center()
            .gap_1p5()
            .text_size(px(12.))
            .text_color(theme.text_dim)
            .child(icon_el(Icon::Keyboard, px(13.), theme.text_dim.into()))
            .child("Press")
            .child(kbd(double_shift, cx))
            .child("to find files and actions, or")
            .children(binding_label(&FindAction, window).map(|k| kbd(k, cx)))
            .child("for any command");

        div()
            .id("welcome")
            .size_full()
            .overflow_y_scroll()
            .bg(theme.background)
            .child(
                div()
                    .min_h(gpui::relative(1.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .px_6()
                    .py(px(40.))
                    .child(
                        div()
                            .w_full()
                            .max_w(px(900.))
                            .flex()
                            .flex_col()
                            .gap(px(28.))
                            .child(header)
                            .child(tiles)
                            .child(
                                div()
                                    .flex()
                                    .gap_6()
                                    .when(wide, |el| el.flex_row().items_start())
                                    .when(!wide, |el| el.flex_col())
                                    .child(left)
                                    .child(right),
                            )
                            .child(tip),
                    ),
            )
    }

    /// The welcome screen's Git card: repository state and actions, or a way
    /// to get a repository.
    fn render_welcome_git(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::get(cx).clone();
        let section = div().flex().flex_col().gap_2().child(section_label("Git", &theme));
        let body_text = |text: String| div().text_size(px(12.)).text_color(theme.text_dim).child(text);

        if self.repo.is_none() {
            let (title, detail) = match self.root.as_deref() {
                Some(root) => ("Not a Git repository", format!("Start tracking changes in {} with Git.", folder_name(root))),
                None => ("No repository open", "Clone a repository, or open a folder that's already under version control.".to_string()),
            };
            return section.child(
                card(&theme)
                    .p(px(14.))
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(icon_el(Icon::GitBranch, px(15.), theme.text_muted.into()))
                                    .child(div().text_color(theme.text).font_weight(FontWeight::MEDIUM).child(title)),
                            )
                            .child(body_text(detail)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when(self.root.is_some(), |el| {
                                el.child(self.primary_button("w-git-init", Icon::Plus, "Initialize", cx, |ws, _, window, cx| {
                                    ws.git_init(&GitInit, window, cx)
                                }))
                            })
                            .child(self.primary_button("w-git-clone", Icon::Download, "Clone…", cx, |ws, _, window, cx| {
                                ws.git_clone(&GitClone, window, cx)
                            })),
                    ),
            )
            .into_any_element();
        };

        let status = &self.git_status;
        let branch = status.branch.clone().unwrap_or_else(|| "detached HEAD".into());
        let changes = status.files.len();
        let web = self.git_remote.as_deref().and_then(git::web_url);
        let counter = |icon: Icon, n: u32, tip: &'static str, id: &'static str| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_0p5()
                .px_1p5()
                .h(px(20.))
                .rounded(px(5.))
                .bg(theme.hover)
                .text_size(px(11.5))
                .text_color(theme.text_muted)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .child(icon_el(icon, px(11.), theme.text_muted.into()))
                .child(n.to_string())
        };

        let summary = div()
            .flex()
            .flex_col()
            .gap_2()
            .p(px(14.))
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .id("w-git-branch")
                    .flex()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_click(cx.listener(|ws, _, w, cx| ws.git_branches(&GitBranches, w, cx)))
                    .tooltip(|_, cx| tooltip_view("Switch or create a branch", cx))
                    .child(icon_el(Icon::GitBranch, px(15.), theme.accent.into()))
                    .child(div().flex_1().min_w_0().truncate().text_color(theme.text).font_weight(FontWeight::SEMIBOLD).child(branch))
                    .when(status.ahead > 0, |el| el.child(counter(Icon::ArrowUp, status.ahead, "Commits to push", "w-ahead")))
                    .when(status.behind > 0, |el| el.child(counter(Icon::ArrowDown, status.behind, "Commits to pull", "w-behind")))
                    .child(icon_el(Icon::ChevronDown, px(13.), theme.text_dim.into())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_size(px(12.))
                    .map(|el| {
                        if changes == 0 {
                            el.text_color(theme.text_dim)
                                .child(icon_el(Icon::CircleCheck, px(13.), theme.success.into()))
                                .child("Working tree clean")
                        } else {
                            el.text_color(theme.text_muted)
                                .child(icon_el(Icon::CircleAlert, px(13.), theme.git_modified.into()))
                                .child(format!("{changes} changed file{}", if changes == 1 { "" } else { "s" }))
                        }
                    })
                    .when_some(self.git_busy, |el, busy| el.child(div().flex_1()).child(div().text_color(theme.accent).child(busy))),
            )
            .when_some(self.last_commit.clone(), |el, c| {
                el.child(
                    div()
                        .flex()
                        .gap_2()
                        .text_size(px(12.))
                        .child(div().pt(px(1.)).child(icon_el(Icon::GitCommit, px(13.), theme.text_dim.into())))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .min_w_0()
                                .child(div().truncate().text_color(theme.text_muted).child(c.summary))
                                .child(
                                    div()
                                        .truncate()
                                        .text_color(theme.text_dim)
                                        .child(format!("{} · {} · {}", c.sha, c.author, git::relative_time(c.time))),
                                ),
                        ),
                )
            })
            .when_some(web.clone(), |el, url| {
                let host = url.trim_start_matches("https://").to_string();
                el.child(
                    div()
                        .id("w-git-remote")
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(12.))
                        .text_color(theme.text_dim)
                        .cursor_pointer()
                        .hover(|s| s.text_color(theme.accent))
                        .on_click(cx.listener(|ws, _, w, cx| ws.open_remote(&OpenRemote, w, cx)))
                        .tooltip(|_, cx| tooltip_view("Open in Browser", cx))
                        .child(icon_el(Icon::Link, px(13.), theme.text_dim.into()))
                        .child(div().flex_1().min_w_0().truncate().child(host))
                        .child(icon_el(Icon::ExternalLink, px(12.), theme.text_dim.into())),
                )
            });

        section
            .child(
            card(&theme)
                .child(summary)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .p_1()
                        .child(
                            welcome_row("w-git-commit", Icon::GitCommit, "Commit…", binding_label(&ShowCommit, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.show_commit(&ShowCommit, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-pull", Icon::ArrowDown, "Update Project", binding_label(&GitPull, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_pull(&GitPull, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-push", Icon::ArrowUp, "Push…", binding_label(&GitPush, window), &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_push(&GitPush, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-fetch", Icon::Refresh, "Fetch", None, &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_fetch(&GitFetch, w, cx))),
                        )
                        .child(
                            welcome_row("w-git-branches", Icon::GitBranch, "Branches…", None, &theme, cx)
                                .on_click(cx.listener(|ws, _, w, cx| ws.git_branches(&GitBranches, w, cx))),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let run_problems = self.run_problems_shown(cx);
        let editor = self.active_editor().cloned();
        let diagnostics = editor
            .as_ref()
            .map(|e| e.read(cx).doc.diagnostics.clone())
            .unwrap_or_default();
        let positions: Vec<(usize, usize)> = editor
            .as_ref()
            .map(|e| {
                let doc = &e.read(cx).doc;
                diagnostics
                    .iter()
                    .map(|d| {
                        let row = doc.row_of(d.range.start);
                        let col = doc.slice(doc.line_start(row)..d.range.start).chars().count();
                        (row + 1, col + 1)
                    })
                    .collect()
            })
            .unwrap_or_default();

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.))
            .px_3()
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme.border_subtle)
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child("PROBLEMS"),
            )
            .child(
                div()
                    .px_1p5()
                    .rounded_full()
                    .bg(theme.active)
                    .text_size(px(11.))
                    .text_color(theme.text_muted)
                    .child((diagnostics.len() + run_problems.len()).to_string()),
            )
            .child(div().flex_1())
            .child(self.icon_button("close-problems", Icon::X, false, cx, |ws, _, window, cx| {
                ws.toggle_problems(&ToggleProblems, window, cx)
            }));

        // A section per source when the last Run reported problems too.
        let section = |id: SharedString, icon: Icon, title: String| {
            div()
                .id(id)
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1p5()
                .h(px(24.))
                .px_3()
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_dim)
                .child(icon_el(icon, px(12.), theme.text_dim.into()))
                .child(title)
        };
        let row = |id: SharedString, icon: Icon, color: Hsla, message: String| {
            div()
                .id(id)
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_2()
                .h(px(26.))
                .mx_1p5()
                .px_2()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|s| s.bg(theme.hover))
                .child(icon_el(icon, px(14.), color))
                .child(div().min_w_0().text_color(theme.text).truncate().child(message))
        };
        let run_rows: Vec<AnyElement> = self
            .run_problems
            .iter()
            .filter(|_| !run_problems.is_empty())
            .flat_map(|(name, _)| {
                let title = section("run-problems".into(), Icon::Play, name.clone()).into_any_element();
                let rows = run_problems.iter().enumerate().map(|(ix, p)| {
                    let (icon, color) = severity_icon(p.severity, &theme);
                    let place = match p.column {
                        Some(column) => format!("{}:{}:{column}", self.display_rel(&p.path), p.line),
                        None => format!("{}:{}", self.display_rel(&p.path), p.line),
                    };
                    let (path, line, column) = (p.path.clone(), p.line, p.column);
                    row(SharedString::from(format!("run-problem-{ix}")), icon, color, p.message.clone())
                        .on_click(cx.listener(move |ws, _, window, cx| ws.open_at(path.clone(), Some(line), column, window, cx)))
                        .child(div().flex_1())
                        .child(div().flex_shrink_0().text_size(px(12.)).text_color(theme.text_dim).child(place))
                        .into_any_element()
                });
                std::iter::once(title).chain(rows).collect::<Vec<_>>()
            })
            .collect();
        let file_title = (!run_rows.is_empty() && !diagnostics.is_empty())
            .then(|| editor.as_ref().and_then(|e| e.read(cx).doc.path.as_deref().map(folder_name)))
            .flatten()
            .map(|name| section("file-problems".into(), Icon::File, name).into_any_element());

        let list = if diagnostics.is_empty() && run_rows.is_empty() {
            div()
                .flex()
                .items_center()
                .gap_2()
                .p_3()
                .text_color(theme.text_dim)
                .child(icon_el(Icon::CircleCheck, px(15.), theme.success.into()))
                .child("No problems detected")
                .into_any_element()
        } else {
            div()
                .id("problems-list")
                .flex_1()
                .overflow_y_scroll()
                .py_1()
                .children(file_title)
                .children(diagnostics.into_iter().zip(positions).enumerate().map(|(ix, (d, (line, col)))| {
                    let (icon, color) = severity_icon(d.severity, &theme);
                    let target = d.range.start;
                    let editor = editor.clone();
                    // One row per problem: a multi-line message (Pyright's
                    // details) reads as one line, like the inline hint.
                    let message = d.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
                    row(SharedString::from(format!("problem-{ix}")), icon, color, message)
                        .on_click(move |_, window, cx| {
                            if let Some(editor) = editor.as_ref() {
                                editor.update(cx, |e, cx| e.go_to(target, window, cx));
                            }
                        })
                        .child(div().text_color(theme.text_dim).flex_shrink_0().child(SharedString::from(d.rule.to_string())))
                        .child(div().flex_1())
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(12.))
                                .text_color(theme.text_dim)
                                .child(format!("Ln {line}, Col {col}")),
                        )
                }))
                .children(run_rows)
                .into_any_element()
        };

        island(&theme)
            .h(self.problems_height)
            .flex_shrink_0()
            .child(header)
            .child(list)
            .child(self.resize_handle(Edge::Problems, cx))
    }

    fn on_root_mouse_move(&mut self, event: &MouseMoveEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(resize) = self.resizing else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.resizing = None;
            cx.notify();
            return;
        }
        let viewport = window.viewport_size();
        match resize.edge {
            Edge::Problems => {
                // Keep room for the tab bar and a few editor lines above the panel.
                let max = (viewport.height - px(TITLEBAR_HEIGHT + TAB_HEIGHT + STATUSBAR_HEIGHT + 120.)).max(px(90.));
                self.problems_height = (resize.size + (resize.start - event.position.y)).clamp(px(90.), max);
            }
            Edge::Sidebar => {
                // Leave the editor at least 320pt wide.
                let max = (viewport.width - px(320.)).max(px(160.));
                self.sidebar_width = (resize.size + (event.position.x - resize.start)).clamp(px(160.), max);
            }
            Edge::Pane(ix) if ix + 1 < self.pane_sizes.len() => {
                let bounds = self.panes_bounds.get();
                let (total, delta) = match self.pane_axis {
                    PaneAxis::Row => (bounds.size.width, event.position.x - resize.start),
                    PaneAxis::Column => (bounds.size.height, event.position.y - resize.start),
                };
                if total > px(0.) {
                    let pair = self.pane_drag_pair;
                    // Every pane keeps at least a tenth of the space.
                    let share = ((resize.size + delta) / total).clamp(0.1, (pair - 0.1).max(0.1));
                    self.pane_sizes[ix] = share;
                    self.pane_sizes[ix + 1] = pair - share;
                }
            }
            Edge::Pane(_) => {}
        }
        cx.notify();
    }

    /// Forgets where a drag would drop. Called when a drag ends any way at
    /// all, since GPUI doesn't redraw by itself when one is cancelled.
    fn clear_drag_state(&mut self, cx: &mut Context<Self>) {
        // The drop overlays are drag_over styles: they only need a redraw.
        self.tree_drop = None;
        cx.notify();
    }

    fn on_root_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.tree_drop.is_some() {
            self.clear_drag_state(cx);
        }
        if self.resizing.take().is_some() {
            self.save_state();
            self.schedule_session_save(cx);
            cx.notify();
        }
    }

    /// The active file's language server: its name, what it's busy with, or
    /// what's wrong (click to copy the install command or to restart).
    fn render_lsp_status(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let path = self.active_editor()?.read(cx).doc.path.clone()?;
        let status = self.lsp.read(cx).status(&path)?;
        let dim: Hsla = theme.text_dim.into();
        let (label, color, tip): (String, Hsla, Option<String>) = match &status {
            LspStatus::Starting => ("Starting language server…".into(), dim, None),
            LspStatus::Busy(work) => {
                let mut short = work.clone();
                if short.chars().count() > 48 {
                    short = short.chars().take(47).collect::<String>() + "…";
                }
                (short, dim, Some(work.clone()))
            }
            LspStatus::Ready(name) => (name.clone(), dim, None),
            LspStatus::Missing(install) => {
                ("No language server".into(), theme.warning.into(), Some(format!("Install one with `{install}`. Click to copy.")))
            }
            LspStatus::Failed(message) => ("Language server stopped".into(), theme.error.into(), Some(format!("{message}. Click to restart."))),
        };
        // A small light: green when ready, the accent while working.
        let light: Hsla = match &status {
            LspStatus::Ready(_) => theme.success.into(),
            LspStatus::Starting | LspStatus::Busy(_) => theme.accent.into(),
            LspStatus::Missing(_) => theme.warning.into(),
            LspStatus::Failed(_) => theme.error.into(),
        };
        Some(
            div()
                .id("lsp-status")
                .flex()
                .items_center()
                .gap_1p5()
                .text_color(color)
                .child(
                    div().size(px(6.)).rounded_full().bg(light).shadow(vec![gpui::BoxShadow {
                        color: light.alpha(0.6),
                        offset: point(px(0.), px(0.)),
                        blur_radius: px(4.),
                        spread_radius: px(0.),
                    }]),
                )
                .when(matches!(status, LspStatus::Missing(_) | LspStatus::Failed(_)), |el| el.cursor_pointer())
                .child(label)
                .when_some(tip, |el, tip| el.tooltip(move |_, cx| tooltip_view(tip.clone(), cx)))
                .on_click(cx.listener(move |ws, _, window, cx| match &status {
                    LspStatus::Missing(install) => {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(install.to_string()));
                        ws.toast(
                            ToastKind::Success,
                            "Install command copied",
                            Some(format!("Run `{install}` in a terminal, then restart the language server.")),
                            cx,
                        );
                    }
                    LspStatus::Failed(_) => ws.restart_language_server(&RestartLanguageServer, window, cx),
                    _ => {}
                }))
                .into_any_element(),
        )
    }

    fn render_statusbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let lsp_item = self.render_lsp_status(&theme, cx);
        let active_editor = self.active_editor().cloned();
        let editor = self.active_editor().map(|e| e.read(cx));
        // The file's problems and the last Run's.
        let run_problems = self.run_problems_shown(cx);
        let count = |sev: Severity| {
            editor.map_or(0, |e| e.doc.diagnostics.iter().filter(|d| d.severity == sev).count())
                + run_problems.iter().filter(|p| p.severity == sev).count()
        };
        let (errors, warnings, infos) = (count(Severity::Error), count(Severity::Warning), count(Severity::Info));

        let item = |child: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .h_full()
                .px_2()
                .child(child)
        };
        let counter = |icon: Icon, n: usize, color: Hsla| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .when(n > 0, |el| el.text_color(color))
                .child(icon_el(icon, px(13.), if n > 0 { color } else { theme.text_dim.into() }))
                .child(n.to_string())
        };

        let mut right: Vec<AnyElement> = Vec::new();
        // An image tab has no caret, indentation or line endings.
        if let Some(e) = editor.filter(|e| e.doc.image.is_none()) {
            if let Some(mode) = e.vim_status() {
                right.push(
                    item(div().font_family(theme.mono_font.clone()).text_size(px(11.)).text_color(theme.text).child(mode).into_any_element())
                        .into_any_element(),
                );
            }
            let (line, col) = e.cursor_position();
            let selected = e.selected_chars();
            let cursors = e.selections().len();
            let mut pos = format!("Ln {line}, Col {col}");
            if cursors > 1 {
                pos.push_str(&format!("  ({cursors} cursors)"));
            } else if selected > 0 {
                pos.push_str(&format!("  ({selected} selected)"));
            }
            right.push(
                item(div().font_family(theme.mono_font.clone()).text_size(px(11.)).text_color(theme.text_muted).child(pos).into_any_element())
                    .into_any_element(),
            );
            if let Some((name, error)) = e.external_linter_status() {
                let (icon, color, label) = match error {
                    None => (Icon::CircleCheck, Hsla::from(theme.success), name.to_string()),
                    Some(_) => (Icon::TriangleAlert, Hsla::from(theme.warning), format!("{name} failed")),
                };
                let detail = error.map(|e| e.to_string());
                right.push(
                    item(
                        div()
                            .id("external-linter")
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(icon_el(icon, px(13.), color))
                            .child(label)
                            .when_some(detail, |el, detail| {
                                el.tooltip(move |_, cx| tooltip_view(detail.clone(), cx))
                            })
                            .into_any_element(),
                    )
                    .into_any_element(),
                );
            }
            if let Some(lsp_item) = lsp_item {
                right.push(item(lsp_item).into_any_element());
            }
            right.push(item(e.indent_label().into_any_element()).into_any_element());
            let menu_item = |id: &'static str, label: String, tip: &'static str| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    // Same size and rounding as the docks and the problem counts.
                    .h(px(24.))
                    .px_2()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover).text_color(theme.text))
                    .tooltip(move |_, cx| tooltip_view(tip, cx))
                    .child(label)
            };
            let tab = active_editor.clone();
            right.push(
                menu_item("line-endings", if e.doc.uses_crlf { "CRLF".into() } else { "LF".into() }, "Line Endings")
                    .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                        if let Some(editor) = tab.clone() {
                            ws.open_line_endings_menu(editor, event.position(), window, cx);
                        }
                    }))
                    .into_any_element(),
            );
            let tab = active_editor.clone();
            right.push(
                menu_item("encoding", e.doc.encoding.label(), "File Encoding")
                    .on_click(cx.listener(move |ws, event: &ClickEvent, window, cx| {
                        if let Some(editor) = tab.clone() {
                            ws.open_encoding_menu(editor, event.position(), window, cx);
                        }
                    }))
                    .into_any_element(),
            );
            if e.doc.language == Language::Python
                && self.root.is_some()
                && let Some(dir) = e.doc.path.as_deref().and_then(Path::parent)
            {
                let label = match self.shown_venv(dir) {
                    Some(venv) => crate::python::label(&venv, self.root.as_deref()),
                    None => "No venv".into(),
                };
                right.push(
                    menu_item("python-venv", label, "Python Environment")
                        .on_click(cx.listener(|ws, event: &ClickEvent, window, cx| ws.open_venv_menu(event.position(), window, cx)))
                        .into_any_element(),
                );
            }
            let (icon, color) = match e.doc.language {
                Language::Html => (Icon::CodeXml, Hsla::from(theme.text_muted)),
                Language::Markdown => (Icon::FileText, Hsla::from(theme.text_muted)),
                Language::PlainText => (Icon::FileText, theme.text_dim.into()),
                _ => (Icon::Braces, Hsla::from(theme.text_muted)),
            };
            right.push(
                item(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(icon_el(icon, px(13.), color))
                        .child(e.doc.language.name())
                        .into_any_element(),
                )
                .into_any_element(),
            );
        }

        // Panel toggles sit in two small docks, like segmented controls.
        let dock = || dock(&theme);
        let toggle = |button: Stateful<gpui::Div>, on: bool| {
            button.size(px(20.)).rounded(px(7.)).when(on, |el| segment_on(el, &theme))
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .h(px(STATUSBAR_HEIGHT))
            .w_full()
            .px(px(GAP))
            .text_size(px(11.5))
            .text_color(theme.text_dim)
            // Shows or hides the sidebar; its Files / Commit switch is inside it.
            .child(
                dock().child(
                    toggle(
                        self.icon_button("toggle-sidebar", Icon::PanelLeft, self.show_sidebar, cx, |ws, _, window, cx| {
                            ws.set_sidebar_visible(!ws.show_sidebar, window, cx)
                        }),
                        self.show_sidebar,
                    )
                    .tooltip(|_, cx| tooltip_view("Toggle Sidebar", cx)),
                ),
            )
            .child(
                div()
                    .id("diagnostic-counts")
                    .flex()
                    .items_center()
                    .gap_2()
                    // Same size and rounding as the docks beside it.
                    .h(px(24.))
                    .px_2()
                    .rounded(px(9.))
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.hover).text_color(theme.text_muted))
                    .on_click(cx.listener(|ws, _, window, cx| ws.toggle_problems(&ToggleProblems, window, cx)))
                    .child(counter(Icon::CircleX, errors, theme.error.into()))
                    .child(counter(Icon::TriangleAlert, warnings, theme.warning.into()))
                    .when(infos > 0, |el| el.child(counter(Icon::CircleAlert, infos, theme.info.into()))),
            )
            .when_some(self.git_busy, |el, busy| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .text_color(theme.text_muted)
                        .child(icon_el(Icon::Refresh, px(12.), theme.accent.into()))
                        .child(busy),
                )
            })
            .child(div().flex_1())
            .when_some(updater::pending_version(cx).map(|v| format!("Kin {v} is downloaded")), |el, tip| {
                el.child(
                    div()
                        .id("restart-to-update")
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(24.))
                        .px_2()
                        .rounded(px(9.))
                        .cursor_pointer()
                        .text_color(theme.accent)
                        .bg(theme.accent_soft)
                        .hover(|s| s.bg(Hsla::from(theme.accent).alpha(0.24)))
                        .on_click(cx.listener(|ws, _, _, cx| ws.restart_to_update(cx)))
                        .child(icon_el(Icon::Download, px(13.), theme.accent.into()))
                        .child("Restart to Update")
                        .tooltip(move |_, cx| tooltip_view(tip.clone(), cx)),
                )
            })
            .children(right)
            .child(
                dock()
                    .ml_1()
                    .when(self.repo.is_some(), |el| {
                        el.child(
                            toggle(
                                self.icon_button("toggle-git-log", Icon::GitCommit, self.show_git_log, cx, |ws, _, window, cx| {
                                    ws.toggle_git_log(&ToggleGitLog, window, cx)
                                }),
                                self.show_git_log,
                            )
                            .tooltip(|window, cx| status_tooltip("Git Log", &ToggleGitLog, window, cx)),
                        )
                    })
                    .when(self.debug.is_some(), |el| {
                        el.child(
                            toggle(
                                self.icon_button("toggle-debug", Icon::Bug, self.show_debug, cx, |ws, _, window, cx| {
                                    ws.toggle_debug_panel(&ToggleDebugPanel, window, cx)
                                }),
                                self.show_debug,
                            )
                            .tooltip(|window, cx| status_tooltip("Debug", &ToggleDebugPanel, window, cx)),
                        )
                    })
                    .child(
                        toggle(
                            self.icon_button("toggle-terminal", Icon::Terminal, self.show_terminal, cx, |ws, _, window, cx| {
                                ws.toggle_terminal(&ToggleTerminal, window, cx)
                            }),
                            self.show_terminal,
                        )
                        .tooltip(|window, cx| status_tooltip("Terminal", &ToggleTerminal, window, cx)),
                    )
                    .child(
                        toggle(
                            self.icon_button("toggle-problems", Icon::PanelBottom, self.show_problems, cx, |ws, _, window, cx| {
                                ws.toggle_problems(&ToggleProblems, window, cx)
                            }),
                            self.show_problems,
                        )
                        .tooltip(|window, cx| status_tooltip("Toggle Problems", &ToggleProblems, window, cx)),
                    ),
            )
            // Windows and Linux have no app menu, so Settings needs a way in.
            .child({
                let on = matches!(self.modal, Some(Modal::Preferences(_)));
                toggle(self.icon_button("open-settings", Icon::Settings, on, cx, |ws, _, window, cx| ws.open_settings(&OpenSettings, window, cx)), on)
                    .size(px(24.))
                    .tooltip(|window, cx| status_tooltip("Settings", &OpenSettings, window, cx))
            })
    }

    fn update_window_title(&mut self, window: &mut Window, cx: &App) {
        let mut title = match self.active_editor() {
            Some(e) => {
                let doc = &e.read(cx).doc;
                format!("{}{}", doc.file_name(), if doc.is_dirty() { " •" } else { "" })
            }
            None => "Kin".into(),
        };
        if let Some(root) = self.root.as_deref() {
            title.push_str(&format!(" — {}", folder_name(root)));
        }
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_window_title(window, cx);
        // A drag that ended without a drop (Escape, left the window) leaves no highlight.
        if !cx.has_active_drag() {
            self.tree_drop = None;
        }
        let theme = Theme::get(cx).clone();
        let modal = self.render_modal(window, cx);
        let toasts = self.render_toasts(cx);
        let terminal_panel = self.show_terminal.then(|| self.render_terminal_panel(cx).into_any_element());
        let panes = self.render_panes(window, cx).into_any_element();

        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::new_file))
            .on_action(cx.listener(Self::open_file))
            .on_action(cx.listener(Self::open_folder))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::save_as))
            .on_action(cx.listener(Self::close_tab))
            .on_action(cx.listener(Self::next_tab))
            .on_action(cx.listener(Self::prev_tab))
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_action(cx.listener(Self::toggle_problems))
            .on_action(cx.listener(Self::find))
            .on_action(cx.listener(Self::replace))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::find_in_files))
            .on_action(cx.listener(Self::replace_in_files))
            .on_action(cx.listener(Self::search_everywhere))
            .on_action(cx.listener(Self::go_to_file))
            .on_action(cx.listener(Self::recent_files))
            .on_action(cx.listener(Self::find_action))
            .on_action(cx.listener(Self::go_to_line))
            .on_action(cx.listener(Self::file_structure))
            .on_action(cx.listener(Self::open_settings))
            .on_action(cx.listener(Self::open_settings_file))
            .on_action(cx.listener(Self::restart_language_server))
            .on_action(cx.listener(Self::toggle_terminal))
            .on_action(cx.listener(Self::split_right))
            .on_action(cx.listener(Self::rerun))
            .on_action(cx.listener(Self::stop_run))
            .on_action(cx.listener(Self::run_configuration))
            .on_action(cx.listener(Self::split_down))
            .on_action(cx.listener(Self::next_pane))
            .on_action(cx.listener(Self::close_other_tabs))
            .on_action(cx.listener(Self::close_all_tabs))
            .on_action(cx.listener(Self::toggle_pin_tab))
            .on_action(cx.listener(Self::new_terminal))
            .on_action(cx.listener(Self::close_terminal))
            .on_action(cx.listener(Self::open_keymap))
            .on_action(cx.listener(Self::select_theme))
            .on_action(cx.listener(Self::open_themes_folder))
            .on_action(cx.listener(Self::open_snippets_folder))
            .on_action(cx.listener(Self::reload_themes))
            .on_action(cx.listener(Self::show_commit))
            .on_action(cx.listener(Self::toggle_commit))
            .on_action(cx.listener(Self::git_push))
            .on_action(cx.listener(Self::git_pull))
            .on_action(cx.listener(Self::git_fetch))
            .on_action(cx.listener(Self::git_branches))
            .on_action(cx.listener(Self::toggle_git_log))
            .on_action(cx.listener(Self::show_file_history))
            .on_action(cx.listener(Self::git_clone))
            .on_action(cx.listener(Self::git_init))
            .on_action(cx.listener(Self::open_remote))
            .on_action(cx.listener(Self::toggle_auto_save))
            .on_action(cx.listener(Self::check_for_updates))
            .on_action(cx.listener(Self::welcome_setup))
            .on_action(cx.listener(Self::go_to_symbol))
            .on_action(cx.listener(Self::split_terminal))
            .on_action(cx.listener(Self::select_opened_file))
            .on_action(cx.listener(Self::debug_configuration))
            .on_action(cx.listener(Self::attach_to_process))
            .on_action(cx.listener(Self::add_conditional_breakpoint))
            .on_action(cx.listener(Self::add_logpoint))
            .on_action(cx.listener(Self::open_extensions))
            .on_action(cx.listener(Self::project_trust))
            .on_action(cx.listener(Self::vim_command_line))
            .on_action(cx.listener(Self::reload_extensions))
            .on_action(cx.listener(Self::resume))
            .on_action(cx.listener(Self::pause_program))
            .on_action(cx.listener(Self::step_over))
            .on_action(cx.listener(Self::step_into))
            .on_action(cx.listener(Self::step_out))
            .on_action(cx.listener(Self::toggle_debug_panel))
            .on_action(cx.listener(Self::install_shell_command))
            .on_action(cx.listener(Self::new_window))
            .on_action(cx.listener(Self::close_window))
            .on_action(cx.listener(Self::minimize_window))
            .on_action(cx.listener(Self::zoom_window))
            .on_action(cx.listener(Self::open_folder_in_this_window))
            .on_action(cx.listener(Self::recent_projects))
            .on_action(cx.listener(Self::cancel))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .capture_key_down(cx.listener(Self::on_any_key_down))
            .on_mouse_move(cx.listener(Self::on_root_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_root_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_root_mouse_up))
            .on_drop(cx.listener(|ws, paths: &gpui::ExternalPaths, window, cx| ws.open_dropped(paths.paths(), window, cx)))
            .when_some(self.resizing, |el, r| {
                el.cursor(match r.edge {
                    Edge::Problems => CursorStyle::ResizeUpDown,
                    Edge::Sidebar => CursorStyle::ResizeLeftRight,
                    Edge::Pane(_) if self.pane_axis == PaneAxis::Row => CursorStyle::ResizeLeftRight,
                    Edge::Pane(_) => CursorStyle::ResizeUpDown,
                })
            })
            .relative()
            .size_full()
            .flex()
            .flex_col()
            // The base the islands float on, faintly lit with the accent from above.
            .bg(chrome_bg(&theme, 0.45))
            .text_color(theme.text)
            .font_family(theme.ui_font.clone())
            .text_size(px(theme.ui_font_size))
            .child(self.render_titlebar(window, cx))
            .when_some(self.onboarding.as_ref().map(|(o, _)| o.clone()), |el, onboarding| {
                el.child(
                    island(&theme)
                        .flex_1()
                        .min_h_0()
                        .mx(px(GAP))
                        .mb(px(GAP))
                        .child(div().size_full().overflow_hidden().child(onboarding)),
                )
            })
            .when(self.onboarding.is_none(), |el| el.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .px(px(GAP))
                    .gap(px(GAP))
                    .when(self.show_sidebar, |el| el.child(self.render_sidebar(cx)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(GAP))
                            .child(div().flex_1().min_h_0().child(panes))
                            .when(self.show_problems, |el| el.child(self.render_problems(cx)))
                            .when(self.show_terminal, |el| el.child(terminal_panel.unwrap()))
                            .when(self.show_git_log, |el| el.child(self.render_git_log_panel(cx)))
                            .when(self.show_debug, |el| el.children(self.render_debug_panel(cx))),
                    ),
            )
            .child(self.render_statusbar(cx)))
            .children(toasts)
            .children(self.render_context_menu(cx))
            .children(modal)
            .child({
                // A Finder drag that leaves the window or is cancelled only sends
                // FileDropEvent::Exited; listen for it to drop the highlights.
                let this = cx.entity().downgrade();
                gpui::canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        let this = this.clone();
                        window.on_mouse_event(move |event: &gpui::FileDropEvent, _, _, cx| {
                            if matches!(event, gpui::FileDropEvent::Exited) {
                                this.update(cx, |ws, cx| ws.clear_drag_state(cx)).ok();
                            }
                        });
                    },
                )
                .absolute()
                .size_0()
            })
    }
}

use crate::ui::{
    GAP, ISLAND_INSET, chrome_bg, dock, soft_shadow, file_icon, folder_icon, island, popover, segment_on, selected_edge,
};

/// Copies a file or folder into `dir`, as "name copy" when the name is
/// taken. Returns where it went.
fn copy_into(source: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let name = source.file_name().ok_or_else(|| std::io::Error::other("no file name"))?;
    let mut target = dir.join(name);
    if target.exists() {
        let path = Path::new(name);
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let ext = path.extension().filter(|_| !source.is_dir()).map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
        let stem = if source.is_dir() { name.to_string_lossy().into_owned() } else { stem };
        target = (1..1000)
            .map(|n| dir.join(if n == 1 { format!("{stem} copy{ext}") } else { format!("{stem} copy {n}{ext}") }))
            .find(|p| !p.exists())
            .ok_or_else(|| std::io::Error::other("no free name"))?;
    }
    copy_recursive(source, &target)?;
    Ok(target)
}

fn copy_recursive(source: &Path, target: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        std::fs::create_dir(target)?;
        for entry in std::fs::read_dir(source)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &target.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(source, target).map(|_| ())
    }
}

/// The project's .gitignore and .git/info/exclude.
fn project_ignore(root: &Path) -> Option<ignore::gitignore::Gitignore> {
    let mut builder = ignore::gitignore::GitignoreBuilder::new(root);
    builder.add(root.join(".gitignore"));
    builder.add(root.join(".git/info/exclude"));
    builder.build().ok()
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The platform's name for showing a file in the file manager.
fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Open Containing Folder"
    }
}

/// A tooltip naming the action and its current shortcut.
fn status_tooltip(label: &str, action: &dyn gpui::Action, window: &mut Window, cx: &mut App) -> gpui::AnyView {
    let text = match binding_label(action, window) {
        Some(keys) => format!("{label} ({keys})"),
        None => label.to_string(),
    };
    tooltip_view(text, cx)
}

fn severity_icon(severity: Severity, theme: &Theme) -> (Icon, Hsla) {
    match severity {
        Severity::Error => (Icon::CircleX, theme.error.into()),
        Severity::Warning => (Icon::TriangleAlert, theme.warning.into()),
        Severity::Info => (Icon::CircleAlert, theme.info.into()),
    }
}

// ---- Windows ------------------------------------------------------------------------

/// Opens a Kin window and runs `setup` on its workspace. With `near` (the
/// bounds of the window it was opened from) it's cascaded a little down and
/// to the right so it doesn't cover that window exactly.
/// Where modals start: a little below the title bar, lower in tall windows.
fn modal_top(window: &Window) -> Pixels {
    (window.viewport_size().height * 0.12).max(px(TITLEBAR_HEIGHT + 12.))
}

/// The tallest a modal can be and still end above the window's bottom edge.
pub fn modal_max_height(window: &Window) -> Pixels {
    (window.viewport_size().height - modal_top(window) - px(16.)).max(px(120.))
}

pub fn open_window(
    near: Option<Bounds<Pixels>>,
    cx: &mut App,
    setup: impl FnOnce(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
) -> Option<WindowHandle<Workspace>> {
    let (bounds, display_id) = match near {
        Some(b) => (WindowBounds::Windowed(Bounds { origin: b.origin + point(px(28.), px(28.)), size: b.size }), None),
        None => saved_placement(cx)
            .unwrap_or_else(|| (WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx)), None)),
    };
    let options = WindowOptions {
        window_bounds: Some(bounds),
        display_id,
        titlebar: Some(TitlebarOptions {
            title: Some("Kin".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(16.), px(13.))),
        }),
        window_min_size: Some(size(px(480.), px(320.))),
        app_id: Some("dev.kin.editor".into()),
        ..Default::default()
    };
    let window = match cx.open_window(options, |window, cx| cx.new(|cx| Workspace::new(window, cx))) {
        Ok(window) => window,
        Err(err) => {
            eprintln!("couldn't open a window: {err}");
            return None;
        }
    };
    window
        .update(cx, |ws, window, cx| {
            setup(ws, window, cx);
            ws.window_ready(window, cx);
        })
        .ok();
    Some(window)
}

/// Where the last window was, if it was saved: on the same display when it's
/// still connected (kept fully on screen), else centered with the same size.
fn saved_placement(cx: &App) -> Option<(WindowBounds, Option<gpui::DisplayId>)> {
    let saved = SessionState::load().window?;
    let display = saved.display.as_ref().and_then(|uuid| {
        cx.displays().into_iter().find(|d| d.uuid().ok().is_some_and(|u| u.to_string() == *uuid))
    });
    let mut size = size(px(saved.width.max(480.)), px(saved.height.max(320.)));
    let bounds = match &display {
        Some(display) => {
            let area = display.bounds().size;
            size = size.min(&area);
            let x = saved.x.clamp(0., f32::from(area.width - size.width));
            let y = saved.y.clamp(0., f32::from(area.height - size.height));
            Bounds { origin: point(px(x), px(y)), size }
        }
        None => Bounds::centered(None, size, cx),
    };
    let bounds = if saved.fullscreen { WindowBounds::Fullscreen(bounds) } else { WindowBounds::Windowed(bounds) };
    Some((bounds, display.map(|d| d.id())))
}

/// The window that has `root` open as its project. The window that's
/// currently being updated can't be read, so callers check their own.
fn window_with_project(root: &Path, cx: &App) -> Option<WindowHandle<Workspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|w| w.downcast::<Workspace>())
        .find(|w| w.read(cx).is_ok_and(|ws| ws.root.as_deref() == Some(root)))
}

/// Quits once every window has saved or discarded its edits. Windows with
/// unsaved changes ask in turn; their answer continues the quit. Must run
/// outside any window update (use `cx.defer`).
pub fn quit(cx: &mut App) {
    for handle in cx.windows() {
        let Some(workspace) = handle.downcast::<Workspace>() else { continue };
        let ready = workspace
            .update(cx, |ws, window, cx| {
                let ready = ws.prepare_to_close(CloseIntent::Quit, window, cx);
                if !ready {
                    window.activate_window();
                }
                ready
            })
            .unwrap_or(true);
        if !ready {
            return;
        }
    }
    cx.quit();
}

/// The user cancelled quitting, so every window guards its edits again.
fn cancel_quit(cx: &mut App) {
    for handle in cx.windows() {
        if let Some(workspace) = handle.downcast::<Workspace>() {
            workspace.update(cx, |ws, _, _| ws.close_confirmed = false).ok();
        }
    }
}

fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A small all-caps heading above a welcome-screen section.
fn section_label(text: &'static str, theme: &Theme) -> impl IntoElement {
    div()
        .px_1()
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_dim)
        .child(text.to_uppercase())
}

/// A rounded panel grouping welcome-screen rows.
fn card(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(12.))
        .bg(theme.panel)
        .border_1()
        .border_color(theme.border_subtle)
        .overflow_hidden()
}

/// The line of light under the current tab: the accent, fading out to both ends.
fn tab_glow(theme: &Theme) -> impl IntoElement {
    let accent: Hsla = theme.accent.into();
    let clear = accent.alpha(0.);
    div()
        .absolute()
        .bottom(px(-1.))
        .left(px(6.))
        .right(px(6.))
        .h(px(2.))
        .flex()
        .child(div().flex_1().h_full().bg(gpui::linear_gradient(90., gpui::linear_color_stop(clear, 0.), gpui::linear_color_stop(accent, 1.))))
        .child(div().flex_1().h_full().bg(gpui::linear_gradient(90., gpui::linear_color_stop(accent, 0.), gpui::linear_color_stop(clear, 1.))))
}

/// Search Everywhere's shortcut (double Shift works in every keymap).
fn double_shift_label() -> &'static str {
    if cfg!(target_os = "macos") { "⇧⇧" } else { "Shift Shift" }
}

/// A compact action row: icon, label and the action's shortcut.
fn welcome_row(
    id: &'static str,
    icon: Icon,
    label: &'static str,
    keys: Option<String>,
    theme: &Theme,
    cx: &App,
) -> Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap_2p5()
        .h(px(32.))
        .px_2p5()
        .rounded(px(8.))
        .cursor_pointer()
        .text_color(theme.text_muted)
        .hover(|s| s.bg(theme.hover).text_color(theme.text))
        .active(|s| s.bg(theme.active))
        .child(icon_el(icon, px(15.), theme.text_dim.into()))
        .child(div().flex_1().min_w_0().truncate().child(label))
        .when_some(keys, |el, k| el.child(kbd(k, cx)))
}

/// An on/off pill like a native switch.
/// A stable tint per project name for its monogram.
fn monogram_color(name: &str, theme: &Theme) -> Hsla {
    let palette = [theme.accent, theme.success, theme.info, theme.warning, theme.syntax.function, theme.syntax.keyword, theme.git_added, theme.git_modified];
    let hash = name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    palette[hash as usize % palette.len()].into()
}


/// Runs one Git operation; blocking. Returns a message for a success toast.
fn perform_git(dir: &Path, op: GitOp, status: &RepoStatus) -> Result<Option<String>, String> {
    let push = |dir: &Path, force: bool| -> Result<String, String> {
        if !git::has_remote(dir) {
            return Err("No remote is configured. Add one with: git remote add origin <url>".into());
        }
        let branch = git::status(dir).ok().and_then(|s| s.branch).or_else(|| status.branch.clone());
        let has_upstream = git::status(dir).ok().and_then(|s| s.upstream).is_some();
        let verb = if force { "Force-pushed" } else { "Pushed" };
        match (has_upstream, branch) {
            (false, Some(branch)) => git::push(dir, Some(&branch), force).map(|_| format!("{verb} {branch} to origin")),
            _ => git::push(dir, None, force).map(|_| verb.to_string()),
        }
    };
    match op {
        GitOp::Stage(rels) => git::stage(dir, &rels).map(|_| None),
        GitOp::Unstage(rels) => git::unstage(dir, &rels, status.head_oid.is_some()).map(|_| None),
        GitOp::StageLines { rel, text, rows } => git::stage_lines(dir, &rel, &text, rows).map(|_| None),
        GitOp::UnstageLines { rel, text, rows } => git::unstage_lines(dir, &rel, text.as_deref(), rows).map(|_| None),
        GitOp::Merge(branch) => git::merge(dir, &branch).map(|out| {
            Some(if out == "CONFLICT" {
                "Merge stopped on conflicts".into()
            } else if out.contains("Already up to date") {
                "Already up to date".into()
            } else {
                format!("Merged {branch}")
            })
        }),
        GitOp::Rebase(onto) => git::rebase(dir, &onto).map(|out| {
            Some(if out == "CONFLICT" { "Rebase stopped on conflicts".into() } else { format!("Rebased onto {onto}") })
        }),
        GitOp::Abort(state) => git::abort(dir, state).map(|_| Some(format!("{} aborted", state.noun()))),
        GitOp::Continue(state) => git::continue_op(dir, state).map(|_| {
            Some(match git::repo_state(dir) {
                // A rebase can stop again on the next commit.
                Some(_) => "Stopped on more conflicts".into(),
                None => format!("{} finished", state.noun()),
            })
        }),
        GitOp::Discard(files) => {
            let n = files.len();
            git::discard(dir, &files).map(|_| Some(format!("Rolled back {n} file{}", if n == 1 { "" } else { "s" })))
        }
        GitOp::Commit { message, amend, all, push: and_push } => {
            if all {
                git::run(dir, &["add", "--all"])?;
            }
            let summary = git::commit(dir, &message, amend)?;
            if and_push {
                push(dir, false)?;
                Ok(Some(format!("Committed and pushed {summary}")))
            } else {
                Ok(Some(format!("Committed {summary}")))
            }
        }
        // `Push` opens the dialog before it gets here; treat it as a plain push.
        GitOp::Push | GitOp::PushNow { force: false } => push(dir, false).map(Some),
        GitOp::PushNow { force: true } => push(dir, true).map(Some),
        GitOp::Pull => git::pull(dir).map(|out| {
            Some(if out.contains("Already up to date") { "Already up to date".into() } else { "Project updated".into() })
        }),
        GitOp::Fetch => git::fetch(dir).map(|_| Some("Fetched all remotes".into())),
        GitOp::Checkout(branch) => git::checkout(dir, &branch).map(|_| Some(format!("Switched to {}", branch.name))),
        GitOp::CreateBranch(name) => git::create_branch(dir, &name).map(|_| Some(format!("Created and switched to {name}"))),
        GitOp::Init => git::init(dir).map(|_| Some("Initialized a Git repository".into())),
    }
}
