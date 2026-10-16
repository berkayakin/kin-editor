//! The integrated terminal: a shell on a pseudo-terminal, emulated by
//! `alacritty_terminal` and painted with GPUI like the editor. Keys go to
//! the shell (the keymap turns off app shortcuts in the "Terminal"
//! context); text arrives through the input handler, so dead keys and IMEs
//! work. Find (⌘F) searches the scrollback and the screen.

use std::{
    borrow::Cow,
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alacritty_terminal::{
    event::{Event as TermEvent, EventListener, Notify, OnResize, WindowSize},
    event_loop::{EventLoop, Msg, Notifier},
    grid::{Dimensions, Scroll},
    index::{Column, Direction, Line, Point as GridPoint, Side},
    selection::{Selection, SelectionType},
    sync::FairMutex,
    term::{
        Config, Term, TermMode,
        cell::Flags,
        search::{Match, RegexIter, RegexSearch},
        test::TermSize,
    },
    tty,
    vte::ansi::{Color, CursorShape, NamedColor, Rgb},
};
use futures::{StreamExt, channel::mpsc};
use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable, FontStyle, FontWeight, GlobalElementId,
    Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, KeyDownEvent, LayoutId, Modifiers, ModifiersChangedEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render,
    ScrollWheelEvent, ShapedLine, SharedString, Style, Subscription, Task, TextRun, UTF16Selection, UnderlineStyle,
    Window, actions, div, fill, font, point, prelude::*, px, relative, size,
};

use crate::{
    assets::Icon,
    editor::{Cancel, Editor, EditorEvent, MoveDown, MoveUp, Newline, NewlineBelow},
    run::RunTarget,
    settings::Settings,
    theme::Theme,
    ui::{HoverFill, icon_el, input_box, tooltip_view},
};

actions!(terminal, [TerminalCopy, TerminalPaste, TerminalClear, TerminalFind]);

/// Matches beyond this aren't collected (a huge scrollback full of hits):
/// the newest ones are kept.
const MAX_MATCHES: usize = 5000;

/// Find in the output: the query field and its matches.
struct Search {
    input: Entity<Editor>,
    /// In grid coordinates as of `history`, top to bottom.
    matches: Vec<Match>,
    /// Scrollback lines when `matches` were found: new output moves lines up.
    history: usize,
    current: Option<usize>,
    /// Output arrived since `matches` were found.
    stale: bool,
    /// A search running in the background (see `schedule_search`).
    task: Option<Task<()>>,
    _subscription: Subscription,
}

/// Forwards the emulator's events to the view's task.
#[derive(Clone)]
struct Listener(mpsc::UnboundedSender<TermEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let _ = self.0.unbounded_send(event);
    }
}

pub enum TerminalEvent {
    TitleChanged,
    /// The shell exited; the tab should close.
    Exited,
    /// ⌘/Ctrl-click on a `path:line:col` in the output (1-based line and column).
    OpenFile { path: PathBuf, line: Option<u32>, column: Option<u32> },
}

/// Something clickable in the output.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    Url(String),
    File { path: PathBuf, line: Option<u32>, column: Option<u32> },
}

pub struct TerminalView {
    term: Arc<FairMutex<Term<Listener>>>,
    notifier: Notifier,
    title: SharedString,
    /// A name the user gave the tab, shown instead of the shell's title.
    custom_title: Option<SharedString>,
    /// The shell's process, to find the folder it's in now.
    pid: Option<u32>,
    focus_handle: FocusHandle,
    /// Columns, rows and cell size last sent to the shell.
    size: Option<(u16, u16, Pixels, Pixels)>,
    /// Geometry from the last frame, for mouse hit-testing.
    origin: Point<Pixels>,
    cell: (Pixels, Pixels),
    selecting: bool,
    /// Remainder of fractional wheel scrolling, in lines.
    scroll_rest: f32,
    /// Set for a Run tab: what runs, and its exit code once it's done.
    run: Option<(RunTarget, RunState)>,
    /// Where the shell started, for relative paths in its output.
    cwd: PathBuf,
    /// The link under the pointer while ⌘/Ctrl is held: screen row, columns.
    link: Option<(usize, Range<usize>, Link)>,
    last_mouse: Option<Point<Pixels>>,
    search: Option<Search>,
    _events: Task<()>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    Running,
    /// Exit code (None when it was killed by a signal).
    Exited(Option<i32>),
}

impl EventEmitter<TerminalEvent> for TerminalView {}

