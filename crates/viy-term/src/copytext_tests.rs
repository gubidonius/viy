use pretty_assertions::assert_eq;
use ratatui::text::Span;
use viy_protocol::NoticeEvent;
use viy_protocol::NoticeLevel;
use viy_testkit::hostile::CORPUS;

use super::*;
use crate::cells::Cell;
use crate::cells::Reasoning;
use crate::cells::ToolCard;
use crate::markdown::Row;
use crate::sanitize::sanitize_line_after;
use crate::sanitize::unsafe_in_cell;

const END: Point = Point {
    row: usize::MAX,
    column: usize::MAX,
};

fn copy(rows: &[Row], from: Point, to: Point) -> String {
    copy_text(rows.iter().map(|r| (&r.line, &r.copy)), from, to)
}

fn drawn(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|r| r.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

const MESSAGE: &str = "The lighthouse keeper counted the ships that passed the northern \
reef and wrote each one down in the logbook by the window.\n\nShe’d also mended the \
lamp’s brass hinge before the storm came in.";

#[test]
fn a_wrapped_message_copies_as_typed_at_any_width() {
    for width in [13, 40, 100] {
        let rows = Cell::user(MESSAGE).rows(width, false);
        // Row 0 is the blank row between turns.
        assert_eq!(copy(&rows, Point::new(1, 0), END), MESSAGE, "width {width}");
    }
}

#[test]
fn the_gutter_is_left_out_of_a_partial_selection() {
    let rows = Cell::user("explain how the tunnel guard restarts ssh\nthanks").rows(24, false);
    assert_eq!(
        drawn(&rows),
        [
            "",
            "you › explain how the",
            "      tunnel guard",
            "      restarts ssh",
            "      thanks",
        ]
    );
    // From the gutter of the first row to `tunnel` on the second.
    assert_eq!(
        copy(&rows, Point::new(1, 2), Point::new(2, 12)),
        "explain how the tunnel"
    );
    // A drag ends where it started, either way round.
    assert_eq!(
        copy(&rows, Point::new(3, 18), Point::new(2, 13)),
        copy(&rows, Point::new(2, 13), Point::new(3, 18)),
    );
    assert_eq!(
        copy(&rows, Point::new(2, 13), Point::new(3, 18)),
        "guard restarts ssh"
    );
    // The end cell is not taken: up to `restarts ` and its space dropped.
    assert_eq!(
        copy(&rows, Point::new(2, 13), Point::new(3, 15)),
        "guard restarts"
    );
    // A selection inside the gutter copies nothing.
    assert_eq!(copy(&rows, Point::new(2, 0), Point::new(2, 6)), "");
}

#[test]
fn the_spaces_a_wrap_took_out_come_back() {
    let rows = Cell::answer("one two  three four", false).rows(8, false);
    assert_eq!(drawn(&rows), ["", "one two", "three", "four"]);
    assert_eq!(copy(&rows, Point::new(1, 0), END), "one two  three four");
}

#[test]
fn a_long_word_cut_by_the_width_is_joined_without_a_gap() {
    let rows = Cell::user("supercalifragilistic").rows(12, false);
    assert_eq!(
        drawn(&rows)[1..],
        ["you › superc", "      alifra", "      gilist", "      ic"]
    );
    assert_eq!(copy(&rows, Point::new(1, 0), END), "supercalifragilistic");
}

#[test]
fn wide_chars_copy_whole_and_rejoin() {
    let text = "日本語のテキストを折り返す";
    let rows = Cell::answer(text, false).rows(10, false);
    assert_eq!(copy(&rows, Point::new(1, 0), END), text);
    // The second cell of a wide char takes the char.
    assert_eq!(copy(&rows, Point::new(1, 1), Point::new(1, 3)), "日本");
}

#[test]
fn markdown_copies_its_lines_without_hanging_indents_or_code_frames() {
    let source = "- a list item long enough to wrap twice here\n  - nested item\n\n\
> a quote long enough to wrap here\n\n```rust\nlet x = 1;\n```\n";
    let rows = Cell::answer(source, true).rows(24, false);
    assert_eq!(
        drawn(&rows),
        [
            "",
            "• a list item long",
            "  enough to wrap twice",
            "  here",
            "  ◦ nested item",
            "",
            "│ a quote long enough to",
            "│ wrap here",
            "",
            "╭─ rust",
            "let x = 1;",
            "╰─",
        ]
    );
    assert_eq!(
        copy(&rows, Point::new(1, 0), END),
        "• a list item long enough to wrap twice here\n  ◦ nested item\n\n\
│ a quote long enough to wrap here\n\nlet x = 1;"
    );
}

#[test]
fn a_long_code_line_rejoins_as_written() {
    let line = "let value = compute_the_answer(first_argument, second);";
    let rows = Cell::answer(&format!("```\n{line}\n```\n"), true).rows(20, false);
    assert!(rows.len() > 4, "{:?}", drawn(&rows));
    // Row 0 is the answer's blank row; the frames are left out.
    assert_eq!(copy(&rows, Point::new(1, 0), END), line);
}

#[test]
fn a_blank_row_in_a_quote_copies_as_an_empty_line() {
    let rows = Cell::answer("> first\n>\n> second\n", true).rows(30, false);
    assert_eq!(drawn(&rows)[1..], ["│ first", "│ ", "│ second"]);
    assert_eq!(copy(&rows, Point::new(1, 0), END), "│ first\n\n│ second");
}

#[test]
fn tool_output_copies_without_its_prefix_or_the_hidden_lines_hint() {
    let mut card = ToolCard::new("c1", "run_shell", "ls");
    card.output = "line 1\nline 2\nline 3\nline 4\nline 5\n".into();
    let rows = card.rows(40, false, None);
    assert_eq!(
        drawn(&rows)[1..],
        [
            "  └ … +2 lines (Ctrl-T shows all)",
            "    line 3",
            "    line 4",
            "    line 5",
        ]
    );
    assert_eq!(copy(&rows, Point::new(1, 0), END), "line 3\nline 4\nline 5");
    // A wrapped header and a long output line cut by the width rejoin.
    card.output = "0123456789abcdefghij".into();
    let rows = card.rows(12, false, None);
    assert_eq!(
        drawn(&rows),
        [
            "▸",
            "    run_shel",
            "    l  ls",
            "  └ 01234567",
            "    89abcdef",
            "    ghij",
        ]
    );
    assert_eq!(
        copy(&rows, Point::new(0, 0), END),
        "▸ run_shell  ls\n0123456789abcdefghij"
    );
}

#[test]
fn trailing_spaces_end_no_line() {
    let rows = Cell::answer("padded   \nnext", false).rows(40, false);
    assert_eq!(copy(&rows, Point::new(1, 0), END), "padded\nnext");
}

#[test]
fn nothing_hostile_reaches_a_copy() {
    let notice = |message: &str| NoticeEvent {
        level: NoticeLevel::Warn,
        code: "hostile".into(),
        message: message.to_string(),
    };
    for text in CORPUS {
        let mut card = ToolCard::new("c1", text, text);
        card.output = (*text).to_string();
        let cells = [
            Cell::user(text),
            Cell::answer(text, true),
            Cell::answer(text, false),
            Cell::notice(&notice(text)),
            Cell::lines(vec![Line::from(Span::raw(*text))]),
            Cell::Tool(card),
        ];
        for cell in cells {
            for width in [7, 80] {
                let rows = cell.rows(width, true);
                let copied = copy(&rows, Point::new(0, 0), END);
                for line in copied.split('\n') {
                    assert!(!unsafe_in_cell(line), "{text:?} copied as {line:?}");
                    assert_eq!(
                        sanitize_line_after(None, line),
                        line,
                        "{text:?} at width {width}: a char was not made visible"
                    );
                }
            }
        }
    }
}

#[test]
fn a_table_copies_as_drawn_and_a_record_rejoins() {
    let boxed = Cell::answer("| a | b |\n|---|---|\n| 1 | 2 |\n", true).rows(40, false);
    let shown = drawn(&boxed)[1..].join("\n");
    assert_eq!(copy(&boxed, Point::new(1, 0), END), shown.trim_end());
    // Too many columns for the width: `header: value` records, wrapped
    // like prose and joined again.
    let wide = "| name | role | team | site | note |\n|---|---|---|---|---|\n\
| Ada | engineer | compilers | London | wrote the first program |\n";
    let records = Cell::answer(wide, true).rows(14, false);
    let copied = copy(&records, Point::new(1, 0), END);
    assert!(
        copied.contains("note: wrote the first program"),
        "{copied:?} from {:?}",
        drawn(&records)
    );
}

#[test]
fn code_copies_without_the_indents_drawn_before_it() {
    // Indented code: viy draws four spaces where the source had its own.
    let rows = Cell::answer("Run:\n\n    let x = 1;\n    let y = 2;\n", true).rows(40, false);
    assert_eq!(
        drawn(&rows)[1..],
        ["Run:", "", "    let x = 1;", "    let y = 2;"]
    );
    assert_eq!(
        copy(&rows, Point::new(1, 0), END),
        "Run:\n\nlet x = 1;\nlet y = 2;"
    );
    // Fenced code inside a list item: the item's indent is not the code's.
    let rows = Cell::answer("- build it:\n\n  ```sh\n  make all\n  ```\n", true).rows(40, false);
    assert_eq!(
        drawn(&rows)[1..],
        ["• build it:", "", "  ╭─ sh", "  make all", "  ╰─"]
    );
    assert_eq!(
        copy(&rows, Point::new(1, 0), END),
        "• build it:\n\nmake all"
    );
}

#[test]
fn a_list_marker_on_a_code_frame_is_kept() {
    let rows = Cell::answer("1. ```sh\n   make\n   ```\n2. done\n", true).rows(40, false);
    assert_eq!(
        drawn(&rows)[1..],
        ["1. ╭─ sh", "   make", "   ╰─", "2. done"]
    );
    assert_eq!(copy(&rows, Point::new(1, 0), END), "1.\nmake\n2. done");
}

#[test]
fn reasoning_copies_its_trace_without_the_label_and_never_the_hint() {
    let shown = Reasoning {
        text: "check the lock first".into(),
        took: None,
        shown: true,
    };
    let rows = shown.rows(40, false);
    assert_eq!(drawn(&rows)[1..], ["[thinking] check the lock first"]);
    assert_eq!(copy(&rows, Point::new(0, 0), END), "\ncheck the lock first");
    let folded = Reasoning {
        shown: false,
        took: Some(std::time::Duration::from_millis(4200)),
        ..shown
    };
    let rows = folded.rows(40, false);
    assert_eq!(drawn(&rows), ["thought for 4.2 s · Ctrl-T shows it"]);
    assert_eq!(copy(&rows, Point::new(0, 0), END), "");
}

#[test]
fn a_tool_header_copies_without_its_padding_and_status() {
    let mut card = ToolCard::new("c1", "run_shell", "ls");
    card.end = Some(viy_protocol::ToolCallEndEvent {
        call_id: "c1".into(),
        status: viy_protocol::ToolCallStatus::Completed,
        detail: Some("exit 0".into()),
        duration_ms: 4,
        sandboxed: true,
        network: false,
    });
    let rows = card.rows(60, false, None);
    let head = &drawn(&rows)[0];
    assert!(head.ends_with("exit 0  sandbox  4 ms"), "{head:?}");
    assert_eq!(copy(&rows, Point::new(0, 0), END), "▸ run_shell  ls");
}

#[test]
fn the_header_is_never_copied() {
    let header = crate::header::Header {
        colors: crate::header::Colors::True,
        version: "0.1.0".into(),
        directory: "~/project".into(),
        ghost: false,
        art: true,
        dissolve: 0,
    };
    let rows = Cell::Header(header).rows(100, false);
    assert!(rows.len() > 3);
    assert!(rows.iter().all(|r| r.copy.omit));
    let mut all = rows;
    all.extend(Cell::user("hello").rows(100, false));
    assert_eq!(copy(&all, Point::new(0, 0), END), "\nhello");
}
