# Kuana vs Zed: architecture review

*October 2026. Kuana `2c7e082` (~47.5k lines, one crate) compared with Zed `main` @ `5a5fbe7`.*

Method: 14 reviewers each read one Kuana subsystem in full next to its Zed counterpart:

- buffer
- display pipeline
- editor
- syntax
- extensions
- LSP
- workspace
- project/fs
- git
- terminal
- settings/theme
- debugger/tasks
- a cross-cutting performance sweep
- code structure/testing

They produced 126 findings, all listed in the appendix. Each headline claim below was re-checked against the code. Findings that only appear in the appendix were *not* verified one by one, so read their line numbers as pointers.

> **License note.** Most Zed crates that matter here are **GPL-3**: `text`, `rope`, `language`, `editor`, `project`, `workspace`, `lsp`, `terminal`, `settings`, `theme`, `task`, `dap`, `extension_host` and `grammars`. Kuana is MIT/Apache. Copy Zed's *designs*, not its code. The Apache-licensed pieces you *can* reuse directly are `gpui`, `gpui_sum_tree` (published on crates.io as 0.2.2), `gpui_collections`, `gpui_util` and Zed's `extension_api` WIT. Tree-sitter queries should come from the grammar crates' own `queries/` folders, not from Zed's `crates/grammars`.

---

## Verdict

Kuana's foundations are sound. These choices are right and Zed would be overkill:

- `ropey` with UTF-8 byte offsets
- no CRDT
- the git CLI instead of libgit2
- JSON state files instead of SQLite
- one root folder per window
- a hand-written Vim parser
- Zed's extension folder layout

The problems come from **three structural decisions** that every subsystem then works around:

1. **Nothing is a cached view, and the Workspace builds everything.** Every caret blink, terminal wakeup, transition frame or keystroke re-runs layout for every visible editor, terminal and list. GPUI already has the fix (`AnyView::cached`), and Kuana never calls it.
2. **The main thread does whole-file work on every edit.** This covers the tree-sitter reparse (inside `Document::edit`), the soft-wrap rebuild, and run targets, conflicts, outline and folds, all computed *inside prepaint*, some with disk I/O. Add find-as-you-type, session save and focus-reload of every tab. Zed pushes all of this to background tasks over cheap snapshots, and Kuana already has those snapshots for free: a ropey `Rope` clone and a tree-sitter `Tree` clone are O(1).
3. **Closed lists instead of registries.** The `Language` enum, a 16-entry highlight vocabulary, one language server per language, the `DebugSpec` enum, the `Modal` enum, hard-wired bottom panels, and a 9k-line `Workspace` that owns everything. For an open-source editor this is the extensibility ceiling. Built-in languages and extension languages run **two different code paths**, so extensions are second-class and contributors have two systems to learn.

None of this needs a rewrite. Nearly every item below has a small first step that can land on its own.

---

## Top priorities

Effort sizes: **S** is hours, **M** is days, **L** is a week or more.

| # | What | Why | Effort | First step |
|---|---|---|---|---|
| 1 | **Cache views** (`AnyView::cached`) for editors, terminals, Git panel/log, Problems, debug panel | Whole-window relayout on every notify, the biggest constant-factor perf win | S | `workspace.rs:6771` (`render_pane`) and `:4141` (terminals): `AnyView::from(e.clone()).cached(StyleRefinement::default().size_full())` |
| 2 | **Fix the `Document::edit` result/edit mismatch** | Carets jump to offset 0, and overlapping edits delete text (`()a` + 2 carets + Backspace loses `a`) | S | Normalize and coalesce edits up front; keep no-op edits in the result math |
| 3 | **Reload only when mtime changed** | Window focus re-reads, decodes, compares and **fully re-parses every open tab** and rebuilds the tree | S | `Document::reload_from_disk`: `if !self.changed_on_disk() { return Ok(false) }` (the helper already exists at `document.rs:346`) |
| 4 | **Decide "large file" before parsing** | A 20–500 MB file is fully tree-sitter-parsed on the UI thread *before* the guard switches to plain text | S | `Document::open` checks the size before `from_text` builds `Syntax` |
| 5 | **Lenient settings parsing** | One bad value (`"tab_size": "4"`, an enum from a newer Kuana) resets *every* setting at startup | S | `deserialize_with = lenient` on `Option` fields; collect errors as warnings |
| 6 | **Shell-quote extension task variables** | `run.rs:150` joins expanded args with spaces: paths with spaces break, and file contents (a symbol name) can inject shell | S | `sh_quote` every substituted value; one-pass `$VAR` expansion |
| 7 | **Move derived per-version data off prepaint** | Run targets (with `fs` I/O), conflicts, outline, fold regions and Markdown blocks are recomputed in prepaint after each edit | M | A `Derived<T>` helper (version, `Arc<T>`, task) like the existing lint/blame pattern |
| 8 | **Background + incremental parse** | `Syntax::reparse` runs synchronously in `Document::edit`; injection trees reparse from scratch | M | Edit and reuse the old injection trees; then a 1 ms budget plus background parse |
| 9 | **Incremental soft wrap** | `WrapKey` includes `version`, so each keystroke rewraps the whole file (Markdown/plain text wrap by default) | M | `WrapMap::edit(old_rows, new_rows)` fed by the edit records |
| 10 | **Cap long lines** | Minified or one-line JSON: whole line highlighted, copied, shaped every frame | S–M | Zed's `MAX_LINE_LEN = 1024`: always wrap past ~512 columns, highlight per visible segment |
| 11 | **Add CI** | Only `release.yml` (on tags) exists, so tests and clippy never run on PRs | S | `ci.yml`: Linux `cargo test` + `clippy -D warnings`, then a macOS/Windows matrix |
| 12 | **One language registry** (built-ins as data) | Two pipelines; built-ins can't use the injections/indents/textobjects/runnables queries extensions get | L | `assets/languages/<name>/{config.toml,*.scm}` loaded through the extension path |
| 13 | **Open highlight vocabulary** | 16 fixed buckets drop or mis-map standard captures and Zed theme styles | M | `HighlightId(u16)` → `theme.syntax_styles`, longest-dot-prefix match |
| 14 | **Several language servers per file, plus configurable servers** | An extension server *replaces* the built-in one; there's no `initializationOptions` or `workspace/configuration` | L | `servers::specs(lang) -> SmallVec`, `lsp.<id>.{settings,initialization_options}` |
| 15 | **Buffer entity separate from Editor** | A file can't be in two panes; workspace edits must open tabs; models depend on views | L/XL | Wrap `Document` in an `Entity` inside Editor first (no behavior change) |

---

## Status

Work on branch `review-fixes` (not merged or pushed yet). Phase 1 (the week-1 quick wins in §7) is done:

