//! Key bindings modeled on the JetBrains IDE keymaps: the macOS keymap on
//! macOS and the Windows/Linux keymap elsewhere. Double Shift (Search
//! Everywhere) isn't a regular binding; the workspace detects it.
//!
//! The user's `keymap.json` is added on top; see [`install`].

use std::{collections::HashMap, path::PathBuf, rc::Rc};

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, KeyBindingMetaIndex, NoAction};
use serde::Deserialize;

use crate::{
    editor::*,
    picker::ActionEntry,
    settings::strip_jsonc,
    theme::{self, config_dir},
    workspace::{
        CloseTab, DecreaseFontSize, FileStructure, Find, IncreaseFontSize, OpenKeymap, OpenSettings, ResetFontSize, RestartLanguageServer, FindAction, FindInFiles, FindNext, FindPrevious, GoToFile, GoToLine,
        CloseWindow, NewFile, NewWindow, NextTab, OpenFile, OpenFolder, OpenFolderInThisWindow,
        PrevTab, Quit, RecentFiles, RecentProjects, Replace,
        GitBranches, GitClone, GitFetch, GitInit, GitPull, GitPush, OpenRemote, OpenThemesFolder, ReloadThemes, ReplaceInFiles,
        Save, SaveAs, SearchEverywhere, SelectTheme, ShowCommit, ToggleCommit, ToggleProblems,
        ToggleAutoSave, ToggleSidebar,
    },
};

