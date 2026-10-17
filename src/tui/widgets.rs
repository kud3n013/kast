//! Shared drawing pieces: header bar, footer key hints, dialogs, toasts, the too-small screen.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Flex, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::app::{App, Header, ToastKind};
use super::theme::Theme;

pub const MIN_W: u16 = 60;
pub const MIN_H: u16 = 20;

pub fn block<'a>(theme: &Theme, title: impl Into<Line<'a>>) -> Block<'a> {
    Block::default().borders(Borders::ALL).border_type(theme.sym.border).border_style(theme.accent()).title(title.into().style(theme.title()))
}

pub fn header(f: &mut Frame, area: Rect, h: &Header, theme: &Theme) {
    let mode = if h.root { "root" } else { "user" };
    let full = format!(" kast │ {} (UID {}, GID {}) │ {} │ {} │ {mode} ", h.user, h.uid, h.gid, h.home, h.host);
    let mid = format!(" kast │ {} ({}:{}) │ {} │ {mode} ", h.user, h.uid, h.gid, h.host);
    let short = format!(" kast │ {} │ {mode} ", h.user);
    let w = area.width as usize;
    let text = [full, mid, short].into_iter().find(|t| t.chars().count() <= w).unwrap_or_else(|| " kast".into());
    let text = if theme.sym.ellipsis == "..." { text.replace('│', "|") } else { text };
    f.render_widget(Paragraph::new(text).style(theme.header()), area);
}

/// Bottom line of `key: action` hints; drops hints from the end when the line is too narrow.
pub fn footer(f: &mut Frame, area: Rect, hints: &[(&str, &str)], theme: &Theme) {
    let mut spans: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for (k, what) in hints {
        let k = if theme.sym.up_down == "↑/↓" { k.to_string() } else { super::screens::help::ascii_keys(k) };
        let len = k.chars().count() + what.chars().count() + 3;
        if used + len > area.width as usize {
            break;
        }
        used += len;
        spans.push(Span::styled(format!(" {k}"), theme.bold()));
        spans.push(Span::styled(format!(" {what} "), theme.dim()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn too_small(f: &mut Frame, area: Rect, theme: &Theme) {
    let msg = Text::from(vec![
        Line::styled("Terminal too small", theme.warn()),
        Line::raw(format!("need {MIN_W}×{MIN_H}, have {}×{}", area.width, area.height)),
        Line::styled("Enlarge the window, or press q to quit", theme.dim()),
    ]);
    let h = 3.min(area.height);
    let [mid] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(area);
    f.render_widget(Paragraph::new(msg).alignment(Alignment::Center).wrap(Wrap { trim: true }), mid);
}

/// A centered box of at most `w`×`h` cells, cleared so it draws over the screen below.
pub fn popup(f: &mut Frame, area: Rect, w: u16, h: u16) -> Rect {
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h.min(area.height))]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    r
}

pub fn toast(f: &mut Frame, area: Rect, app: &App) {
    let Some(t) = &app.toast else { return };
    let theme = &app.theme;
    let (sym, style) = match t.kind {
        ToastKind::Ok => (theme.sym.ok, theme.ok()),
        ToastKind::Error => (theme.sym.fail, theme.error()),
        ToastKind::Info => (theme.sym.pending, theme.info()),
    };
    let text = format!(" {sym} {} ", t.text);
    let len = text.chars().count() as u16;
    let w = (len + 2).min(area.width);
    // Too wide for the screen: wrapped
    let rows = wrapped_rows(&text, w.saturating_sub(2).max(1) as usize).min(4) as u16;
    let h = (rows + 2).min(area.height);
    let r = Rect { x: area.x + area.width.saturating_sub(w), y: area.y + area.height.saturating_sub(h + 1), width: w, height: h };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(Line::styled(text, style)).wrap(Wrap { trim: false }).block(block(theme, "")), r);
}

/// Rows `text` takes when word-wrapped at `width` columns.
fn wrapped_rows(text: &str, width: usize) -> usize {
    let (mut rows, mut col) = (1, 0);
    for word in text.split(' ') {
        let n = word.chars().count();
        let need = if col == 0 { n } else { col + 1 + n };
        if need <= width {
            col = need;
        } else {
            // A new row unless the line is empty; a word longer than the width breaks further
            rows += usize::from(col > 0) + n.saturating_sub(1) / width;
            col = if n == 0 { 0 } else { (n - 1) % width + 1 };
        }
    }
    rows
}

/// A Yes/No dialog; `yes_selected` is the highlighted button.
pub struct Dialog<'a> {
    pub title: &'a str,
    pub lines: Vec<Line<'a>>,
    pub yes: &'a str,
    pub no: &'a str,
    pub yes_selected: bool,
}

