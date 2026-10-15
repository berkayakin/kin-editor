# Settings and project trust

## The Settings dialog

The first time Kin starts, **Welcome Setup** asks for the basics: a theme (applied as you click),
the keymap to start from (each shown with a few of its shortcuts) and editor basics such as font
size, auto-save, reformat on save and soft wrap. Its choices go to `settings.json` like any other.
Open it again any time from the Kin menu or Find Action (**Welcome Setup**).

**Settings…** (⌘, / Ctrl+Alt+S, or the gear at the top right) opens the Settings
dialog, the same on every platform. Its pages are Appearance (themes), Editor (font, indentation,
wrapping), Saving, Languages (per-language options, language server and formatter),
[Extensions](extensions.md), Keymap, Terminal & Run and Updates. The search field looks through all of them; with it focused, ↑/↓ switch
pages. Changes apply right away. An option you changed has an undo button next to its name,
which puts the default back.

Everything is stored in `settings.json`. **Open settings.json** (at the bottom of the dialog and
in Find Action) opens it, created with every option commented out. Edits to the file apply when
you save (or when you come back to Kin after editing it elsewhere). Comments and trailing commas
are fine, and Kin keeps them when it changes a value itself. With the JSON language server
installed (`vscode-langservers-extracted`), `settings.json`, `keymap.json` and your themes get
completion, documentation on hover and checks from schemas Kin makes of its own options, actions
and theme colors.

| Setting | Default | |
| --- | --- | --- |
| `theme` | `"Monokai Pro"` | see [Themes](themes.md) |
| `base_keymap` | `"JetBrains"` | `"JetBrains"`, `"VSCode"`, `"SublimeText"` or `"Zed"`; see [Key bindings](keybindings.md) |
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
| `rulers` | `[]` (`.editorconfig`'s `max_line_length`) | columns for vertical guides, e.g. `[80, 120]` |
| `show_whitespace` | `"selection"` | mark spaces and tabs: `"selection"`, `"boundary"` (indentation and line ends), `"trailing"`, `"all"`, `"none"` |
| `inlay_hints` | `true` | types and parameter names from the language server |
| `code_lens` | `true` | the language server's code lenses above lines ("3 references", "▶ Run \| Debug") |
| `languages` | | per-language overrides of the ten above, by the name in the status bar |
| `vim_mode` | `false` | Vim's modes and keys; see [Vim mode](editing.md#vim-mode) |
| `icon_theme` | built-in | an extension's icon theme by name, e.g. `"Soft Charmed Icons"` |
| `visual_effects` | `"full"` | how much decoration the interface draws: `"full"`, `"reduced"` (solid colors instead of gradients) or `"minimal"` (also no shadows, glows or animations); Settings → Appearance. The older `"reduce_visual_effects": true` still works, as `"minimal"` |
| `language_servers` | | per language: `false` turns its server off, `["cmd", "args"]` replaces it |
| `lsp` | | per server, by id (Kin's, e.g. `typescript`, or Zed's, e.g. `vtsls`, `basedpyright`): `settings` (what it asks for through `workspace/configuration`; a change is sent to it), `initialization_options` (applied when it restarts) and `binary` (`{ "path", "arguments" }`), as in Zed |
| `terminal_shell` | login shell | the terminal's shell and arguments, e.g. `["/opt/homebrew/bin/fish", "--login"]` |
| `run_configurations` | | `[{ "name", "command", "cwd" }]` added to Run…; see [Run and test](run-and-debug.md#run-and-test) |
| `formatters` | | per language: `"language_server"`, a command reading stdin and writing stdout (`{file}` is the path), or `false` |
| `trust_all_projects` | `false` | run language servers, formatters and project tools in every folder without asking; see [Project trust](#project-trust) |
| `native_grammars` | `false` | compile extensions' grammars to native code with your C compiler instead of WebAssembly (only for extensions in trusted folders) |
| `auto_update` | `true` | download new releases in the background; see [Releases and updates](install.md#updates) |

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

## Project trust

A cloned repository can make an editor run its code without asking: a language server builds it,
ESLint and Prettier load its JavaScript config, `node_modules/.bin` and venv tools are its own
programs, and Git runs a `core.fsmonitor` command from its config. So the first time you open a
folder, Kin asks whether you trust it. Until you do, it stays in **Restricted Mode** (shown in the
title bar): language servers don't start, formatters and ESLint don't run, the folder's own tools
aren't used, terminals don't activate its venv, and Git ignores its fsmonitor. Editing, Git, Run
and Debug work as usual.

**Trust Folder** remembers the folder; tick **Trust every project in …** to trust its parent
folder instead (for example the folder you keep your repositories in). Click **Restricted Mode**
in the title bar or use **Project Trust…** to decide later or to stop trusting a folder.
**Settings → Trust** lists trusted folders, and `trust_all_projects` turns the checks off.
