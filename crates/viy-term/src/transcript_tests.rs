use pretty_assertions::assert_eq;

use super::*;

fn cell(text: &str) -> Cell {
    Cell::lines(
        text.split('\n')
            .map(|l| Line::from(l.to_string()))
            .collect(),
    )
}

fn numbered(n: usize) -> Vec<Cell> {
    (1..=n).map(|i| cell(&format!("line {i}"))).collect()
}

/// The rows `view` draws for `cells` in a `width` x `height` area.
fn draw(view: &mut TranscriptView, cells: &[Cell], width: u16, height: u16) -> Vec<String> {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    view.render(cells, area, &mut buf);
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[test]
fn a_short_transcript_sits_at_the_bottom() {
    let mut view = TranscriptView::default();
    assert_eq!(
        draw(&mut view, &[cell("one\ntwo")], 10, 4),
        ["", "", "one", "two"]
    );
}

#[test]
fn it_follows_the_end_as_rows_arrive() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(5);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 3", "line 4", "line 5"]
    );
    cells.push(cell("line 6"));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 4", "line 5", "line 6"]
    );
    assert!(view.is_following());
    assert_eq!(view.rows_below(), 0);
}

#[test]
fn scrolled_back_it_holds_still_while_rows_arrive() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(10);
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(4);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 4", "line 5", "line 6"]
    );
    assert_eq!(view.rows_below(), 4);
    cells.extend(numbered(2));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 4", "line 5", "line 6"]
    );
    assert_eq!(view.rows_below(), 6);
    assert!(!view.is_following());
}

#[test]
fn scrolling_down_to_the_end_follows_again() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(10);
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(5);
    draw(&mut view, &cells, 10, 3);
    view.scroll_down(2);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 5", "line 6", "line 7"]
    );
    view.scroll_down(10);
    assert!(view.is_following());
    cells.push(cell("line 11"));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 9", "line 10", "line 11"]
    );
}

#[test]
fn home_and_end() {
    let mut view = TranscriptView::default();
    let cells = numbered(10);
    draw(&mut view, &cells, 10, 3);
    view.home();
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 1", "line 2", "line 3"]
    );
    view.scroll_up(1);
    assert_eq!(draw(&mut view, &cells, 10, 3)[0], "line 1");
    view.end();
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 8", "line 9", "line 10"]
    );
}

#[test]
fn nothing_scrolls_when_everything_fits() {
    let mut view = TranscriptView::default();
    let cells = numbered(2);
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(1);
    view.home();
    assert!(view.is_following());
}

#[test]
fn long_lines_wrap_at_the_width() {
    let mut view = TranscriptView::default();
    assert_eq!(
        draw(&mut view, &[cell("abcdefghij")], 4, 3),
        ["abcd", "efgh", "ij"]
    );
}

#[test]
fn a_resize_rewraps_and_keeps_the_text_at_the_top() {
    let mut view = TranscriptView::default();
    let cells = vec![
        cell("first"),
        cell("aaaa bbbb cccc dddd eeee"),
        cell("last one"),
        cell("tail 1"),
        cell("tail 2"),
        cell("tail 3"),
    ];
    // At 5 columns the long cell is five rows; read from its fourth.
    draw(&mut view, &cells, 5, 3);
    view.home();
    draw(&mut view, &cells, 5, 3);
    view.scroll_down(4);
    assert_eq!(draw(&mut view, &cells, 5, 3), ["dddd", "eeee", "last"]);
    // At 10 columns the row with "dddd" is the long cell's second; it
    // stays at the top.
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["cccc dddd", "eeee", "last one"]
    );
}

#[test]
fn a_reset_follows_the_new_transcript() {
    let mut view = TranscriptView::default();
    draw(&mut view, &numbered(10), 10, 3);
    view.scroll_up(3);
    view.reset();
    assert_eq!(
        draw(&mut view, &[cell("new chat")], 10, 3),
        ["", "", "new chat"]
    );
    assert!(view.is_following());
}

#[test]
fn controls_in_the_text_are_drawn_visible() {
    let mut view = TranscriptView::default();
    assert_eq!(draw(&mut view, &[cell("a\x1b[2Jb")], 10, 1), ["a␛[2Jb"]);
}

fn live(rows: &[&str]) -> Vec<Row> {
    rows.iter()
        .map(|r| Row {
            line: Line::from(r.to_string()),
            key: (0, 0),
            copy: CopyRule::default(),
        })
        .collect()
}