impl TerminalView {
    /// Starts a shell in `cwd`, or with `run`, a command in the user's shell
    /// that stays on screen with its exit code when done.
    pub fn create(cwd: PathBuf, run: Option<RunTarget>, cx: &mut App) -> std::io::Result<Entity<Self>> {
        let configured = Settings::get(cx).terminal_shell.clone();
        let venv = crate::python::venv(&cwd);
        let shell = match &run {
            Some(target) => {
                // A login, interactive shell so PATH matches the user's terminal.
                let program = configured
                    .as_ref()
                    .and_then(|c| c.first().cloned())
                    .or_else(|| std::env::var("SHELL").ok())
                    .unwrap_or_else(|| "/bin/sh".into());
                // After the profile, which may reorder PATH.
                let setup = venv.as_deref().map(crate::python::exports).unwrap_or_default();
                Some(vec![program, "-l".into(), "-i".into(), "-c".into(), setup + &run_script(&target.command)])
            }
            None => configured.clone(),
        };
        // A new shell activates the project's venv, typed in like a user would:
        // a login profile can reorder PATH set beforehand.
        // The activate script is the project's own shell script.
        let activate = match (&run, &venv) {
            (None, Some(venv)) if crate::trust::is_trusted(&cwd) => {
                let program = configured.as_ref().and_then(|c| c.first().cloned()).or_else(|| std::env::var("SHELL").ok());
                let program = program.unwrap_or_else(|| if cfg!(windows) { "powershell".into() } else { "sh".into() });
                crate::python::activate_command(venv, &program)
            }
            _ => None,
        };
        let (tx, rx) = mpsc::unbounded();
        let listener = Listener(tx);
        let mut env = HashMap::new();
        env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());
        env.insert("TERM_PROGRAM".to_string(), "Kuana".to_string());
        let shell = shell.and_then(|command| {
            let (program, args) = command.split_first()?;
            Some(tty::Shell::new(program.clone(), args.to_vec()))
        });
        // A Run tab must show everything the program printed before it exited.
        let drain_on_exit = run.is_some();
        // Windows has more options.
        #[allow(clippy::needless_update)]
        let options = tty::Options { shell, working_directory: Some(cwd.clone()), drain_on_exit, env, ..Default::default() };
        let window_size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, window_size, 0)?;
        #[cfg(unix)]
        let pid = Some(pty.child().id());
        #[cfg(not(unix))]
        let pid = None;
        let term = Term::new(Config::default(), &TermSize::new(80, 24), listener.clone());
        let term = Arc::new(FairMutex::new(term));
        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)?;
        let notifier = Notifier(event_loop.channel());
        event_loop.spawn();
        if let Some(line) = activate {
            // The shell reads it once it's ready.
            notifier.notify((line + "\r").into_bytes());
        }
        Ok(cx.new(|cx| {
            let mut view = Self::new(term, notifier, rx, cwd, cx);
            view.pid = pid;
            if let Some(target) = run {
                view.title = target.name.clone().into();
                view.run = Some((target, RunState::Running));
            }
            view
        }))
    }

    /// What a Run tab runs and how it went.
    pub fn run(&self) -> Option<&(RunTarget, RunState)> {
        self.run.as_ref()
    }

    /// Everything on screen and in the scrollback, as text (wrapped lines joined).
    pub fn output_text(&self) -> String {
        let term = self.term.lock();
        let columns = term.columns();
        let mut out = String::new();
        for line in term.topmost_line().0..=term.bottommost_line().0 {
            let row = &term.grid()[Line(line)];
            let text: String = (0..columns)
                .map(|c| &row[Column(c)])
                .filter(|cell| !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER))
                .map(|cell| cell.c)
                .collect();
            if row[Column(columns - 1)].flags.contains(Flags::WRAPLINE) {
                out.push_str(&text);
            } else {
                out.push_str(text.trim_end());
                out.push('\n');
            }
        }
        out
    }

    /// Interrupts the running program (Ctrl+C).
    pub fn interrupt(&mut self, cx: &mut Context<Self>) {
        self.input(&b"\x03"[..], cx);
    }

    fn new(
        term: Arc<FairMutex<Term<Listener>>>,
        notifier: Notifier,
        mut rx: mpsc::UnboundedReceiver<TermEvent>,
        cwd: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let events = cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                // Handle everything that piled up, then redraw once.
                let mut batch = vec![event];
                while let Ok(more) = rx.try_recv() {
                    batch.push(more);
                }
                let alive = this
                    .update(cx, |view, cx| {
                        for event in batch {
                            view.handle_event(event, cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });
        let shell_name = std::env::var("SHELL")
            .ok()
            .and_then(|s| s.rsplit('/').next().map(str::to_string))
            .unwrap_or_else(|| "Terminal".into());
        Self {
            term,
            notifier,
            title: shell_name.into(),
            custom_title: None,
            pid: None,
            focus_handle: cx.focus_handle(),
            size: None,
            origin: Point::default(),
            cell: (px(8.), px(16.)),
            selecting: false,
            scroll_rest: 0.,
            run: None,
            cwd,
            link: None,
            last_mouse: None,
            search: None,
            _events: events,
        }
    }

    pub fn title(&self) -> SharedString {
        self.custom_title.clone().unwrap_or_else(|| self.title.clone())
    }

    /// The name the user gave the tab, if any.
    pub fn custom_title(&self) -> Option<&SharedString> {
        self.custom_title.as_ref()
    }

    /// Renames the tab; None goes back to the shell's own title.
    pub fn set_custom_title(&mut self, title: Option<String>, cx: &mut Context<Self>) {
        self.custom_title = title.filter(|t| !t.trim().is_empty()).map(Into::into);
        cx.emit(TerminalEvent::TitleChanged);
        cx.notify();
    }

    /// The folder the shell is in now (after any `cd`), else where it started.
    pub fn current_dir(&self) -> PathBuf {
        self.pid.and_then(process_cwd).unwrap_or_else(|| self.cwd.clone())
    }

    fn handle_event(&mut self, event: TermEvent, cx: &mut Context<Self>) {
        match event {
            // A Run tab keeps its configuration's name.
            TermEvent::Title(title) if self.run.is_none() => {
                self.title = title.into();
                cx.emit(TerminalEvent::TitleChanged);
            }
            TermEvent::PtyWrite(text) => self.write(text.into_bytes()),
            TermEvent::ClipboardStore(_, text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            TermEvent::ClipboardLoad(_, format) => {
                let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
                self.write(format(&text).into_bytes());
            }
            TermEvent::ColorRequest(index, format) => {
                let theme = Theme::get(cx);
                let color = palette(theme, index).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
                self.write(format(color).into_bytes());
            }
            TermEvent::TextAreaSizeRequest(format) => {
                if let Some((cols, rows, w, h)) = self.size {
                    let size = WindowSize { num_lines: rows, num_cols: cols, cell_width: f32::from(w) as u16, cell_height: f32::from(h) as u16 };
                    self.write(format(size).into_bytes());
                }
            }
            TermEvent::ChildExit(status) => {
                if let Some((_, state)) = self.run.as_mut() {
                    *state = RunState::Exited(status.code());
                }
                cx.emit(TerminalEvent::Exited);
            }
            TermEvent::Exit => {
                if let Some((_, state @ RunState::Running)) = self.run.as_mut() {
                    *state = RunState::Exited(None);
                }
                cx.emit(TerminalEvent::Exited);
            }
            TermEvent::Wakeup => {
                if let Some(search) = self.search.as_mut() {
                    search.stale = true;
                    // While output streams, at most one search at a time.
                    if search.task.is_none() {
                        self.schedule_search(Duration::from_millis(100), false, cx);
                    }
                }
            }
            _ => {}
        }
    }

    fn write(&self, bytes: impl Into<Cow<'static, [u8]>>) {
        self.notifier.notify(bytes);
    }

    /// Sends typed input: back to the live screen, without a selection.
    fn input(&mut self, bytes: impl Into<Cow<'static, [u8]>>, cx: &mut Context<Self>) {
        {
            let mut term = self.term.lock();
            term.scroll_display(Scroll::Bottom);
            term.selection = None;
        }
        self.write(bytes);
        cx.notify();
    }

    fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys typed into Find aren't for the shell.
        if self.search.as_ref().is_some_and(|s| s.input.focus_handle(cx).is_focused(window)) {
            return;
        }
        if let Some(bytes) = escape_for(&event.keystroke, self.mode()) {
            cx.stop_propagation();
            self.input(bytes, cx);
        }
        // Plain text arrives through the input handler.
    }

    fn copy(&mut self, _: &TerminalCopy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.term.lock().selection_to_string().filter(|t| !t.is_empty()) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, _: &TerminalPaste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else { return };
        // Shells expect Enter as \r; bracketed paste lets them tell pasted
        // newlines from typed ones.
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bytes = if self.mode().contains(TermMode::BRACKETED_PASTE) {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text
        };
        self.input(bytes.into_bytes(), cx);
    }

    fn clear(&mut self, _: &TerminalClear, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_all(cx);
    }

    /// Clears the screen and the scrollback.
    pub fn clear_all(&mut self, cx: &mut Context<Self>) {
        // Ctrl+L redraws the prompt at the top; the scrollback goes too.
        self.term.lock().grid_mut().clear_history();
        self.input(&b"\x0c"[..], cx);
    }

    // ---- Find --------------------------------------------------------------------------

    /// Find (⌘F / Ctrl+Shift+F in the terminal): opens the query field, with
    /// the selection as the query.
    fn open_search(&mut self, _: &TerminalFind, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.term.lock().selection_to_string().filter(|t| !t.trim().is_empty() && !t.contains('\n'));
        let input = match &self.search {
            Some(search) => search.input.clone(),
            None => {
                let input = cx.new(|cx| Editor::single_line("Find", cx));
                let subscription = cx.subscribe(&input, |this: &mut Self, _, _: &EditorEvent, cx| {
                    this.schedule_search(Duration::from_millis(30), true, cx);
                });
                self.search = Some(Search {
                    input: input.clone(),
                    matches: Vec::new(),
                    history: 0,
                    current: None,
                    stale: false,
                    task: None,
                    _subscription: subscription,
                });
                input
            }
        };
        if let Some(text) = selected {
            input.update(cx, |e, cx| e.set_text(text.trim(), cx));
        }
        input.update(cx, |e, cx| e.select_all_text(cx));
        window.focus(&input.focus_handle(cx));
        cx.notify();
    }

    fn close_search(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// Finds the query in the scrollback and on screen here and now (for
    /// stepping through matches, which mustn't use old ones).
    fn find_matches(&mut self, cx: &App) {
        let Some(search) = self.search.as_mut() else { return };
        let query = search.input.read(cx).text(cx);
        search.task = None;
        search.stale = false;
        let (matches, history) = find_in(&self.term.lock(), &query);
        self.take_matches(matches, history);
    }

    /// Searches in the background after `delay`, scanning the whole
    /// scrollback with the terminal locked there, not while drawing. A new
    /// query (`jump`) shows the newest match.
    fn schedule_search(&mut self, delay: Duration, jump: bool, cx: &mut Context<Self>) {
        let Some(search) = self.search.as_mut() else { return };
        let input = search.input.clone();
        let term = self.term.clone();
        search.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let Ok(query) = this.update(cx, |this, cx| {
                // Output from here on is caught by the next search.
                if let Some(search) = this.search.as_mut() {
                    search.stale = false;
                }
                input.read(cx).text(cx)
            }) else {
                return;
            };
            let (matches, history) = cx.background_executor().spawn(async move { find_in(&term.lock(), &query) }).await;
            this.update(cx, |this, cx| {
                let Some(search) = this.search.as_mut() else { return };
                // This task is running; let it finish instead of dropping it.
                if let Some(task) = search.task.take() {
                    task.detach();
                }
                let again = search.stale;
                this.take_matches(matches, history);
                if jump && let Some(search) = this.search.as_mut() {
                    // The newest output first: the bottom match.
                    search.current = search.matches.len().checked_sub(1);
                    this.reveal_match();
                }
                if again {
                    this.schedule_search(Duration::from_millis(100), false, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// New matches, keeping the current one where it was: lines move up as
    /// output scrolls, but their place from the top of the history doesn't.
    fn take_matches(&mut self, matches: Vec<Match>, history: usize) {
        let Some(search) = self.search.as_mut() else { return };
        let from_top = |m: &Match, history: usize| (m.start().line.0 + history as i32, m.start().column.0);
        let current = search.current.and_then(|i| search.matches.get(i)).map(|m| from_top(m, search.history));
        search.matches = matches;
        search.history = history;
        search.current = current.and_then(|at| search.matches.iter().position(|m| from_top(m, history) == at));
    }

    /// ↵ / ↑ goes to older output, ⇧↵ / ↓ to newer, wrapping around.
    fn step_match(&mut self, older: bool, cx: &mut Context<Self>) {
        if self.search.as_ref().is_some_and(|s| s.stale) {
            self.find_matches(cx);
        }
        let Some(search) = self.search.as_mut() else { return };
        let len = search.matches.len();
        if len == 0 {
            return;
        }
        search.current = Some(match (search.current, older) {
            (None, _) => len - 1,
            (Some(i), true) => (i + len - 1) % len,
            (Some(i), false) => (i + 1) % len,
        });
        self.reveal_match();
        cx.notify();
    }

    /// Scrolls the current match into view, centered when it was off screen.
    fn reveal_match(&mut self) {
        let Some(search) = self.search.as_ref() else { return };
        let Some(m) = search.current.and_then(|i| search.matches.get(i)) else { return };
        let mut term = self.term.lock();
        let rows = term.screen_lines() as i32;
        let offset = term.grid().display_offset() as i32;
        let line = m.start().line.0;
        if !(-offset..rows - offset).contains(&line) {
            let target = (rows / 2 - line).clamp(0, term.grid().history_size() as i32);
            term.scroll_display(Scroll::Delta(target - offset));
        }
    }

    fn on_find_older(&mut self, _: &Newline, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(true, cx);
    }

    fn on_find_newer(&mut self, _: &NewlineBelow, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(false, cx);
    }

    fn on_find_up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(true, cx);
    }

    fn on_find_down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        self.step_match(false, cx);
    }

    fn render_search(&self, search: &Search, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::get(cx).clone();
        let count = match (search.current, search.matches.len()) {
            (_, 0) if search.input.read(cx).text(cx).is_empty() => String::new(),
            (_, 0) => "No results".into(),
            (Some(i), n) => format!("{}/{n}{}", i + 1, if n == MAX_MATCHES { "+" } else { "" }),
            (None, n) => n.to_string(),
        };
        let button = |id: &'static str, icon: Icon, tip: &'static str, cx: &mut Context<Self>, f: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            div()
                .id(id)
                .size(px(22.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .hover_bg(theme.hover)
                .tooltip(move |_, cx| tooltip_view(tip, cx))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| f(this, window, cx)))
                .child(icon_el(icon, px(13.), theme.text_muted.into()))
        };
        div()
            .absolute()
            .top_1()
            .right_4()
            .flex()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(px(12.))
            .bg(theme.elevated)
            .border_1()
            .border_color(gpui::Hsla::from(theme.border).alpha(0.85))
            .shadow(crate::ui::soft_shadow(&theme, 2))
            // Clicks here aren't selections in the output.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().w(px(220.)).child(input_box(&search.input, Some(Icon::Search), window, cx)))
            .child(div().min_w(px(64.)).px_1().text_size(px(12.)).text_color(theme.text_dim).child(count))
            .child(button("terminal-find-older", Icon::ChevronUp, "Previous Match (↵)", cx, |this, _, cx| this.step_match(true, cx)))
            .child(button("terminal-find-newer", Icon::ChevronDown, "Next Match (⇧↵)", cx, |this, _, cx| this.step_match(false, cx)))
            .child(button("terminal-find-close", Icon::X, "Close (Esc)", cx, |this, window, cx| this.close_search(&Cancel, window, cx)))
    }

    // ---- Mouse -------------------------------------------------------------------------

    /// The grid cell under a window position, and which half of it.
    fn grid_point(&self, position: Point<Pixels>) -> (GridPoint, Side) {
        let term = self.term.lock();
        let (w, h) = self.cell;
        let x = (position.x - self.origin.x).max(px(0.));
        let y = (position.y - self.origin.y).max(px(0.));
        let col = ((x / w) as usize).min(term.columns().saturating_sub(1));
        let row = ((y / h) as usize).min(term.screen_lines().saturating_sub(1));
        let side = if (x / w).fract() < 0.5 { Side::Left } else { Side::Right };
        let line = Line(row as i32 - term.grid().display_offset() as i32);
        (GridPoint::new(line, Column(col)), side)
    }

    /// Whether a program asked for mouse events (vim, htop…); Shift overrides.
    fn mouse_reporting(&self, modifiers: &Modifiers) -> bool {
        self.mode().intersects(TermMode::MOUSE_MODE) && !modifiers.shift
    }

    fn report_mouse(&mut self, button: u8, position: Point<Pixels>, pressed: bool, modifiers: &Modifiers) {
        let (point, _) = self.grid_point(position);
        let row = point.line.0 + self.term.lock().grid().display_offset() as i32;
        let mut code = button;
        if modifiers.shift {
            code += 4;
        }
        if modifiers.alt {
            code += 8;
        }
        if modifiers.control {
            code += 16;
        }
        let action = if pressed { 'M' } else { 'm' };
        self.write(format!("\x1b[<{code};{};{}{action}", point.column.0 + 1, row + 1).into_bytes());
    }

    /// The URL or existing file at `position` (in screen coordinates).
    fn link_at(&self, position: Point<Pixels>) -> Option<(usize, Range<usize>, Link)> {
        let (point, _) = self.grid_point(position);
        let term = self.term.lock();
        let screen_row = (point.line.0 + term.grid().display_offset() as i32).max(0) as usize;
        let row = &term.grid()[point.line];
        let chars: Vec<char> = (0..term.columns()).map(|c| row[Column(c)].c).collect();
        drop(term);
        let (range, text) = word_at(&chars, point.column.0)?;
        Some((screen_row, range, parse_link(&text, &self.cwd)?))
    }

    fn update_link(&mut self, position: Point<Pixels>, modifiers: &Modifiers, cx: &mut Context<Self>) {
        let link = if modifiers.secondary() { self.link_at(position) } else { None };
        if link != self.link {
            self.link = link;
            cx.notify();
        }
    }

    fn on_modifiers_changed(&mut self, event: &ModifiersChangedEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(position) = self.last_mouse {
            self.update_link(position, &event.modifiers, cx);
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        // ⌘/Ctrl-click opens a link, even in programs that take the mouse.
        if event.modifiers.secondary()
            && let Some((_, _, link)) = self.link_at(event.position)
        {
            // Focus moves away, so this view won't see ⌘ go up: drop the underline now.
            self.link = None;
            cx.notify();
            match link {
                Link::Url(url) => cx.open_url(&url),
                Link::File { path, line, column } => cx.emit(TerminalEvent::OpenFile { path, line, column }),
            }
            return;
        }
        if self.mouse_reporting(&event.modifiers) {
            self.report_mouse(0, event.position, true, &event.modifiers);
            return;
        }
        let (point, side) = self.grid_point(event.position);
        let ty = match event.click_count {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.lock().selection = Some(Selection::new(ty, point, side));
        self.selecting = true;
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.last_mouse = Some(event.position);
        if event.pressed_button.is_none() {
            self.update_link(event.position, &event.modifiers, cx);
        }
        if !self.selecting || event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let (point, side) = self.grid_point(event.position);
        if let Some(selection) = self.term.lock().selection.as_mut() {
            selection.update(point, side);
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.mouse_reporting(&event.modifiers) {
            self.report_mouse(0, event.position, false, &event.modifiers);
            return;
        }
        self.selecting = false;
        // A click without a drag leaves no selection.
        let mut term = self.term.lock();
        if term.selection.as_ref().is_some_and(|s| s.ty == SelectionType::Simple && term.selection_to_string().is_none_or(|t| t.is_empty())) {
            term.selection = None;
        }
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(self.cell.1);
        self.scroll_rest += f32::from(delta.y) / f32::from(self.cell.1);
        let lines = self.scroll_rest.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_rest -= lines as f32;
        let mode = self.mode();
        if self.mouse_reporting(&event.modifiers) {
            let button = if lines > 0 { 64 } else { 65 };
            for _ in 0..lines.unsigned_abs().min(10) {
                self.report_mouse(button, event.position, true, &event.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // Full-screen programs without mouse support (less, man) scroll with arrows.
            let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            self.write(arrow.repeat(lines.unsigned_abs() as usize));
        } else {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    // ---- Layout ------------------------------------------------------------------------

    fn layout(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) -> TerminalFrame {
        let theme = Theme::get(cx).clone();
        let font_size = px(theme.editor_font_size - 1.);
        let mono = font(theme.mono_font.clone());
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&mono);
        let cell_w = text_system.advance(font_id, font_size, 'm').map(|s| s.width).unwrap_or(font_size * 0.6);
        let cell_h = (font_size * 1.35).round();
        let padding = px(8.);
        let inner = Bounds::from_corners(bounds.origin + point(padding, px(4.)), bounds.bottom_right() - point(padding, px(4.)));
        let cols = ((inner.size.width / cell_w).floor() as u16).max(2);
        let rows = ((inner.size.height / cell_h).floor() as u16).max(1);
        if self.size != Some((cols, rows, cell_w, cell_h)) {
            self.size = Some((cols, rows, cell_w, cell_h));
            self.term.lock().resize(TermSize::new(cols as usize, rows as usize));
            self.notifier.on_resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: f32::from(cell_w) as u16,
                cell_height: f32::from(cell_h) as u16,
            });
        }
        self.origin = inner.origin;
        self.cell = (cell_w, cell_h);
        let focused = self.focus_handle.is_focused(window) && window.is_window_active();
        let term = self.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let default_bg: Hsla = theme.background.into();
        let mut frame = TerminalFrame { bounds, line_height: cell_h, ..Default::default() };
        // Find's matches on screen, a row at a time (a match can wrap).
        if let Some(search) = &self.search {
            for (ix, m) in search.matches.iter().enumerate() {
                let (start, end) = (m.start(), m.end());
                if end.line.0 + offset < 0 || start.line.0 + offset >= rows as i32 {
                    continue;
                }
                let color = if search.current == Some(ix) { theme.search_active } else { theme.search_match };
                for line in start.line.0..=end.line.0 {
                    let row = line + offset;
                    if !(0..rows as i32).contains(&row) {
                        continue;
                    }
                    let from = if line == start.line.0 { start.column.0 } else { 0 };
                    let to = if line == end.line.0 { end.column.0 + 1 } else { cols as usize };
                    let x = inner.origin.x + cell_w * from as f32;
                    let y = inner.origin.y + cell_h * row as f32;
                    frame.highlights.push(fill(Bounds::new(point(x, y), size(cell_w * (to - from) as f32, cell_h)), color));
                }
            }
        }
        // Cells per row: (column, char, fg, bg, flags).
        let mut rows_cells: Vec<Vec<(usize, char, Hsla, Hsla, Flags)>> = vec![Vec::new(); rows as usize];
        let cursor = content.cursor;
        let block_cursor = focused && cursor.shape == CursorShape::Block;
        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row >= rows as i32 {
                continue;
            }
            let cell = indexed.cell;
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let mut fg = resolve(&theme, content.colors, cell.fg, cell.flags);
            let mut bg = resolve(&theme, content.colors, cell.bg, Flags::empty());
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if content.selection.is_some_and(|s| s.contains(indexed.point)) {
                bg = theme.selection.into();
            }
            if block_cursor && indexed.point == cursor.point {
                fg = default_bg;
            }
            rows_cells[row as usize].push((indexed.point.column.0, cell.c, fg, bg, cell.flags));
        }

        // The cursor, under the text.
        let cursor_row = cursor.point.line.0 + offset;
        if cursor.shape != CursorShape::Hidden && (0..rows as i32).contains(&cursor_row) {
            let x = inner.origin.x + cell_w * cursor.point.column.0 as f32;
            let y = inner.origin.y + cell_h * cursor_row as f32;
            let color: Hsla = theme.cursor.into();
            let wide = term.grid()[cursor.point].flags.contains(Flags::WIDE_CHAR);
            let width = if wide { cell_w * 2. } else { cell_w };
            let quad = match (focused, cursor.shape) {
                (true, CursorShape::Beam) => fill(Bounds::new(point(x, y), size(px(2.), cell_h)), color),
                (true, CursorShape::Underline) => fill(Bounds::new(point(x, y + cell_h - px(2.)), size(width, px(2.))), color),
                (true, _) => fill(Bounds::new(point(x, y), size(width, cell_h)), color),
                (false, _) => gpui::outline(Bounds::new(point(x, y), size(width, cell_h)), color, gpui::BorderStyle::Solid),
            };
            frame.cursor = Some(quad);
        }
        drop(term);

        for (row, cells) in rows_cells.iter().enumerate() {
            let y = inner.origin.y + cell_h * row as f32;
            // Backgrounds: runs of equal non-default color.
            let mut i = 0;
            while i < cells.len() {
                let (col, _, _, bg, flags) = cells[i];
                let width_of = |f: Flags| if f.contains(Flags::WIDE_CHAR) { 2 } else { 1 };
                let mut end = col + width_of(flags);
                let mut j = i + 1;
                while j < cells.len() && cells[j].3 == bg && cells[j].0 == end {
                    end = cells[j].0 + width_of(cells[j].4);
                    j += 1;
                }
                if bg != default_bg {
                    let x = inner.origin.x + cell_w * col as f32;
                    frame.backgrounds.push(fill(Bounds::new(point(x, y), size(cell_w * (end - col) as f32, cell_h)), bg));
                }
                i = j;
            }
            // Text: runs of equal style, each placed at its column so wide or
            // fallback glyphs can't shift the rest of the row.
            let mut i = 0;
            while i < cells.len() {
                let (col, c, fg, _, flags) = cells[i];
                let mut text = String::new();
                text.push(if c == '\0' { ' ' } else { c });
                let mut j = i + 1;
                let ascii = c.is_ascii() && !flags.contains(Flags::WIDE_CHAR);
                while ascii
                    && j < cells.len()
                    && cells[j].2 == fg
                    && cells[j].4 == flags
                    && cells[j].0 == cells[j - 1].0 + 1
                    && cells[j].1.is_ascii()
                {
                    text.push(if cells[j].1 == '\0' { ' ' } else { cells[j].1 });
                    j += 1;
                }
                if !text.trim().is_empty() && !flags.contains(Flags::HIDDEN) {
                    let mut run_font = mono.clone();
                    if flags.intersects(Flags::BOLD) {
                        run_font.weight = FontWeight::BOLD;
                    }
                    if flags.intersects(Flags::ITALIC) {
                        run_font.style = FontStyle::Italic;
                    }
                    let color = if flags.contains(Flags::DIM) { fg.alpha(fg.a * 0.6) } else { fg };
                    let run = TextRun {
                        len: text.len(),
                        font: run_font,
                        color,
                        background_color: None,
                        underline: flags.intersects(Flags::ALL_UNDERLINES).then(|| UnderlineStyle {
                            thickness: px(1.),
                            color: Some(color),
                            wavy: flags.contains(Flags::UNDERCURL),
                        }),
                        strikethrough: flags.contains(Flags::STRIKEOUT).then(|| gpui::StrikethroughStyle {
                            thickness: px(1.),
                            color: Some(color),
                        }),
                    };
                    let shaped = text_system.shape_line(text.into(), font_size, &[run], None);
                    frame.lines.push((point(inner.origin.x + cell_w * col as f32, y), shaped));
                }
                i = j;
            }
        }
        if let Some((row, cols, _)) = &self.link {
            let y = inner.origin.y + cell_h * (*row + 1) as f32 - px(1.);
            let x = inner.origin.x + cell_w * cols.start as f32;
            frame.link = Some(fill(Bounds::new(point(x, y), size(cell_w * cols.len() as f32, px(1.))), theme.accent));
        }
        frame
    }
}

/// The run of non-space characters around `col`, without the brackets,
/// quotes and punctuation that usually surround a path or URL in output.
fn word_at(chars: &[char], col: usize) -> Option<(Range<usize>, String)> {
    let stop = |c: char| c.is_whitespace() || "\"'`<>()[]{},|".contains(c);
    if chars.get(col).is_none_or(|c| stop(*c)) {
        return None;
    }
    let start = chars[..col].iter().rposition(|c| stop(*c)).map_or(0, |i| i + 1);
    let mut end = chars[col..].iter().position(|c| stop(*c)).map_or(chars.len(), |i| col + i);
    // A sentence's full stop or a trailing colon isn't part of it.
    while end > start + 1 && ".:;!?".contains(chars[end - 1]) {
        end -= 1;
    }
    (col < end).then(|| (start..end, chars[start..end].iter().collect()))
}

/// The query's matches in the scrollback and on screen (case-insensitive
/// unless it has capitals), top to bottom, the newest `MAX_MATCHES`; and
/// how many scrollback lines there were.
fn find_in(term: &Term<Listener>, query: &str) -> (Vec<Match>, usize) {
    let history = term.grid().history_size();
    let mut matches = Vec::new();
    if !query.is_empty()
        && let Ok(mut regex) = RegexSearch::new(&regex::escape(query))
    {
        // From the bottom up, so a capped list keeps the newest.
        let bottom = GridPoint::new(term.bottommost_line(), term.last_column());
        let top = GridPoint::new(term.topmost_line(), Column(0));
        matches.extend(RegexIter::new(bottom, top, Direction::Left, term, &mut regex).take(MAX_MATCHES));
        matches.reverse();
    }
    (matches, history)
}

/// A process's working directory.
#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is a buffer of `size` bytes for PROC_PIDVNODEPATHINFO.
    let read = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut info).cast(), size) };
    if read != size {
        return None;
    }
    // SAFETY: the kernel fills `vip_path` with a NUL-terminated path.
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    let path = PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes()));
    path.is_dir().then_some(path)
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok().filter(|p| p.is_dir())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// A URL, or a file that exists (relative to `cwd`) with an optional
/// `:line` and `:column`, as compilers and test runners print them.
fn parse_link(text: &str, cwd: &Path) -> Option<Link> {
    if text.starts_with("https://") || text.starts_with("http://") {
        return Some(Link::Url(text.to_string()));
    }
    let mut parts = text.splitn(3, ':');
    let path = parts.next()?;
    let line = parts.next().and_then(|n| n.parse().ok());
    let column = parts.next().and_then(|n| n.parse().ok());
    let path = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from)?.join(rest),
        None => cwd.join(path),
    };
    path.is_file().then_some(Link::File { path, line, column })
}

