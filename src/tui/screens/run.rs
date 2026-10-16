//! Progress of a running job: gauges (overall + step for the automatic run), a step
//! checklist, and a live log pane that follows the tail until scrolled up.

use std::collections::VecDeque;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Gauge, Paragraph, Wrap};

use crate::report::{Level, Progress};
use crate::tui::app::App;
use crate::tui::theme::Theme;
use crate::tui::widgets::{block, ellipsize};
use crate::ui::{calc_overall_progress, eta_countdown_at, format_seconds, overall_eta};

pub const LOG_CAPACITY: usize = 5000;

/// Bounded log with an optional in-progress (`\r`-rewritten) last line.
#[derive(Debug, Default, Clone)]
pub struct LogBuffer {
    lines: VecDeque<(Level, String)>,
    transient: Option<String>,
}

impl LogBuffer {
    pub fn push(&mut self, level: Level, line: impl Into<String>) {
        if self.lines.len() == LOG_CAPACITY {
            self.lines.pop_front();
        }
        self.lines.push_back((level, line.into()));
        self.transient = None;
    }

    pub fn set_transient(&mut self, line: impl Into<String>) {
        self.transient = Some(line.into());
    }

    /// Lines to show, the transient one last.
    pub fn len(&self) -> usize {
        self.lines.len() + usize::from(self.transient.is_some())
    }

    pub fn get(&self, i: usize) -> Option<(Level, &str)> {
        if i < self.lines.len() {
            self.lines.get(i).map(|(l, s)| (*l, s.as_str()))
        } else if i == self.lines.len() {
            self.transient.as_deref().map(|s| (Level::Raw, s))
        } else {
            None
        }
    }
}

/// Scroll position of a log pane: following the tail, or `offset` lines above it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Scroll {
    pub offset: usize,
}