#[test]
fn the_live_tail_is_drawn_under_the_cells_and_scrolls_with_them() {
    let mut view = TranscriptView::default();
    let cells = numbered(3);
    view.set_live(live(&["live a", "live b"]));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 3", "live a", "live b"]
    );
    view.scroll_up(2);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 1", "line 2", "line 3"]
    );
    assert_eq!(view.rows_below(), 2);
    // It grows below the view.
    view.set_live(live(&["live a", "live b", "live c"]));
    assert_eq!(draw(&mut view, &cells, 10, 3)[0], "line 1");
    assert_eq!(view.rows_below(), 3);
}

#[test]
fn read_back_into_the_tail_the_text_stays_when_it_becomes_a_cell() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(2);
    view.set_live(live(&["a", "b", "c", "d", "e", "f"]));
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(2);
    assert_eq!(draw(&mut view, &cells, 10, 3), ["b", "c", "d"]);
    // "a" to "d" move up into a cell; the tail goes on with the rest.
    cells.push(cell("a\nb\nc\nd"));
    view.set_live(live(&["e", "f", "g"]));
    assert_eq!(draw(&mut view, &cells, 10, 3), ["b", "c", "d"]);
    assert_eq!(view.rows_below(), 3);
    assert!(!view.is_following());
}

#[test]
fn a_tail_that_shrinks_under_the_view_follows_the_end() {
    let mut view = TranscriptView::default();
    let cells = numbered(1);
    view.set_live(live(&["a", "b", "c", "d", "e"]));
    draw(&mut view, &cells, 10, 2);
    view.scroll_up(1);
    assert_eq!(draw(&mut view, &cells, 10, 2), ["c", "d"]);
    view.set_live(live(&["a"]));
    assert_eq!(draw(&mut view, &cells, 10, 2), ["line 1", "a"]);
    assert!(view.is_following());
}

#[test]
fn read_back_into_the_tail_a_resize_keeps_its_row() {
    let mut view = TranscriptView::default();
    let cells = vec![cell("aaaa bbbb")];
    view.set_live(live(&["a", "b", "c", "d", "e"]));
    draw(&mut view, &cells, 10, 2);
    view.scroll_up(2);
    assert_eq!(draw(&mut view, &cells, 10, 2), ["b", "c"]);
    // The cell is two rows at 5 columns, not one: the tail's row stays.
    assert_eq!(draw(&mut view, &cells, 5, 2), ["b", "c"]);
}

// Selection (kb d150).

const MESSAGE: &str = "explain how the tunnel guard restarts ssh\nthanks";

/// `view` drawn into a `width` x `height` buffer.
fn render(view: &mut TranscriptView, cells: &[Cell], width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    view.render(cells, area, &mut buf);
    buf
}

/// The reversed cells of screen row `y`, as `x` marks.
fn reversed(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width)
        .map(|x| {
            if buf[(x, y)].modifier.contains(Modifier::REVERSED) {
                'x'
            } else {
                '.'
            }
        })
        .collect::<String>()
        .trim_end_matches('.')
        .to_string()
}

#[test]
fn a_drag_selects_the_text_not_the_gutter_and_release_gives_it() {
    let mut view = TranscriptView::default();
    let cells = [Cell::user(MESSAGE)];
    let now = Instant::now();
    assert_eq!(
        draw(&mut view, &cells, 24, 5),
        [
            "",
            "you › explain how the",
            "      tunnel guard",
            "      restarts ssh",
            "      thanks",
        ]
    );
    // From the gutter of the first row to the end of `ssh`.
    assert!(view.press(2, 1, false, now));
    view.drag(10, 2);
    view.drag(17, 3);
    let buf = render(&mut view, &cells, 24, 5);
    assert_eq!(reversed(&buf, 1), "......xxxxxxxxxxxxxxx");
    assert_eq!(reversed(&buf, 2), "......xxxxxxxxxxxx");
    assert_eq!(reversed(&buf, 3), "......xxxxxxxxxxxx");
    assert_eq!(reversed(&buf, 4), "");
    assert_eq!(
        view.release(17, 3).as_deref(),
        Some("explain how the tunnel guard restarts ssh")
    );
    // Released, it stays selected and drawn until something drops it.
    assert!(view.has_selection());
    // Dragged the other way, the same text.
    view.clear_selection();
    assert!(view.press(17, 3, false, now + Duration::from_secs(1)));
    view.drag(6, 1);
    assert_eq!(
        view.release(6, 1).as_deref(),
        Some("explain how the tunnel guard restarts ssh")
    );
}

#[test]
fn a_click_selects_nothing_and_drops_what_was_selected() {
    let mut view = TranscriptView::default();
    let cells = [Cell::user(MESSAGE)];
    let now = Instant::now();
    draw(&mut view, &cells, 24, 5);
    view.press(6, 1, false, now);
    view.drag(12, 1);
    assert_eq!(view.release(12, 1).as_deref(), Some("explain"));
    view.press(8, 2, false, now + Duration::from_secs(1));
    assert_eq!(view.release(8, 2), None);
    assert!(!view.has_selection());
    // Off the rows (the blank space above a short transcript): no
    // selection starts.
    let mut short = TranscriptView::default();
    draw(&mut short, &numbered(1), 10, 3);
    assert!(!short.press(0, 0, false, now));
    assert!(short.press(0, 2, false, now));
}

