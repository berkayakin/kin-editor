//! Markdown preview: the document parsed with pulldown-cmark into a few
//! block kinds, drawn with GPUI next to the text (see the editor's split
//! view). Code blocks are highlighted with the editor's own grammars;
//! local images show; links open in the browser.

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{
    AnyElement, App, FontStyle, FontWeight, HighlightStyle, InteractiveText, IntoElement, SharedString, StrikethroughStyle,
    StyledText, UnderlineStyle, div, font, img, prelude::*, px,
};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ropey::Rope;

use crate::{
    syntax::{Highlight, Language, Syntax},
    theme::Theme,
};

/// Text with inline styling: byte ranges of `text` and how they look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inline {
    pub text: String,
    pub styles: Vec<(Range<usize>, Style)>,
    /// Links and where they go.
    pub links: Vec<(Range<usize>, String)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Style {
    Bold,
    Italic,
    Code,
    Strike,
    Link,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    Heading(u8, Inline),
    Paragraph(Inline),
    /// A paragraph that's just an image; `path` is the local file it shows.
    Image { src: String, alt: String, path: Option<PathBuf> },
    /// A fenced block, highlighted when its language is one Kin knows.
    Code { lang: String, text: String, highlights: Vec<(Range<usize>, Highlight)> },
    Quote(Vec<Block>),
    /// `start`: the first number of an ordered list.
    List { start: Option<u64>, items: Vec<Item> },
    Table { aligns: Vec<Alignment>, rows: Vec<Vec<Inline>> },
    Rule,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    /// A task list item: done or not.
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

/// The document's blocks. Code is highlighted and images found here, once per
/// version, not per frame; `base` is the document's folder (for relative images).
pub fn parse(markdown: &str, base: Option<&Path>) -> Vec<Block> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder { base: base.map(Path::to_path_buf), ..Builder::default() };
    for event in Parser::new_ext(markdown, options) {
        builder.event(event);
    }
    builder.finish()
}

/// Turns the event stream into blocks. Containers (quotes, lists, items)
/// are a stack; text collects into the innermost open inline.
#[derive(Default)]
struct Builder {
    /// Open containers and the blocks gathered inside each; the bottom is the document.
    stack: Vec<(Container, Vec<Block>)>,
    inline: Option<Inline>,
    /// Inline styles opened and not closed yet: where they started.
    open: Vec<(Style, usize, Option<String>)>,
    code: Option<(String, String)>,
    image: Option<(String, String)>,
    table: Option<(Vec<Alignment>, Vec<Vec<Inline>>)>,
    heading: Option<u8>,
    base: Option<PathBuf>,
}

enum Container {
    Document,
    Quote,
    List(Option<u64>, Vec<Item>),
    Item(Option<bool>),
}

impl Builder {
    fn blocks(&mut self) -> &mut Vec<Block> {
        if self.stack.is_empty() {
            self.stack.push((Container::Document, Vec::new()));
        }
        &mut self.stack.last_mut().unwrap().1
    }

    fn text(&mut self, text: &str) {
        if let Some((_, code)) = &mut self.code {
            code.push_str(text);
        } else if let Some((_, alt)) = &mut self.image {
            alt.push_str(text);
        } else {
            self.inline.get_or_insert_with(Inline::default).text.push_str(text);
        }
    }