pub fn bindings() -> Vec<KeyBinding> {
    let e = Some("Editor");
    // Shared by both keymaps (`secondary` is ⌘ on macOS, Ctrl elsewhere).
    let mut b = vec![
        KeyBinding::new("secondary-n", NewFile, None),
        KeyBinding::new("secondary-o", OpenFile, None),
        KeyBinding::new("secondary-s", Save, None),
        KeyBinding::new("secondary-shift-s", SaveAs, None),
        KeyBinding::new("secondary-q", Quit, None),
        KeyBinding::new("secondary-f", Find, None),
        KeyBinding::new("secondary-r", Replace, None),
        KeyBinding::new("secondary-shift-f", FindInFiles, None),
        KeyBinding::new("secondary-shift-r", ReplaceInFiles, None),
        KeyBinding::new("secondary-e", RecentFiles, None),
        KeyBinding::new("secondary-f12", FileStructure, None),
        KeyBinding::new("ctrl-space", ShowCompletions, e),
        KeyBinding::new("secondary-b", GoToDeclaration, e),
        KeyBinding::new("alt-f7", FindUsages, e),
        KeyBinding::new("shift-f6", RenameSymbol, e),
        KeyBinding::new("alt-enter", ShowContextActions, e),
        KeyBinding::new("secondary-alt-l", ReformatCode, e),
        KeyBinding::new("secondary-=", IncreaseFontSize, None),
        KeyBinding::new("secondary-+", IncreaseFontSize, None),
        KeyBinding::new("secondary--", DecreaseFontSize, None),
        KeyBinding::new("secondary-shift-a", FindAction, None),
        KeyBinding::new("secondary-p", GoToFile, None),
        KeyBinding::new("f3", FindNext, None),
        KeyBinding::new("shift-f3", FindPrevious, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("secondary-shift-]", NextTab, None),
        KeyBinding::new("secondary-shift-[", PrevTab, None),
        KeyBinding::new("secondary-shift-m", ToggleProblems, None),
        // JetBrains "Quick Switch Scheme".
        KeyBinding::new("ctrl-`", SelectTheme, None),
        // Git (JetBrains: Commit, Push, Update Project).
        KeyBinding::new("secondary-k", ShowCommit, None),
        KeyBinding::new("secondary-shift-k", GitPush, None),
        KeyBinding::new("secondary-t", GitPull, None),
        KeyBinding::new("ctrl-alt-shift-down", NextChange, e),
        KeyBinding::new("ctrl-alt-shift-up", PreviousChange, e),
        KeyBinding::new("escape", Cancel, Some("DiffView")),
        KeyBinding::new("escape", Cancel, Some("ContextMenu")),
        KeyBinding::new("up", MoveUp, Some("ContextMenu")),
        KeyBinding::new("down", MoveDown, Some("ContextMenu")),
        KeyBinding::new("enter", Newline, Some("ContextMenu")),
        // Editor basics
        KeyBinding::new("left", MoveLeft, e),
        KeyBinding::new("right", MoveRight, e),
        KeyBinding::new("up", MoveUp, e),
        KeyBinding::new("down", MoveDown, e),
        KeyBinding::new("shift-left", SelectLeft, e),
        KeyBinding::new("shift-right", SelectRight, e),
        KeyBinding::new("shift-up", SelectUp, e),
        KeyBinding::new("shift-down", SelectDown, e),
        KeyBinding::new("home", MoveLineStart, e),
        KeyBinding::new("end", MoveLineEnd, e),
        KeyBinding::new("shift-home", SelectLineStart, e),
        KeyBinding::new("shift-end", SelectLineEnd, e),
        KeyBinding::new("pageup", PageUp, e),
        KeyBinding::new("pagedown", PageDown, e),
        KeyBinding::new("backspace", Backspace, e),
        KeyBinding::new("shift-backspace", Backspace, e),
        KeyBinding::new("delete", Delete, e),
        KeyBinding::new("enter", Newline, e),
        KeyBinding::new("shift-enter", NewlineBelow, e),
        KeyBinding::new("secondary-enter", Submit, e),
        KeyBinding::new("tab", Indent, e),
        KeyBinding::new("shift-tab", Outdent, e),
        KeyBinding::new("escape", Cancel, e),
        KeyBinding::new("secondary-a", SelectAll, e),
        KeyBinding::new("secondary-c", Copy, e),
        KeyBinding::new("secondary-x", Cut, e),
        KeyBinding::new("secondary-v", Paste, e),
        KeyBinding::new("secondary-z", Undo, e),
        KeyBinding::new("secondary-shift-z", Redo, e),
        KeyBinding::new("secondary-/", ToggleComment, e),
        KeyBinding::new("secondary-d", DuplicateLine, e),
        KeyBinding::new("secondary-shift-u", ToggleCase, e),
        KeyBinding::new("ctrl-shift-j", JoinLines, e),
        KeyBinding::new("alt-shift-up", MoveLineUp, e),
        KeyBinding::new("alt-shift-down", MoveLineDown, e),
    ];

    if cfg!(target_os = "macos") {
        b.extend([
            // The Windows-style Ctrl variants also work on macOS.
            KeyBinding::new("ctrl-f", Find, None),
            KeyBinding::new("ctrl-r", Replace, None),
            KeyBinding::new("ctrl-shift-f", FindInFiles, None),
            KeyBinding::new("ctrl-shift-r", ReplaceInFiles, None),
            KeyBinding::new("cmd-g", FindNext, None),
            KeyBinding::new("cmd-shift-g", FindPrevious, None),
            KeyBinding::new("cmd-shift-o", GoToFile, None),
            KeyBinding::new("cmd-l", GoToLine, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("f1", QuickDocumentation, e),
            KeyBinding::new("cmd-w", CloseTab, None),
            KeyBinding::new("ctrl-right", NextTab, None),
            KeyBinding::new("ctrl-left", PrevTab, None),
            KeyBinding::new("cmd-1", ToggleSidebar, None),
            KeyBinding::new("cmd-6", ToggleProblems, None),
            KeyBinding::new("cmd-0", ToggleCommit, None),
            KeyBinding::new("cmd-alt-z", RollbackHunk, e),
            KeyBinding::new("cmd-alt-o", OpenFolder, None),
            // Zed's window shortcuts (JetBrains has none by default).
            KeyBinding::new("cmd-shift-n", NewWindow, None),
            KeyBinding::new("cmd-shift-w", CloseWindow, None),
            KeyBinding::new("cmd-left", MoveLineStart, e),
            KeyBinding::new("cmd-right", MoveLineEnd, e),
            KeyBinding::new("cmd-shift-left", SelectLineStart, e),
            KeyBinding::new("cmd-shift-right", SelectLineEnd, e),
            KeyBinding::new("cmd-up", MoveDocStart, e),
            KeyBinding::new("cmd-down", MoveDocEnd, e),
            KeyBinding::new("cmd-home", MoveDocStart, e),
            KeyBinding::new("cmd-end", MoveDocEnd, e),
            KeyBinding::new("cmd-shift-up", SelectDocStart, e),
            KeyBinding::new("cmd-shift-down", SelectDocEnd, e),
            KeyBinding::new("alt-left", MoveWordLeft, e),
            KeyBinding::new("alt-right", MoveWordRight, e),
            KeyBinding::new("alt-shift-left", SelectWordLeft, e),
            KeyBinding::new("alt-shift-right", SelectWordRight, e),
            KeyBinding::new("alt-backspace", DeleteWordLeft, e),
            KeyBinding::new("alt-delete", DeleteWordRight, e),
            KeyBinding::new("cmd-backspace", DeleteLine, e),
            KeyBinding::new("alt-up", ExtendSelection, e),
            KeyBinding::new("alt-down", ShrinkSelection, e),
            KeyBinding::new("ctrl-g", SelectNextOccurrence, e),
            KeyBinding::new("ctrl-shift-g", UnselectOccurrence, e),
            KeyBinding::new("ctrl-cmd-g", SelectAllOccurrences, e),
            KeyBinding::new("cmd-alt-/", ToggleBlockComment, e),
            KeyBinding::new("cmd-alt-enter", NewlineAbove, e),
            KeyBinding::new("cmd-alt-up", AddCursorAbove, e),
            KeyBinding::new("cmd-alt-down", AddCursorBelow, e),
            KeyBinding::new("ctrl-a", MoveLineStart, e),
            KeyBinding::new("ctrl-e", MoveLineEnd, e),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, e),
        ]);
    } else {
        b.extend([
            KeyBinding::new("ctrl-shift-n", GoToFile, None),
            KeyBinding::new("ctrl-g", GoToLine, None),
            KeyBinding::new("ctrl-alt-s", OpenSettings, None),
            KeyBinding::new("ctrl-q", QuickDocumentation, e),
            KeyBinding::new("ctrl-f4", CloseTab, None),
            KeyBinding::new("alt-right", NextTab, None),
            KeyBinding::new("alt-left", PrevTab, None),
            KeyBinding::new("alt-1", ToggleSidebar, None),
            KeyBinding::new("alt-6", ToggleProblems, None),
            KeyBinding::new("alt-0", ToggleCommit, None),
            KeyBinding::new("ctrl-alt-z", RollbackHunk, e),
            KeyBinding::new("ctrl-alt-o", OpenFolder, None),
            KeyBinding::new("ctrl-home", MoveDocStart, e),
            KeyBinding::new("ctrl-end", MoveDocEnd, e),
            KeyBinding::new("ctrl-shift-home", SelectDocStart, e),
            KeyBinding::new("ctrl-shift-end", SelectDocEnd, e),
            KeyBinding::new("ctrl-left", MoveWordLeft, e),
            KeyBinding::new("ctrl-right", MoveWordRight, e),
            KeyBinding::new("ctrl-shift-left", SelectWordLeft, e),
            KeyBinding::new("ctrl-shift-right", SelectWordRight, e),
            KeyBinding::new("ctrl-backspace", DeleteWordLeft, e),
            KeyBinding::new("ctrl-delete", DeleteWordRight, e),
            KeyBinding::new("ctrl-y", DeleteLine, e),
            KeyBinding::new("ctrl-w", ExtendSelection, e),
            KeyBinding::new("ctrl-shift-w", ShrinkSelection, e),
            KeyBinding::new("alt-j", SelectNextOccurrence, e),
            KeyBinding::new("alt-shift-j", UnselectOccurrence, e),
            KeyBinding::new("ctrl-alt-shift-j", SelectAllOccurrences, e),
            KeyBinding::new("ctrl-shift-/", ToggleBlockComment, e),
            KeyBinding::new("ctrl-alt-enter", NewlineAbove, e),
            KeyBinding::new("ctrl-alt-up", AddCursorAbove, e),
            KeyBinding::new("ctrl-alt-down", AddCursorBelow, e),
        ]);
    }
    b
}

/// Everything Find Action / Search Everywhere can run.
pub fn action_entries() -> Vec<ActionEntry> {
    fn a(name: &'static str, action: impl Action) -> ActionEntry {
        ActionEntry { name, action: Box::new(action) }
    }
    vec![
        a("Search Everywhere", SearchEverywhere),
        a("Find", Find),
        a("Replace", Replace),
        a("Find in Files", FindInFiles),
        a("Replace in Files", ReplaceInFiles),
        a("Find Next Occurrence", FindNext),
        a("Find Previous Occurrence", FindPrevious),
        a("Go to File", GoToFile),
        a("Go to Line:Column", GoToLine),
        a("File Structure", FileStructure),
        a("Go to Declaration", GoToDeclaration),
        a("Find Usages", FindUsages),
        a("Rename…", RenameSymbol),
        a("Show Context Actions", ShowContextActions),
        a("Reformat Code", ReformatCode),
        a("Basic Completion", ShowCompletions),
        a("Quick Documentation", QuickDocumentation),
        a("Restart Language Server", RestartLanguageServer),
        a("Recent Files", RecentFiles),
        a("New File", NewFile),
        a("Open File…", OpenFile),
        a("Open Folder…", OpenFolder),
        a("Open Folder in This Window…", OpenFolderInThisWindow),
        a("Recent Projects…", RecentProjects),
        a("New Window", NewWindow),
        a("Close Window", CloseWindow),
        a("Save", Save),
        a("Save As…", SaveAs),
        a("Toggle Auto Save", ToggleAutoSave),
        a("Close Tab", CloseTab),
        a("Next Tab", NextTab),
        a("Previous Tab", PrevTab),
        a("Toggle Project Sidebar", ToggleSidebar),
        a("Toggle Problems", ToggleProblems),
        a("Duplicate Line or Selection", DuplicateLine),
        a("Delete Line", DeleteLine),
        a("Join Lines", JoinLines),
        a("Move Line Up", MoveLineUp),
        a("Move Line Down", MoveLineDown),
        a("Start New Line", NewlineBelow),
        a("Start New Line Before Current", NewlineAbove),
        a("Comment with Line Comment", ToggleComment),
        a("Comment with Block Comment", ToggleBlockComment),
        a("Extend Selection", ExtendSelection),
        a("Shrink Selection", ShrinkSelection),
        a("Add Selection for Next Occurrence", SelectNextOccurrence),
        a("Unselect Occurrence", UnselectOccurrence),
        a("Select All Occurrences", SelectAllOccurrences),
        a("Add Caret Above", AddCursorAbove),
        a("Add Caret Below", AddCursorBelow),
        a("Toggle Case", ToggleCase),
        a("Select All", SelectAll),
        a("Select Line", SelectLine),
        a("Undo", Undo),
        a("Redo", Redo),
        a("Commit…", ShowCommit),
        a("Toggle Commit Panel", ToggleCommit),
        a("Push…", GitPush),
        a("Update Project (Pull)", GitPull),
        a("Fetch", GitFetch),
        a("Branches…", GitBranches),
        a("Clone Repository…", GitClone),
        a("Get from Version Control…", GitClone),
        a("Initialize Git Repository", GitInit),
        a("Open Repository in Browser", OpenRemote),
        a("Rollback Lines", RollbackHunk),
        a("Next Change", NextChange),
        a("Previous Change", PreviousChange),
        a("Toggle Inline Git Blame", ToggleInlineBlame),
        a("Soft-Wrap", ToggleSoftWrap),
        a("Settings", OpenSettings),
        a("Keymap", OpenKeymap),
        a("Increase Font Size", IncreaseFontSize),
        a("Decrease Font Size", DecreaseFontSize),
        a("Reset Font Size", ResetFontSize),
        a("Select Theme…", SelectTheme),
        a("Open Themes Folder", OpenThemesFolder),
        a("Reload Themes", ReloadThemes),
        a("Quit", Quit),
    ]
}

// ---- User keymap -----------------------------------------------------------------

/// Marks bindings from `keymap.json`, so shortcut hints show them first.
pub const USER_BINDING: KeyBindingMetaIndex = KeyBindingMetaIndex(1);

pub fn keymap_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("keymap.json"))
}