#[test]
fn a_double_click_takes_a_word_and_a_triple_its_logical_line() {
    let mut view = TranscriptView::default();
    let cells = [Cell::user(MESSAGE)];
    let now = Instant::now();
    draw(&mut view, &cells, 24, 5);
    view.press(8, 2, false, now);
    view.release(8, 2);
    view.press(8, 2, false, now + Duration::from_millis(100));
    assert_eq!(view.release(8, 2).as_deref(), Some("tunnel"));
    view.press(8, 2, false, now + Duration::from_millis(200));
    assert_eq!(
        view.release(8, 2).as_deref(),
        Some("explain how the tunnel guard restarts ssh")
    );
    // Too slow, or elsewhere: a click again.
    view.press(8, 2, false, now + Duration::from_secs(2));
    assert_eq!(view.release(8, 2), None);
}

#[test]
fn shift_click_moves_the_end_of_the_selection() {
    let mut view = TranscriptView::default();
    let cells = [Cell::user(MESSAGE)];
    let now = Instant::now();
    draw(&mut view, &cells, 24, 5);
    view.press(6, 1, false, now);
    view.drag(12, 1);
    view.release(12, 1);
    assert!(view.press(11, 4, true, now + Duration::from_secs(1)));
    assert_eq!(
        view.release(11, 4).as_deref(),
        Some("explain how the tunnel guard restarts ssh\nthanks")
    );
}

#[test]
fn a_selection_holds_the_view_still_while_rows_arrive() {
    let mut view = TranscriptView::default();
    let now = Instant::now();
    assert_eq!(
        draw(&mut view, &numbered(10), 10, 3),
        ["line 8", "line 9", "line 10"]
    );
    view.press(0, 1, false, now);
    view.drag(5, 1);
    assert_eq!(view.release(5, 1).as_deref(), Some("line 9"));
    // New rows below do not move the text under the selection.
    assert_eq!(
        draw(&mut view, &numbered(12), 10, 3),
        ["line 8", "line 9", "line 10"]
    );
    assert_eq!(view.selected_text().as_deref(), Some("line 9"));
    assert_eq!(view.rows_below(), 2);
    // Dropped, the view stays where it was read.
    view.clear_selection();
    assert_eq!(draw(&mut view, &numbered(12), 10, 3)[0], "line 8");
}

#[test]
fn a_resize_or_ctrl_t_drops_the_selection() {
    let mut view = TranscriptView::default();
    let cells = [Cell::user(MESSAGE)];
    let now = Instant::now();
    draw(&mut view, &cells, 24, 5);
    view.press(6, 1, false, now);
    view.drag(12, 1);
    view.release(12, 1);
    draw(&mut view, &cells, 30, 5);
    assert!(!view.has_selection());
    draw(&mut view, &cells, 30, 5);
    view.press(6, 1, false, now + Duration::from_secs(1));
    view.drag(12, 1);
    view.release(12, 1);
    view.set_expanded(true);
    assert!(!view.has_selection());
}

#[test]
fn a_drag_held_at_the_top_scrolls_back_and_takes_the_selection_along() {
    let mut view = TranscriptView::default();
    let cells = numbered(10);
    let now = Instant::now();
    draw(&mut view, &cells, 10, 3);
    view.press(6, 2, false, now);
    // Along the row pressed: no scrolling.
    view.drag(0, 2);
    assert_eq!(view.next_edge_scroll(now), None);
    view.drag(0, 0);
    assert_eq!(view.next_edge_scroll(now), Some(now));
    assert!(view.tick_edge(now));
    assert!(!view.tick_edge(now + Duration::from_millis(10)));
    assert!(view.tick_edge(now + EDGE_SCROLL));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 6", "line 7", "line 8"]
    );
    assert_eq!(
        view.release(0, 0).as_deref(),
        Some("line 6\nline 7\nline 8\nline 9\nline 10")
    );
    assert_eq!(view.next_edge_scroll(now), None);
}

#[test]
fn the_live_tail_can_be_selected() {
    let mut view = TranscriptView::default();
    let cells = numbered(1);
    view.set_live(live(&["live a", "live b"]));
    let now = Instant::now();
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 1", "live a", "live b"]
    );
    view.press(0, 0, false, now);
    view.drag(5, 2);
    assert_eq!(
        view.release(5, 2).as_deref(),
        Some("line 1\nlive a\nlive b")
    );
}

