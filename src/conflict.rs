//! Merge conflict regions marked by `<<<<<<<`, `|||||||` (diff3 base),
//! `=======` and `>>>>>>>` lines, and how to resolve them.

use std::ops::Range;

/// One conflict. Rows are buffer lines; the content ranges exclude markers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    /// The `<<<<<<<` row.
    pub start: usize,
    /// The `>>>>>>>` row.
    pub end: usize,
    pub ours: Range<usize>,
    pub base: Option<Range<usize>>,
    pub theirs: Range<usize>,
    /// What follows the markers, e.g. "HEAD" and "feature".
    pub ours_label: String,
    pub theirs_label: String,
}

impl Conflict {
    pub fn rows(&self) -> Range<usize> {
        self.start..self.end + 1
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Keep the current branch's side (ours).
    Current,
    /// Keep the merged-in side (theirs).
    Incoming,
    /// Keep both, current first.
    Both,
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Resolution::Current => "Accept Current",
            Resolution::Incoming => "Accept Incoming",
            Resolution::Both => "Accept Both",
        }
    }

    /// The rows that replace the whole conflict.
    pub fn kept(self, c: &Conflict) -> Vec<Range<usize>> {
        match self {
            Resolution::Current => vec![c.ours.clone()],
            Resolution::Incoming => vec![c.theirs.clone()],
            Resolution::Both => vec![c.ours.clone(), c.theirs.clone()],
        }
    }
}

fn marker<'a>(line: &'a str, mark: char) -> Option<&'a str> {
    let line = line.trim_end_matches(['\n', '\r']);
    let rest = line.strip_prefix(&*mark.to_string().repeat(7))?;
    // Exactly seven: `========` is a Markdown underline, not a marker.
    if rest.starts_with(mark) {
        return None;
    }
    match rest.strip_prefix(' ') {
        Some(label) => Some(label.trim()),
        None if rest.is_empty() => Some(""),
        None => None,
    }
}

/// The conflicts in `text` (rows are `\n`-separated lines).
pub fn parse(text: &str) -> Vec<Conflict> {
    if !text.contains("<<<<<<<") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut open: Option<(usize, String, Option<usize>, Option<usize>)> = None; // start, label, base row, ======= row
    for (row, line) in text.split('\n').enumerate() {
        if let Some(label) = marker(line, '<') {
            open = Some((row, label.to_string(), None, None));
        } else if let Some((_, _, base, sep @ None)) = open.as_mut() {
            if marker(line, '|').is_some() && base.is_none() {
                *base = Some(row);
            } else if marker(line, '=').is_some_and(str::is_empty) {
                *sep = Some(row);
            }
        } else if let Some(label) = marker(line, '>')
            && let Some((start, ours_label, base, Some(sep))) = open.take()
        {
            out.push(Conflict {
                start,
                end: row,
                ours: start + 1..base.unwrap_or(sep),
                base: base.map(|b| b + 1..sep),
                theirs: sep + 1..row,
                ours_label,
                theirs_label: label.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_and_three_way_conflicts() {
        let text = "a\n<<<<<<< HEAD\nours\n=======\ntheirs 1\ntheirs 2\n>>>>>>> feature\nb\n<<<<<<< HEAD\nx\n||||||| base\no\n=======\ny\n>>>>>>> other\n";
        let c = parse(text);
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].start, c[0].end, c[0].ours.clone(), c[0].theirs.clone()), (1, 6, 2..3, 4..6));
        assert_eq!((c[0].ours_label.as_str(), c[0].theirs_label.as_str()), ("HEAD", "feature"));
        assert_eq!((c[1].ours.clone(), c[1].base.clone(), c[1].theirs.clone()), (9..10, Some(11..12), 13..14));
        assert_eq!(Resolution::Both.kept(&c[0]), [2..3, 4..6]);
    }

    #[test]
    fn ignores_lookalikes_and_unfinished_conflicts() {
        assert!(parse("Title\n========\n<<<<<<<< not a marker\n").is_empty());
        assert!(parse("<<<<<<< HEAD\nours\n=======\ntheirs\n").is_empty());
        // An empty side is fine.
        let c = parse("<<<<<<< HEAD\n=======\nt\n>>>>>>> x\n");
        assert_eq!((c[0].ours.clone(), c[0].theirs.clone()), (1..1, 2..3));
    }
}