pub fn dialog(f: &mut Frame, area: Rect, theme: &Theme, d: &Dialog) {
    let (title, lines, yes_label, no_label, yes_selected) = (d.title, &d.lines, d.yes, d.no, d.yes_selected);
    let content_w = lines.iter().map(|l| l.width()).max().unwrap_or(20).max(36) as u16;
    let w = (content_w + 4).min(area.width.saturating_sub(2)).max(30.min(area.width));
    // Wrapped lines can take more rows than lines.len()
    let inner_w = w.saturating_sub(2).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner_w)).sum();
    let h = (rows as u16 + 4).min(area.height);
    let r = popup(f, area, w, h);
    let blk = block(theme, format!(" {title} "));
    let inner = blk.inner(r);
    f.render_widget(blk, r);
    let [body, buttons] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false }), body);
    let btn = |label: &str, on: bool| {
        let s = if on { theme.selected() } else { Style::new() };
        Span::styled(format!(" {label} "), s)
    };
    let line = Line::from(vec![btn(yes_label, yes_selected), Span::raw("   "), btn(no_label, !yes_selected)]).alignment(Alignment::Center);
    f.render_widget(Paragraph::new(line), buttons);
}

/// A one-line text field (path entry, filter).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    pub value: String,
    /// Cursor position in chars
    pub cursor: usize,
}

impl Input {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Input { value, cursor }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.value.char_indices().nth(char_idx).map_or(self.value.len(), |(i, _)| i)
    }

    pub fn insert(&mut self, text: &str) {
        // Pasted text may end in a newline; a path never contains one
        let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        let at = self.byte_at(self.cursor);
        self.value.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    pub fn set(&mut self, value: impl Into<String>) {
        *self = Input::new(value);
    }

    /// Editing keys; false if the key isn't one.
    pub fn key(&mut self, k: &ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Char('u') if ctrl => {
                let at = self.byte_at(self.cursor);
                self.value.drain(..at);
                self.cursor = 0;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.value.chars().count(),
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Delete if self.cursor < self.value.chars().count() => {
                let at = self.byte_at(self.cursor);
                self.value.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Draw inside `area` (one line); shows the cursor when focused.
    pub fn render(&self, f: &mut Frame, area: Rect, theme: &Theme, focused: bool) {
        let w = area.width.max(1) as usize;
        // Keep the cursor visible by scrolling the text horizontally
        let skip = (self.cursor + 1).saturating_sub(w);
        let shown: String = self.value.chars().skip(skip).take(w).collect();
        f.render_widget(Paragraph::new(shown).style(if focused { theme.bold() } else { Style::new() }), area);
        if focused {
            f.set_cursor_position((area.x + (self.cursor - skip) as u16, area.y));
        }
    }
}

/// Shorten `s` to `max` chars with an ellipsis.
pub fn ellipsize(s: &str, max: usize, ellipsis: &str) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(ellipsis.chars().count());
    s.chars().take(keep).collect::<String>() + ellipsis
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ellipsize_keeps_short_and_cuts_long() {
        assert_eq!(ellipsize("abc", 5, "…"), "abc");
        assert_eq!(ellipsize("abcdefgh", 5, "…"), "abcd…");
        assert_eq!(ellipsize("abcdefgh", 5, "..."), "ab...");
    }

    #[test]
    fn wrapped_rows_counts_word_wrap() {
        assert_eq!(wrapped_rows("short", 10), 1);
        assert_eq!(wrapped_rows("aaaa bbbb cccc", 9), 2);
        assert_eq!(wrapped_rows("aaaa bbbb cccc", 4), 3);
        assert_eq!(wrapped_rows("aaaaaaaaaa", 4), 3);
    }

    #[test]
    fn input_edits_unicode_and_paste() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        let mut i = Input::new("/hö");
        i.key(&k(KeyCode::Backspace));
        assert_eq!(i.value, "/h");
        i.key(&k(KeyCode::Home));
        i.insert("~/x.zip\n");
        assert_eq!((i.value.as_str(), i.cursor), ("~/x.zip/h", 7));
        i.key(&k(KeyCode::Delete));
        i.key(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!((i.value.as_str(), i.cursor), ("h", 0));
        assert!(!i.key(&k(KeyCode::Enter)));
    }
}