#[test]
fn text_changing_under_a_selection_drops_it() {
    let mut view = TranscriptView::default();
    let cells = numbered(1);
    view.set_live(live(&["live a", "live b"]));
    let now = Instant::now();
    draw(&mut view, &cells, 10, 3);
    view.press(0, 2, false, now);
    view.drag(5, 2);
    assert_eq!(view.release(5, 2).as_deref(), Some("live b"));
    // Rows arriving below leave it.
    view.set_live(live(&["live a", "live b", "live c"]));
    draw(&mut view, &cells, 10, 3);
    assert!(view.has_selection());
    // Its row drawn anew with other text: dropped.
    view.set_live(live(&["live a", "live B", "live c"]));
    draw(&mut view, &cells, 10, 3);
    assert!(!view.has_selection());
    // A cell laid out again with other rows, the same.
    let mut view = TranscriptView::default();
    let mut cells = vec![cell("one"), cell("two")];
    draw(&mut view, &cells, 10, 3);
    view.press(0, 2, false, now + Duration::from_secs(1));
    view.drag(2, 2);
    assert_eq!(view.release(2, 2).as_deref(), Some("two"));
    cells[0] = cell("one\nmore");
    view.changed(0);
    draw(&mut view, &cells, 10, 3);
    assert!(!view.has_selection());
}

#[test]
fn a_drag_over_nothing_a_copy_takes_selects_nothing() {
    let mut view = TranscriptView::default();
    let cells = [Cell::user(MESSAGE)];
    let now = Instant::now();
    draw(&mut view, &cells, 24, 5);
    // Within the `you › ` gutter.
    view.press(0, 1, false, now);
    view.drag(4, 1);
    assert!(!view.has_selection());
    assert_eq!(view.release(4, 1), None);
    assert!(!view.has_selection());
}

#[test]
fn shift_click_at_an_edge_does_not_scroll() {
    let mut view = TranscriptView::default();
    let cells = numbered(10);
    let now = Instant::now();
    draw(&mut view, &cells, 10, 3);
    view.press(0, 1, false, now);
    view.drag(5, 1);
    view.release(5, 1);
    assert!(view.press(3, 2, true, now + Duration::from_secs(1)));
    assert_eq!(view.next_edge_scroll(now), None);
    assert!(!view.tick_edge(now + Duration::from_secs(1)));
}

#[test]
fn a_drag_held_at_the_bottom_keeps_the_view_held_and_ends_under_the_pointer() {
    let mut view = TranscriptView::default();
    let mut cells = numbered(10);
    let now = Instant::now();
    draw(&mut view, &cells, 10, 3);
    view.scroll_up(3);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 5", "line 6", "line 7"]
    );
    view.press(0, 0, false, now);
    view.drag(3, 2);
    assert!(view.tick_edge(now));
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 6", "line 7", "line 8"]
    );
    // The end follows the pointer's cell, column 3 of the row under it.
    assert_eq!(
        view.selected_text().as_deref(),
        Some("line 5\nline 6\nline 7\nline")
    );
    for n in 1..=3 {
        view.tick_edge(now + EDGE_SCROLL * n);
    }
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 8", "line 9", "line 10"]
    );
    // At the end: nothing to scroll, no more frames asked for.
    assert!(!view.tick_edge(now + EDGE_SCROLL * 4));
    assert_eq!(view.next_edge_scroll(now + EDGE_SCROLL * 5), None);
    // Still held: rows arriving do not move the text.
    cells = numbered(12);
    assert_eq!(
        draw(&mut view, &cells, 10, 3),
        ["line 8", "line 9", "line 10"]
    );
    assert!(!view.is_following());
}

#[test]
fn a_short_transcript_keeps_its_place_while_a_selection_is_live() {
    let mut view = TranscriptView::default();
    let now = Instant::now();
    assert_eq!(
        draw(&mut view, &numbered(2), 10, 5),
        ["", "", "", "line 1", "line 2"]
    );
    view.press(0, 3, false, now);
    view.drag(5, 3);
    assert_eq!(view.release(5, 3).as_deref(), Some("line 1"));
    // New rows go below; `line 1` stays on the screen row it was on.
    assert_eq!(
        draw(&mut view, &numbered(4), 10, 5),
        ["", "", "", "line 1", "line 2"]
    );
    assert_eq!(view.rows_below(), 2);
    assert_eq!(view.selected_text().as_deref(), Some("line 1"));
    // Dropped, it goes back to the bottom.
    view.clear_selection();
    assert_eq!(
        draw(&mut view, &numbered(4), 10, 5),
        ["", "line 1", "line 2", "line 3", "line 4"]
    );
}
