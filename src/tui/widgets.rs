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
    let full = format!(" kas │ {} (UID {}, GID {}) │ {} │ {} │ {mode} ", h.user, h.uid, h.gid, h.home, h.host);
    let mid = format!(" kas │ {} ({}:{}) │ {} │ {mode} ", h.user, h.uid, h.gid, h.host);
    let short = format!(" kas │ {} │ {mode} ", h.user);
    let w = area.width as usize;
    let text = [full, mid, short].into_iter().find(|t| t.chars().count() <= w).unwrap_or_else(|| " kas".into());
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
    let w = (text.chars().count() as u16 + 2).min(area.width);
    let r = Rect { x: area.x + area.width.saturating_sub(w), y: area.y + area.height.saturating_sub(4), width: w, height: 3.min(area.height) };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(Line::styled(text, style)).block(block(theme, "")), r);
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
}