impl Drop for TerminalView {
    fn drop(&mut self) {
        // Closes the pty; the shell gets SIGHUP.
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

/// What a terminal frame paints.
#[derive(Default)]
pub struct TerminalFrame {
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    backgrounds: Vec<PaintQuad>,
    /// Find's matches, over the cell backgrounds and under the text.
    highlights: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    /// Under the link the pointer is on (⌘/Ctrl held).
    link: Option<PaintQuad>,
    hitbox: Option<Hitbox>,
}

/// The shell script for a Run tab: shows the command, runs it, then says
/// how it ended (the tab stays open). The no-op INT trap lets the wrapper
/// outlive a Stop; the command runs in a subshell, which resets the trap so
/// ⌃C still stops it. (`status` is read-only in zsh.)
fn run_script(command: &str) -> String {
    let shown = command.replace('\'', r"'\''");
    format!(
        "trap : INT\nprintf '\\033[2m$ %s\\033[0m\\n' '{shown}'\n(\n{command}\n)\nkin_code=$?\nprintf '\\n\\033[2mProcess finished with exit code %s\\033[0m\\n' \"$kuana_code\"\nexit $kuana_code\n"
    )
}

// ---- Colors ------------------------------------------------------------------------

fn to_hsla(rgb: Rgb) -> Hsla {
    gpui::Rgba { r: rgb.r as f32 / 255., g: rgb.g as f32 / 255., b: rgb.b as f32 / 255., a: 1. }.into()
}

fn to_rgb(color: gpui::Rgba) -> Rgb {
    Rgb { r: (color.r * 255.).round() as u8, g: (color.g * 255.).round() as u8, b: (color.b * 255.).round() as u8 }
}

/// Colors 0–15 from the theme, 16–255 the standard xterm cube and grays,
/// then the default foreground, background and cursor.
fn palette(theme: &Theme, index: usize) -> Option<Rgb> {
    let ansi = [
        theme.terminal_black,
        theme.terminal_red,
        theme.terminal_green,
        theme.terminal_yellow,
        theme.terminal_blue,
        theme.terminal_magenta,
        theme.terminal_cyan,
        theme.terminal_white,
        theme.terminal_bright_black,
        theme.terminal_bright_red,
        theme.terminal_bright_green,
        theme.terminal_bright_yellow,
        theme.terminal_bright_blue,
        theme.terminal_bright_magenta,
        theme.terminal_bright_cyan,
        theme.terminal_bright_white,
    ];
    Some(match index {
        0..=15 => to_rgb(ansi[index]),
        16..=231 => {
            let i = index - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u8 };
            Rgb { r: level(i / 36), g: level(i / 6 % 6), b: level(i % 6) }
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            Rgb { r: v, g: v, b: v }
        }
        i if i == NamedColor::Foreground as usize => to_rgb(theme.text),
        i if i == NamedColor::Background as usize => to_rgb(theme.background),
        i if i == NamedColor::Cursor as usize => to_rgb(theme.cursor),
        _ => return None,
    })
}

fn resolve(theme: &Theme, overrides: &alacritty_terminal::term::color::Colors, color: Color, flags: Flags) -> Hsla {
    let index = match color {
        Color::Spec(rgb) => return to_hsla(rgb),
        Color::Indexed(i) => i as usize,
        Color::Named(named) => {
            let n = named as usize;
            let dim_start = NamedColor::DimBlack as usize;
            if flags.contains(Flags::BOLD) && n < 8 {
                // Bold text in a basic color uses the bright variant, like most terminals.
                n + 8
            } else if (dim_start..dim_start + 8).contains(&n) {
                // Dim colors aren't in the palette; DIM text is faded when painted.
                n - dim_start
            } else {
                n
            }
        }
    };
    overrides[index].or_else(|| palette(theme, index)).map(to_hsla).unwrap_or(theme.text.into())
}

// ---- Keys --------------------------------------------------------------------------

/// The bytes a key sends, for keys that aren't plain text. None for text
/// (handled by the input handler) and for ⌘ shortcuts.
fn escape_for(keystroke: &gpui::Keystroke, mode: TermMode) -> Option<Cow<'static, [u8]>> {
    let m = keystroke.modifiers;
    if m.platform {
        return None;
    }
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let cursor_key = |letter: char| -> Cow<'static, [u8]> {
        if param > 1 {
            format!("\x1b[1;{param}{letter}").into_bytes().into()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes().into()
        } else {
            format!("\x1b[{letter}").into_bytes().into()
        }
    };
    let tilde = |code: u8| -> Cow<'static, [u8]> {
        if param > 1 { format!("\x1b[{code};{param}~").into_bytes().into() } else { format!("\x1b[{code}~").into_bytes().into() }
    };
    let meta = |bytes: &[u8]| -> Cow<'static, [u8]> {
        let mut out = Vec::with_capacity(bytes.len() + 1);
        if m.alt {
            out.push(0x1b);
        }
        out.extend_from_slice(bytes);
        out.into()
    };
    Some(match keystroke.key.as_str() {
        "enter" => meta(b"\r"),
        "tab" if m.shift => b"\x1b[Z"[..].into(),
        "tab" => b"\t"[..].into(),
        "escape" => b"\x1b"[..].into(),
        "backspace" if m.control => meta(b"\x08"),
        "backspace" => meta(b"\x7f"),
        "space" if m.control => b"\x00"[..].into(),
        "up" => cursor_key('A'),
        "down" => cursor_key('B'),
        "right" => cursor_key('C'),
        "left" => cursor_key('D'),
        "home" => cursor_key('H'),
        "end" => cursor_key('F'),
        "insert" => tilde(2),
        "delete" => tilde(3),
        "pageup" => tilde(5),
        "pagedown" => tilde(6),
        "f1" | "f2" | "f3" | "f4" => {
            let letter = ['P', 'Q', 'R', 'S'][keystroke.key[1..].parse::<usize>().ok()? - 1];
            if param > 1 { format!("\x1b[1;{param}{letter}").into_bytes().into() } else { format!("\x1bO{letter}").into_bytes().into() }
        }
        key if key.starts_with('f') && key.len() <= 3 => {
            let n: u8 = key[1..].parse().ok()?;
            let code = match n {
                5 => 15,
                6..=10 => n + 11,
                11 | 12 => n + 12,
                _ => return None,
            };
            tilde(code)
        }
        key if m.control => {
            // Ctrl+letter and the punctuation that maps to control codes.
            let c = key.chars().next()?;
            let byte = match c {
                'a'..='z' => c as u8 - b'a' + 1,
                '@' | '2' => 0,
                '[' | '3' => 27,
                '\\' | '4' => 28,
                ']' | '5' => 29,
                '^' | '6' => 30,
                '_' | '-' | '7' => 31,
                '/' => 31,
                '?' | '8' => 127,
                _ => return None,
            };
            if key.chars().count() != 1 {
                return None;
            }
            meta(&[byte])
        }
        // Option types characters on macOS (`@` on many layouts), so only
        // elsewhere does Alt act as Meta.
        _ if m.alt && !cfg!(target_os = "macos") => {
            let text = keystroke.key_char.clone().unwrap_or_else(|| keystroke.key.clone());
            meta(text.as_bytes())
        }
        _ => return None,
    })
}

