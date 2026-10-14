<p align="center">
  <img src="assets/app-icon/logo-macos.svg" alt="Kin logo" width="128">
</p>

# Kin

A small, fast code editor written in Rust, rendered entirely on the GPU with
[GPUI](https://www.gpui.rs) (Metal on macOS, DirectX on Windows), with
platform-native text rendering (CoreText / DirectWrite).

<p align="center">
  <img src="docs/screenshots/default.png" alt="Kin with the file tree and a Rust file open, in the default Clay Black theme">
</p>

## Features

- Rope-backed buffer (`ropey`) with undo/redo; only visible lines are shaped and painted
- Tree-sitter syntax highlighting for JavaScript/JSX, TypeScript/TSX, HTML (including `<script>` blocks),
  CSS, Rust, Go, Python, PHP, shell scripts, JSON, YAML, TOML and Markdown
- Linting off the main thread:
  - **JavaScript** — syntax errors plus `no-unused-vars`, `no-undef`, `no-const-assign`,
    `no-dupe-keys`, `eqeqeq`, `use-isnan`, `no-cond-assign`, `no-debugger`, `no-var` (built on oxc)
  - **TypeScript** — the same rules except `no-undef` (TypeScript checks names itself); type-only
    uses count as uses; `.d.ts` files get syntax errors only
  - **HTML** — unclosed/stray/void closing tags, duplicate attributes and ids, `img` alt,
    `lang`, doctype, `<title>`, obsolete tags, `target="_blank"` without `rel`; inline scripts are linted as JS
  - **Markdown** — the markdownlint rules (MD001–MD07x: heading structure, list style, code
    fence languages, trailing spaces, duplicate headings, broken reference and relative links…)
    via [rumdl](https://rumdl.dev). A project's `.markdownlint.json`/`.yaml`, `.rumdl.toml` or
    `pyproject.toml` `[tool.rumdl]` is used when present; otherwise line length (MD013) is off
  - **ESLint** — if the file's project has `node_modules/.bin/eslint`, it runs on open/save with the
    project's own config (JavaScript and TypeScript); built-in rules then drop back to syntax errors only
  - **CSS, Rust, Go, Python, PHP, shell, JSON, YAML, TOML** — syntax errors from the tree-sitter parser
    (unexpected and missing tokens)
- Language servers (LSP) for code intelligence: diagnostics, completion, hover documentation,
  signature help, Go to Declaration, Find Usages, Rename and context actions (quick fixes and
  refactorings); see [Language servers](#language-servers)
- Reformat Code (⌥⌘L / Ctrl+Alt+L) and optional format on save, with the project's formatter
  (e.g. its Prettier), the language server, or rustfmt/gofmt/ruff/black/taplo/shfmt on PATH
- Inline diagnostics, gutter markers, scrollbar markers and a Problems panel
- Multi-cursor (next/all occurrences, ⌥-click, add caret above/below), syntax-aware Extend Selection,
  auto-closing pairs, smart indent, line/block comments, move/duplicate/join/delete lines
- Matching-bracket highlight (ignores brackets in strings and comments) and an editor right-click menu
- Soft wrap that keeps indentation, on by default for Markdown and plain text (View → Soft-Wrap
  toggles it per file)
- Find/Replace bar with case, whole-word and regex options; Find/Replace in Files with preview
  (searches unsaved buffers too, honors .gitignore)
- Search Everywhere (double Shift), fuzzy Go to File, Recent Files, Find Action, Go to Line, and
  File Structure: the current file's functions, classes, headings, selectors or keys
- Git: clone, init, gutter change markers with rollback, inline blame, file status colors, a Commit panel
  (stage, discard, commit, amend, push), staging single changes, side-by-side diffs with changed words
  highlighted, a log with a branch graph and file history, merge/rebase with conflict resolution in
  the editor, branches, pull and fetch
- Welcome screen with quick-start tiles, Recent Projects, a Git card for the open repository and quick actions
- 31 built-in themes (24 dark, 7 light) and JSON user themes with live preview
- Multiple windows, one project each (like Zed), with a Recent Projects picker
- Integrated terminal (⌥F12 / Alt+F12): your login shell in tabs, full colors, mouse support for
  full-screen programs, selection and copy/paste; shell keys like Ctrl+R reach the shell
- Code folding by indentation (Markdown by section), with gutter arrows and a `⋯` placeholder;
  folds are kept with the project
- Split panes (side by side or stacked) with draggable dividers; drag tabs to reorder them or
  move them between panes; pinned tabs; a tab menu with Close Others / All / to the Right
- Run and test: ▶ in the gutter next to tests and `main` functions (Rust, Go, Python, JS/TS),
  project commands (cargo, npm scripts, go, make, pytest) and your own run configurations, each
  in its own terminal tab with a pass/fail icon; see [Run and test](#run-and-test)
- Image preview: PNG, JPEG, GIF (animated), WebP, BMP, TIFF, ICO, TGA and QOI files open as a
  picture with their dimensions and size (SVG stays text); Git diffs show an image's before and after
- File tree with new file/folder, rename, duplicate, copy/paste, Move to Trash, Find in Folder and
  Reveal in Finder; tabs, custom title bar, IME support
- A Settings dialog for every option, with search across its pages; see [Settings](#settings)
- Automatic updates from GitHub Releases (signed), installed with **Restart to Update**; see
  [Releases and updates](#releases-and-updates)

## Running

```bash
cargo run --release -- path/to/folder path/to/file.js
```

Each folder on the command line opens in its own window; files open in the first one.

GPUI's `runtime_shaders` feature is enabled, so no separate Metal toolchain is needed on macOS.

### App icon

The icon (a black ‹K› on a lavender, rose and orange gradient) lives in [`assets/app-icon/`](assets/app-icon):

- **macOS:** the Dock icon is set at startup, so it shows even with `cargo run`. To build a
  `Kin.app` that uses `macos/Kin.icns`, run `cargo install cargo-bundle` and then
  `cargo bundle --release`.
- **Windows:** `build.rs` embeds `windows/kin.ico` into the executable, so it shows in
  Explorer, the title bar and the taskbar.
- **Linux:** the window's app id is `dev.kin.editor`. Copy `linux/hicolor/` into
  `~/.local/share/icons/hicolor/` and `linux/dev.kin.editor.desktop` into
  `~/.local/share/applications/`.

## Releases and updates

Pushing a version tag builds and publishes a GitHub Release
([`.github/workflows/release.yml`](.github/workflows/release.yml)):

```bash
# bump `version` in Cargo.toml first; the tag must match it
git tag v0.2.0
git push origin v0.2.0
```

The release gets a universal macOS `Kin.app` (`.dmg` and `.zip`), a Windows `.zip` and a
Linux `.tar.gz`, plus `latest.json`, which the workflow signs with
[minisign](https://jedisct1.github.io/minisign/) using the `MINISIGN_SECRET_KEY` repository
secret. Tags with a hyphen (`v0.3.0-beta.1`) become pre-releases, which the updater skips.

Released builds check for updates shortly after launch and every few hours (unless
`auto_update` is `false`; **Check for Updates…** works either way), download in the background
and show **Restart to Update** in the status bar. They only install an update whose
`latest.json` carries a valid signature from the key in `src/updater.rs`. Builds from source
don't update themselves. **About Kin** (first in the app menu, and in Find Action) shows the
version and the commit it was built from; **Copy Version** copies both for bug reports.

The macOS app is only ad-hoc signed (no Apple Developer ID), so macOS blocks the first launch
of a downloaded copy: allow it under System Settings → Privacy & Security → **Open Anyway**, or
run `xattr -dr com.apple.quarantine /Applications/Kin.app`. Updates don't need this.

## Key bindings

Kin follows the JetBrains IDE keymaps: the macOS keymap on macOS and the
Windows/Linux keymap elsewhere. On macOS the Ctrl variants of Find/Replace
(Ctrl+F, Ctrl+R, Ctrl+Shift+F, Ctrl+Shift+R) work too. Every action is also
reachable through Find Action, which shows its shortcut.

| Action | macOS | Windows / Linux |
| --- | --- | --- |
| Search Everywhere | Double ⇧ | Double Shift |
| Go to File | ⌘⇧O | Ctrl+Shift+N |
| Recent Files | ⌘E | Ctrl+E |
| Find Action | ⌘⇧A | Ctrl+Shift+A |
| Go to Line:Column | ⌘L | Ctrl+G |
| File Structure | ⌘F12 | Ctrl+F12 |
| Find / Replace | ⌘F / ⌘R | Ctrl+F / Ctrl+R |
| Next / previous occurrence | ⌘G / ⌘⇧G, F3 / ⇧F3 | F3 / Shift+F3 |
| Find / Replace in Files | ⌘⇧F / ⌘⇧R | Ctrl+Shift+F / Ctrl+Shift+R |
| Extend / shrink selection | ⌥↑ / ⌥↓ | Ctrl+W / Ctrl+Shift+W |
| Add next occurrence / unselect / select all | ⌃G / ⌃⇧G / ⌃⌘G | Alt+J / Alt+Shift+J / Ctrl+Alt+Shift+J |
| Duplicate / delete line | ⌘D / ⌘⌫ | Ctrl+D / Ctrl+Y |
| Move line up / down | ⌥⇧↑ / ⌥⇧↓ | Alt+Shift+↑ / ↓ |
| Join lines | ⌃⇧J | Ctrl+Shift+J |
| Line / block comment | ⌘/ / ⌥⌘/ | Ctrl+/ / Ctrl+Shift+/ |
| Toggle case | ⌘⇧U | Ctrl+Shift+U |
| Start new line / line above | ⇧↩ / ⌥⌘↩ | Shift+Enter / Ctrl+Alt+Enter |
| Add caret above / below | ⌥⌘↑ / ⌥⌘↓ | Ctrl+Alt+↑ / ↓ |
| Project sidebar / Problems | ⌘1 / ⌘6 | Alt+1 / Alt+6 |
| Terminal (open, focus, hide) | ⌥F12 | Alt+F12 |
| Run… / Run again | ⌃⌥R / ⌃R | Alt+Shift+F10 / Shift+F10 |
| Run test or function at caret | ⌃⇧R | Ctrl+Shift+F10 |
| Stop | ⌘F2 | Ctrl+F2 |
| Split and Move Right / Down | ⌘\\ / ⌘⇧\\ | Ctrl+\\ / Ctrl+Shift+\\ |
| Next pane | ⌥⇥ | — |
| New / close terminal tab (in the terminal) | ⌘T / ⌘W | Ctrl+Shift+T / Ctrl+Shift+W |
| Copy / paste / clear (in the terminal) | ⌘C / ⌘V / ⌘K | Ctrl+Shift+C / Ctrl+Shift+V / — |
| Close tab / next / previous | ⌘W / ⌃→ / ⌃← | Ctrl+F4 / Alt+→ / Alt+← |
| Open file / folder | ⌘O / ⌥⌘O | Ctrl+O / Ctrl+Alt+O |
| New window / close window | ⌘⇧N / ⌘⇧W | — / Alt+F4 |
| Select theme | ⌃\` | Ctrl+\` |
| Commit panel / toggle it | ⌘K / ⌘0 | Ctrl+K / Alt+0 |
| Push / Update Project (pull) | ⌘⇧K / ⌘T | Ctrl+Shift+K / Ctrl+T |
| Rollback lines (change under caret) | ⌥⌘Z | Ctrl+Alt+Z |
| Next / previous change | ⌃⌥⇧↓ / ⌃⌥⇧↑ | Ctrl+Alt+Shift+↓ / ↑ |
| Git log | ⌘9 | Alt+9 |
| Next / previous change (in a diff) | F7 / ⇧F7 | F7 / Shift+F7 |
| Settings | ⌘, | Ctrl+Alt+S |
| Increase / decrease font size | ⌥⌘= / ⌥⌘- | Ctrl+Alt+= / Ctrl+Alt+- |
| Collapse / expand (fold) | ⌘- / ⌘= | Ctrl+- / Ctrl+= |
| Collapse all / expand all | ⌘⇧- / ⌘⇧= | Ctrl+Shift+- / Ctrl+Shift+= |
| Basic completion | ⌃Space | Ctrl+Space |
| Go to Declaration | ⌘B or ⌘-click | Ctrl+B or Ctrl+click |
| Find Usages | ⌥F7 | Alt+F7 |
| Rename | ⇧F6 | Shift+F6 |
| Show Context Actions | ⌥↩ | Alt+Enter |
| Quick Documentation | F1 | Ctrl+Q |
| Reformat Code | ⌥⌘L | Ctrl+Alt+L |

### Your own bindings

Change a shortcut in **Settings → Keymap**: click an action and press the new keys (Esc
cancels, **No Shortcut** removes it, the undo button restores the built-in keys). These go into
`keymap.json` in the config folder, which **Open keymap.json** (on that page and in Find Action)
opens for editing by hand. Its bindings are added on top of the built-in keymap, apply when you
save, and show in menus and Find Action:

```jsonc
[
  {
    "context": "Editor",
    "bindings": {
      "secondary-shift-d": "Duplicate Line or Selection", // a Find Action name…
      "f6": "editor::DeleteLine",                          // …or an action id
      "secondary-d": null                                  // null removes a built-in binding
    }
  }
]
```

`secondary` is ⌘ on macOS and Ctrl elsewhere. Leave out `context` to bind everywhere; a binding
in a more specific context (like `"Editor"`) still wins there. Mistakes show up as a notification.

## Language servers

Kin starts a language server the first time you open a file it handles, one per server and
project, and shows it in the status bar (with indexing progress). It never downloads anything:
it uses the server in the project's `node_modules/.bin` or on your PATH (also the login shell's
PATH, `~/.cargo/bin`, `~/go/bin`, Homebrew and nvm, so it works when Kin is opened from the Dock).
If none is installed, the status bar says **No language server**; click it to copy the install
command.

| Language | Server | Install |
| --- | --- | --- |
| Rust | rust-analyzer | `rustup component add rust-analyzer` |
| Go | gopls | `go install golang.org/x/tools/gopls@latest` |
| JavaScript, TypeScript | typescript-language-server or vtsls | `npm install -g typescript typescript-language-server` |
| Python | basedpyright, pyright or pylsp | `pip install basedpyright` |
| PHP | intelephense or phpactor | `npm install -g intelephense` |
| HTML, CSS, JSON | vscode-langservers-extracted | `npm install -g vscode-langservers-extracted` |
| YAML | yaml-language-server | `npm install -g yaml-language-server` |
| TOML | taplo | `cargo install taplo-cli --locked --features lsp` |
| Shell | bash-language-server | `npm install -g bash-language-server` |
| Markdown | marksman | `brew install marksman` |

With a server running, its diagnostics replace Kin's built-in syntax check (the other built-in
rules and ESLint still run). Hover a symbol for its documentation and any problem there (the
popover scrolls, and a copy button that appears on hover copies its text); completion opens as you type (Enter inserts, Tab replaces the rest of the word). **Restart
Language Server** (Find Action) restarts it after installing or when it's stuck. To use another
server or turn one off, see `language_servers` below.

## Formatting

**Reformat Code** formats the file, or the selection when the language server supports range
formatting. Kin picks, in order: a formatter installed in the project (`node_modules/.bin`, e.g.
Prettier), the language server, then a formatter on PATH: Prettier (JS, TS, CSS, HTML, JSON,
YAML, Markdown), rustfmt (with the edition from Cargo.toml), gofmt, ruff or black, taplo, shfmt.
Only the changed lines are replaced, so the caret stays put and one undo reverts it. Formatters
read their own config (`.prettierrc`, `rustfmt.toml`, `pyproject.toml`…). Set `format_on_save`
(per language too) to format on Save; choose a formatter per language with `formatters`.

## Panes and tabs

**Split and Move Right / Down** (View menu, a tab's right-click menu, or ⌘\\ / ⌘⇧\\) moves the
current tab into a new pane; if it's the pane's only tab, the new pane starts empty for the next
file you open. A file has one editor, so it isn't shown in two panes at once. Drag a tab to
reorder it, onto another pane's tab bar or editor to move it there; a pane closes with its last
tab. Drag the line between panes to resize them.

Right-click a tab for Close, Close Others, Close All, Close Tabs to the Right, Pin Tab and the
split commands. Pinned tabs stay first, show a pin (click it to unpin) and are kept by Close
Others / Close All. The panes, their tabs, sizes and pins come back with the project.

## Terminal

**Terminal** (⌥F12 / Alt+F12, or the status bar button) opens a shell in the project folder in
the bottom panel, which it shares with Problems. Press it again to jump between the terminal and
the editor. Each **+** tab is its own shell; a tab closes when its shell exits.

While the terminal has focus, keys go to the shell: Ctrl shortcuts (Ctrl+R history search, Ctrl+F,
Ctrl+E…) aren't taken by Kin, except a few for getting around (⌥F12, tab switching, Find Action).
On macOS, Option types characters as usual (`@` on many layouts). Full-screen programs (vim,
htop, less) get mouse and wheel events; hold Shift to select text in them instead. Colors come
from the theme's `terminal_*` keys.

## Run and test

A ▶ in the gutter marks what can run in the file; click it to run that one thing:

| Language | ▶ on | Runs with |
| --- | --- | --- |
| Rust | `#[test]` functions, `#[cfg(test)]` modules, `fn main` | `cargo test … --exact`, `cargo run` (with `--bin`/`--example`/`--test` as needed) |
| Go | `Test…`, `Benchmark…`, `Example…` functions, `func main` | `go test -run '^Name$'`, `go run .` |
| Python | `test_…` functions, `Test…` classes and methods, `if __name__ == "__main__"` | `python3 -m pytest file::Class::test`, `python3 file` |
| JavaScript, TypeScript | `test(…)`, `it(…)`, `describe(…)` | the project's vitest or jest, else `node --test` |

**Run at caret** (⌃⇧R / Ctrl+Shift+F10) runs the innermost test or function around the caret.
**Run…** (⌃⌥R / Alt+Shift+F10) lists the last run first, then your `run_configurations`, the
project's commands (cargo run/test/build and binaries, `package.json` scripts with its package
manager, go, Makefile targets, pytest) and the current file's tests. **Run** (⌃R / Shift+F10)
repeats the last one, **Stop** (⌘F2 / Ctrl+F2) interrupts it.

Each run gets a terminal tab named after it, which stays open with the exit code when it ends and
shows ▶ while running, ✓ on success and ✕ on failure. Running the same thing again reuses its tab.
Unsaved files are saved first. Add your own commands in `settings.json`:

```jsonc
"run_configurations": [
  { "name": "Dev server", "command": "npm run dev -- --port 3000" },
  { "name": "API tests", "command": "pytest -x tests/api", "cwd": "backend" }
]
```

`cwd` is relative to the project folder. Commands run through your login shell, so aliases and
PATH work as in the terminal.

## Settings

**Settings…** (⌘, / Ctrl+Alt+S, or the gear at the right of the status bar) opens the Settings
dialog, the same on every platform. Its pages are Appearance (themes), Editor (font, indentation,
wrapping), Saving, Languages (per-language options, language server and formatter), Keymap and
Terminal & Run. The search field looks through all of them; with it focused, ↑/↓ switch
pages. Changes apply right away. An option you changed has an undo button next to its name,
which puts the default back.

Everything is stored in `settings.json`. **Open settings.json** (at the bottom of the dialog and
in Find Action) opens it, created with every option commented out. Edits to the file apply when
you save (or when you come back to Kin after editing it elsewhere). Comments and trailing commas
are fine, and Kin keeps them when it changes a value itself.

| Setting | Default | |
| --- | --- | --- |
| `theme` | `"Clay Black"` | see [Themes](#themes) |
| `auto_save` | `"after_delay"` | `"off"`, `"after_delay"`, `"on_focus_change"`, `"on_window_change"` |
| `auto_save_delay` | `1000` | milliseconds |
| `editor_font_family` | built-in choice | any installed font |
| `editor_font_size` | `14` | the zoom actions adjust it on top (remembered between launches) |
| `editor_line_height` | `1.57` | multiple of the font size |
| `tab_size` | `4` (2 for HTML, JSON, YAML) | columns per indent and per tab |
| `hard_tabs` | `false` (`true` for Go) | indent with tabs |
| `soft_wrap` | `false` (`true` for Markdown and plain text) | |
| `trim_trailing_whitespace` | `false` | on save; lines with a caret are left alone |
| `insert_final_newline` | `false` | on save |
| `format_on_save` | `false` | reformat on Save (auto-save never reformats) |
| `languages` | | per-language overrides of the six above, by the name in the status bar |
| `language_servers` | | per language: `false` turns its server off, `["cmd", "args"]` replaces it |
| `terminal_shell` | login shell | the terminal's shell and arguments, e.g. `["/opt/homebrew/bin/fish", "--login"]` |
| `run_configurations` | | `[{ "name", "command", "cwd" }]` added to Run…; see [Run and test](#run-and-test) |
| `formatters` | | per language: `"language_server"`, a command reading stdin and writing stdout (`{file}` is the path), or `false` |
| `auto_update` | `true` | download new releases in the background; see [Releases and updates](#releases-and-updates) |

```jsonc
{
  "editor_font_size": 15,
  "tab_size": 2,
  "languages": {
    "Python": { "tab_size": 4 },
    "Markdown": { "soft_wrap": false }
  }
}
```

A file's existing indentation wins over `tab_size`/`hard_tabs`. A project's `.editorconfig`
wins over both: `indent_style`, `indent_size`, `tab_width`, `trim_trailing_whitespace` and
`insert_final_newline` are supported, read up to the nearest `root = true`.

## Windows and projects

Each window holds one project folder, so several projects can be open side by side.

- **Open Folder…** (⌥⌘O / Ctrl+Alt+O) opens the folder in a new window. A window with no
  project yet is reused instead.
- **Open Folder in This Window…** replaces the window's project. Its tabs, including unsaved
  edits, are kept for when you open it again.
- **Recent Projects…** (also opened by clicking the project name in the title bar) lists recent
  folders. Enter opens one in this window; ⌘↩ / Ctrl+Enter or ⌘/Ctrl-click opens it in a new
  window. The welcome screen's Recent Projects list works the same way.
- **New Window** (⌘⇧N) opens an empty window.
- Opening a folder that's already open in another window brings that window forward.
- Quitting or closing a project window doesn't ask about unsaved edits: they're kept and come back
  with the project's tabs. Windows without a project folder still ask.

These are all in the File menu and Find Action.

## Remembered state

Kin reopens the last project folder when started without arguments, lists the last ten
folders under Recent Projects (shared by all windows; hover a welcome-screen row to remove it), and remembers
panel sizes, whether the sidebar, Problems panel, terminal and Git log are open (all but the sidebar start
closed the first time), which sidebar view (Files or Commit) shows, the font zoom and the diff layout. This lives in
`state.json` next to `settings.json` in the config folder.

Each project also reopens with the tabs it had, the active tab and each caret. Unsaved edits
are kept as well (like VS Code's hot exit) and undo goes back to the file's saved text. These
live in `sessions/` in the config folder, one file per project.

## Git

Kin uses the `git` command line, so your config, credential helpers, SSH keys and hooks
all apply. Prompts are disabled, so a remote that needs a password fails fast with a
message instead of hanging; use an SSH agent or a credential helper.

- **Gutter:** green/blue/red bars mark added, changed and deleted lines against `HEAD`,
  updated as you type; staged changes are drawn faded. Click a bar to see the original lines,
  **Stage** (or **Unstage**) just that change, or **Rollback** it. Stage Lines / Unstage Lines are
  also in the editor's right-click menu and Find Action, and stage what the editor shows, unsaved
  edits included.
- **Inline blame:** the caret line shows its author, age and commit message
  (Find Action → Toggle Inline Git Blame).
- **File tree:** changed files are colored with a status letter; folders containing
  changes are tinted.
- **Commit panel** (⌘K / ⌘0, or the Commit button at the left of the status bar, which shows the
  number of changes): the branch with fetch, pull and push at the top, then staged and unstaged
  changes with stage, unstage, open and roll back on hover; click a file for its diff. Write a message and press ⌘↩ to commit (with nothing
  staged, everything is committed), or use **Commit and Push**. **Amend** starts from the
  previous message. In a folder that isn't a repository it offers **Initialize Repository**.
- **Diffs** show side by side or unified (the choice is remembered), with syntax colors and the
  changed words within a line highlighted. Each change has **Stage** / **Unstage**; F7 / ⇧F7 step
  through changes; collapsed unchanged lines expand on click. Images show the previous and new
  version side by side; other binary files say there's no text diff.
- **Branches:** the Commit panel's header shows the branch, with the number of commits to pull
  and to push on its Pull and Push buttons. Click the branch (or use **Branches…** in the Git
  menu) to switch to a local or remote branch, or type a new name to create one. Hover another branch
  for **Merge** (into the current one) and **Rebase** (the current one onto it).
- **Merge conflicts:** a merge or rebase that stops on conflicts shows a banner in the Commit
  panel with **Abort**. Conflicted files open in the editor, where each conflict is tinted
  (current side green, incoming blue) with **Accept Current / Accept Incoming / Accept Both**
  on its first line (also in the right-click menu and Find Action). Mark a file resolved with
  ✓ on hover (Kin asks if markers remain), then **Commit Merge** / **Continue**. The merge
  message is filled in, and committing is blocked while conflicts remain.
- **Log** (⌘9 / Alt+9, or the status bar button): commits of all branches (toggle) with a branch
  graph, branch and tag labels, and a filter by message, author, hash or branch. Select a commit
  for its message, author, date, hash (copy) and changed files; click a file for its diff.
  **Show History** (editor right-click menu, Git menu) lists one file's commits, following renames.
- **Push / Pull / Fetch:** from the Commit panel, the keyboard, or Find Action. Pushing a
  new branch sets its upstream. Results appear as notifications.
- **Push** (⌘⇧K / Ctrl+Shift+K) first opens a dialog with the branch and where it goes (a new
  branch is marked), and the commits to push. Click a commit for its message and files, and a
  file for its diff (closing it returns to the dialog). It warns when the remote has commits you
  don't have; **Force push** replaces them only if the remote hasn't moved since your last fetch
  (`--force-with-lease`). Enter pushes, Esc cancels.

- **Clone:** **Clone Repository…** on the welcome screen, in the File and Git menus, or Find Action
  (also as "Get from Version Control"). Enter an HTTPS or SSH URL (one on the clipboard is
  filled in) and a parent folder; progress is shown live, **Stop** cancels and removes the partial
  folder, and the clone opens when it's done. **Shallow clone** fetches only the latest commit.
- **Welcome screen:** with no tabs open, it shows the current branch, ahead/behind counts,
  changed files, the last commit and the remote (click to open it in the browser), with
  Commit, Update, Push, Fetch and Branches. In a folder that isn't a repository it offers
  **Initialize**.

Status refreshes after saves and Git actions, when the window regains focus, and every few
seconds. Open files without unsaved edits reload when a checkout, pull or rollback changes
them on disk.

## Themes

Pick a theme with **Select Theme** (Ctrl+\` like JetBrains' Quick Switch Scheme, or Find
Action). Moving through the list previews each theme live; Enter keeps it
(saved to `settings.json`), Escape restores the previous one.

Built in: Clay Black (default; near-black with neutral grays), Clay Dark, One Dark, Dracula, Nord, Tokyo Night, Gruvbox Dark,
Catppuccin Mocha, Rosé Pine, Solarized Dark, Monokai Pro, Everforest Dark, Kanagawa, Ayu Dark, Night Owl,
Material Palenight, GitHub Dark, Poimandres, Oxocarbon, SynthWave '84, Vesper, Horizon, Vitesse Dark, Cobalt2,
and the light themes Clay Light, GitHub Light, Solarized Light, Catppuccin Latte, Rosé Pine Dawn, Gruvbox Light
and One Light. Their sources in [`assets/themes/`](assets/themes) use the same format as user themes.

### Making your own

Put a `.json` file in the themes folder — `~/.config/kin/themes/` on macOS and Linux
(`$XDG_CONFIG_HOME/kin/themes` if set), `%APPDATA%\kin\themes\` on Windows.
**Open Themes Folder** (in Find Action or the theme picker) creates it with a starter
theme. Then use **Reload Themes**, or just reopen the picker.

A theme only needs the colors it changes. Everything else comes from `extends`, or from
Clay Black / Clay Light depending on `appearance`:

```json
{
  "name": "Forest",
  "appearance": "dark",
  "extends": "Gruvbox Dark",
  "colors": { "background": "#1c2420", "accent": "#7fbf8f", "accent_soft": "#7fbf8f2b" },
  "syntax": { "keyword": "#7fbf8f" }
}
```

Colors are `#rgb`, `#rrggbb` or `#rrggbbaa`. A user theme with the same name as a built-in
replaces it. Mistakes (unknown keys, bad colors, `extends` loops) are listed at the bottom
of the theme picker, with a "did you mean" hint for misspelled keys.

| Key | Used for |
| --- | --- |
| `background`, `panel`, `elevated` | editor, sidebars/bars, popups |
| `border`, `border_subtle` | dividers |
| `hover`, `active` | hovered and pressed rows/buttons (usually translucent) |
| `text`, `text_muted`, `text_dim` | primary, secondary and faint text |
| `accent`, `accent_soft` | highlights, focused inputs, selected rows |
| `cursor`, `selection`, `selection_inactive`, `current_line` | editor caret and selection |
| `line_number`, `line_number_active`, `indent_guide` | gutter and guides |
| `search_match`, `search_active` | find results |
| `bracket_match` | the bracket pair around the cursor |
| `error`, `warning`, `info`, `success` | diagnostics and status |
| `git_added`, `git_modified`, `git_deleted`, `git_untracked`, `git_conflict` | Git gutter bars, file status colors and diffs |
| `terminal_black` … `terminal_white`, `terminal_bright_black` … `terminal_bright_white` | the terminal's 16 ANSI colors |

`syntax` keys: `plain`, `keyword`, `string`, `number`, `constant`, `function`, `method`,
`property`, `type`, `variable_builtin`, `comment`, `operator`, `punctuation`, `tag`,
`attribute`, `escape`. Set `"italic_comments": false` for upright comments.

## Layout

- `src/main.rs` — app setup: fonts, theme, keymap, menus, windows, command-line paths
- `src/workspace.rs` — window chrome: title bar, file tree, panes and tabs, problems, status bar,
  modals and file operations
- `src/document.rs` — rope buffer, edits, undo history, file I/O
- `src/editor.rs` — editor view, input handling and the GPU element that paints it
- `src/wrap.rs` — soft wrap: where long lines break into display rows
- `src/fold.rs`, `src/editor/fold.rs` — code folding regions and fold UI
- `src/syntax.rs` — tree-sitter parsing, highlighting, bracket matching and file outlines
- `src/lsp/` — language server client: JSON-RPC transport, protocol types, server discovery
- `src/editor/lsp.rs` — completion, hover, signature help and LSP requests in the editor
- `src/format.rs`, `src/editor/format.rs` — Reformat Code and format on save
- `src/lint/` — built-in JS/HTML/Markdown linters, tree-sitter syntax errors and the ESLint bridge
- `src/search.rs`, `src/find_bar.rs`, `src/project_search.rs` — in-file and project search
- `src/picker.rs` — Search Everywhere, Go to File, Recent Files, Recent Projects, Find Action,
  File Structure, Go to Line
- `src/terminal.rs` — the integrated terminal
- `src/run.rs`, `src/editor/run.rs` — run and test targets and the gutter ▶
- `src/keymap.rs` — JetBrains-style key bindings
- `src/git.rs`, `src/git_panel.rs`, `src/diff_view.rs`, `src/git_log.rs`, `src/push_dialog.rs`,
  `src/branch_picker.rs`, `src/clone_dialog.rs` — Git backend and UI
- `src/conflict.rs`, `src/editor/conflict.rs` — merge conflict markers and their resolution
- `src/preview.rs` — image files
- `src/settings.rs`, `src/editorconfig.rs`, `src/preferences.rs`, `src/preferences/` —
  `settings.json`, `.editorconfig` and the Settings dialog
- `src/state.rs` — remembered UI state and per-project sessions
- `src/theme.rs`, `src/theme_picker.rs`, `assets/themes/` — theme loading, picker and built-in themes
- `src/updater.rs`, `.github/workflows/release.yml` — auto-update and the release build
- `src/about.rs` — the About Kin window
- `src/ui.rs`, `src/context_menu.rs`, `src/input_dialog.rs` — shared widgets, menus and prompts
- `src/assets.rs`, `assets/icons/` — rounded, Lucide-style SVG icons and the title font embedded into the binary
- `assets/file-icons/`, `src/file_icons.rs` — file-type icons ([Charmed Icons](https://github.com/jmesrje/zed-charmed-icons)) and their file-name associations
- `assets/app-icon/` — the app logo (`logo.svg`) and its per-platform icons

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

Bundled third-party assets keep their own licenses: file-type icons from
[Charmed Icons](https://github.com/littensy/charmed-icons) by littensy and jmesrje (MIT,
[`assets/file-icons/LICENSE`](assets/file-icons/LICENSE)) and the Vidaloka font (SIL Open Font
License, [`assets/fonts/OFL.txt`](assets/fonts/OFL.txt)).
