//! What a copy of the transcript gives (kb d150): the text of the rows a
//! selection covers, as Codex's transcript view copies it
//! (`transcript_view/text.rs`): logical lines, not screen rows. A cell's
//! gutter, a wrapped row's hanging indent and code's indent are left out,
//! rows a wrap cut apart are joined again with the spaces the wrap took
//! out, a line ends without its trailing spaces, and what is not the
//! chat's text (the header, a code block's frame, hints, a tool card's
//! padding and status) is not copied.
//!
//! The text is the rows' text as drawn, after the sanitizer (kb d076),
//! never the source: a control, bidi or zero-width char the model sent is
//! copied as the `␛` or `<U+202E>` shown for it, so what is pasted is what
//! was read. Tabs are copied as the spaces drawn for them.

use ratatui::buffer::CellWidth;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

/// How a copy reads one row: its text is in cells `decor..end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyRule {
    /// Cells on the left that are not text: the cell's gutter (`you › `,
    /// a tool's `  └ `), a wrapped row's hanging indent, code's indent.
    pub decor: usize,
    /// Where the text ends: a tool card's header stops before the padding
    /// that puts its status on the right. `usize::MAX` for the whole row.
    pub end: usize,
    /// `Some` when the row goes on with the line of the row above, cut
    /// apart by a wrap: the spaces the wrap took out between them (none
    /// where a long word or code was cut). `None` when the row starts a
    /// line.
    pub wrap: Option<usize>,
    /// Never copied: the header, a code block's frame, a hint such as
    /// "+N lines" or "Ctrl-T shows it".
    pub omit: bool,
}

impl Default for CopyRule {
    fn default() -> Self {
        Self::row(0, None)
    }
}

impl CopyRule {
    /// A row a copy leaves out.
    pub const OMIT: Self = Self {
        decor: 0,
        end: 0,
        wrap: None,
        omit: true,
    };

    /// A row whose text is after `decor` cells, going on with the line
    /// above if `wrap` is `Some` (see [`CopyRule::wrap`]).
    pub fn row(decor: usize, wrap: Option<usize>) -> Self {
        Self {
            decor,
            end: usize::MAX,
            wrap,
            omit: false,
        }
    }

    /// A row that starts a line, its text in its first `end` cells.
    pub fn upto(end: usize) -> Self {
        Self {
            end,
            ..Self::default()
        }
    }
}

/// A place in a run of rows: the row, and a cell in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

impl Point {
    pub fn new(row: usize, column: usize) -> Self {
        Self { row, column }
    }
}

/// The text of `rows` from `from` up to `to`, `to`'s cell not included,
/// either way round; `usize::MAX` as a column takes the row to its end.
/// Rows are numbered from 0 in the order given.
pub fn copy_text<'a, 'l: 'a>(
    rows: impl IntoIterator<Item = (&'a Line<'l>, &'a CopyRule)>,
    from: Point,
    to: Point,
) -> String {
    let (from, to) = if to < from { (to, from) } else { (from, to) };
    let mut out = String::new();
    let mut started = false;
    let picked = rows
        .into_iter()
        .enumerate()
        .skip(from.row)
        .take((to.row - from.row).saturating_add(1));
    for (n, (line, rule)) in picked {
        if rule.omit {
            continue;
        }
        if started {
            match rule.wrap {
                Some(gap) => out.extend(std::iter::repeat_n(' ', gap)),
                None => {
                    trim_spaces(&mut out);
                    out.push('\n');
                }
            }
        }
        started = true;
        let lo = if n == from.row { from.column } else { 0 };
        let hi = if n == to.row { to.column } else { usize::MAX };
        push_cells(&mut out, line, lo.max(rule.decor), hi.min(rule.end));
    }
    trim_spaces(&mut out);
    out
}

/// Appends the text `line` draws in cells `lo..hi`, a wide char when any
/// of its cells is in them. The graphemes are taken as ratatui draws a
/// line (`Buffer::set_stringn`): none holding a control, none of no width.
fn push_cells(out: &mut String, line: &Line<'_>, lo: usize, hi: usize) {
    let mut column = 0;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            if grapheme.contains(char::is_control) {
                continue;
            }
            let width = usize::from(grapheme.cell_width());
            if width == 0 {
                continue;
            }
            if column >= hi {
                return;
            }
            if column + width > lo {
                out.push_str(grapheme);
            }
            column += width;
        }
    }
}

/// The cells of the word under `column` in `line`, the end not included,
/// by Unicode's word bounds (Codex's `SelectionUnit::Word`): a run of
/// spaces or a punctuation mark is a word of its own. `(column, column)`
/// past the text.
pub fn word_at(line: &Line<'_>, column: usize) -> (usize, usize) {
    let mut text = String::new();
    // Each drawn grapheme: where it starts in `text`, and its first cell.
    let mut cells: Vec<(usize, usize)> = Vec::new();
    let mut end = 0;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            let width = usize::from(grapheme.cell_width());
            if grapheme.contains(char::is_control) || width == 0 {
                continue;
            }
            cells.push((text.len(), end));
            text.push_str(grapheme);
            end += width;
        }
    }
    let column_of = |byte: usize| {
        cells
            .iter()
            .find(|&&(at, _)| at >= byte)
            .map_or(end, |&(_, cell)| cell)
    };
    text.split_word_bound_indices()
        .map(|(start, word)| (column_of(start), column_of(start + word.len())))
        .find(|&(lo, hi)| (lo..hi).contains(&column))
        .unwrap_or((column, column))
}

fn trim_spaces(out: &mut String) {
    let kept = out.trim_end_matches(' ').len();
    out.truncate(kept);
}

#[cfg(test)]
#[path = "copytext_tests.rs"]
mod tests;
