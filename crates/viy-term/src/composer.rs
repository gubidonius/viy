//! The input box (ui-plan 5, flags F33), after Codex's
//! `bottom_pane/chat_composer.rs`: the text area, its keys (Codex's default
//! editor keymap, `keymap.rs`), the history, slash completion, and Codex's
//! paste-burst guard (`paste_burst.rs`).
//!
//! What rp's readline did: Enter sends; Ctrl-J, Alt-Enter and a line ending
//! in `\` insert a newline (rpctl kb f087); a bracketed paste arrives as one
//! block whose newlines never send (`test_paste.py`); a sent line is
//! trimmed, as rp's `_read_input(...).strip()`; Shift+Tab steps the chat
//! mode. Shift+Enter inserts a newline too, as Codex's, where the terminal
//! tells it from Enter (kb d153). A terminal without bracketed paste types a paste as keys: Enter
//! within 120 ms of a burst of keys under 8 ms apart is a newline, not a
//! send (Codex's direct-insert path; no person types that fast).
//!
//! Shell mode (kb d149), as Codex's: a `!` typed into the empty box is not
//! text but the mode, shown in the prompt; a second makes it `!!`, whose
//! output only the user sees; Backspace in the empty box takes one back.
//! Only a key typed by a person enters it. A paste, text the chat puts in
//! the box (a rewound message, a line given back) and a recalled message
//! keep their `!` as text, and a `!` followed within 8 ms by another key
//! came from a paste and goes back into the text. A recalled shell command
//! comes back in its mode: the history keeps the mode apart from the text.
//!
//! Images (kb d191, Codex's `chat_composer/attachment_state.rs`): each is
//! an element of the text, `[Image #N]`, that edits as one piece; deleting
//! it drops the image. The placeholders are numbered from 1 in the order
//! they stand in the text, renumbered after every edit, and the images go
//! with the message in that order (Codex numbers them in the order they
//! were attached; by position, a line given back or a draft put back in
//! front of what is typed keeps its numbers in step with the text).

use std::time::Duration;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use tokio::time::Instant;

use viy_protocol::ImageInput;

use crate::history::History;
pub use crate::history::Shell;
use crate::slash::Completion;
use crate::slash::completions;
use crate::textarea::TextArea;

/// Codex's `PASTE_BURST_CHAR_INTERVAL` (Linux and macOS).
const BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);
/// Codex's `PASTE_BURST_MIN_CHARS`.
const BURST_MIN_CHARS: u32 = 3;
/// Codex's `PASTE_ENTER_SUPPRESS_WINDOW`.
const BURST_ENTER_WINDOW: Duration = Duration::from_millis(120);
/// Rows the slash popup shows at most.
pub const POPUP_ROWS: usize = 6;

/// What a key asks of the chat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Send this, trimmed and not empty: a message or a slash command, and
    /// the images of its `[Image #N]` placeholders, in their order.
    Submit(String, Vec<ImageInput>),
    /// Run this in the shell, trimmed and not empty (`!` or `!!`).
    Shell {
        command: String,
        shell: Shell,
    },
    /// Enter in shell mode with nothing typed: say what it does.
    ShellHelp,
    /// Shift+Tab: the next chat mode.
    CycleMode,
}

/// Keys arriving faster than anyone types.
#[derive(Debug, Default)]
struct PasteBurst {
    last: Option<Instant>,
    run: u32,
    /// Until when Enter inserts a newline.
    window: Option<Instant>,
}

impl PasteBurst {
    fn on_char(&mut self, now: Instant) {
        let close = self
            .last
            .is_some_and(|t| now.saturating_duration_since(t) <= BURST_CHAR_INTERVAL);
        self.run = if close { self.run + 1 } else { 1 };
        self.last = Some(now);
        if self.run >= BURST_MIN_CHARS {
            self.window = Some(now + BURST_ENTER_WINDOW);
        }
    }

    /// Whether Enter now is part of a paste; it keeps the burst going.
    fn enter_is_newline(&mut self, now: Instant) -> bool {
        if self.window.is_some_and(|until| now <= until) {
            self.window = Some(now + BURST_ENTER_WINDOW);
            self.last = Some(now);
            true
        } else {
            false
        }
    }

