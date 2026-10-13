mod assets;
mod branch_picker;
mod clone_dialog;
mod context_menu;
mod diff_view;
mod document;
mod editor;
mod editorconfig;
mod find_bar;
mod format;
mod git;
mod git_panel;
mod input_dialog;
mod keymap;
mod lint;
mod lsp;
mod picker;
mod project_search;
mod search;
mod settings;
mod state;
mod syntax;
mod terminal;
mod theme;
mod theme_picker;
mod ui;
mod workspace;
mod wrap;

use std::path::PathBuf;

use gpui::{App, Application, Menu, MenuItem};

use crate::{
    editor::*,
    workspace::{
        CloseTab, CloseWindow, DecreaseFontSize, FileStructure, Find, IncreaseFontSize, OpenKeymap, OpenSettings,
        ResetFontSize, SplitDown, SplitRight, ToggleTerminal, FindAction, FindInFiles, GitBranches, GitClone, GitFetch,
        GitInit, GitPull, GitPush, GoToFile, GoToLine, NewFile, NewWindow, NextTab, OpenFile,
        OpenFolder, OpenFolderInThisWindow, OpenRemote, PrevTab, Quit, RecentFiles, RecentProjects,
        Replace, ReplaceInFiles, Save, SaveAs, SearchEverywhere, SelectTheme, ShowCommit,
        ToggleProblems, ToggleSidebar,
    },
};

fn menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Kin".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::action("Keymap…", OpenKeymap),
                MenuItem::separator(),
                MenuItem::action("Quit Kin", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New File", NewFile),
                MenuItem::action("New Window", NewWindow),
                MenuItem::separator(),
                MenuItem::action("Open File…", OpenFile),
                MenuItem::action("Open Folder…", OpenFolder),
                MenuItem::action("Open Folder in This Window…", OpenFolderInThisWindow),
                MenuItem::action("Recent Projects…", RecentProjects),
                MenuItem::action("Clone Repository…", GitClone),
                MenuItem::separator(),
                MenuItem::action("Save", Save),
                MenuItem::action("Save As…", SaveAs),
                MenuItem::separator(),
                MenuItem::action("Close Tab", CloseTab),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Undo", Undo),
                MenuItem::action("Redo", Redo),
                MenuItem::separator(),
                MenuItem::action("Cut", Cut),
                MenuItem::action("Copy", Copy),
                MenuItem::action("Paste", Paste),
                MenuItem::separator(),
                MenuItem::action("Select All", SelectAll),
                MenuItem::action("Select Next Occurrence", SelectNextOccurrence),
                MenuItem::action("Toggle Comment", ToggleComment),
                MenuItem::action("Duplicate Line", DuplicateLine),
                MenuItem::action("Rename…", RenameSymbol),
                MenuItem::action("Reformat Code", ReformatCode),
                MenuItem::separator(),
                MenuItem::action("Find…", Find),
                MenuItem::action("Replace…", Replace),
                MenuItem::action("Find in Files…", FindInFiles),
                MenuItem::action("Replace in Files…", ReplaceInFiles),
            ],
        },
        Menu {
            name: "Navigate".into(),
            items: vec![
                MenuItem::action("Search Everywhere", SearchEverywhere),
                MenuItem::action("Go to File…", GoToFile),
                MenuItem::action("Recent Files…", RecentFiles),
                MenuItem::action("Go to Line…", GoToLine),
                MenuItem::action("File Structure…", FileStructure),
                MenuItem::separator(),
                MenuItem::action("Go to Declaration", GoToDeclaration),
                MenuItem::action("Find Usages", FindUsages),
                MenuItem::action("Find Action…", FindAction),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Toggle Sidebar", ToggleSidebar),
                MenuItem::action("Toggle Problems", ToggleProblems),
                MenuItem::action("Terminal", ToggleTerminal),
                MenuItem::action("Soft-Wrap", ToggleSoftWrap),
                MenuItem::separator(),
                MenuItem::action("Select Theme…", SelectTheme),
                MenuItem::action("Increase Font Size", IncreaseFontSize),
                MenuItem::action("Decrease Font Size", DecreaseFontSize),
                MenuItem::action("Reset Font Size", ResetFontSize),
                MenuItem::separator(),
                MenuItem::action("Split and Move Right", SplitRight),
                MenuItem::action("Split and Move Down", SplitDown),
                MenuItem::separator(),
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PrevTab),
            ],
        },
        Menu {
            name: "Git".into(),
            items: vec![
                MenuItem::action("Commit…", ShowCommit),
                MenuItem::action("Update Project…", GitPull),
                MenuItem::action("Push…", GitPush),
                MenuItem::action("Fetch", GitFetch),
                MenuItem::separator(),
                MenuItem::action("Branches…", GitBranches),
                MenuItem::action("Open Repository in Browser", OpenRemote),
                MenuItem::separator(),
                MenuItem::action("Clone Repository…", GitClone),
                MenuItem::action("Initialize Repository", GitInit),
            ],
        },
    ]
}

