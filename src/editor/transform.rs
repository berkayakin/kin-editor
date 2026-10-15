//! Line and case commands: sort, reverse and de-duplicate lines; convert
//! the selection (or the word at the caret) to upper, lower, title, snake,
//! kebab, camel or Pascal case.

use std::ops::Range;

use gpui::{Context, Window};

use super::{
    ConvertToCamelCase, ConvertToKebabCase, ConvertToLowerCase, ConvertToPascalCase, ConvertToSnakeCase, ConvertToTitleCase,
    ConvertToUpperCase, Editor, RemoveDuplicateLines, ReverseLines, SortLinesAscending, SortLinesDescending, word_range,
};
use crate::document::EditKind;

impl Editor {
    pub(super) fn sort_lines_ascending(&mut self, _: &SortLinesAscending, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(cx, |lines| lines.sort_by(|a, b| natural_cmp(a, b)));
    }

    pub(super) fn sort_lines_descending(&mut self, _: &SortLinesDescending, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(cx, |lines| lines.sort_by(|a, b| natural_cmp(b, a)));
    }

    pub(super) fn reverse_lines(&mut self, _: &ReverseLines, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(cx, |lines| lines.reverse());
    }

    pub(super) fn remove_duplicate_lines(&mut self, _: &RemoveDuplicateLines, _: &mut Window, cx: &mut Context<Self>) {
        self.transform_lines(cx, |lines| {
            let mut seen = std::collections::HashSet::new();
            lines.retain(|l| seen.insert(l.clone()));
        });
    }

    /// Rewrites the selected lines (each block of them), or every line when
    /// nothing is selected.
    fn transform_lines(&mut self, cx: &mut Context<Self>, f: impl Fn(&mut Vec<String>)) {
        if self.is_single_line() {
            return;
        }
        let nothing_selected = self.selections.iter().all(|s| s.is_empty());
        let blocks: Vec<Range<usize>> =
            if nothing_selected { vec![0..self.doc.read(cx).line_count()] } else { self.selected_row_blocks(cx) };
        let mut edits = Vec::new();
        for block in blocks {
            // A trailing empty line (the file's final newline) stays where it is.
            let mut end = block.end;
            if end > block.start + 1 && end == self.doc.read(cx).line_count() && self.doc.read(cx).line_text(end - 1).is_empty() {
                end -= 1;
            }
            let mut lines: Vec<String> = (block.start..end).map(|r| self.doc.read(cx).line_text(r).to_string()).collect();
            let before = lines.clone();
            f(&mut lines);
            if lines == before {
                continue;
            }
            let start = self.doc.read(cx).line_start(block.start);
            let stop = self.doc.read(cx).line_start(end - 1) + self.doc.read(cx).line_text(end - 1).len();
            edits.push((start..stop, lines.join("\n")));
        }
        self.edit_and_remap(edits, EditKind::Other, cx);
    }

    pub(super) fn convert_to_upper_case(&mut self, _: &ConvertToUpperCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, |s| s.to_uppercase());
    }

    pub(super) fn convert_to_lower_case(&mut self, _: &ConvertToLowerCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, |s| s.to_lowercase());
    }

    pub(super) fn convert_to_title_case(&mut self, _: &ConvertToTitleCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, title_case);
    }

    pub(super) fn convert_to_snake_case(&mut self, _: &ConvertToSnakeCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, |s| join_words(s, "_", Case::Lower));
    }

    pub(super) fn convert_to_kebab_case(&mut self, _: &ConvertToKebabCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, |s| join_words(s, "-", Case::Lower));
    }

    pub(super) fn convert_to_camel_case(&mut self, _: &ConvertToCamelCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, |s| join_words(s, "", Case::Camel));
    }

    pub(super) fn convert_to_pascal_case(&mut self, _: &ConvertToPascalCase, _: &mut Window, cx: &mut Context<Self>) {
        self.convert_case(cx, |s| join_words(s, "", Case::Pascal));
    }

    /// Converts each selection, or the word at each caret, line by line (so
    /// indentation and line breaks stay).
    fn convert_case(&mut self, cx: &mut Context<Self>, f: impl Fn(&str) -> String) {
        self.edit_selections(EditKind::Other, cx, |doc, s| {
            let range = if s.is_empty() { word_range(doc, s.head) } else { s.range() };
            let text = doc.slice(range.clone());
            let converted: Vec<String> = text
                .split('\n')
                .map(|line| {
                    let body = line.trim();
                    let lead = &line[..line.len() - line.trim_start().len()];
                    let trail = &line[line.trim_end().len()..];
                    if body.is_empty() { line.to_string() } else { format!("{lead}{}{trail}", f(body)) }
                })
                .collect();
            (range, converted.join("\n"), None)
        });
    }
}

/// Compares like a person would: case-insensitively, numbers by value
/// ("item 2" before "item 10"); ties keep a stable byte order.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn chunks(s: &str) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, text)) if *d == digit => text.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    }
    let (ca, cb) = (chunks(a), chunks(b));
    for ((da, ta), (db, tb)) in ca.iter().zip(&cb) {
        let order = match (da, db) {
            (true, true) => {
                let (na, nb) = (ta.trim_start_matches('0'), tb.trim_start_matches('0'));
                na.len().cmp(&nb.len()).then_with(|| na.cmp(nb))
            }
            _ => ta.to_lowercase().cmp(&tb.to_lowercase()),
        };
        if order.is_ne() {
            return order;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            start = false;
        } else {
            out.push(c);
            start = c.is_whitespace() || c == '-' || c == '_';
        }
    }
    out
}

/// The words of an identifier or phrase: split at spaces, `_`, `-`, `.`,
/// and where case changes (`parseHTTPResponse` → parse, HTTP, Response).
fn words(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut words = Vec::new();
    let mut word = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            continue;
        }
        if let Some(&prev) = word.chars().last().as_ref() {
            let next = chars.get(i + 1).copied();
            let boundary = (c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit()))
                || (c.is_uppercase() && prev.is_uppercase() && next.is_some_and(char::is_lowercase));
            if boundary {
                words.push(std::mem::take(&mut word));
            }
        }
        word.push(c);
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

enum Case {
    Lower,
    Camel,
    Pascal,
}

fn join_words(s: &str, separator: &str, case: Case) -> String {
    let capitalize = |w: &str| {
        let mut chars = w.chars();
        chars.next().map(|first| first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect::<String>()).unwrap_or_default()
    };
    words(s)
        .iter()
        .enumerate()
        .map(|(i, w)| match case {
            Case::Lower => w.to_lowercase(),
            Case::Camel if i == 0 => w.to_lowercase(),
            Case::Camel | Case::Pascal => capitalize(w),
        })
        .collect::<Vec<_>>()
        .join(separator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_cases() {
        assert_eq!(join_words("parseHTTPResponse", "_", Case::Lower), "parse_http_response");
        assert_eq!(join_words("user_id", "", Case::Camel), "userId");
        assert_eq!(join_words("user-id 2", "", Case::Pascal), "UserId2");
        assert_eq!(join_words("Hello World", "-", Case::Lower), "hello-world");
        assert_eq!(join_words("version2Name", "_", Case::Lower), "version2_name");
        assert_eq!(title_case("the QUICK brown-fox"), "The Quick Brown-Fox");
    }

    #[test]
    fn sorts_naturally() {
        let mut lines = vec!["item 10", "Item 2", "apple", "item 1"];
        lines.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(lines, ["apple", "item 1", "Item 2", "item 10"]);
    }
}