impl Scroll {
    pub fn follow(&self) -> bool {
        self.offset == 0
    }
    pub fn up(&mut self, n: usize, len: usize) {
        self.offset = (self.offset + n).min(len.saturating_sub(1));
    }
    pub fn down(&mut self, n: usize) {
        self.offset = self.offset.saturating_sub(n);
    }
    pub fn top(&mut self, len: usize) {
        self.offset = len.saturating_sub(1);
    }
    pub fn bottom(&mut self) {
        self.offset = 0;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Finished {
    Ok { message: String, issues: Vec<String> },
    Canceled(String),
}

#[derive(Debug, Clone)]
pub struct RunState {
    pub title: String,
    pub auto: bool,
    pub started: i64,
    pub progress: Option<Progress>,
    pub log: LogBuffer,
    pub scroll: Scroll,
    /// Items that ended with ✘ (failed packages)
    pub failures: Vec<String>,
    pub cancel_requested: bool,
    pub finished: Option<Finished>,
}

pub const AUTO_STEPS: [&str; 4] = ["System upgrade", "kud3n's repository", "Packages", "Services"];

impl RunState {
    pub fn new(title: impl Into<String>, auto: bool, now: i64) -> Self {
        RunState {
            title: title.into(),
            auto,
            started: now,
            progress: None,
            log: LogBuffer::default(),
            scroll: Scroll::default(),
            failures: Vec::new(),
            cancel_requested: false,
            finished: None,
        }
    }

    pub fn running(&self) -> bool {
        self.finished.is_none()
    }

    pub fn on_progress(&mut self, p: Progress) {
        if matches!(p.mark.as_deref(), Some("✘")) && !self.failures.contains(&p.item) {
            self.failures.push(p.item.clone());
        }
        self.progress = Some(p);
    }

    pub fn push_log(&mut self, level: Level, line: String) {
        self.log.push(level, line);
        if !self.scroll.follow() {
            // Keep the same lines in view while new ones arrive
            self.scroll.offset += 1;
        }
    }
}

fn pct(step: i64, total: i64) -> u16 {
    if total > 0 { (step.clamp(0, total) * 100 / total) as u16 } else { 0 }
}

fn gauge<'a>(theme: &Theme, title: String, percent: u16, label: String, style: Style) -> Gauge<'a> {
    Gauge::default().block(block(theme, title)).gauge_style(style).percent(percent.min(100)).label(label).use_unicode(theme.sym.ellipsis == "…")
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &RunState) {
    let theme = &app.theme;
    let wide = body.width >= 100;
    let p = st.progress.as_ref();
    let spinner = theme.sym.spinner[app.frame % theme.sym.spinner.len()];

    let gauges_h = if st.auto { 6 } else { 3 };
    let status_h = 2 + st.finished.as_ref().map_or(1, |f| match f {
        Finished::Ok { issues, .. } => 1 + issues.len().min(5) as u16,
        Finished::Canceled(_) => 1,
    });
    let [top, status, log_area] = Layout::vertical([Constraint::Length(gauges_h), Constraint::Length(status_h), Constraint::Min(3)]).areas(body);

    let (gauge_area, steps_area) = if st.auto && wide {
        let [g, s] = Layout::horizontal([Constraint::Min(40), Constraint::Length(30)]).areas(top);
        (g, Some(s))
    } else {
        (top, None)
    };

    let (s_step, s_total) = p.map_or((0, 0), |p| (p.s_step, p.s_total));
    let s_start = p.map_or(st.started, |p| p.s_start);
    let step_title = p.map_or_else(|| st.title.clone(), |p| p.title.clone());
    let s_pct = pct(s_step, s_total);
    let step_label = format!(
        "{s_pct}% · {} elapsed · ETA {}",
        format_seconds(app.now - s_start),
        if st.running() { eta_countdown_at(app.now, s_start, s_step, s_total, 15) } else { "00:00".into() }
    );
    if st.auto {
        let [o, s] = Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).areas(gauge_area);
        let o_step = p.map_or(1, |p| p.o_step);
        let o_total = p.map_or(4, |p| p.o_total);
        let o_start = p.map_or(st.started, |p| p.o_start);
        let o_pct = if st.running() { calc_overall_progress(o_step, s_step, s_total) } else { 100 };
        let o_elapsed = (app.now - o_start).max(0);
        let label = format!("{o_pct}% · {} elapsed · ETA {}", format_seconds(o_elapsed), format_seconds(overall_eta(o_pct, o_elapsed)));
        let title = format!(" Overall: step {o_step}/{o_total} · {step_title} ");
        f.render_widget(gauge(theme, title, o_pct as u16, label, theme.gauge_overall()), o);
        let title = format!(" Step {s_step}/{s_total} ");
        f.render_widget(gauge(theme, title, s_pct, step_label, theme.gauge_step()), s);
    } else {
        let title = format!(" {} · {s_step}/{s_total} ", st.title);
        f.render_widget(gauge(theme, title, s_pct, step_label, theme.gauge_overall()), gauge_area);
    }

    if let Some(area) = steps_area {
        let o_step = p.map_or(1, |p| p.o_step) as usize;
        let lines: Vec<Line> = AUTO_STEPS
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let (sym, style) = if i + 1 < o_step || st.finished.is_some() && i < o_step {
                    (theme.sym.ok, theme.ok())
                } else if i + 1 == o_step {
                    (spinner, theme.accent())
                } else {
                    (theme.sym.pending, theme.dim())
                };
                Line::from(vec![Span::styled(format!(" {sym} "), style), Span::raw(*name)])
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(lines)).block(block(theme, " Steps ")), area);
    }

    // Status: current item, or how the job ended
    let mut lines: Vec<Line> = Vec::new();
    match &st.finished {
        None => {
            let item = p.map_or("Starting…", |p| p.item.as_str());
            let mark = p.and_then(|p| p.mark.as_deref()).map(|m| if m == "✘" { theme.sym.fail } else { theme.sym.ok });
            let (sym, style) = match mark {
                Some(m) if m == theme.sym.fail => (m, theme.error()),
                Some(m) => (m, theme.ok()),
                None => (spinner, theme.accent()),
            };
            let count = if s_total > 0 { format!("{s_step}/{s_total}: ") } else { String::new() };
            let max = status.width.saturating_sub(8) as usize;
            let mut spans = vec![Span::styled(format!("{sym} "), style), Span::raw(ellipsize(&format!("{count}{item}"), max, theme.sym.ellipsis))];
            if st.cancel_requested {
                spans.push(Span::styled("  (canceling…)", theme.warn()));
            } else if !st.failures.is_empty() {
                spans.push(Span::styled(format!("  {} {} failed", theme.sym.fail, st.failures.len()), theme.error()));
            }
            lines.push(Line::from(spans));
        }
        Some(Finished::Ok { message, issues }) if issues.is_empty() => {
            lines.push(Line::styled(format!("{} {message}", theme.sym.ok), theme.ok()));
        }
        Some(Finished::Ok { message, issues }) => {
            lines.push(Line::styled(format!("{} {message}, with {} issue(s):", theme.sym.warn, issues.len()), theme.warn()));
            for i in issues.iter().take(5) {
                lines.push(Line::from(vec![Span::styled(format!("  {} ", theme.sym.fail), theme.error()), Span::raw(i.as_str())]));
            }
            if issues.len() > 5 {
                lines.push(Line::styled(format!("  … and {} more (see the log)", issues.len() - 5), theme.dim()));
            }
        }
        Some(Finished::Canceled(msg)) => lines.push(Line::styled(format!("{} {msg}", theme.sym.warn), theme.warn())),
    }
    f.render_widget(Paragraph::new(Text::from(lines)).block(block(theme, " Status ")), status);

    log_view(f, log_area, theme, &st.log, st.scroll);
}