    /// Any other key ends a burst.
    fn clear(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub struct Composer {
    area: TextArea,
    history: History,
    burst: PasteBurst,
    /// The popup's choice, and the input it was offered for.
    selected: usize,
    /// Esc closed the popup for this input.
    dismissed: Option<String>,
    /// The width the input was last laid out at, for Up and Down.
    width: usize,
    /// What the last press picked (a point, a word, a line): a drag
    /// keeps it and runs from its far end.
    pressed: (usize, usize),
    /// Shell mode, and when the `!` that set it was typed, until the key
    /// after it shows it was typed by a person.
    shell: Option<Shell>,
    shell_typed: Option<Instant>,
    /// The images of the text's elements, by element id (kb d191).
    attachments: Vec<(u64, ImageInput)>,
}

/// The placeholder of the `n`th image, from 1 (Codex's
/// `local_image_label_text`).
pub fn image_label(n: usize) -> String {
    format!("[Image #{n}]")
}

/// The byte ranges of the `[Image #N]` placeholders written in `text`, in
/// order.
fn labels_in(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("[Image #").map(|i| from + i) {
        let digits = text[at + 8..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let end = at + 8 + digits;
        if digits > 0 && text[end..].starts_with(']') {
            found.push(at..end + 1);
            from = end + 1;
        } else {
            from = at + 8;
        }
    }
    found
}

impl Composer {
    pub fn new(history: History) -> Self {
        Self {
            history,
            width: 80,
            ..Self::default()
        }
    }

    pub fn text(&self) -> &str {
        self.area.text()
    }

    /// Nothing typed, not even the `!` of shell mode.
    pub fn is_empty(&self) -> bool {
        self.area.is_empty() && self.shell.is_none()
    }

    /// Shell mode, if the box is in it.
    pub fn shell(&self) -> Option<Shell> {
        self.shell
    }

    /// Empties the box (Ctrl-C with a draft), shell mode and images too.
    pub fn clear(&mut self) {
        self.area.take();
        self.leave_shell();
        self.attachments.clear();
    }

    /// The images the text's placeholders stand for, in their order.
    pub fn images(&self) -> Vec<ImageInput> {
        self.area
            .elements()
            .iter()
            .filter_map(|(id, _)| self.attachments.iter().find(|(a, _)| a == id))
            .map(|(_, image)| image.clone())
            .collect()
    }

    /// Attaches `image` at the cursor as its `[Image #N]`, in place of the
    /// selection.
    pub fn attach(&mut self, image: ImageInput) {
        let id = self.area.insert_element(&image_label(0));
        self.attachments.push((id, image));
        self.reconcile(0);
    }

    /// A draft in the box in place of what it held: `/rewind`'s message
    /// and its images.
    pub fn set_draft(&mut self, text: &str, images: Vec<ImageInput>) {
        self.set_text("");
        self.splice(0..0, "", text, images, "");
    }

    /// Writes `before`, `text` and `after` in place of `range` (which holds
    /// no image), `text`'s `[Image #N]` placeholders standing for `images`
    /// in order; images it has no placeholder for get one after it. The
    /// cursor goes to the end. Where the written text ends, once every
    /// placeholder is numbered again.
    pub fn splice(
        &mut self,
        range: std::ops::Range<usize>,
        before: &str,
        text: &str,
        images: Vec<ImageInput>,
        after: &str,
    ) -> usize {
        let mut text = text.to_string();
        for n in labels_in(&text).len()..images.len() {
            if !text.is_empty() && !text.ends_with(char::is_whitespace) {
                text.push(' ');
            }
            text.push_str(&image_label(n + 1));
        }
        self.area.remove_range(range.clone());
        let at = range.start + before.len();
        self.area
            .insert_at(range.start, &format!("{before}{text}{after}"));
        let mut images = images.into_iter();
        for label in labels_in(&text) {
            let Some(image) = images.next() else { break };
            if let Some(id) = self.area.mark(at + label.start..at + label.end) {
                self.attachments.push((id, image));
            }
        }
        let end = self.reconcile(at + text.len());
        self.area.move_to_end();
        end
    }

    /// Drops the images whose placeholder an edit took, and numbers the
    /// placeholders left from 1 in text order (Codex's
    /// `remove_deleted_local_placeholders` and `relabel_local_images`).
    /// `pos` again, after the numbers that came before it changed length.
    fn reconcile(&mut self, mut pos: usize) -> usize {
        let elements = self.area.elements();
        self.attachments
            .retain(|(id, _)| elements.iter().any(|(e, _)| e == id));
        // Numbered from the last, so the ranges not renamed yet still hold.
        for (n, (id, range)) in elements.into_iter().enumerate().rev() {
            let delta = self
                .area
                .rename_element(id, &image_label(n + 1))
                .unwrap_or(0);
            if range.end <= pos {
                pos = pos.saturating_add_signed(delta);
            }
        }
        pos
    }

    /// Puts `text` in the box to edit: `/rewind`'s prefill, a line given
    /// back. Never shell mode: a `!` in it is text.
    pub fn set_text(&mut self, text: &str) {
        self.leave_shell();
        self.area.set_text(text);
        self.attachments.clear();
    }

    /// Shell mode becomes text: its `!` goes in front, the images kept (a
    /// line given back makes the box text, kb d149).
    pub fn shell_to_text(&mut self) {
        self.shell_was_pasted();
    }

    /// Puts back a shell command the user typed and the chat could not
    /// take (it was closing), in its mode.
    pub fn set_shell(&mut self, command: &str, shell: Shell) {
        self.area.set_text(command);
        self.attachments.clear();
        self.shell = Some(shell);
        self.shell_typed = None;
    }

    fn leave_shell(&mut self) {
        self.shell = None;
        self.shell_typed = None;
    }

    /// The `!` of shell mode came from a paste: it goes back into the
    /// text, at the start.
    fn shell_was_pasted(&mut self) {
        if let Some(shell) = self.shell.take() {
            self.area.insert_at(0, shell.bang());
            self.area.move_to_end();
        }
        self.shell_typed = None;
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// A bracketed paste: one block, its newlines kept.
    pub fn paste(&mut self, text: &str) {
        self.burst.clear();
        self.area.insert_str(text);
        self.reconcile(0);
    }

    /// The popup's offers for the input now; none once Esc closed it, and
    /// none for a shell command.
    pub fn popup(&self) -> Vec<Completion> {
        if self.shell.is_some()
            || self.area.cursor() != self.area.text().len()
            || self.dismissed.as_deref() == Some(self.area.text())
        {
            return Vec::new();
        }
        completions(self.area.text())
    }

    /// The popup's choice.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The input in rows `width` cells wide, and the cursor in them.
    pub fn layout(&mut self, width: u16) -> (Vec<String>, ratatui::layout::Position) {
        self.width = usize::from(width.max(1));
        self.area.layout(width)
    }

    /// A left press on row `row`, column `column` of the input as last
    /// laid out (kb d150): the cursor goes there; a second or third click
    /// selects the word or the line there (Codex's composer mouse).
    pub fn press(&mut self, row: usize, column: usize, clicks: u8) {
        let pos = self.area.pos_at(self.laid_width(), row, column);
        let range = match clicks {
            2 => self.area.word_around(pos),
            3 => self.area.line_around(pos),
            _ => pos..pos,
        };
        self.pressed = (range.start, range.end);
        self.area.select(range.start, range.end);
    }

    /// The pointer moved to row `row`, column `column` with the button
    /// down: the selection runs from what the press picked to there.
    pub fn drag(&mut self, row: usize, column: usize) {
        let pos = self.area.pos_at(self.laid_width(), row, column);
        let (start, end) = self.pressed;
        if pos < start {
            self.area.select(end, pos);
        } else if pos > end {
            self.area.select(start, pos);
        } else {
            self.area.select(start, end);
        }
    }

    /// The selected text, if any is.
    pub fn selected_text(&self) -> Option<&str> {
        self.area.selected()
    }

    /// Removes the selected text and returns it (Ctrl+X).
    pub fn cut(&mut self) -> Option<String> {
        let cut = self.area.take_selection();
        self.reconcile(0);
        cut
    }

    pub fn clear_selection(&mut self) {
        self.area.clear_selection();
    }

    /// The selection's cells in the rows last laid out: row, first and
    /// last column, the end not included.
    pub fn selected_cells(&self) -> Vec<(usize, usize, usize)> {
        self.area.selected_cells(self.laid_width())
    }

    fn laid_width(&self) -> u16 {
        u16::try_from(self.width).unwrap_or(u16::MAX)
    }

    /// Takes one key, at `now`.
    pub fn key(&mut self, key: KeyEvent, now: Instant) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        // The key after a `!`: sooner than anyone types, it is a paste.
        if let Some(at) = self.shell_typed.take()
            && now.saturating_duration_since(at) <= BURST_CHAR_INTERVAL
        {
            self.shell_was_pasted();
        }
        if let KeyCode::Char('!') = key.code
            && !ctrl
            && !alt
            && self.area.is_empty()
            && self.shell != Some(Shell::Private)
        {
            self.burst.on_char(now);
            self.shell = Some(match self.shell {
                None => Shell::Shared,
                Some(_) => Shell::Private,
            });
            self.shell_typed = Some(now);
            return Action::None;
        }
        if key.code == KeyCode::Backspace
            && !ctrl
            && !alt
            && self.area.is_empty()
            && let Some(shell) = self.shell
        {
            self.shell = match shell {
                Shell::Private => Some(Shell::Shared),
                Shell::Shared => None,
            };
            return Action::None;
        }
        let before = self.area.text().to_string();
        if !matches!(key.code, KeyCode::Char(_) | KeyCode::Enter | KeyCode::Tab) || ctrl || alt {
            self.burst.clear();
        }
        let popup = self.popup();
        let action = match key.code {
            KeyCode::Enter if !alt && self.burst.enter_is_newline(now) => self.newline(),
            // Shift+Enter only where the terminal tells it from Enter (the
            // kitty keyboard protocol, [`crate::tui`]; kb d153).
            KeyCode::Enter if alt || shift => self.newline(),
            KeyCode::Enter if self.area.text().ends_with('\\') => {
                let end = self.area.text().len();
                self.area.remove_range(end - 1..end);
                self.area.move_to_end();
                self.newline()
            }
            // With the popup open, Enter takes its choice as Codex's command
            // popup does: `/res` Enter runs /resume; a command that needs
            // its argument goes into the input to finish.
            KeyCode::Enter if !popup.is_empty() => {
                let choice = popup[self.selected.min(popup.len() - 1)].clone();
                if choice.runs {
                    self.area.set_text(choice.text.trim_end());
                    self.submit()
                } else {
                    self.area.set_text(&choice.text);
                    Action::None
                }
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Char('j') if ctrl => self.newline(),
            KeyCode::BackTab => Action::CycleMode,
            KeyCode::Tab if !popup.is_empty() => {
                let choice = &popup[self.selected.min(popup.len() - 1)];
                self.area.set_text(&choice.text);
                Action::None
            }
            KeyCode::Tab => {
                self.area.insert_char('\t');
                Action::None
            }
            KeyCode::Esc if !popup.is_empty() => {
                self.dismissed = Some(self.area.text().to_string());
                Action::None
            }
            KeyCode::Up if !popup.is_empty() => {
                self.selected = self.selected.checked_sub(1).unwrap_or(popup.len() - 1);
                return Action::None;
            }
            KeyCode::Down if !popup.is_empty() => {
                self.selected = (self.selected + 1) % popup.len();
                return Action::None;
            }
            KeyCode::Up => self.up(),
            KeyCode::Char('p') if ctrl => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Char('n') if ctrl => self.down(),
            KeyCode::Left if ctrl || alt => self.edit(TextArea::move_word_left),
            KeyCode::Right if ctrl || alt => self.edit(TextArea::move_word_right),
            KeyCode::Char('b') if alt => self.edit(TextArea::move_word_left),
            KeyCode::Char('f') if alt => self.edit(TextArea::move_word_right),
            KeyCode::Left => self.edit(TextArea::move_left),
            KeyCode::Char('b') if ctrl => self.edit(TextArea::move_left),
            KeyCode::Right => self.edit(TextArea::move_right),
            KeyCode::Char('f') if ctrl => self.edit(TextArea::move_right),
            KeyCode::Home => self.edit(TextArea::move_home),
            KeyCode::Char('a') if ctrl => self.edit(TextArea::move_home),
            KeyCode::End => self.edit(TextArea::move_end),
            KeyCode::Char('e') if ctrl => self.edit(TextArea::move_end),
            KeyCode::Backspace if ctrl || alt => self.edit(TextArea::delete_word_left),
            KeyCode::Char('w') if ctrl => self.edit(TextArea::delete_word_left),
            KeyCode::Char('h') if ctrl && alt => self.edit(TextArea::delete_word_left),
            KeyCode::Backspace => self.edit(TextArea::backspace),
            KeyCode::Char('h') if ctrl => self.edit(TextArea::backspace),
            KeyCode::Delete if ctrl || alt => self.edit(TextArea::delete_word_right),
            KeyCode::Char('d') if alt => self.edit(TextArea::delete_word_right),
            KeyCode::Delete => self.edit(TextArea::delete),
            KeyCode::Char('d') if ctrl => self.edit(TextArea::delete),
            KeyCode::Char('u') if ctrl => self.edit(TextArea::kill_to_line_start),
            KeyCode::Char('k') if ctrl => self.edit(TextArea::kill_to_line_end),
            KeyCode::Char('y') if ctrl => self.edit(TextArea::yank),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.burst.on_char(now);
                self.area.insert_char(c);
                Action::None
            }
            _ => Action::None,
        };
        if self.area.text() != before {
            self.selected = 0;
            self.reconcile(0);
        }
        action
    }

    fn edit(&mut self, f: fn(&mut TextArea)) -> Action {
        f(&mut self.area);
        Action::None
    }

    fn newline(&mut self) -> Action {
        self.area.insert_char('\n');
        Action::None
    }

    fn up(&mut self) -> Action {
        if self.history.browses(self.area.text(), self.area.cursor()) {
            if let Some(text) = self.history.older() {
                self.recall(&text);
            }
        } else {
            self.area.move_up(self.width);
        }
        Action::None
    }

    fn down(&mut self) -> Action {
        if self.history.browses(self.area.text(), self.area.cursor()) {
            if let Some(text) = self.history.newer() {
                self.recall(&text);
            }
        } else {
            self.area.move_down(self.width);
        }
        Action::None
    }

    /// A history entry in the box, in the mode the history kept for it,
    /// with the images it was sent with in this chat.
    fn recall(&mut self, text: &str) {
        let images = self.history.shown_images();
        self.set_draft(text, images);
        self.shell = self.history.shown_shell();
        self.shell_typed = None;
    }

    /// The input, trimmed, to send; nothing when it is blank. It goes into
    /// the history; a history file that cannot be written loses the entry,
    /// never the message (rp ignores the same failure).
    fn submit(&mut self) -> Action {
        let text = self.area.text().trim().to_string();
        let images = self.images();
        if let Some(shell) = self.shell {
            self.area.take();
            self.attachments.clear();
            self.leave_shell();
            if text.is_empty() {
                return Action::ShellHelp;
            }
            let _ = self.history.record_shell(&text, shell);
            return Action::Shell {
                command: text,
                shell,
            };
        }
        if text.is_empty() {
            return Action::None;
        }
        self.area.take();
        self.attachments.clear();
        self.dismissed = None;
        let _ = self.history.record_with_images(&text, images.clone());
        Action::Submit(text, images)
    }
}

#[cfg(test)]
impl Composer {
    /// Empties the input without sending it.
    pub(crate) fn take_text_for_test(&mut self) {
        self.area.take();
    }
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod tests;