- [x] Cached views: editors, terminals, Git panel/log, debug panel (`7e10ceb`). Problems is drawn by the workspace, not a view, so it isn't cached yet. Still to do: typed editor events instead of the workspace's blanket `observe`.
- [x] `Document::edit` result mapping + overlap merging (`16af6a8`)
- [x] Reload only on mtime change (`6c224e5`)
- [x] Large-file guard before parsing, `set_language` too (`a19b1d5`)
- [x] Lenient settings (`58abf7a`)
- [x] Extension task quoting (`5608ba6`)
- [x] Markdown preview highlights and images computed in `parse` (`3c1ca20`)
- [x] Parser pool (`e57bd6d`). Injection parsers were already reused across reparses; the cost was in one-off parses.
- [x] LSP cancel-on-drop + 3 s format-on-save limit (`2f58b77`). The timeout lives in `Workspace::save` (dropping the task cancels the request); there's no general `request_timeout`.
- [x] LSP re-register on rename/move/Save As (`cf406c6`)
- [x] Git CRLF/encoding for diff bases and line staging (`181c5bd`). The diff view still compares git's raw text.
- [x] Symlinked folders in the tree (`e6e3c44`)
- [x] CI (`676716a`), clippy-clean (`9bafc1a`), dev profile (`2aac931`)

Phase 2 (§7 weeks 2–3) is done too, except lazy session restore:

- [x] `Derived<T>`: outline, folds, run targets, conflicts and the Markdown preview computed in the background (`c4ec18a`). Skipped, deliberately: a watcher-invalidated cache of run facts (the disk reads now run in the background, debounced), and gating conflict parsing on Git status (it's off the UI thread now, and markers in files Git doesn't flag would stop showing).
- [x] Patch-based position shifting, `debug_line` shifted (`619b643`); `Diagnostic.message: Arc<str>` (`a099a87`). Undo/redo still shift per record (no patch composition).
- [x] Incremental soft wrap (`5a9b5fb`)
- [x] Long-line cap: lines over 1 KB break at 512 columns; visible-only, cached highlighting; folded lines skipped; diagnostics and hunks binary-searched (`de71364`). Also found: bracket matching was quadratic in big arrays (`da415f9`). Selections and scrollbar markers still loop in full.
- [x] Background parse with a 1 ms budget (10 ms on open); injection trees edited and reused (`547a938`)
- [x] Smaller moves: settings early return (`d108da2`), background session save (`b9dad72`), find bar (`2b9176e`), terminal find + newest 5000 matches (`2d06f12`), Go to File top-K scoring (`65912d4`), Problems and Git changes as `uniform_list` (`3e26bc3`, `fa647f3`). Not done: lazy session restore (needs a placeholder tab, since tabs are `Entity<Editor>`); `BackgroundExecutor::scoped` parallel scoring.

Phase 3, in the order agreed (Track B, then A, then C). Track B (servers, debuggers, schemas) is done on branch `track-b` (a worktree at `../code-editor-track-b`, rebased on `review-fixes`):

- [x] Several language servers per file: extension servers run beside Kuana's; diagnostics combined, completion and code actions merged, the rest to the first capable server (`a73f2fe`). Tests drive fake servers (a Python script) through the store.
- [x] `lsp.<id>` settings as in Zed (`settings`, `initialization_options`, `binary`), Zed ids accepted, dotted keys read as nested sections; inlay hints on by default for typescript-language-server, vtsls and gopls (`2717e50`). Checked with basedpyright in the app.
- [x] Extensions declare installers (npm, Go, GitHub assets per platform) (`41c7e2c`)
- [x] Files outside the project reuse the running server (`28f1cf2`); restart no longer marked failed by the old process (`273614a`); `exit` after the `shutdown` answer (`190b849`)
- [x] `DebugAdapter` trait; `.vscode/launch.json` and `.zed/debug.json` configurations in Debug…; extension `[debug_adapters]` (`c8fab59`). Checked a launch.json lldb session in the app.
- [x] JSON schemas for settings.json, keymap.json and themes, given to vscode-json-language-server (`111b085`). Not checked in the app (server not installed here).
- Not done: a data-driven `ServerDef` replacing the built-in Rust table (the specs stay static, with defaults by program); per-language server ordering/disabling by id (`languages.<L>.language_servers`).

Track A (languages and extensions) is done on branch `track-a` (same worktree, on top of `track-b`):

- [x] Built-in languages as data in `assets/languages` (`34de708`); highlights, outlines and comments compared byte for byte against the old code before it was removed. Built-ins use their grammars' injection queries: CSS in `<style>`, fenced code, front matter and HTML in Markdown, HTML around PHP, tagged templates (`cb7721c`).
- [x] Open highlight names: themes color any capture by its longest dotted prefix; Zed themes keep all their syntax colors (`f424fa5`). Built-in themes render the same (checked per byte).
- [x] Reloads reuse unchanged extensions instead of leaking them again (`4d316d5`); not converted to `Arc` (the `&'static` defs run through LSP, debugger and syntax code). Extensions can replace a built-in language's queries (`d84668c`).
- [x] Grammars compile to WebAssembly with a downloaded wasi-sdk (asks first; 180 MB download, 170 MB on disk); native builds behind `native_grammars` for trusted folders (`c01b7f0`). Checked in the app: prompt, download, build, live highlighting.
- [x] Download progress in the status bar with a live tooltip, for every download (`8d6ec70`, asked for during this track).
- [x] `kuana extension check` (`d037bf4`); run against Zed's installed extensions.
- Not done: brackets/indents/text-object queries for built-in languages (they keep the hand-written bracket matcher and indent heuristic); built-in run detection still hand-written.

Track C (structure) is done on branch `track-c` (same worktree, on top of `track-a`; review-fixes and the bracket queries merged in):

- [x] C1: an editor's document is an entity (`0e2df34`); Split Right / Down show the file in both panes, one document in two editors with their own carets, carets following the other's edits through a patch log, sessions bringing splits back (`ff6522e`).
- [x] Repo repair: Git tests run with `GIT_DIR` set had made the repository bare; git commands now clear `GIT_DIR` and friends (`8a583de`).
- [x] C2: `workspace.rs` split into a core and 22 files by area (`4906456`); Problems panel (`0a89d90`), file tree (`eb96eb8`) and terminal panel (`4660ca2`) are entities with events; one `BottomPanel` instead of four flags (`fb2c815`); `ModalLayer` (`e124ee8`). The `PaneItem` seam came later: panes hold editors, images (`ImageView`) and Markdown previews (`MarkdownPreview`), each an `Item` (`src/pane_item.rs`); the Git diff is still a modal.
- [x] C3: one project snapshot, scanned once and updated from watcher events, for Go to File and Find in Files (`fa22068`); language servers get `workspace/didChangeWatchedFiles` (`8dba5a8`). The tree still lists folders as they're expanded (it shows ignored folders, which the snapshot doesn't enter).
- [x] C4: block rows in the display map; an opened Git hunk shows its deleted lines inline (`e157d5c`). Not done: inline diagnostics and code lens as block rows (code lens needs `textDocument/codeLens` and client commands).
- Found and filed separately: ⌘T in the terminal runs Update Project (keymap precedence; reproduces before Track C).

---

## 1. Correctness bugs worth fixing now

These are small and independent.

