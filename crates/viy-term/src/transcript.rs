//! The transcript above the live region: the cells of [`crate::cells`],
//! which viy lays out, scrolls and draws itself (ui-plan 2 and 3; Codex's
//! `transcript_view.rs`). It follows the bottom until scrolled back, and
//! then holds still while rows arrive. The reading position is an anchor in
//! a cell (a row's key), not a screen row, so a resize lays every cell out
//! again at the new width and the same text stays at the top (Codex's
//! anchors).
//!
//! Below the cells is the live tail, as in Codex's transcript view: what
//! the turn running is writing (the reasoning trace, the answer's open
//! block, the tool cards running), drawn again each frame and scrolled
//! with the rest. Read back into it, the view keeps the same row at the top
//! when the tail moves up into the cells.

use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Position as ScreenPosition;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use tokio::time::Instant;

pub use crate::cells::Cell;
use crate::copytext::CopyRule;
use crate::copytext::Point;
use crate::copytext::copy_text;
use crate::copytext::word_at;
use crate::markdown::Row;

/// Clicks this close together, on the same cell, count as a double or
/// triple click (Codex's `click_count`).
const MULTI_CLICK: Duration = Duration::from_millis(400);
/// How often a drag held at the top or bottom edge scrolls a row.
pub const EDGE_SCROLL: Duration = Duration::from_millis(50);

/// What the mouse has picked in the transcript (kb d150), in its rows:
/// counted down the whole transcript, the live tail last, so rows that
/// arrive below never move it. A resize, Ctrl-T or a new transcript
/// clears it, and so does a change to the text of the rows it covers (a
/// tool card folding, an answer laid out again, the live tail redrawn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    /// What the press picked, `to` not included: nothing for a click, a
    /// word for a double click, a logical line for a triple.
    from: Point,
    to: Point,
    /// The cell under the pointer, once the drag moved off the press.
    head: Option<Point>,
    /// The button is down.
    dragging: bool,
    /// The screen row pressed, and the pointer's cell while dragging.
    pressed: u16,
    pointer: (u16, u16),
    /// The pointer left the row pressed (Codex's `moved_vertically`):
    /// only then does a drag held at an edge scroll.
    moved: bool,
    /// What the rows it covers drew when it was made or last moved.
    drawn: u64,
}

impl Selection {
    /// The cells picked, the end not included: the cell pressed and the
    /// one under the pointer both are, whichever way the drag went.
    fn range(&self) -> (Point, Point) {
        let pressed = Point::new(self.from.row, self.from.column.saturating_add(1));
        match self.head {
            None => (self.from, self.to),
            Some(head) if head < self.from => (head, self.to.max(pressed)),
            Some(head) => (
                self.from,
                self.to
                    .max(Point::new(head.row, head.column.saturating_add(1))),
            ),
        }
    }

    fn is_empty(&self) -> bool {
        let (from, to) = self.range();
        from >= to
    }
}

/// Where the last render put the rows: the transcript's row `first` at
/// screen row `top`, `count` rows down, inside `area`.
#[derive(Debug, Clone, Copy, Default)]
struct Drawn {
    area: Rect,
    top: u16,
    first: usize,
    count: usize,
}

/// Where the view is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Position {
    /// The last rows, and the rows that arrive.
    #[default]
    Latest,
    /// Scrolled back: this text at the top.
    Reading(Anchor),
    /// Scrolled back into the live tail: its row this far down at the top.
    Live(usize),
}

/// A place in the text: a cell and the key of a row in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Anchor {
    cell: usize,
    key: (usize, usize),
}

/// A cell laid out at one width.
#[derive(Debug, Default)]
struct Layout {
    rows: Vec<Line<'static>>,
    /// Each row's key, growing down the cell.
    starts: Vec<(usize, usize)>,
    /// How a copy reads each row.
    copies: Vec<CopyRule>,
}