// ---- View --------------------------------------------------------------------------

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.as_ref().map(|search| self.render_search(search, window, cx).into_any_element());
        let theme = Theme::get(cx);
        div()
            .id("terminal")
            .key_context("Terminal")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .bg(theme.background)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::open_search))
            .on_action(cx.listener(Self::close_search))
            .on_action(cx.listener(Self::on_find_older))
            .on_action(cx.listener(Self::on_find_newer))
            .on_action(cx.listener(Self::on_find_up))
            .on_action(cx.listener(Self::on_find_down))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(TerminalElement { view: cx.entity() })
            .children(search)
    }
}

/// Text input (and IME composition) goes to the shell.
impl EntityInputHandler for TerminalView {
    fn text_for_range(&mut self, _: Range<usize>, _: &mut Option<Range<usize>>, _: &mut Window, _: &mut Context<Self>) -> Option<String> {
        None
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(&mut self, _: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        if !text.is_empty() {
            self.input(text.to_string().into_bytes(), cx);
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
        // Composition in progress; the final text comes via replace_text_in_range.
    }

    fn bounds_for_range(&mut self, _: Range<usize>, _: Bounds<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<Bounds<Pixels>> {
        // Put the IME window at the cursor.
        let cursor = self.term.lock().grid().cursor.point;
        let (w, h) = self.cell;
        Some(Bounds::new(
            self.origin + point(w * cursor.column.0 as f32, h * cursor.line.0 as f32),
            size(w, h),
        ))
    }

    fn character_index_for_point(&mut self, _: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        None
    }
}

struct TerminalElement {
    view: Entity<TerminalView>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalFrame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
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
    ) -> TerminalFrame {
        let mut frame = self.view.update(cx, |view, cx| view.layout(bounds, window, cx));
        frame.hitbox = Some(window.insert_hitbox(bounds, HitboxBehavior::Normal));
        frame
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut TerminalFrame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(&focus_handle, ElementInputHandler::new(bounds, self.view.clone()), cx);
        if let Some(hitbox) = frame.hitbox.as_ref() {
            let style = if frame.link.is_some() { CursorStyle::PointingHand } else { CursorStyle::IBeam };
            window.set_cursor_style(style, hitbox);
        }
        window.with_content_mask(Some(ContentMask { bounds: frame.bounds }), |window| {
            if let Some(underline) = frame.link.take() {
                window.paint_quad(underline);
            }
            for quad in frame.backgrounds.drain(..).chain(frame.highlights.drain(..)) {
                window.paint_quad(quad);
            }
            if let Some(cursor) = frame.cursor.take() {
                window.paint_quad(cursor);
            }
            for (origin, line) in &frame.lines {
                line.paint(*origin, frame.line_height, window, cx).ok();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn finds_a_process_working_directory() {
        let dir = std::env::temp_dir().join(format!("kuana-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = std::process::Command::new("sleep").arg("5").current_dir(&dir).spawn().unwrap();
        let found = process_cwd(child.id()).and_then(|p| p.canonicalize().ok());
        let expected = dir.canonicalize().ok();
        child.kill().ok();
        child.wait().ok();
        std::fs::remove_dir(&dir).ok();
        assert!(found.is_some());
        assert_eq!(found, expected);
    }

    fn keys(source: &str, mode: TermMode) -> Option<Vec<u8>> {
        escape_for(&gpui::Keystroke::parse(source).unwrap(), mode).map(|c| c.into_owned())
    }

    #[test]
    fn find_keeps_the_newest_matches() {
        let (tx, _rx) = mpsc::unbounded();
        let mut term = Term::new(Config::default(), &TermSize::new(40, 10), Listener(tx));
        let mut parser = alacritty_terminal::vte::ansi::Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new();
        let lines = MAX_MATCHES + 10;
        let output: String = (0..lines).map(|i| format!("hit {i}\r\n")).collect();
        parser.advance(&mut term, output.as_bytes());
        let (matches, _) = find_in(&term, "hit");
        assert_eq!(matches.len(), MAX_MATCHES);
        assert!(matches.windows(2).all(|w| w[0].start() < w[1].start()), "top to bottom");
        let text = |m: &Match| term.bounds_to_string(GridPoint::new(m.start().line, Column(0)), GridPoint::new(m.start().line, Column(12)));
        assert!(text(matches.last().unwrap()).starts_with(&format!("hit {}", lines - 1)));
        assert!(text(&matches[0]).starts_with("hit 10"), "{}", text(&matches[0]));
    }

    #[test]
    fn finds_links_in_output() {
        let dir = std::env::temp_dir().join(format!("kuana-term-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        let row: Vec<char> = "  --> src/main.rs:12:5 (see https://example.com/a?b=1).".chars().collect();
        let at = |needle: &str| row.iter().collect::<String>().find(needle).unwrap() + 1;
        let (range, text) = word_at(&row, at("src")).unwrap();
        assert_eq!((range, text.as_str()), (6..22, "src/main.rs:12:5"));
        assert_eq!(
            parse_link(&text, &dir),
            Some(Link::File { path: dir.join("src/main.rs"), line: Some(12), column: Some(5) })
        );
        let (_, url) = word_at(&row, at("https")).unwrap();
        assert_eq!(parse_link(&url, &dir), Some(Link::Url("https://example.com/a?b=1".into())));
        // Plain words and missing files aren't links.
        let (_, word) = word_at(&row, at("see")).unwrap();
        assert_eq!(parse_link(&word, &dir), None);
        assert_eq!(parse_link("src/missing.rs:3", &dir), None);
        assert!(word_at(&row, 0).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn maps_special_keys() {
        let none = TermMode::empty();
        assert_eq!(keys("enter", none).unwrap(), b"\r");
        assert_eq!(keys("up", none).unwrap(), b"\x1b[A");
        assert_eq!(keys("up", TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        assert_eq!(keys("ctrl-left", none).unwrap(), b"\x1b[1;5D");
        assert_eq!(keys("ctrl-c", none).unwrap(), &[3]);
        assert_eq!(keys("ctrl-r", none).unwrap(), &[18]);
        assert_eq!(keys("shift-tab", none).unwrap(), b"\x1b[Z");
        assert_eq!(keys("f5", none).unwrap(), b"\x1b[15~");
        assert_eq!(keys("f1", none).unwrap(), b"\x1bOP");
        assert_eq!(keys("pagedown", none).unwrap(), b"\x1b[6~");
        // Text goes through the input handler, ⌘ shortcuts to the app.
        assert!(keys("a", none).is_none());
        assert!(keys("cmd-c", none).is_none());
    }

    #[test]
    fn xterm_palette() {
        let theme = Theme::blank_for_tests();
        assert_eq!(palette(&theme, 16), Some(Rgb { r: 0, g: 0, b: 0 }));
        assert_eq!(palette(&theme, 231), Some(Rgb { r: 255, g: 255, b: 255 }));
        assert_eq!(palette(&theme, 232), Some(Rgb { r: 8, g: 8, b: 8 }));
    }
}