- **`Document::edit` zips results with the wrong records** (`document.rs:427-465`). The function skips no-op edits (`continue`), so it records only the applied ones, but then zips *all* of `order` with `records`. From the first skipped edit on, every later caret gets the next edit's range, and the trailing ones get `0..0`. Overlapping edits aren't merged either, so two carets in `()a` plus Backspace delete `(a`. Zed normalizes first (`language/src/buffer.rs:2905-2945`): it skips empties and coalesces overlaps. Add unit tests for Delete at EOF with two carets, Cut with mixed empty/non-empty selections, and the `()a` case.
- **Settings all-or-nothing** (`settings.rs:343`, `init` at `:563`). Zed's `fallible_options` (`settings_content/src/fallible_options.rs`) drops only the bad field.
- **Extension task shell injection** (`run.rs:150`). Zed keeps `args: Vec<String>` and quotes at spawn (`util/src/shell_builder.rs`).
- **LSP documents keyed by path** are never re-registered on rename, move or Save As. The server keeps editing the old URI. Zed: `lsp_store.rs:5089-5110` (didClose old, register new).
- **Format-on-save can hang forever.** There's no request timeout and no `$/cancelRequest`. Add cancel-on-drop in `rpc.rs` and a ~3 s format timeout, then save unformatted with the existing toast.
- **Git diff bases ignore CRLF and encoding.** On CRLF files every line shows as changed, and hunk staging rewrites the whole index blob. Normalize in `load_diff_base` (`workspace.rs:2633-2656`) and re-apply the index blob's line endings when staging.
- **Symlinked folders** show as files in the tree, and clicking one *switches the window's project* to the link target (`click_entry` falls through to `open_path` → `set_root`).
- **Linux watcher.** One recursive inotify watch walks `node_modules`/`target` while the main thread waits, fails completely at the watch limit, and drops `Rescan` events. Zed watches per directory as it scans and skips ignored dirs (`fs/src/fs_watcher.rs`, `worktree.rs:5424`).
- **Terminal find** caps matches at the *oldest* 5000, so the newest ones are unreachable. Zero-width and combining characters are dropped from rendering.
- **LSP diagnostics ignore `version`**, so diagnostics computed for an older text get applied to the new one.
- **Vim Normal mode leaves text input enabled.** Option-produced characters and dead keys can insert text. Dot-repeat replays raw keys rather than the change.
- **Hunks are row numbers**, so Rollback or Stage can act on stale rows while you type.

---

## 2. Performance

Zed's rule is *the main thread only does work proportional to what's on screen; everything proportional to file or project size runs on a background executor over a snapshot, tagged with a version, and stale results are dropped*. Kuana already follows this for lint, blame, git diff and status, so the work is to apply the same pattern everywhere else.

### 2.1 Whole-window re-render

`rg '\.cached\(' src` finds nothing. In GPUI, `cx.notify()` marks the view and its ancestors dirty, and only views rendered through `AnyView::cached` get their previous layout reused (`gpui-0.2.2/src/view.rs:103`). Today, the following all re-run the root `Workspace::render` and every editor's 820-line `layout_frame`:

- a caret blink
- a terminal `Wakeup` during `cargo build`
- each frame of a hover transition

Problems and the Git changes list also build a row for *every* item each frame (no `uniform_list`).

Zed renders every pane through a cached view (`workspace/src/pane_group.rs:579-581`) and every dock panel the same way (`dock.rs:1364`).

**Fix:** wrap editors, terminals, the Git panel/log, Problems and the debug panel in `.cached(...)`. Theme reloads already go through `cx.refresh_windows()`, which bypasses the cache. Then audit setters that change what an editor draws from outside (`set_search_highlights`, debug line, workspace-pushed diagnostics) so each one calls `cx.notify()` on the editor itself. Next:

- Replace the blanket `cx.observe(&editor, notify)` in the workspace with typed events: `SelectionsChanged`, `Edited`, `DirtyChanged`, `Saved`.
- Make the status bar and breadcrumbs small entities that subscribe to those events.
- Virtualize Problems and Git changes with `uniform_list`.

### 2.2 Whole-file work per edit on the main thread

| Work | Where | Zed |
|---|---|---|
| Full tree-sitter reparse; injection trees from scratch | `Document::edit` → `syntax.reparse` (`document.rs:467`); `syntax.rs` injections parsed with `None` old tree | Sync parse with a 1 ms budget, then background (`language/src/buffer.rs:1937-2012`); `SyntaxMap` reparses layers with their old trees |
| Soft-wrap rebuild of every line | `WrapKey { version, len, folds }` (`wrap.rs:22-35`) → `WrapMap::new` from `layout_frame` | Only the edited rows are rewrapped, in the background (`display_map/wrap_map.rs`) |
| Run targets (`fs` I/O for crate dir/package), conflicts (`rope.to_string()` + scan), outline, fold regions, Markdown parse | lazily in prepaint after each version change | `refresh_runnables` debounced, in the background, for the visible range (`editor/src/runnables.rs:164-250`) |
| Position shifting | each `raw_replace` shifts folds, breakpoints, snippet stops, inlays and 3 diagnostic lists, then **clones and sorts all diagnostics** (`document.rs:554-571`, `727-745`) | Anchors never move; one `Patch` per transaction |
| In-file find | find bar copies the whole buffer to a `String` and regex-scans it per keystroke and per edit | `find_matches` in `background_spawn` over a snapshot |
| Hot-exit session save | serializes every dirty buffer to pretty JSON after each typing pause | per-item, only what changed, in the background |
| Inlay hints | whole file after every edit pause and every diagnostics publish | visible range, debounced |
| Format result | whole-file diff on the UI thread; fails if you typed meanwhile | |

**One helper fixes most of the table:** `struct Derived<T> { version: u64, value: Arc<T>, task: Option<Task<()>> }`. It clones the rope and tree, computes on the background executor, keeps showing the stale value, and applies the result only if `doc.version()` still matches. Use it for run targets, outline, folds, conflicts, Markdown blocks and find matches. Memoize per-path facts (crate dir, package name) in a map that the watcher invalidates, so `file_targets` never touches the disk. Gate conflict parsing on git reporting the file as conflicted.

**Position tracking**, in two steps:

1. **S.** `Document::edit`, `undo` and `redo` build a sorted `Patch(Vec<Edit { old, new }>)`. Every tracked list is shifted *once per transaction* with a merge-walk, and merged diagnostics are shifted in place instead of re-cloned. Make `Diagnostic.message` an `Arc<str>`.
2. **M.** Add a non-CRDT `Anchor`/`Markers` store (a sorted `Vec`, or `gpui_sum_tree` with offset-delta summaries) and a `Tracked` trait. New features then get position tracking without touching `raw_replace`. `debug_line` is already a forgotten case: it is never shifted.

### 2.3 Per-frame work not bounded by the viewport