impl Layout {
    fn new(cell: &Cell, width: usize, expanded: bool) -> Self {
        let mut layout = Self::default();
        for row in cell.rows(width, expanded) {
            layout.rows.push(row.line);
            layout.starts.push(row.key);
            layout.copies.push(row.copy);
        }
        layout
    }

    /// The row holding `key`.
    fn row_of(&self, key: (usize, usize)) -> usize {
        self.starts
            .partition_point(|&start| start <= key)
            .saturating_sub(1)
    }
}

#[derive(Debug, Default)]
pub struct TranscriptView {
    position: Position,
    /// The width the layouts are for.
    width: u16,
    /// One per cell, in order.
    layouts: Vec<Layout>,
    /// Rows the last render showed: a page.
    page: usize,
    /// Tool output and reasoning drawn whole (Ctrl-T).
    expanded: bool,
    /// A cell to bring into view on the next render.
    reveal: Option<usize>,
    /// The live tail's rows, below the cells.
    live: Vec<Row>,
    /// Rows of the cells at the last layout, and whether they have been
    /// laid out anew since (a width or Ctrl-T), so a row in the live tail
    /// can be found again after the cells grew.
    laid: usize,
    relaid: bool,
    selection: Option<Selection>,
    /// The view stopped following the end for a selection.
    holding: bool,
    /// While holding a transcript shorter than the area: the blank rows
    /// above it when the selection began, kept so its text stays put.
    held_gap: Option<usize>,
    /// The transcript opens with the header, drawn from the top.
    headed: bool,
    /// The last press: when, where, and how many clicks it made.
    last_click: Option<(Instant, u16, u16, u8)>,
    /// When a drag held at the edge last scrolled.
    edge_scrolled: Option<Instant>,
    drawn: Drawn,
}

impl TranscriptView {
    /// Cell `index` and the ones after it changed: they are laid out again
    /// on the next render.
    pub fn changed(&mut self, index: usize) {
        self.layouts.truncate(index);
    }

    /// Draws tool output and reasoning whole, or folded again.
    pub fn set_expanded(&mut self, expanded: bool) {
        if expanded != self.expanded {
            self.expanded = expanded;
            self.layouts.clear();
            self.relaid = true;
            self.clear_selection();
        }
    }

    /// Forgets the layouts, for a transcript whose cells were replaced
    /// (rewind, `/new`, resume), and follows its end.
    pub fn reset(&mut self) {
        self.layouts.clear();
        self.position = Position::Latest;
        self.clear_selection();
    }

    pub fn is_following(&self) -> bool {
        self.position == Position::Latest
    }

    /// Rows the last render showed.
    pub fn page(&self) -> usize {
        self.page
    }

    /// The live tail's rows, drawn below the cells from the next render on.
    pub fn set_live(&mut self, rows: Vec<Row>) {
        self.live = rows;
    }

    /// Rows below the view, for the footer's "↓ N rows (End)".
    pub fn rows_below(&self) -> usize {
        self.total().saturating_sub(self.top() + self.shown())
    }

    /// Rows the view shows at most: the page, less the blank rows a held
    /// short transcript keeps above it.
    fn shown(&self) -> usize {
        self.page - self.held_gap.unwrap_or(0).min(self.page)
    }

    /// Scrolls back `rows`; nothing when all of it fits.
    pub fn scroll_up(&mut self, rows: usize) {
        if self.total() > self.page {
            self.position = self.position_at(self.top().saturating_sub(rows));
        }
    }

    /// Scrolls forward `rows`; reaching the end follows it again.
    pub fn scroll_down(&mut self, rows: usize) {
        let top = self.top() + rows;
        self.position = if top + self.page >= self.total() {
            Position::Latest
        } else {
            self.position_at(top)
        };
    }

    /// The transcript's start.
    pub fn home(&mut self) {
        if self.total() > self.page {
            self.position = self.position_at(0);
        }
    }

