# Themes

Pick a theme with **Select Theme** (Ctrl+\` like JetBrains' Quick Switch Scheme, or Find
Action). Moving through the list previews each theme live; Enter keeps it
(saved to `settings.json`), Escape restores the previous one.

Built in: Monokai Pro (default), Clay Black (near-black with neutral grays), Clay Dark, One Dark, Dracula, Nord, Tokyo Night,
Gruvbox Dark, Catppuccin Mocha, Rosé Pine, Solarized Dark, Everforest Dark, Kanagawa, Ayu Dark, Night Owl,
Material Palenight, GitHub Dark, Poimandres, Oxocarbon, SynthWave '84, Vesper, Horizon, Vitesse Dark, Cobalt2,
and the light themes Clay Light, GitHub Light, Solarized Light, Catppuccin Latte, Rosé Pine Dawn, Gruvbox Light
and One Light. Their sources in [`assets/themes/`](../assets/themes) use the same format as user themes.

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
`attribute`, `escape`. Any tree-sitter capture name works too, for finer colors:
`"keyword.control"`, `"string.regex"`, `"variable.parameter"`. A capture takes the color of
the longest part of its name the theme sets (`keyword.control.return` → `keyword.control` →
`keyword`). Zed themes keep all their syntax colors. Set `"italic_comments": false` for
upright comments.