- **Long lines.** The visible range is computed in *buffer lines*, so one 2 MB line is highlighted (with a per-byte `Vec<Highlight>`), copied twice and shaped every frame. Cursor up/down re-shape it again. Zed caps display rows (`MAX_LINE_LEN = 1024`, `editor/src/editor.rs:302`, which also wraps at 512 columns with wrap off).
- **Folded lines in the viewport** still get highlighted and copied.
- **Selections, hunks, search matches, diagnostics and scrollbar markers** are looped over in full every frame. Binary-search each sorted list to the visible range, and cache scrollbar markers by version.
- **Markdown preview** builds a new `Syntax` (parser + full parse + highlight) for *every fenced code block on every render* (`markdown_preview.rs:485`), and stats every image. For WASM grammars that also creates a new `WasmStore` each time. Compute highlights once, inside the cached `parse`.
- **Terminal find** re-scans the whole scrollback in `layout()` every frame while output streams. Move it to a debounced background task on `Wakeup`.
- **Terminal non-ASCII cells** are shaped one run per cell, because `force_width` is unused.

### 2.4 Main-thread I/O

- **Window focus** (`workspace.rs:654-661`) runs `reload_changed_files` (every tab: read + decode + full compare + full reparse, no mtime check), `rebuild_tree` (sync `read_dir` of every expanded folder) and `SessionState::load()`. The same path runs after every Pull/Checkout/Merge.
- **Opening files and restoring a session** read and parse every tab synchronously. Restore only the active tab per pane eagerly.
- **Go to File / Search Everywhere** fuzzy-match every project file on the UI thread per keystroke, allocating per candidate and doing a full sort. Score in `background_executor().scoped` with a bounded top-K, a generation counter and a cancel flag. gpui 0.2.2 has `BackgroundExecutor::scoped`.
- **Find in Files** is a sequential, non-streaming, uncancellable walk per query. It copies the whole line for each match, so minified files blow up memory. Replace All acts on a truncated result set and writes files on the main thread.
- **Git fan-out.** Every status change spawns 2 `git` processes *per open tab* and re-diffs even when nothing changed. Inline blame runs an uncancellable `git blame` per caret line, piping the whole buffer.
- **Settings reload** re-applies all editors (sync `.editorconfig` reads, rewraps, inlay-hint requests). `apply_fonts` enumerates system fonts and reads `state.json` on every reload *and every zoom step*.

### 2.5 Perf tooling

There are no benchmarks, no tracing spans and no frame-time instrumentation. Zed has `editor_benchmarks`, `project_benchmarks`, `ztracing` and `miniprofiler_ui`. For Kuana:

- Add an env-gated frame-time log (prepaint + paint per view).
- Add a few `#[ignore]` benches: open a 5 MB file, type in a 50k-line Markdown file with wrap on, Go to File over 100k paths.
- Track those numbers in CI.

---

## 3. The extension story

This matters most for an open-source project. Reusing Zed's on-disk extension format was the right call: authors can port Zed extensions. The machinery behind it still has a ceiling.

### 3.1 One language pipeline, not two

Built-ins are a closed `Language` enum with match arms and a Rust-coded `LangConfig`. Extension languages go through `extensions.rs`. Built-ins can't use `injections.scm`, `indents.scm`, `textobjects.scm`, `runnables.scm` or `brackets.scm`. Extension languages, in turn, miss the hand-coded built-in behavior.

In Zed, built-ins are just data. They are registered through the same `LanguageRegistry::register_language` that extensions use (`languages/src/lib.rs:346-390`), with lazy loaders.

**Do:**

- Ship `assets/languages/<name>/{config.toml, *.scm}` embedded with `include_str!`/`include_dir!`.
- Add `GrammarSource { Native(fn() -> TsLanguage), Wasm(PathBuf) }`.
- Make the registry hold `Arc<LanguageEntry>`, with `Language` staying a `Copy` `u16` handle. Give built-ins fixed indices so `Language::Rust` can remain a `const` during migration.
- Let an extension's query file override a built-in's.

Adding a language then becomes "add a folder". Today it means editing `syntax.rs`, `lint/mod.rs`, `ui::file_icon` and the status bar.

### 3.2 Open highlight vocabulary

`Highlight` has 16 buckets. Captures such as `string.escape`, `markup.heading`, `punctuation.special` and `variable.special` get dropped or mis-mapped, and Zed themes lose most of their `style.syntax` entries on import. Zed builds a `HighlightMap` per grammar from `query.capture_names()` against the theme, with longest-dot-prefix matching, rebuilt when the theme changes (`language/src/language.rs:1236-1280`).

**Do:** `HighlightId(u16)` → `theme.syntax_styles: Vec<HighlightStyle>`. Keep the 16 as the *required* base set (the existing all-themes test), accept any extra `syntax.<dotted.name>`, and import all of a Zed theme's syntax styles. **First step (S):** replace `from_capture`'s match with a longest-prefix alias table.

### 3.3 Grammars: WASM only, and a parser pool

`extensions.rs:791` builds extension grammars with the system `cc` and `dlopen`s them (`:863`). The consequences:

- arbitrary native code from a git repository runs inside Kuana's process
- it needs a C toolchain on the user's machine (a problem on Windows)
- it is a per-platform ABI risk
- a leaked library can never be unloaded

Zed compiles every grammar to `.wasm` with a pinned wasi-sdk it downloads itself (`extension/src/extension_builder.rs:270-330`). Kuana **already loads WASM grammars**, so:

- reuse `lsp/install.rs`'s verified downloader for wasi-sdk
- emit `.wasm`
- keep native builds behind an explicit opt-in tied to project trust until then
- then delete `libloading`

Also add Zed's parser pool (`language/src/language.rs:131-155`: `PARSERS: Mutex<Vec<Parser>>`, each with one `WasmStore`). Kuana creates a new `WasmStore` per parser, which recompiles tree-sitter's WASM stdlib each time. That includes the syntax-error linter and, per frame, the Markdown preview.

### 3.4 Language servers

- **One server per document.** `servers::spec(language)` returns a single spec, and an extension's server *replaces* the built-in one (`servers.rs:34-37`). An extension serving 3 languages spawns 3 processes. You can't run ESLint or Tailwind *alongside* tsserver. Zed keeps a `Vec` of adapters per language, sends each request to the first server that has the capability, and fans out diagnostics, completion and code actions (`lsp_store.rs:9893`, `lsp_command.rs:101`).
- **No configuration.** There are no `initializationOptions`, `workspace/configuration` is answered with nulls, and the installer list is closed. Grow `ServerSpec` into a data-driven `ServerDef`:
  - `initialization_options`, `settings`, `language_ids`, `root_markers`, `disk_based_sources`
  - an `installer: GitHub{repo, asset} | Npm{package} | Go{module}`, which `install.rs` already implements; only the list is hard-coded today
  - extensions declare it in `extension.toml`
  - users override it via `lsp.<server-id>.{settings, initialization_options, binary}`, Zed's key names, so Zed users' settings just work

  This gives most of what Zed's WASM `LspAdapter` does without hosting WASM code.
- **Files outside the project** (Go to Definition into a dependency) start *another* rust-analyzer/gopls. Reuse the running server with the same id (`lsp_store.rs:3338-3400`).
- **Server identity** is only `(id, root)`. A restarted server can be marked failed by its predecessor's exit, and `exit` is sent before the `shutdown` reply arrives.

### 3.5 Debug adapters and tasks

`DebugSpec` is a closed enum (Cargo/Go/Python/Node/Attach). Extensions, settings and tasks can't add an adapter or a launch configuration. Zed has `trait DebugAdapter` (`dap/src/adapters.rs:349-397`) plus extension-declared adapters and `debug.json`. Do the following:

