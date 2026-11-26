//! The input box's text (flags F33), after Codex's `bottom_pane/textarea.rs`:
//! the text with a cursor on a grapheme boundary, word-wrapped rows that
//! keep the byte ranges they show (cached per width), moves by grapheme,
//! word, line and screen row with a remembered column, and one kill buffer
//! for Ctrl-K, Ctrl-U and the word deletions, which Ctrl-Y yanks back.
//! A mouse selection (kb d150) runs from an anchor to the cursor: typing,
//! Backspace, Delete and a paste replace it, a move or another edit drops
//! it (Codex's textarea `mouse_selection`).
//!
//! Elements (Codex's text elements, kb d191) are ranges of the text that
//! edit as one piece: an image's `[Image #N]`. The cursor and a
//! selection's ends never stop inside one, a move steps over it, Backspace
//! and Delete take it whole, an edit that touches one takes all of it, and
//! the layout never breaks a row inside one. A kill takes its text, which
//! Ctrl-Y gives back as plain text.
//!
//! The text is kept as typed or pasted, and sent so; what is drawn goes
//! through the sanitizer (ui-plan 7, kb d076): a pasted escape shows as
//! `␛`, and the rows measure what is drawn. Codex keeps only text that is
//! safe to draw, so its rows index drawn and stored text alike; here the
//! rows index the stored text and their cells carry what is drawn.

use std::cell::Ref;
use std::cell::RefCell;
use std::ops::Range;

use ratatui::layout::Position;
use unicode_segmentation::UnicodeSegmentation;

use crate::wrap::TextRow;
use crate::wrap::row_of;
use crate::wrap::word_wrap_keeping;

#[derive(Debug, Default, Clone)]
pub struct TextArea {
    text: String,
    /// A byte offset on a grapheme boundary.
    cursor: usize,
    /// The column Up and Down aim for, kept while they repeat.
    preferred: Option<usize>,
    /// What the last kill took.
    killed: String,
    /// The other end of a mouse selection, the cursor being this one.
    anchor: Option<usize>,
    /// The rows for the last width laid out.
    rows: RefCell<Option<(usize, Vec<TextRow>)>>,
    /// The elements, in text order, none overlapping.
    elements: Vec<Element>,
    next_id: u64,
}

/// A range that edits as one piece, and the id that names it while its
/// range moves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    id: u64,
    range: Range<usize>,
}

impl PartialEq for TextArea {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text && self.cursor == other.cursor
    }
}

impl Eq for TextArea {}

/// What a char is for word moves: Codex's and readline's classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

impl TextArea {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The cursor's byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Replaces the text and puts the cursor at its end: a prefill or a
    /// recalled history entry. No element is left.
    pub fn set_text(&mut self, text: &str) {
        self.text = normalize_newlines(text);
        self.cursor = self.text.len();
        self.elements.clear();
        self.changed();
    }