/// One block of `keymap.json`: bindings, optionally limited to a context.
#[derive(Deserialize)]
struct Section {
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    bindings: HashMap<String, serde_json::Value>,
}

/// Replaces all key bindings with the built-in keymap plus the user's
/// `keymap.json`. User bindings come later, so they win. Returns problems
/// with the user file; the bindings that are fine still apply.
pub fn install(cx: &mut App) -> Vec<String> {
    let (user, problems) = user_bindings(cx);
    cx.clear_key_bindings();
    cx.bind_keys(bindings());
    cx.bind_keys(user);
    problems
}

fn user_bindings(cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let Some(text) = keymap_path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return (out, problems);
    };
    if text.trim().is_empty() {
        return (out, problems);
    }
    let sections: Vec<Section> = match serde_json::from_str(&strip_jsonc(&text)) {
        Ok(sections) => sections,
        Err(err) => return (out, vec![format!("keymap.json: {err}")]),
    };
    // Find Action's names work too, so nobody has to look up ids.
    let by_label: HashMap<&'static str, Box<dyn Action>> =
        action_entries().into_iter().map(|e| (e.name, e.action)).collect();
    for section in sections {
        let context = match section.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => match KeyBindingContextPredicate::parse(c) {
                Ok(predicate) => Some(Rc::new(predicate)),
                Err(err) => {
                    problems.push(format!("keymap.json: invalid context \"{c}\": {err}"));
                    continue;
                }
            },
            None => None,
        };
        for (keys, value) in section.bindings {
            let action: Box<dyn Action> = match &value {
                // null removes a built-in binding.
                serde_json::Value::Null => Box::new(NoAction),
                serde_json::Value::String(name) => match by_label.get(name.as_str()) {
                    Some(action) => action.boxed_clone(),
                    None => match cx.build_action(name, None) {
                        Ok(action) => action,
                        Err(_) => {
                            let names = by_label.keys().copied().chain(cx.all_action_names().iter().copied());
                            problems.push(match theme::closest(name, names) {
                                Some(near) => format!("keymap.json: unknown action \"{name}\" (did you mean \"{near}\"?)"),
                                None => format!("keymap.json: unknown action \"{name}\""),
                            });
                            continue;
                        }
                    },
                },
                _ => {
                    problems.push(format!("keymap.json: \"{keys}\" should map to an action name or null"));
                    continue;
                }
            };
            match KeyBinding::load(&keys, action, context.clone(), false, None, cx.keyboard_mapper().as_ref()) {
                Ok(binding) => out.push(binding.with_meta(USER_BINDING)),
                Err(err) => problems.push(format!("keymap.json: invalid keys \"{keys}\": {err}")),
            }
        }
    }
    (out, problems)
}

/// Written by Open Keymap when there's no file yet.
pub const KEYMAP_TEMPLATE: &str = r#"// Kin key bindings, added on top of the built-in JetBrains keymap.
// Changes apply when you save this file.
//
// Keys: "cmd-shift-d", "ctrl-alt-l", "f5", or two strokes like "cmd-k cmd-s".
//       "secondary" means Cmd on macOS and Ctrl elsewhere.
// Actions: a name from Find Action ("Duplicate Line or Selection") or an
//          action id ("editor::DuplicateLine"). null removes a built-in binding.
// Context: "Editor" for the code editor; leave it out to bind everywhere
//          (bindings in a more specific context still win there).
[
  // {
  //   "context": "Editor",
  //   "bindings": {
  //     "secondary-shift-d": "Duplicate Line or Selection",
  //     "secondary-d": null
  //   }
  // }
]
"#;