/// Run as a bare binary (`cargo run`), macOS names the app after the executable, `kin`,
/// in the menu bar and Dock. Give it the display name; a bundled `Kin.app` already has
/// it in Info.plist. Must run before the application starts.
#[cfg(target_os = "macos")]
fn set_app_name() {
    use objc2::{rc::Retained, runtime::AnyObject};
    use objc2_foundation::{NSBundle, NSMutableDictionary, NSString, ns_string};

    let bundle = NSBundle::mainBundle();
    if bundle.bundlePath().to_string().ends_with(".app") {
        return;
    }
    let Some(info) = bundle.infoDictionary() else { return };
    // Without a bundle, Foundation builds this dictionary in memory and it is mutable.
    let info: Retained<NSMutableDictionary<NSString, AnyObject>> = unsafe { Retained::cast_unchecked(info) };
    info.insert(ns_string!("CFBundleName"), &*NSString::from_str("Kin"));
}

/// Sets the Dock icon so it also shows when running the bare binary
/// (`cargo run`); a bundled `.app` gets it from `Kin.icns` as well.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    static ICON: &[u8] = include_bytes!("../assets/app-icon/macos/icon_1024.png");

    let Some(mtm) = MainThreadMarker::new() else { return };
    let data = NSData::with_bytes(ICON);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
    }
}

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();

    #[cfg(target_os = "macos")]
    set_app_name();

    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            #[cfg(target_os = "macos")]
            set_dock_icon();
            assets::load_fonts(cx);
            // Settings first: the theme and key bindings come from them.
            let problems = settings::init(cx);
            cx.set_global(settings::StartupProblems(problems));
            theme::init(cx);
            cx.on_action(|_: &IncreaseFontSize, cx| theme::zoom(Some(1.), cx));
            cx.on_action(|_: &DecreaseFontSize, cx| theme::zoom(Some(-1.), cx));
            cx.on_action(|_: &ResetFontSize, cx| theme::zoom(None, cx));
            // Quitting asks each window with unsaved edits in turn.
            cx.on_action(|_: &Quit, cx| cx.defer(workspace::quit));
            cx.on_action(|_: &NewWindow, cx| {
                workspace::open_window(None, cx, |_, _, _| {});
            });
            cx.set_menus(menus());

            // Each folder on the command line gets its own window, like Zed;
            // files open in the first one. Without paths, reopen the last project.
            let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) = args
                .into_iter()
                .map(|p| p.canonicalize().unwrap_or(p))
                .partition(|p| p.is_dir());
            let mut dirs = dirs.into_iter();
            let first_dir = dirs.next();
            let restore = first_dir.is_none() && files.is_empty();
            let mut last = workspace::open_window(None, cx, move |workspace, window, cx| {
                if restore {
                    workspace.restore_last_project(window, cx);
                }
                if let Some(dir) = first_dir {
                    workspace.set_root(dir, window, cx);
                }
                for path in files {
                    workspace.open_path(path, window, cx);
                }
            });
            for dir in dirs {
                let near = last.and_then(|w| w.update(cx, |_, window, _| window.bounds()).ok());
                last = workspace::open_window(near, cx, move |workspace, window, cx| workspace.set_root(dir, window, cx)).or(last);
            }
            cx.activate(true);
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        });
}