pub fn level_style(theme: &Theme, level: Level) -> (Style, &'static str) {
    match level {
        Level::Error => (theme.error(), "ERROR "),
        Level::Warn => (theme.warn(), "WARN  "),
        Level::Success => (theme.ok(), "OK    "),
        Level::Step => (theme.accent(), "==>   "),
        Level::Info => (Style::new(), "INFO  "),
        Level::Raw | Level::Blank => (theme.dim(), "      "),
    }
}

/// A log pane showing the lines that fit, ending `scroll.offset` lines above the tail.
pub fn log_view(f: &mut Frame, area: Rect, theme: &Theme, log: &LogBuffer, scroll: Scroll) {
    let title = if scroll.follow() { " Log (following) ".to_string() } else { format!(" Log (paused, {} newer line(s); G to follow) ", scroll.offset) };
    let blk = block(theme, title);
    let inner = blk.inner(area);
    let rows = inner.height as usize;
    let end = log.len().saturating_sub(scroll.offset);
    let start = end.saturating_sub(rows);
    let lines: Vec<Line> = (start..end)
        .filter_map(|i| log.get(i))
        .map(|(level, text)| {
            let (style, tag) = level_style(theme, level);
            Line::from(vec![Span::styled(tag, style), Span::styled(text.to_string(), if level == Level::Raw { theme.dim() } else { style })])
        })
        .collect();
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(blk), area);
}

pub const HINTS_RUNNING: [(&str, &str); 4] = [("PgUp/PgDn", "scroll"), ("G", "follow"), ("Esc", "cancel"), ("?", "help")];
pub const HINTS_DONE: [(&str, &str); 4] = [("Enter", "back to menu"), ("PgUp/PgDn", "scroll"), ("l", "full log"), ("?", "help")];

pub const HELP: [(&str, &str); 7] = [
    ("PgUp/PgDn  ↑/↓", "Scroll the log (pauses following)"),
    ("g / G", "Oldest line / follow the newest"),
    ("Mouse wheel", "Scroll the log"),
    ("Esc  Ctrl+C  q", "Cancel the running job (asks first)"),
    ("Enter", "Back to the menu once the job is done"),
    ("l", "Open the full log file (when done)"),
    ("?", "This help"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_is_bounded_and_transient_is_replaced() {
        let mut b = LogBuffer::default();
        for i in 0..LOG_CAPACITY + 10 {
            b.push(Level::Info, format!("l{i}"));
        }
        assert_eq!(b.len(), LOG_CAPACITY);
        assert_eq!(b.get(0).unwrap().1, "l10");
        b.set_transient(" 10%");
        b.set_transient(" 50%");
        assert_eq!(b.len(), LOG_CAPACITY + 1);
        assert_eq!(b.get(LOG_CAPACITY).unwrap().1, " 50%");
        b.push(Level::Raw, "done");
        assert_eq!(b.len(), LOG_CAPACITY);
        assert_eq!(b.get(LOG_CAPACITY - 1).unwrap().1, "done");
    }

    #[test]
    fn scrolling_pauses_and_keeps_view() {
        let mut st = RunState::new("x", false, 0);
        for i in 0..20 {
            st.push_log(Level::Info, format!("{i}"));
        }
        assert!(st.scroll.follow());
        st.scroll.up(5, st.log.len());
        st.push_log(Level::Info, "new".into());
        assert_eq!(st.scroll.offset, 6);
        st.scroll.bottom();
        assert!(st.scroll.follow());
        st.scroll.up(100, st.log.len());
        assert_eq!(st.scroll.offset, 20);
    }

    #[test]
    fn failures_are_collected_once() {
        let mut st = RunState::new("x", false, 0);
        let p = |item: &str, mark: Option<&str>| Progress {
            auto: false,
            o_step: 0,
            o_total: 1,
            o_start: 0,
            title: "t".into(),
            s_start: 0,
            s_step: 1,
            s_total: 2,
            item: item.into(),
            recent_log: String::new(),
            mark: mark.map(str::to_string),
        };
        st.on_progress(p("zen", Some("✘")));
        st.on_progress(p("zen", Some("✘")));
        st.on_progress(p("obsidian", Some("✔")));
        assert_eq!(st.failures, vec!["zen".to_string()]);
    }
}