    /// Brings cell `index` into view on the next render, if it is not.
    pub fn reveal(&mut self, index: usize) {
        self.reveal = Some(index);
    }

    /// Follows the end again.
    pub fn end(&mut self) {
        self.position = Position::Latest;
    }

    /// Draws `cells` into `area`: the page at the reading position, or the
    /// last rows; a transcript shorter than the area sits at its bottom,
    /// right above the live region, as a shell's output does, but one that
    /// opens with the centipede header sits at the top, the header being
    /// the top of the chat, until it fills the area and scrolls.
    pub fn render(&mut self, cells: &[Cell], area: Rect, buf: &mut Buffer) {
        self.prepare(cells, area.width, area.height);
        let top = self.top();
        let end = (top + self.shown()).min(self.total());
        self.headed = matches!(cells.first(), Some(Cell::Header(_)));
        let gap = match self.held_gap {
            Some(held) => held.min(self.page),
            None if self.headed => 0,
            None => self.page - (end - top),
        };
        let y = (area.y..area.bottom()).skip(gap);
        for (y, row) in y.zip(self.rows(top, end)) {
            buf.set_line(area.x, y, row, area.width);
        }
        self.drawn = Drawn {
            area,
            top: area.y + u16::try_from(gap).unwrap_or(area.height),
            first: top,
            count: end - top,
        };
        // Text that changed under the selection is no longer what it
        // picked.
        if self
            .selection
            .is_some_and(|selection| self.fingerprint(selection.range()) != selection.drawn)
        {
            self.clear_selection();
        }
        self.highlight(buf);
    }

    /// Lays `cells` out for an area of `width` x `height`, so
    /// [`Self::rows_below`] counts what the next render shows; rendering
    /// does it too.
    pub fn prepare(&mut self, cells: &[Cell], width: u16, height: u16) {
        let before = self.laid;
        self.lay_out(cells, width);
        let relaid = std::mem::take(&mut self.relaid);
        self.laid = self.cells_total();
        self.page = usize::from(height);
        // What was read in the live tail may have moved up into the cells,
        // to the same row: the text stays put.
        if let Position::Live(offset) = self.position
            && !relaid
        {
            self.position = self.position_at(before + offset);
        }
        if let Some(cell) = self.reveal.take()
            && cell < self.layouts.len()
        {
            let start: usize = self.layouts[..cell].iter().map(|l| l.rows.len()).sum();
            let end = start + self.layouts[cell].rows.len();
            let top = self.top();
            if start < top || end > top + self.page {
                // A third of a page above it, for what led to it.
                let row = start.saturating_sub(self.page / 3);
                self.position = self.position_at(row);
            }
        }
        if self.position != Position::Latest
            && !self.holding
            && self.top() + self.page >= self.total()
        {
            self.position = Position::Latest;
        }
    }