- add a `DebugAdapter` trait
- port the 4 built-ins to it
- add a data variant `DebugSpec::Config { adapter, request, config, build }`
- let `extension.toml` declare `[debug_adapters.<id>] command/args/transport`, like `[language_servers]`

Built-in run detection is hand-written per language in Rust, next to a separate `runnables.scm` path for extensions. Moving built-ins to `runnables.scm` (§3.1) unifies it.

### 3.6 Authoring experience

- **No manifest schema or versioning.** Mistakes in `extension.toml`, `config.toml` or `.scm` fail silently or surface only in Preferences after install. Add a `schema_version` field and a headless `kuana extension check <dir>` that does the following:
  - parse the manifests
  - build the grammar
  - compile every query against it
  - report unknown keys and captures
  - run in CI for extension repos
- **No JSON schema** for `settings.json`, `keymap.json` or themes. Generate them (e.g. `schemars` on the settings structs) and point the editor's own JSON LSP at them. Users get completion, and so do extension authors writing themes.
- **The registry leaks** every `LanguageDef`, query and `ServerSpec` again on each reload (`Box::leak` at `extensions.rs:221, 232, 427, 534`). Use `Arc` with an index table, or leak exactly once per extension version.
- Extensions can't ship snippets for built-in languages through the same path. Snippets are also re-read from disk on the UI thread for every completion.

---

## 4. Core architecture

### 4.1 Buffer ≠ Editor

`Editor` owns `Document` (`editor.rs:224`). As a result:

- a file can't be open in two panes
- `apply_workspace_edit` and Replace in Files must open tabs synchronously to edit files
- LSP state points at editors
- Document mixes model state (text, undo, diagnostics) with view state (folds, snippet stops, inlay display)

Zed: `Buffer` entities live in a `BufferStore` (`project/src/buffer_store.rs`) with deduplicated background loads, and editors hold view state only.

**Path:**

1. Make it `doc: Entity<Document>` inside Editor, with accessors and no behavior change. Document emits `Edited { patch, version }`, `Saved`, `Reloaded`, `LanguageChanged` and `DiagnosticsChanged`.
2. Move folds, snippet stops and inlay display into Editor, shifted through the patch event.
3. Add a `BufferStore` keyed by canonical path. LspStore holds `WeakEntity<Document>`, and workspace edits go to buffers directly.

This is also what unblocks split editors.

### 4.2 Workspace decomposition

`workspace.rs` is 8,986 lines: about 80 fields and one 7,456-line `impl`. It owns the tree, panes, terminals, debugger, Git, auto-save, toasts and modals. Each new panel or modal touches the enum, render, subscriptions, persistence and the status-bar docks.

Zed's seams are traits:

- `Item` for tabs (`workspace/src/item.rs:170`)
- `Panel` for docks (`dock.rs:36`)
- `ModalView` + `ModalLayer`
- `StatusItemView`

**Incremental path:**

1. *(mechanical)* Split into `src/workspace/{tree, panes, terminal_panel, debug_panel, git_ops, status_bar, modals}.rs` as extra `impl Workspace` blocks, the pattern `src/editor/` already uses.
2. Extract child entities with their own `Render` and events: `FileTree`, `TerminalPanel`, `GitState`, `Problems`.
3. Add `trait PaneItem` and implement it for `Editor`. Then make `DiffView` a pane item (deleting `Modal::Diff` and the overlay), and move the image viewer out of Editor.
4. Add a `BottomDock { panels: Vec<Box<dyn PanelHandle>> }`, and persist `bottom_panel: Option<String>` instead of 4 bools.
5. Add a `ModalLayer` with `toggle_modal::<V: ModalView>`.

Per-editor subscriptions currently leak, and the editor talks to the workspace through seven separate event types. Fold them into one `EditorEvent` enum, stored per `EntityId`.

### 4.3 One project snapshot

The tree, Go to File and Find in Files each walk the disk with different exclusion rules, and every structural change triggers a full rescan. Zed has one `Worktree` snapshot (entries with `is_ignored`, `is_symlink`, `is_external` and mtime), built by a background scanner and updated incrementally from FS events (`worktree/src/worktree.rs`).

**For Kuana:** a `Project` entity owning `root`, the `Watcher`, and `snapshot: Arc<Snapshot { entries: Arc<[Entry]>, scan_id }>`. Build it with one `ignore::WalkBuilder::build_parallel()` scan, and update it per changed parent. A sorted `Arc<[Entry]>` is enough to start; reach for `gpui_sum_tree` later. Use the same snapshot for:

- the tree, flattened in the background
- the Go to File index
- Find in Files candidates
- `workspace/didChangeWatchedFiles` to language servers, which never happens today