    /// Ends the current paragraph-like text (also for "tight" list items, whose text has no paragraph).
    fn flush_inline(&mut self) {
        if let Some(inline) = self.inline.take().filter(|i| !i.text.trim().is_empty()) {
            self.blocks().push(Block::Paragraph(inline));
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                let inline = self.inline.get_or_insert_with(Inline::default);
                let start = inline.text.len();
                inline.text.push_str(&t);
                inline.styles.push((start..inline.text.len(), Style::Code));
            }
            Event::SoftBreak => self.text(" "),
            Event::HardBreak => self.text("\n"),
            Event::Rule => {
                self.flush_inline();
                self.blocks().push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                if let Some((Container::Item(task), _)) = self.stack.last_mut() {
                    *task = Some(done);
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => {
                // Shown as written; Kin doesn't render HTML.
                if self.code.is_none() {
                    self.text(t.trim_end_matches('\n'));
                }
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        let at = self.inline.as_ref().map_or(0, |i| i.text.len());
        match tag {
            Tag::Paragraph => self.flush_inline(),
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.heading = Some(heading_level(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.stack.push((Container::Quote, Vec::new()));
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, String::new()));
            }
            Tag::List(start) => {
                self.flush_inline();
                self.stack.push((Container::List(start, Vec::new()), Vec::new()));
            }
            Tag::Item => {
                self.flush_inline();
                self.stack.push((Container::Item(None), Vec::new()));
            }
            Tag::Table(aligns) => {
                self.flush_inline();
                self.table = Some((aligns, Vec::new()));
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some((_, rows)) = &mut self.table {
                    rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.inline = Some(Inline::default()),
            Tag::Emphasis => self.open.push((Style::Italic, at, None)),
            Tag::Strong => self.open.push((Style::Bold, at, None)),
            Tag::Strikethrough => self.open.push((Style::Strike, at, None)),
            Tag::Link { dest_url, .. } => self.open.push((Style::Link, at, Some(dest_url.to_string()))),
            Tag::Image { dest_url, .. } => self.image = Some((dest_url.to_string(), String::new())),
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(),
            TagEnd::Heading(_) => {
                let inline = self.inline.take().unwrap_or_default();
                let level = self.heading.take().unwrap_or(1);
                self.blocks().push(Block::Heading(level, inline));
            }
            TagEnd::CodeBlock => {
                if let Some((lang, text)) = self.code.take() {
                    let text = text.trim_end_matches('\n').to_string();
                    let highlights = code_highlights(&lang, &text);
                    self.blocks().push(Block::Code { lang, text, highlights });
                }
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                if let Some((_, blocks)) = self.stack.pop() {
                    self.blocks().push(Block::Quote(blocks));
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                if let Some((Container::Item(task), blocks)) = self.stack.pop()
                    && let Some((Container::List(_, items), _)) = self.stack.last_mut()
                {
                    items.push(Item { task, blocks });
                }
            }
            TagEnd::List(_) => {
                if let Some((Container::List(start, items), _)) = self.stack.pop() {
                    self.blocks().push(Block::List { start, items });
                }
            }
            TagEnd::TableCell => {
                let cell = self.inline.take().unwrap_or_default();
                if let Some((_, rows)) = &mut self.table
                    && let Some(row) = rows.last_mut()
                {
                    row.push(cell);
                }
            }
            TagEnd::Table => {
                if let Some((aligns, rows)) = self.table.take() {
                    self.blocks().push(Block::Table { aligns, rows });
                }
            }
            TagEnd::Image => {
                if let Some((src, alt)) = self.image.take() {
                    // On its own it's a picture; inside text, its description.
                    let alone = self.inline.as_ref().is_none_or(|i| i.text.trim().is_empty());
                    if alone && self.table.is_none() {
                        self.inline = None;
                        let path = self.base.as_ref().map(|b| b.join(&src)).filter(|p| !src.contains("://") && p.is_file());
                        self.blocks().push(Block::Image { src, alt, path });
                    } else {
                        self.text(&alt);
                    }
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                if let Some((style, start, url)) = self.open.pop() {
                    let inline = self.inline.get_or_insert_with(Inline::default);
                    let range = start..inline.text.len();
                    if let Some(url) = url {
                        inline.links.push((range.clone(), url));
                    }
                    inline.styles.push((range, style));
                }
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush_inline();
        // Anything left open (malformed input) folds into the document.
        while self.stack.len() > 1 {
            let (_, blocks) = self.stack.pop().unwrap();
            self.blocks().extend(blocks);
        }
        self.stack.pop().map(|(_, blocks)| blocks).unwrap_or_default()
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

// ---- Drawing -------------------------------------------------------------------------

/// How to draw.
#[derive(Clone, Copy)]
pub struct Draw {
    /// 1.0 is the UI font size.
    pub zoom: f32,
}

pub fn render(blocks: &[Block], d: Draw, cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    let size = (theme.ui_font_size + 1.) * d.zoom;
    div()
        .flex()
        .flex_col()
        .gap(px(12. * d.zoom))
        .text_size(px(size))
        .line_height(px(size * 1.6))
        .text_color(theme.text)
        .children(blocks.iter().enumerate().map(|(ix, b)| render_block(b, ix, d, cx)))
        .into_any_element()
}

fn render_block(block: &Block, ix: usize, d: Draw, cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    match block {
        Block::Heading(level, inline) => {
            let size = match level {
                1 => 26.,
                2 => 21.,
                3 => 18.,
                4 => 16.,
                _ => 14.5,
            } * d.zoom;
            div()
                .pt(px(if *level <= 2 { 8. } else { 4. }))
                .text_size(px(size))
                .line_height(px(size * 1.3))
                .font_weight(FontWeight::SEMIBOLD)
                .when(*level <= 2, |el| el.pb_1p5().border_b_1().border_color(theme.border_subtle))
                .child(render_inline(inline, ("md-heading", ix), cx))
                .into_any_element()
        }
        Block::Paragraph(inline) => render_inline(inline, ("md-p", ix), cx),
        Block::Image { src, alt, path } => {
            match path {
                Some(path) => img(path.clone()).max_w_full().rounded(px(6.)).into_any_element(),
                None => div().text_color(theme.text_dim).child(format!("[{}]", if alt.is_empty() { src } else { alt })).into_any_element(),
            }
        }
        Block::Code { text, highlights, .. } => div()
            .px_3()
            .py_2()
            .rounded(px(8.))
            .bg(theme.panel)
            .border_1()
            .border_color(theme.border_subtle)
            .font(font(theme.mono_font.clone()))
            .text_size(px((theme.editor_font_size - 1.) * d.zoom))
            .line_height(px((theme.editor_font_size - 1.) * d.zoom * 1.45))
            .child(highlighted_code(text, highlights, cx))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .pl_3()
            .border_l_2()
            .border_color(theme.border)
            .text_color(theme.text_muted)
            .flex()
            .flex_col()
            .gap_2()
            .children(blocks.iter().enumerate().map(|(i, b)| render_block(b, ix * 1000 + i, d, cx)))
            .into_any_element(),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap_1()
            .children(items.iter().enumerate().map(|(i, item)| {
                let marker: AnyElement = match (item.task, start) {
                    (Some(done), _) => div()
                        .mt(px(4.))
                        .size(px(14.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(if done { theme.accent } else { theme.border })
                        .when(done, |el| el.bg(theme.accent).text_color(theme.background).text_size(px(10.)).child("✓"))
                        .into_any_element(),
                    (None, Some(n)) => div().text_color(theme.text_muted).child(format!("{}.", n + i as u64)).into_any_element(),
                    (None, None) => div().text_color(theme.text_muted).child("•").into_any_element(),
                };
                div()
                    .flex()
                    .gap_2()
                    .child(div().min_w(px(18.)).flex().justify_end().child(marker))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(item.blocks.iter().enumerate().map(|(j, b)| render_block(b, (ix * 100 + i) * 100 + j, d, cx))),
                    )
            }))
            .into_any_element(),
        Block::Table { aligns, rows } => div()
            .flex()
            .flex_col()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .children(rows.iter().enumerate().map(|(r, row)| {
                div()
                    .flex()
                    .when(r > 0, |el| el.border_t_1().border_color(theme.border_subtle))
                    .when(r == 0, |el| el.bg(theme.panel).font_weight(FontWeight::SEMIBOLD))
                    .children(row.iter().enumerate().map(|(c, cell)| {
                        let cell_el = div().flex_1().min_w_0().px_2().py_1().child(render_inline(cell, ("md-cell", (ix * 100 + r) * 100 + c), cx));
                        match aligns.get(c) {
                            Some(Alignment::Center) => cell_el.flex().justify_center(),
                            Some(Alignment::Right) => cell_el.flex().justify_end(),
                            _ => cell_el,
                        }
                    }))
            }))
            .into_any_element(),
        Block::Rule => div().h(px(1.)).my_1().bg(theme.border).into_any_element(),
    }
}

/// Styled text; links open in the browser when clicked.
fn render_inline(inline: &Inline, id: (&'static str, usize), cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    let highlights: Vec<(Range<usize>, HighlightStyle)> = inline
        .styles
        .iter()
        .filter(|(r, _)| r.start < r.end)
        .map(|(range, style)| {
            let mut h = HighlightStyle::default();
            match style {
                Style::Bold => h.font_weight = Some(FontWeight::BOLD),
                Style::Italic => h.font_style = Some(FontStyle::Italic),
                Style::Strike => h.strikethrough = Some(StrikethroughStyle { thickness: px(1.), color: None }),
                Style::Code => {
                    h.background_color = Some(theme.hover.into());
                    h.color = Some(theme.syntax.string.into());
                }
                Style::Link => {
                    h.color = Some(theme.accent.into());
                    h.underline = Some(UnderlineStyle { thickness: px(1.), color: None, wavy: false });
                }
            }
            (range.clone(), h)
        })
        .collect();
    let text = StyledText::new(SharedString::from(inline.text.clone())).with_highlights(highlights);
    if inline.links.is_empty() {
        return div().child(text).into_any_element();
    }
    let links = inline.links.clone();
    let ranges: Vec<Range<usize>> = links.iter().map(|(r, _)| r.clone()).collect();
    InteractiveText::new(id, text)
        .on_click(ranges, move |ix, _, cx| {
            if let Some((_, url)) = links.get(ix)
                && (url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:"))
            {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

/// The highlights of a fenced block, when its language is one Kin knows.
fn code_highlights(lang: &str, text: &str) -> Vec<(Range<usize>, Highlight)> {
    let extension = match lang.to_lowercase().as_str() {
        "rust" => "rs".to_string(),
        "javascript" | "jsx" => "js".into(),
        "typescript" => "ts".into(),
        "python" => "py".into(),
        "bash" | "shell" | "zsh" | "console" => "sh".into(),
        "markdown" => "md".into(),
        "golang" => "go".into(),
        other => other.to_string(),
    };
    let language = Language::from_path(&PathBuf::from(format!("code.{extension}")));
    if language == Language::PlainText || lang.is_empty() {
        return Vec::new();
    }
    let rope = Rope::from_str(text);
    Syntax::new(language, &rope).highlights(&rope, 0..text.len())
}

fn highlighted_code(text: &str, highlights: &[(Range<usize>, Highlight)], cx: &App) -> AnyElement {
    let theme = Theme::get(cx);
    let highlights = highlights
        .iter()
        .map(|(range, kind)| (range.clone(), HighlightStyle { color: Some(theme.highlight_color(*kind)), ..Default::default() }));
    div()
        .whitespace_nowrap()
        .overflow_hidden()
        .child(StyledText::new(SharedString::from(text.to_string())).with_highlights(highlights))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_markdown() {
        let md = "# Title\n\nSome **bold** and `code` with a [link](https://x.y).\n\n- one\n- [x] done\n\n1. first\n2. second\n\n> quoted\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|--:|\n| 1 | 2 |\n\n---\n\n![logo](img.png)\n";
        let dir = std::env::temp_dir().join(format!("kin-md-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("img.png"), b"").unwrap();
        let blocks = parse(md, Some(&dir));
        assert!(matches!(&blocks[0], Block::Heading(1, i) if i.text == "Title"));
        let Block::Paragraph(p) = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(p.text, "Some bold and code with a link.");
        assert!(p.styles.contains(&(5..9, Style::Bold)));
        assert!(p.styles.contains(&(14..18, Style::Code)));
        assert_eq!(p.links, vec![(26..30, "https://x.y".to_string())]);
        let Block::List { start: None, items } = &blocks[2] else { panic!("{:?}", blocks[2]) };
        assert_eq!((items[0].task, items[1].task), (None, Some(true)));
        assert!(matches!(&items[1].blocks[0], Block::Paragraph(i) if i.text == "done"));
        assert!(matches!(&blocks[3], Block::List { start: Some(1), items } if items.len() == 2));
        assert!(matches!(&blocks[4], Block::Quote(inner) if matches!(&inner[0], Block::Paragraph(i) if i.text == "quoted")));
        let Block::Code { lang, text, highlights } = &blocks[5] else { panic!("{:?}", blocks[5]) };
        assert_eq!((lang.as_str(), text.as_str()), ("rust", "fn main() {}"));
        assert!(highlights.iter().any(|(r, h)| *r == (0..2) && *h == Highlight::Keyword), "{highlights:?}");
        let Block::Table { aligns, rows } = &blocks[6] else { panic!("{:?}", blocks[6]) };
        assert_eq!((aligns[1], rows.len(), rows[1][1].text.as_str()), (Alignment::Right, 2, "2"));
        assert_eq!(blocks[7], Block::Rule);
        assert_eq!(blocks[8], Block::Image { src: "img.png".into(), alt: "logo".into(), path: Some(dir.join("img.png")) });
        let _ = std::fs::remove_dir_all(&dir);
    }
}