    /// Empties the box and returns what it held.
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.elements.clear();
        self.changed();
        std::mem::take(&mut self.text)
    }

    /// Inserts typed or pasted text at the cursor, in place of the
    /// selection; a paste's `\r\n` and `\r` become newlines and never
    /// submit.
    pub fn insert_str(&mut self, text: &str) {
        self.take_selection();
        let text = normalize_newlines(text);
        let at = self.cursor;
        self.text.insert_str(at, &text);
        self.shift(at, 0, text.len());
        self.cursor = at + text.len();
        self.changed();
    }

    pub fn insert_char(&mut self, c: char) {
        self.take_selection();
        let at = self.cursor;
        self.text.insert(at, c);
        self.shift(at, 0, c.len_utf8());
        self.cursor = at + c.len_utf8();
        self.changed();
    }

    /// Inserts `text` at `pos` (moved off any element), the cursor kept on
    /// the text it was on: a `!` that goes back into the text, a line given
    /// back.
    pub fn insert_at(&mut self, pos: usize, text: &str) {
        let text = normalize_newlines(text);
        let at = self.nearest_edge(self.snap(pos));
        self.text.insert_str(at, &text);
        self.shift(at, 0, text.len());
        if self.cursor >= at {
            self.cursor += text.len();
        }
        self.changed();
    }

    /// Inserts `text` at the cursor as one element, in place of the
    /// selection; its id.
    pub fn insert_element(&mut self, text: &str) -> u64 {
        self.take_selection();
        let at = self.cursor;
        self.insert_str(text);
        self.add_element(at..at + text.len())
    }

    /// Makes `range` of the text an element; `None` when it is empty, off
    /// a char boundary, or meets an element (Codex's `add_element_range`).
    pub fn mark(&mut self, range: Range<usize>) -> Option<u64> {
        let ok = range.start < range.end
            && range.end <= self.text.len()
            && self.text.is_char_boundary(range.start)
            && self.text.is_char_boundary(range.end)
            && !self
                .elements
                .iter()
                .any(|e| e.range.start < range.end && e.range.end > range.start);
        if !ok {
            return None;
        }
        let id = self.add_element(range);
        // A cursor inside it goes to its end.
        self.cursor = self.nearest_edge(self.cursor);
        Some(id)
    }

    fn add_element(&mut self, range: Range<usize>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.elements.push(Element { id, range });
        self.elements.sort_by_key(|e| e.range.start);
        *self.rows.get_mut() = None;
        id
    }

    /// The elements in text order: their ids and ranges.
    pub fn elements(&self) -> Vec<(u64, Range<usize>)> {
        self.elements
            .iter()
            .map(|e| (e.id, e.range.clone()))
            .collect()
    }

    /// The cursor to the end of the text.
    pub fn move_to_end(&mut self) {
        self.move_to(self.text.len());
    }

    /// Writes `text` in place of element `id`'s, which stays one element
    /// (Codex's `replace_element_payload`); how many bytes longer the
    /// text became, if the element is there.
    pub fn rename_element(&mut self, id: u64, text: &str) -> Option<isize> {
        let range = self.elements.iter().find(|e| e.id == id)?.range.clone();
        if self.text[range.clone()] == *text {
            return Some(0);
        }
        self.text.replace_range(range.clone(), text);
        let delta = text.len() as isize - range.len() as isize;
        for e in &mut self.elements {
            if e.id == id {
                e.range.end = range.start + text.len();
            } else if e.range.start >= range.end {
                e.range.start = e.range.start.saturating_add_signed(delta);
                e.range.end = e.range.end.saturating_add_signed(delta);
            }
        }
        if self.cursor >= range.end {
            self.cursor = self.cursor.saturating_add_signed(delta);
        } else if self.cursor > range.start {
            self.cursor = range.start + text.len();
        }
        if let Some(anchor) = self.anchor.filter(|&a| a >= range.end) {
            self.anchor = Some(anchor.saturating_add_signed(delta));
        }
        *self.rows.get_mut() = None;
        Some(delta)
    }

    /// Removes `range` of the text, widened to whole elements.
    pub fn remove_range(&mut self, range: Range<usize>) {
        self.remove(range);
    }

    pub fn backspace(&mut self) {
        if self.take_selection().is_some() {
            return;
        }
        let start = self.prev_atomic(self.cursor);
        self.remove(start..self.cursor);
    }

    pub fn delete(&mut self) {
        if self.take_selection().is_some() {
            return;
        }
        let end = self.next_atomic(self.cursor);
        self.remove(self.cursor..end);
    }

    pub fn move_left(&mut self) {
        self.move_to(self.prev_atomic(self.cursor));
    }

    pub fn move_right(&mut self) {
        self.move_to(self.next_atomic(self.cursor));
    }

    /// To the start of the word before the cursor (Alt-B, Ctrl-Left).
    pub fn move_word_left(&mut self) {
        self.move_to(self.edge(self.word_start(self.cursor), false));
    }

    /// To the end of the word after the cursor (Alt-F, Ctrl-Right).
    pub fn move_word_right(&mut self) {
        self.move_to(self.edge(self.word_end(self.cursor), true));
    }

    /// To the start of the cursor's line.
    pub fn move_home(&mut self) {
        self.move_to(self.line_start(self.cursor));
    }

    /// To the end of the cursor's line.
    pub fn move_end(&mut self) {
        self.move_to(self.line_end(self.cursor));
    }

    /// Deletes the word before the cursor into the kill buffer (Ctrl-W,
    /// Alt-Backspace).
    pub fn delete_word_left(&mut self) {
        let start = self.word_start(self.cursor);
        self.kill(start..self.cursor);
    }

    /// Deletes the word after the cursor into the kill buffer (Alt-D).
    pub fn delete_word_right(&mut self) {
        let end = self.word_end(self.cursor);
        self.kill(self.cursor..end);
    }

    /// Kills to the start of the line (Ctrl-U); at a line's start, the
    /// newline before it.
    pub fn kill_to_line_start(&mut self) {
        let start = self.line_start(self.cursor);
        let start = if start == self.cursor {
            self.cursor.saturating_sub(1)
        } else {
            start
        };
        self.kill(start..self.cursor);
    }

    /// Kills to the end of the line (Ctrl-K); at a line's end, the newline
    /// after it.
    pub fn kill_to_line_end(&mut self) {
        let end = self.line_end(self.cursor);
        let end = if end == self.cursor && end < self.text.len() {
            end + 1
        } else {
            end
        };
        self.kill(self.cursor..end);
    }

    /// Inserts what the last kill took (Ctrl-Y).
    pub fn yank(&mut self) {
        let text = self.killed.clone();
        self.insert_str(&text);
    }

    /// One screen row up at `width`, keeping the column; false on the first
    /// row, where the cursor goes to the start.
    pub fn move_up(&mut self, width: usize) -> bool {
        self.move_row(width, false)
    }

    /// One screen row down at `width`, keeping the column; false on the
    /// last row, where the cursor goes to the end.
    pub fn move_down(&mut self, width: usize) -> bool {
        self.move_row(width, true)
    }

    /// The text in rows at most `width` cells wide, as drawn, and the
    /// cursor's column and row in them. A cursor past a row's last column
    /// (a full row, or spaces hanging past the width) goes to the start of
    /// a row of its own at the end of the text, and onto the last column
    /// elsewhere.
    pub fn layout(&self, width: u16) -> (Vec<String>, Position) {
        let width = usize::from(width.max(1));
        let rows = self.rows(width);
        let mut drawn: Vec<String> = rows.iter().map(TextRow::drawn).collect();
        let row = row_of(&rows, self.cursor);
        let mut column = rows.get(row).map_or(0, |r| r.column_of(self.cursor));
        let mut y = row;
        if column >= width {
            if self.cursor == self.text.len() {
                drawn.push(String::new());
                y += 1;
                column = 0;
            } else {
                column = width - 1;
            }
        }
        (drawn, position(y, column))
    }

    fn rows(&self, width: usize) -> Ref<'_, [TextRow]> {
        let stale = self.rows.borrow().as_ref().is_none_or(|(w, _)| *w != width);
        if stale {
            let whole: Vec<Range<usize>> = self.elements.iter().map(|e| e.range.clone()).collect();
            *self.rows.borrow_mut() = Some((width, word_wrap_keeping(&self.text, width, &whole)));
        }
        Ref::map(self.rows.borrow(), |rows| match rows {
            Some((_, rows)) => rows.as_slice(),
            None => &[],
        })
    }

    fn move_row(&mut self, width: usize, down: bool) -> bool {
        let width = width.max(1);
        let (target, column) = {
            let rows = self.rows(width);
            let row = row_of(&rows, self.cursor);
            let column = self
                .preferred
                .unwrap_or_else(|| rows.get(row).map_or(0, |r| r.column_of(self.cursor)));
            let target = if down {
                rows.get(row + 1)
            } else {
                row.checked_sub(1).and_then(|r| rows.get(r))
            };
            (target.map(|r| r.pos_at(column)), column)
        };
        match target {
            Some(pos) => {
                self.cursor = self.nearest_edge(pos);
                self.preferred = Some(column);
                self.anchor = None;
                true
            }
            None => {
                self.move_to(if down { self.text.len() } else { 0 });
                false
            }
        }
    }

    fn move_to(&mut self, pos: usize) {
        self.cursor = pos;
        self.preferred = None;
        self.anchor = None;
    }

    fn remove(&mut self, range: Range<usize>) {
        let range = self.whole(range);
        self.text.replace_range(range.clone(), "");
        self.shift(range.start, range.len(), 0);
        self.cursor = range.start;
        self.changed();
    }

    fn kill(&mut self, range: Range<usize>) {
        let range = self.whole(range);
        if range.is_empty() {
            return;
        }
        self.killed = self.text[range.clone()].to_string();
        self.remove(range);
    }

    /// `range` widened to every element it meets (Codex's
    /// `expand_range_to_element_boundaries`).
    fn whole(&self, mut range: Range<usize>) -> Range<usize> {
        for e in &self.elements {
            if e.range.start < range.end && e.range.end > range.start {
                range.start = range.start.min(e.range.start);
                range.end = range.end.max(e.range.end);
            }
        }
        range
    }

    /// The elements after an edit at `at` that removed `removed` bytes
    /// and inserted `inserted`: those inside the removed bytes are gone,
    /// those after move (Codex's `shift_elements`; edits are widened to
    /// whole elements first, so none is cut).
    fn shift(&mut self, at: usize, removed: usize, inserted: usize) {
        let end = at + removed;
        if removed > 0 {
            self.elements
                .retain(|e| !(e.range.start >= at && e.range.end <= end));
        }
        for e in &mut self.elements {
            if e.range.start >= end {
                e.range.start = e.range.start - removed + inserted;
                e.range.end = e.range.end - removed + inserted;
            }
        }
    }

    /// The element `pos` is strictly inside.
    fn inside(&self, pos: usize) -> Option<&Range<usize>> {
        self.elements
            .iter()
            .map(|e| &e.range)
            .find(|r| r.start < pos && pos < r.end)
    }

    /// `pos`, or the edge of the element it is inside: its end when
    /// `to_end`, else its start.
    fn edge(&self, pos: usize, to_end: bool) -> usize {
        match self.inside(pos) {
            Some(r) if to_end => r.end,
            Some(r) => r.start,
            None => pos,
        }
    }

    /// `pos`, or the nearer edge of the element it is inside.
    fn nearest_edge(&self, pos: usize) -> usize {
        match self.inside(pos) {
            Some(r) if pos - r.start <= r.end - pos => r.start,
            Some(r) => r.end,
            None => pos,
        }
    }

    /// One step back: over a grapheme, or over the element ending here.
    fn prev_atomic(&self, pos: usize) -> usize {
        match self
            .elements
            .iter()
            .find(|e| e.range.start < pos && pos <= e.range.end)
        {
            Some(e) => e.range.start,
            None => self.edge(self.prev_boundary(pos), false),
        }
    }

    /// One step on: over a grapheme, or over the element starting here.
    fn next_atomic(&self, pos: usize) -> usize {
        match self
            .elements
            .iter()
            .find(|e| e.range.start <= pos && pos < e.range.end)
        {
            Some(e) => e.range.end,
            None => self.edge(self.next_boundary(pos), true),
        }
    }

    fn changed(&mut self) {
        self.preferred = None;
        self.anchor = None;
        *self.rows.get_mut() = None;
    }

    /// The selected bytes, if any are.
    pub fn selection(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.anchor.filter(|&a| a != self.cursor)?;
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub fn selected(&self) -> Option<&str> {
        self.selection().map(|range| &self.text[range])
    }

    /// Selects from `anchor` to `cursor`, both put on the grapheme they
    /// are in, the cursor at `cursor`; a selection that reaches into an
    /// element takes all of it. The same place for both selects nothing
    /// and only moves the cursor, to the nearer edge of an element it is
    /// in.
    pub fn select(&mut self, anchor: usize, cursor: usize) {
        let (anchor, cursor) = (self.snap(anchor), self.snap(cursor));
        let (anchor, cursor) = match anchor.cmp(&cursor) {
            std::cmp::Ordering::Equal => (self.nearest_edge(cursor), self.nearest_edge(cursor)),
            std::cmp::Ordering::Less => (self.edge(anchor, false), self.edge(cursor, true)),
            std::cmp::Ordering::Greater => (self.edge(anchor, true), self.edge(cursor, false)),
        };
        self.cursor = cursor;
        self.anchor = (anchor != self.cursor).then_some(anchor);
        self.preferred = None;
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    /// Removes the selection and returns it (Ctrl+X, or what typing
    /// replaces).
    pub fn take_selection(&mut self) -> Option<String> {
        let range = self.selection()?;
        let taken = self.text[range.clone()].to_string();
        self.remove(range);
        Some(taken)
    }

    /// The byte offset drawn at `column` of row `row` at `width`; below
    /// the last row, the end of the text.
    pub fn pos_at(&self, width: u16, row: usize, column: usize) -> usize {
        let rows = self.rows(usize::from(width.max(1)));
        rows.get(row).map_or(self.text.len(), |r| r.pos_at(column))
    }

    /// The word (by Unicode's word bounds, a run of spaces counting as
    /// one) and the line around `pos`, for a double and a triple click.
    pub fn word_around(&self, pos: usize) -> std::ops::Range<usize> {
        // Words never cross a line: only the line is looked at.
        let start = self.line_start(pos);
        self.text[start..self.line_end(pos)]
            .split_word_bound_indices()
            .map(|(at, word)| start + at..start + at + word.len())
            .find(|range| range.contains(&pos))
            .unwrap_or(pos..pos)
    }

    pub fn line_around(&self, pos: usize) -> std::ops::Range<usize> {
        self.line_start(pos)..self.line_end(pos)
    }

    /// The cells the selection takes in each row at `width`: the row,
    /// and its first and last column, the end not included.
    pub fn selected_cells(&self, width: u16) -> Vec<(usize, usize, usize)> {
        let Some(selection) = self.selection() else {
            return Vec::new();
        };
        let rows = self.rows(usize::from(width.max(1)));
        rows.iter()
            .enumerate()
            .filter(|(_, row)| row.range.start < selection.end && row.range.end > selection.start)
            .filter_map(|(n, row)| {
                let mut cells = row
                    .cells
                    .iter()
                    .filter(|c| c.range.start >= selection.start && c.range.end <= selection.end);
                let first = cells.next()?;
                let last = cells.next_back().unwrap_or(first);
                Some((n, first.column, last.column + last.width))
            })
            .collect()
    }

    /// The start of the grapheme `pos` is in (`pos` on a char boundary),
    /// found from the end of that char backwards, not from the text's
    /// start.
    fn snap(&self, pos: usize) -> usize {
        let pos = pos.min(self.text.len());
        let Some(c) = self.text[pos..].chars().next() else {
            return pos;
        };
        self.text[..pos + c.len_utf8()]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn prev_boundary(&self, pos: usize) -> usize {
        self.text[..pos]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, pos: usize) -> usize {
        self.text[pos..]
            .graphemes(true)
            .next()
            .map_or(pos, |g| pos + g.len())
    }

    /// Back over spaces, then over chars of the class before them.
    fn word_start(&self, pos: usize) -> usize {
        let mut chars = self.text[..pos].char_indices().rev().peekable();
        while chars.next_if(|&(_, c)| class(c) == Class::Space).is_some() {}
        let Some(&(_, first)) = chars.peek() else {
            return 0;
        };
        let kind = class(first);
        let mut start = pos;
        for (i, c) in chars {
            if class(c) != kind {
                break;
            }
            start = i;
        }
        start.min(pos)
    }

    /// Forward over spaces, then over chars of the class after them.
    fn word_end(&self, pos: usize) -> usize {
        let mut chars = self.text[pos..].char_indices().peekable();
        while chars.next_if(|&(_, c)| class(c) == Class::Space).is_some() {}
        let Some(&(_, first)) = chars.peek() else {
            return self.text.len();
        };
        let kind = class(first);
        let mut end = self.text.len();
        for (i, c) in chars {
            if class(c) != kind {
                end = pos + i;
                break;
            }
        }
        end
    }

    fn line_start(&self, pos: usize) -> usize {
        self.text[..pos].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, pos: usize) -> usize {
        self.text[pos..]
            .find('\n')
            .map_or(self.text.len(), |i| pos + i)
    }
}

fn position(row: usize, column: usize) -> Position {
    Position::new(
        u16::try_from(column).unwrap_or(u16::MAX),
        u16::try_from(row).unwrap_or(u16::MAX),
    )
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
#[path = "textarea_tests.rs"]
mod tests;