Make exclusions one user setting (`file_scan_exclusions`, Zed's name).

### 4.4 Display map and editor seams

Inlays, wrap and folds are entangled in `wrap.rs` and `DisplayLine`. There are **no block rows**, and every decoration is hard-coded in the 820-line `layout_frame` and the `on_mouse_down` chain. That blocks:

- inline diagnostics
- inline diff hunks
- code lens
- extension decorations

Grow a small `display_map.rs` instead of porting Zed's five layers:

- `DisplayMap` owning the `WrapMap`, the fold intervals and the visible inlays
- a single row iterator yielding `DisplayRow::Text { line, seg } | Block { id }`
- a `Block { line, placement, height, render }`

Then add Zed-style decoration stores (`highlight_background::<T>`, `highlight_rows::<T>`, gutter marks, inline annotations) and a single `hit_regions` list. A new feature then *registers* a decoration instead of editing `layout_frame`.

**Scroll position** is an `f32` pixel offset from the top. It loses precision in very large files and doesn't stay anchored when rows above change (wrap, folds, inlays). Zed uses `ScrollAnchor` (an anchor plus an offset).

### 4.5 Vim

Vim intercepts keystrokes before the keymap. Its motions duplicate the editor's movement logic, users can't rebind it, and the Normal-mode input bug in §1 comes from this. Zed registers Vim commands as actions under an `Editor && vim_mode == normal` key context (`vim/src/vim.rs:1038-1050`, bindings in `assets/keymaps/vim.json`).

**Do:** add `input_enabled: false` in Normal/Visual first. Then gradually turn motions into actions bound in `keymap.rs` under a `vim_mode` context, sharing a `movement.rs` with the editor.

### 4.6 Git

- **No job queue.** A second operation is *rejected* while one runs, and diff-base reloads race. Zed gives each repo a serialized job channel with coalescing keys (`project/src/git_store.rs:6917-6990`). **First step:** a `VecDeque<GitOp>` instead of the reject branch (`workspace.rs:2724`), plus a `diff_base_generation`.
- **Only one repository per window.** Nested repos and submodules get no status or gutter.
- Diffing the full file in the background with `similar` is fine; Zed's incremental `buffer_diff` isn't needed. The cost is the per-tab process fan-out (§2.4).

### 4.7 Debugger model vs view

`DebugSession` combines the protocol model and the panel view, and only one session can exist. Late DAP responses are applied after the session has moved on (there's no stop generation), so stale frames can show. `setBreakpoints` responses are ignored, so breakpoints that never bound look active. Split `DebugSession` (model) from `DebugPanel` (view), and tag requests with a stop generation.

---

## 5. Code organization, testing, contributor experience

- **CI.** There is none on pushes or PRs (§Top 11). The "keep the build warning-free" rule is enforced only by hand. Add `RUSTFLAGS=-Dwarnings` in CI.
- **UI is untested.** There are 140 `#[test]`s, almost all pure logic, while `Editor` and `Workspace` have essentially none. gpui 0.2.2 ships `test-support` (`TestAppContext`, `VisualTestContext`). Add it as a dev-dependency feature and write a small `EditorTestContext` with marked-text state (`"fn a() {ˇ}"`), like Zed's `editor/src/test/editor_test_context.rs`. The `Document::edit` bug above is exactly what such tests catch.
- **No fakes.** The LSP `Client` only spawns real processes, and config paths and registries are process globals, so tests get `#[ignore]`d. A `FakeServer` over in-memory pipes, and a `config_dir` override, unlock testing LSP and extension flows.
- **Crate split.** One 47.5k-line crate rebuilds with `opt-level=3` for every change, since the workflow is `cargo build --release` and no `[profile]` is set. Do these two things:
  - Add `[profile.dev] opt-level = 1` and `[profile.dev.package."*"] opt-level = 3`, so dependencies stay fast and Kuana's own code builds quickly.
  - Split a gpui-free core crate (`kuana_text`: document, syntax, wrap, fold, search, encoding, lint) to get parallel compiles and a clean model/view seam. One cycle blocks it: `syntax.rs` → `extensions.rs` → gpui/settings/theme/file_icons. Moving the language registry out of `extensions.rs` (§3.1) breaks that cycle.
- **Theme colors.** Adding one theme color means editing all 31 built-in theme JSONs, although `extends` resolution already falls back. Require new keys only in the two base themes.
- **Settings plumbing.** A new setting means about 10 hand-synced edits: field, key list, template, README, `Setting` variant, `is_set`. A small derive macro or a single `SETTINGS: &[SettingDef]` table could generate the key list, template, schema and Preferences rows.
- **Keymap rewrites** drop the user's comments when Preferences changes a shortcut. Settings already has a comment-preserving editor; reuse it.

---

## 6. Where Kuana is right not to copy Zed

The reviewers explicitly recommend keeping these:

- **ropey + UTF-8 byte offsets, no CRDT, no MultiBuffer.** O(1) rope clones give Kuana cheap snapshots already; it only needs to *use* them off-thread.
- **Querying highlights for the visible range only** and relying on GPUI's `LineLayoutCache`, *once long lines are capped*.
- **A prefix-sum `row_starts` index** for display row ↔ buffer line, instead of a SumTree, until files get huge.
- **Git via the CLI**, with free functions instead of a `GitRepository` trait; real-repo tests in temp dirs.
- **JSON state/session files** instead of SQLite; one root per window; a flat pane list.
- **Reader/writer OS threads** for LSP instead of async IO; `serde_json::Value` instead of `lsp-types`; no auto-restart.
- **Not hosting Zed's WASM extension API** (`extension.wasm`) yet. The data-driven `ServerDef`/installer/adapter route (§3.4–3.5) covers most of its value at a fraction of the cost.
- **A hand-written Vim parser**, `EditorMode` reuse for text fields, `Transition` with per-element state, the islands chrome, and `fit_tabs`.
- **The terminal model and view in one entity**, with event batching (drain, then notify once).

---

## 7. Suggested order

1. **Week 1: quick wins, all S.** Cached views; the `Document::edit` fix with tests; the mtime check on reload; large-file guard first; lenient settings; task quoting; LSP cancel-on-drop + format timeout; CRLF diff bases; symlink tree fix; Markdown preview highlights cached in `parse`; parser pool; CI; a dev profile.
2. **Weeks 2–3: off the main thread.** The `Derived<T>` helper (run targets, outline, folds, conflicts, find); `Patch`-based shifting; incremental `WrapMap`; long-line cap; background session save; background Go to File scoring; debounced terminal find; `uniform_list` for Problems/Git.
3. **Month 2: registries.** One language registry with built-ins as data; `HighlightId`; WASM-only grammars; multi-server LSP + `ServerDef` + `lsp.<id>` settings; `kuana extension check`; JSON schemas.
4. **Month 3: structure.** `Entity<Document>` → `BufferStore`; split the workspace into child entities; `PaneItem`/`Panel`/`ModalLayer`; project snapshot; display map with blocks + decoration stores; `DebugAdapter` trait; Git job queue; core crate split.

---

## Appendix: all findings by subsystem

Severity as the reviewer assigned it: **C** critical, **H** high, **M** medium, **L** low. ✗ marks a design the reviewer judged *wrong*, as opposed to merely simpler than Zed. Effort runs S/M/L/XL. Several findings overlap across subsystems (view caching, prepaint caches, wrap, Markdown preview, highlight vocabulary), which is a signal of how central they are. Line references come from the reviewers and are not individually verified.


### Text buffer, edits, undo, and position tracking

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | M | Positions are shifted by hand, once per edited range, and diagnostics are fully cloned and re-sorted for every range |
| H | ✗ | S | Document::edit pairs result ranges with the wrong edits when one edit is a no-op, and doesn't enforce non-overlap |
| H | ✗ | M | Tree-sitter reparse runs synchronously inside Document::edit, and injected languages are reparsed from scratch on every edit |
| H | ✗ | L | The text buffer can't be separated from the Editor view; Document mixes model state with per-view state |
| M | ✗ | M | Async LSP results aren't mapped to the current document version: diagnostics ignore `version`, completion and format edits use the current text or get thrown away |
| M | ✗ | M | Undo model: undo never returns to clean, IME composition steps are separate undo steps, transactions are implicit, deleted breakpoints are lost |
| M | ✗ | S | Reload from disk replaces the whole text, clears undo history, folds and snippet, and leaves carets at old byte offsets, all on the main thread |
| M | ✗ | M | Edits publish no patch, so wrap map, fold regions and find matches are rebuilt from the whole document on every version |
| M | ✗ | M | Large files: opening and saving copy the whole file several times on the main thread, and the `large` guard is bypassed by set_language |

### Display pipeline and rendering: wrap, fold, inlays, layout and paint

| Sev | | Effort | Finding |
|---|---|---|---|
| C | ✗ | M | Soft wrap rebuilds the whole WrapMap on the main thread on every keystroke, fold toggle and resize |
| C | ✗ | M | Long lines are processed in full every frame: no length cap and no per-segment highlighting |
| H | ✗ | S | Editors are not cached views, so any window redraw re-runs every visible editor's full layout_frame |
| H | ✗ | M | Whole-file data (run targets with file-system I/O, conflicts, fold regions, outline) is recomputed synchronously in prepaint after every edit |
| H | ✗ | S | Folded lines inside the viewport still cost highlighting and line copies on every frame |
| H | ✗ | S | Per-frame loops scale with all selections, hunks, matches and diagnostics instead of what is visible |
| H | ✗ | L | No composable display transforms: inlays, wrap and folds are entangled, there are no block rows, and all decorations are hard-coded in an 820-line function |
| M | ✗ | M | Scroll position is an f32 pixel offset from the top: precision loss on big files and no anchoring when rows above change |
| M | ✗ | S | Display decorations are flat offset Vecs re-shifted for every edit range; diagnostics are cloned and re-sorted per range |

### Editor view model: selections, actions, popovers, vim, editing features

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | M | Every editor notify (caret blink, keystroke, scroll, gutter hover) re-renders the whole window: no view caching, and the editor emits too few events |
| H | ✗ | M | Data derived from each document version (conflicts, run targets, outline, fold regions, Markdown parse) is recomputed on the UI thread inside prepaint, at O(file size), after every edit |
| H | ✗ | S | The Markdown preview runs a full tree-sitter parse of every fenced code block, and a filesystem stat for every image, on every frame |
| H | ✗ | L | No decoration, addon or action seams: each feature edits the Editor struct, the 820-line layout_frame and the on_mouse_down chain |
| H | ✗ | L | Vim's keystroke interception leaves editor text input on in Normal mode, records dot-repeat as raw keys, and bypasses keymaps |
| M | ✗ | M | Completion is hard-wired to one LSP server and its JSON items; user snippets ride along as fake LSP items, and filtering runs on the UI thread |
| M | ✗ | M | Multi-cursor selection model: one shared vertical goal, no per-selection identity, and O(rows × cursors) paint and O(cursors × diagnostics) edits |
| M | ✗ | M | IME composition and the clipboard collapse multi-cursor edits; composed text is not shown as marked; whole-line copies paste mid-line |
| M | ✗ | S | LSP popovers are absolutely-positioned children inside the pane's overflow_hidden box, so they are clipped in narrow or short splits |
| M |  | M | editor.rs mixes unrelated concerns, and movement logic is duplicated between the editor and Vim instead of living in a shared module |

### Syntax: tree-sitter parsing, highlighting, injections, queries, outline; linting

| Sev | | Effort | Finding |
|---|---|---|---|
| C | ✗ | S | Large-file guard runs after a full tree-sitter parse of the file |
| C | ✗ | L | All parsing is synchronous on the main thread with no time budget or cancellation |
| H | ✗ | L | Injections are reparsed from scratch, rediscovered by whole-tree walks, merged into one tree per language, and never nested |
| H | ✗ | L | Built-in languages are a hard-coded Rust pipeline parallel to extensions and can't use the query features extensions get |
| H | ✗ | M | Fixed 16-bucket highlight vocabulary drops or mis-maps standard capture names and Zed theme styles |
| H | ✗ | M | Run targets and outline are recomputed over the whole file on the main thread during layout/render, including disk I/O |
| H | ✗ | S | Markdown preview builds a new Syntax (parser + full parse + highlight) for every fenced block on every render |
| M |  | M | Visible-range highlights are recomputed on every frame with no cache and no cap on bytes per frame |
| M | ✗ | M | A parser and a fresh WasmStore per Syntax/injection/lint run, and extension grammars load on the main thread |
| L |  | S | Syntax-error linting re-parses the whole file from scratch, even when an LSP will discard the result |

### Extension system and language registration (key for open-source contributors)

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | L | Built-in languages are a closed enum with hard-coded match arms; extension languages run a second, different code path |
| H | ✗ | M | Extension language servers replace built-in ones, guess commands from a core table, and run once per language |
| H | ✗ | S | A new WasmStore for every WASM-grammar parser recompiles tree-sitter's WASM stdlib each time, per frame in the Markdown preview |
| H | ✗ | M | Extension grammars are compiled with the system C compiler and dlopen-ed into Kuana's process |
| H | ✗ | S | Extension task variables are pasted unquoted into a shell command line |
| M | ✗ | M | Reload leaks every language, grammar and query again, and reparses every open extension-language file |
| M | ✗ | S | Extension data is re-read from disk on the UI thread: per completion (snippets), per frame (Extensions page) and at startup |
| M | ✗ | M | Closed highlight vocabulary: Zed captures and Zed theme keys outside Kuana's 16 classes are dropped |
| M | ✗ | M | No manifest schema, versioning or author check tool; mistakes in extensions fail silently |
| M |  | M | Extensions cannot provide their language server, although Kuana already has verified installers |

### LSP client and language-server management

| Sev | | Effort | Finding |
|---|---|---|---|
| C | ✗ | L | One language server per document: extension servers replace the built-in one, and one extension server spawns a process per language |
| H | ✗ | M | Servers cannot be configured: no initializationOptions, workspace/configuration answered with nulls, closed installer list |
| H | ✗ | M | No request cancellation or timeouts; format-on-save can wait forever before writing the file |
| H | ✗ | S | Open documents are keyed by path and never re-registered when a file is renamed, moved or saved under a new name |
| H | ✗ | M | LSP diagnostics ignore `version` and disk-based staleness, and every replaced range clones and re-sorts all diagnostics |
| H | ✗ | S | Files outside the project start a new server instance (Go to Definition into a dependency spawns another rust-analyzer/gopls) |
| M | ✗ | S | Server identity is the (id, root) key only: a restarted server can be marked failed by its predecessor's exit; tasks accumulate; exit is sent before the shutdown reply |
| M | ✗ | M | Workspace edits (rename, code actions, applyEdit) open every affected file as a tab synchronously and ignore document versions |
| M | ✗ | M | Inlay hints are requested for the whole file after every edit pause and every diagnostics publish |
| M | ✗ | S | Formatting diffs the whole file on the UI thread and fails if the user typed meanwhile |

### Workspace and app architecture: root view, panes/items, docks/panels, modals, notifications, entity graph

| Sev | | Effort | Finding |
|---|---|---|---|
| C | ✗ | M | Any notify redraws the whole window: no view caching, and the root view builds every subtree |
| H | ✗ | XL | Editor owns its Document: no shared Buffer entity or BufferStore, so models depend on views |
| H | ✗ | L | Panes can only hold editors: with no Item trait, other views are bolted on as overlays or inside Editor |
| H | ✗ | L | Bottom panels, modals, context menus, pickers and actions are closed lists inside workspace.rs |
| H | ✗ | S | Hot-exit session save serializes every dirty buffer on the main thread after each pause in typing |
| H | ✗ | M | Go to File and Search Everywhere match the whole file index on the UI thread on every keystroke |
| M | ✗ | S | Problems panel and Git changes list build a row for every item on every frame |
| M | ✗ | L | Project state and filesystem work live in the Workspace view and run on the main thread |
| M | ✗ | S | Per-editor subscriptions leak, and the editor talks to the workspace through seven separate event types |

### Project model, file system, worktree scanning, watching, file finding, project search

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | L | No shared project snapshot: tree, Go to File and Find in Files each re-read the disk, and every structural change triggers a full rescan |
| H | ✗ | S | Find in Files copies the whole line for each match and shapes whole lines in the UI, so minified files cause a memory blowup and stall the UI |
| H | ✗ | M | Symlinked folders show in the tree as files, and clicking one switches the window's project to the link target |
| H | ✗ | M | Go to File and Search Everywhere fuzzy-match every project file on the main thread per keystroke, with allocations per candidate and a full sort |
| H | ✗ | M | The file tree re-reads every expanded folder synchronously on the main thread and clones its whole row list on every render |
| H | ✗ | M | Find in Files is a sequential, non-streaming, uncancellable full walk per query, and Replace All acts on a truncated result set and rewrites files on the main thread |
| H | ✗ | M | On Linux, one recursive inotify watch walks the whole project (node_modules, target, symlink targets) while the main thread waits, and fails completely at the watch limit; overflow events are dropped |
| M | ✗ | M | Disk changes are hand-routed inside Workspace::disk_changed with no typed change events, so language servers never receive workspace/didChangeWatchedFiles |
| M | ✗ | M | Exclusion rules are hard-coded and differ between the tree, Go to File, search, watcher and venv scan, with no user setting |
| M | ✗ | S | In-file Find re-searches on every edit on the main thread, copying the whole document to a String, with no cap on matches |

### Git integration: status, diffs, gutter hunks, blame, log, staging

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | M | Diff bases and hunk staging ignore CRLF normalisation and file encoding: every line shows as changed, and staging rewrites the whole file in the index |
| H | ✗ | M | No Git job queue: a second operation is rejected while one runs, and diff-base reloads race |
| H | ✗ | S | Git panel renders every changed file each frame with no virtualisation, and the status path map is built on the main thread |
| M | ✗ | S | Every status change re-reads HEAD and index for every open tab (2 processes per tab) and re-diffs even when nothing changed |
| M | ✗ | M | Hunks are stored as row numbers and drift during edits; Rollback/Stage can act on stale rows |
| M |  | M | Inline blame spawns an uncancellable `git blame` per cursor line, with the whole buffer piped in each time |
| M | ✗ | L | One repository per workspace: nested repos, submodules and multi-repo folders get no Git support |

### Integrated terminal

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | M | Terminal output re-renders all visible editors every frame: no view in Kuana is `.cached()` |
| H | ✗ | S | Find in terminal re-scans the whole scrollback on the main thread every frame while output streams |
| M | ✗ | S | Each non-ASCII cell is shaped as its own text run because force_width is not used |
| M | ✗ | S | Zero-width and combining characters are dropped from rendering, output_text and link detection |
| M | ✗ | S | Find caps matches at the oldest 5000, so the newest matches cannot be reached |
| M |  | S | Mouse reporting: left button only, no drag or motion reports, always SGR encoding |
| L |  | S | Run tabs keep only 10k lines of scrollback, and output_text copies it on the main thread |
| L |  | S | No option-as-Meta on macOS and no OSC 8 hyperlinks |

### Settings, keymap, themes, and transitions

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | S | One bad value in settings.json throws away every setting at startup |
| H | ✗ | M | Every settings change re-applies all open editors, with sync .editorconfig reads, rewraps and inlay-hint requests |
| M | ✗ | S | apply_fonts enumerates system fonts and reads state.json on every settings reload and zoom step |
| M | ✗ | S | Transitions re-render the whole window on every animation frame because no view is cached |
| M | ✗ | L | Fixed 16-slot syntax palette loses Zed theme fidelity and extension capture names |
| M |  | M | Adding a setting means about 10 hand-synced edits, with no single source of truth |
| M |  | M | No JSON schema for settings.json, keymap.json or theme files |
| L | ✗ | M | Changing a shortcut in Preferences rewrites keymap.json and drops the user's comments |

### Debugger (DAP), run targets, and tasks

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | S | Extension task commands are joined without shell quoting: paths with spaces break, and source text can inject shell commands |
| H | ✗ | M | Run targets are recomputed on the main thread inside layout on every edit, with a full-file copy and disk I/O |
| H | ✗ | L | Debug adapters and launch configs are a closed Rust enum: extensions, settings and tasks cannot be debugged |
| M |  | M | No project debug/launch configurations (debug.json, launch.json), and no env for run or debug targets |
| M | ✗ | S | Late DAP responses are applied after the session moved on (no stop generation), so stale frames and wrong variables can show |
| M |  | M | DebugSession combines the protocol model and the panel view, and only one session can exist |
| M |  | L | Built-in run detection is hand-written per language in Rust, alongside a separate runnables.scm path for extensions |
| L |  | S | setBreakpoints responses and `breakpoint` events are ignored, so unbound breakpoints look active |

### Cross-cutting performance sweep: main-thread blocking, per-frame work, process spawning, allocation

| Sev | | Effort | Finding |
|---|---|---|---|
| C | ✗ | S | Window focus (and every git op) re-reads, re-compares and fully re-parses every open tab on the UI thread, with no mtime check |
| C | ✗ | M | Per-version caches are rebuilt lazily inside paint: each keystroke does several full-buffer copies and scans, plus sync file IO |
| H | ✗ | M | Soft wrap: WrapMap is rebuilt from scratch for the whole file on every edit (and whenever folds change) |
| H | ✗ | M | Search-as-you-type runs on the UI thread: find bar copies and regex-scans the whole buffer per keystroke and per edit; Go to File matches every project file synchronously |
| H | ✗ | S | Hot-exit session save serializes every dirty buffer and writes pretty JSON on the UI thread after every pause in typing |
| H | ✗ | M | Opening files and restoring a session read and fully tree-sitter-parse every file synchronously on the UI thread |
| H | ✗ | S | Markdown preview creates a new tree-sitter parser and fully parses every fenced code block on every frame |
| M | ✗ | M | No view is cached, and Workspace re-renders on every editor notify: each caret blink and each terminal wakeup redraws the whole window, including non-virtualized lists |
| M |  | M | Git process fan-out: every status change re-spawns 2 git processes per open tab; inline blame spawns git blame per caret line |
| M |  | S | No performance tooling: no benchmarks, no tracing spans, no frame-time instrumentation |

### Code organization, crate structure, compile times, testing infrastructure, contributor experience

| Sev | | Effort | Finding |
|---|---|---|---|
| H | ✗ | S | No CI on pushes/PRs: tests and clippy never run automatically, and Windows/Linux breakage is found only at release time |
| H | ✗ | M | The Editor and Workspace views are effectively untested, although gpui 0.2.2 ships TestAppContext/VisualTestContext |
| M | ✗ | S | The dev loop is a non-incremental opt-level-3 rebuild of the whole 47.5k-line crate; no build profiles are configured |
| H | ✗ | L | Workspace is a god object: 80 fields and one 7,456-line `impl Workspace` own the tree, panes, terminals, debugger, Git, auto-save and toasts |
| M |  | M | No fakes or injectable paths: LSP Client only spawns real processes, and config paths and registries are process globals, so tests are #[ignore]d |
| M |  | M | A gpui-free core crate is blocked by one cycle: syntax.rs depends on extensions.rs, which imports gpui, settings, theme and file_icons |
| M |  | S | Extension authors have no headless check; manifest, grammar and query errors surface only in Preferences after install |
| L |  | S | Adding one theme color requires editing all 31 built-in theme JSONs, although the resolver already falls back |