    /// Rows `start..end` of the whole transcript, the live tail last.
    fn rows(&self, start: usize, end: usize) -> impl Iterator<Item = &Line<'static>> {
        self.copy_rows()
            .map(|(line, _)| line)
            .skip(start)
            .take(end - start)
    }

    /// Every row and how a copy reads it, the live tail last.
    fn copy_rows(&self) -> impl Iterator<Item = (&Line<'static>, &CopyRule)> {
        self.layouts
            .iter()
            .flat_map(|layout| layout.rows.iter().zip(&layout.copies))
            .chain(self.live.iter().map(|row| (&row.line, &row.copy)))
    }

    fn lay_out(&mut self, cells: &[Cell], width: u16) {
        if width != self.width {
            self.width = width;
            self.layouts.clear();
            self.relaid = true;
            self.clear_selection();
        }
        self.layouts.truncate(cells.len());
        let width = usize::from(width.max(1));
        for cell in &cells[self.layouts.len()..] {
            self.layouts.push(Layout::new(cell, width, self.expanded));
        }
    }

    fn total(&self) -> usize {
        self.cells_total() + self.live.len()
    }

    /// Rows of the cells, without the live tail.
    fn cells_total(&self) -> usize {
        self.layouts.iter().map(|l| l.rows.len()).sum()
    }

    /// The first row shown.
    fn top(&self) -> usize {
        match self.position {
            Position::Latest => self.total().saturating_sub(self.page),
            Position::Reading(anchor) => self.row_of(anchor),
            Position::Live(offset) => self.cells_total() + offset.min(self.live.len()),
        }
    }

    fn row_of(&self, anchor: Anchor) -> usize {
        let before: usize = self.layouts[..anchor.cell.min(self.layouts.len())]
            .iter()
            .map(|l| l.rows.len())
            .sum();
        before
            + self
                .layouts
                .get(anchor.cell)
                .map_or(0, |l| l.row_of(anchor.key))
    }

    /// Whether text is selected: something a copy would take.
    pub fn has_selection(&self) -> bool {
        self.selected_text().is_some()
    }

    /// Drops the selection; the view stays where it is.
    pub fn clear_selection(&mut self) {
        self.selection = None;
        self.holding = false;
        self.held_gap = None;
        self.edge_scrolled = None;
    }

    /// The selected text as a copy gives it ([`crate::copytext`]); `None`
    /// when it would copy nothing (a gutter, a hint, a code frame).
    pub fn selected_text(&self) -> Option<String> {
        let selection = self.selection.filter(|s| !s.is_empty())?;
        let (from, to) = selection.range();
        Some(copy_text(self.copy_rows(), from, to)).filter(|text| !text.is_empty())
    }

    /// A left press on screen cell `column`, `row`: on the transcript's
    /// rows it starts a selection, and the view stops following the end
    /// until the selection is dropped; true if it did. A second or third
    /// press on the same cell soon after picks the word or the logical
    /// line under it (Codex's `SelectionUnit`); with Shift it moves the
    /// end of the selection there.
    pub fn press(&mut self, column: u16, row: u16, shift: bool, now: Instant) -> bool {
        let Some(point) = self.point_at(column, row, false) else {
            self.last_click = None;
            return false;
        };
        if shift && let Some(mut selection) = self.selection {
            selection.head = Some(point);
            selection.dragging = true;
            selection.pressed = row;
            selection.pointer = (column, row);
            selection.moved = false;
            self.set_selection(selection);
            return true;
        }
        let (from, to) = match self.clicks(column, row, now) {
            2 => self.word(point),
            3 => self.logical_line(point),
            _ => (point, point),
        };
        self.hold();
        self.set_selection(Selection {
            from,
            to,
            head: None,
            dragging: true,
            pressed: row,
            pointer: (column, row),
            moved: false,
            drawn: 0,
        });
        true
    }

    /// The pointer moved to `column`, `row` with the button down.
    pub fn drag(&mut self, column: u16, row: u16) {
        let Some(point) = self.point_at(column, row, true) else {
            return;
        };
        let Some(mut selection) = self.selection.filter(|s| s.dragging) else {
            return;
        };
        // Inside the cell pressed it is still a click.
        if selection.head.is_some() || point != selection.from {
            selection.head = Some(point);
        }
        selection.pointer = (column, row);
        selection.moved |= row != selection.pressed;
        self.set_selection(selection);
    }

    /// The button came up at `column`, `row`: the selected text, to copy.
    /// A click, or a drag over nothing a copy takes, drops the selection.
    pub fn release(&mut self, column: u16, row: u16) -> Option<String> {
        self.drag(column, row);
        let selection = self.selection.as_mut()?;
        if !std::mem::take(&mut selection.dragging) {
            return None;
        }
        self.edge_scrolled = None;
        let text = self.selected_text();
        if text.is_none() {
            self.clear_selection();
        }
        text
    }

    /// While a drag is held on the top or bottom row, scrolls one row
    /// every [`EDGE_SCROLL`], the selection's end going to the cell now
    /// under the pointer (Codex's `tick_selection`); true if it scrolled.
    /// The view stays held, even at the end. Where nothing is left to
    /// scroll it stops until the pointer moves.
    pub fn tick_edge(&mut self, now: Instant) -> bool {
        let Some(edge) = self.edge() else {
            self.edge_scrolled = None;
            return false;
        };
        if self.edge_scrolled.is_some_and(|at| now < at + EDGE_SCROLL) {
            return false;
        }
        let before = self.top();
        let last_top = self.total().saturating_sub(self.shown());
        let top = if edge < 0 {
            before.saturating_sub(1)
        } else {
            (before + 1).min(last_top)
        };
        let Some(mut selection) = self.selection else {
            return false;
        };
        if top == before {
            self.edge_scrolled = None;
            selection.moved = false;
            self.selection = Some(selection);
            return false;
        }
        self.edge_scrolled = Some(now);
        self.position = self.position_at(top);
        let drawn = self.drawn;
        let (column, row) = selection.pointer;
        let offset = i64::from(row) - i64::from(drawn.top);
        let count = drawn.count.max(1);
        selection.head = Some(if offset < 0 {
            Point::new(top, 0)
        } else if usize::try_from(offset).unwrap_or(usize::MAX) >= count {
            let last = (top + count).min(self.total()).saturating_sub(1);
            Point::new(last, usize::MAX)
        } else {
            let column = usize::from(column.saturating_sub(drawn.area.x));
            Point::new(top + usize::try_from(offset).unwrap_or(0), column)
        });
        self.set_selection(selection);
        true
    }

    /// When the next edge scroll is due, while a drag is held at an edge.
    pub fn next_edge_scroll(&self, now: Instant) -> Option<Instant> {
        self.edge()?;
        Some(self.edge_scrolled.map_or(now, |at| at + EDGE_SCROLL))
    }

    /// -1 or 1 while a drag that moved off the row pressed is held at the
    /// top or bottom row of the transcript.
    fn edge(&self) -> Option<i8> {
        let selection = self.selection.filter(|s| s.dragging && s.moved)?;
        let area = self.drawn.area;
        let row = selection.pointer.1;
        if row <= area.y {
            Some(-1)
        } else if row + 1 >= area.bottom() {
            Some(1)
        } else {
            None
        }
    }

    /// Keeps `selection`, with what its rows draw now.
    fn set_selection(&mut self, mut selection: Selection) {
        selection.drawn = self.fingerprint(selection.range());
        self.selection = Some(selection);
    }

    /// What rows `from.row..=to.row` draw, in short.
    fn fingerprint(&self, (from, to): (Point, Point)) -> u64 {
        use std::hash::Hash;
        use std::hash::Hasher;
        let mut hasher = std::hash::DefaultHasher::new();
        let rows = to.row.saturating_sub(from.row).saturating_add(1);
        for (line, _) in self.copy_rows().skip(from.row).take(rows) {
            for span in &line.spans {
                span.content.hash(&mut hasher);
            }
            0xffu8.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Counts a press at `column`, `row` (Codex's `click_count`).
    fn clicks(&mut self, column: u16, row: u16, now: Instant) -> u8 {
        let clicks = self
            .last_click
            .filter(|&(at, x, y, _)| {
                now.saturating_duration_since(at) < MULTI_CLICK && x == column && y == row
            })
            .map_or(1, |(_, _, _, n)| n % 3 + 1);
        self.last_click = Some((now, column, row, clicks));
        clicks
    }

    /// Stops following the end while a selection is live, so new rows do
    /// not move the text under the pointer: a long transcript stays at
    /// the row it shows at the top, a short one at its place on screen.
    fn hold(&mut self) {
        if self.position == Position::Latest {
            if self.total() > self.page {
                self.position = self.position_at(self.top());
            } else if !self.headed && self.total() > 0 {
                self.held_gap = Some(usize::from(self.drawn.top - self.drawn.area.y));
                self.position = self.position_at(0);
            }
        }
        self.holding = true;
    }

    /// The transcript's place under screen cell `column`, `row` as last
    /// drawn. A press must be on a drawn row; a drag past the rows takes
    /// the nearest one, past its end below.
    fn point_at(&self, column: u16, row: u16, clamp: bool) -> Option<Point> {
        let drawn = self.drawn;
        if drawn.count == 0 {
            return None;
        }
        let inside = drawn.area.contains(ScreenPosition::new(column, row));
        let offset = i64::from(row) - i64::from(drawn.top);
        let column = usize::from(column.saturating_sub(drawn.area.x));
        let count = i64::try_from(drawn.count).unwrap_or(i64::MAX);
        let on_rows = (0..count).contains(&offset);
        if !clamp {
            return (inside && on_rows)
                .then(|| Point::new(drawn.first + usize::try_from(offset).unwrap_or(0), column));
        }
        Some(if offset < 0 {
            Point::new(drawn.first, 0)
        } else if on_rows {
            Point::new(drawn.first + usize::try_from(offset).unwrap_or(0), column)
        } else {
            Point::new(drawn.first + drawn.count - 1, usize::MAX)
        })
    }

    /// The word under `point`: its cells on that row.
    fn word(&self, point: Point) -> (Point, Point) {
        let Some((line, _)) = self.copy_rows().nth(point.row) else {
            return (point, point);
        };
        let (start, end) = word_at(line, point.column);
        (Point::new(point.row, start), Point::new(point.row, end))
    }

    /// The logical line under `point`: its rows, up to where a wrap
    /// started it and down to where its last wrap ends.
    fn logical_line(&self, point: Point) -> (Point, Point) {
        let wraps: Vec<bool> = self
            .copy_rows()
            .map(|(_, rule)| rule.wrap.is_some())
            .collect();
        let mut first = point.row.min(wraps.len().saturating_sub(1));
        while first > 0 && wraps[first] {
            first -= 1;
        }
        let mut last = first;
        while wraps.get(last + 1).copied().unwrap_or(false) {
            last += 1;
        }
        (Point::new(first, 0), Point::new(last, usize::MAX))
    }

    /// Reverses the selected cells of the rows drawn, their text only: not
    /// a gutter, an indent or a row a copy leaves out.
    fn highlight(&self, buf: &mut Buffer) {
        let Some(selection) = self.selection.filter(|s| !s.is_empty()) else {
            return;
        };
        let (from, to) = selection.range();
        let drawn = self.drawn;
        let rows = self.copy_rows().skip(drawn.first).take(drawn.count);
        for (i, (line, rule)) in rows.enumerate() {
            let row = drawn.first + i;
            if row < from.row || row > to.row || rule.omit {
                continue;
            }
            let lo = if row == from.row { from.column } else { 0 }.max(rule.decor);
            let hi = if row == to.row { to.column } else { usize::MAX }
                .min(rule.end)
                .min(line.width())
                .min(usize::from(drawn.area.width));
            let y = drawn.top + u16::try_from(i).unwrap_or(u16::MAX);
            for x in lo..hi {
                let x = drawn.area.x + u16::try_from(x).unwrap_or(u16::MAX);
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.modifier.insert(Modifier::REVERSED);
                }
            }
        }
    }

    /// The position with `row` at the top: in a cell, or in the live tail.
    fn position_at(&self, mut row: usize) -> Position {
        for (cell, layout) in self.layouts.iter().enumerate() {
            if let Some(&key) = layout.starts.get(row) {
                return Position::Reading(Anchor { cell, key });
            }
            row -= layout.rows.len();
        }
        Position::Live(row)
    }
}

#[cfg(test)]
#[path = "transcript_tests.rs"]
mod tests;
