//! Scheduled backup (menu `s`, or `s` on the export screen's sources): what the systemd
//! user timer backs up, how often, how many to keep, an optional local copy; on, off, run
//! now; and how the last run went. Reading and changing it are commands for the runtime.

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::schedule::{Spec, Status, TARGETS};
use crate::tui::app::{App, Command, Screen, ToastKind};
use crate::tui::widgets::block;

/// What the schedule screen loads (`Command::LoadSchedule`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleInfo {
    pub status: Status,
    /// From the settings: the last targets and frequency, retention, the local copy folder
    pub targets: Option<Vec<String>>,
    pub every: Option<String>,
    pub keep: u32,
    pub also_local: Option<PathBuf>,
}

pub fn load() -> ScheduleInfo {
    let s = crate::settings::load();
    ScheduleInfo {
        status: crate::schedule::status(),
        targets: s.schedule.targets.clone(),
        every: s.schedule.every.clone(),
        keep: s.keep(),
        also_local: s.also_local(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Target(usize),
    Every,
    Keep,
    AlsoLocal,
    Apply,
    Disable,
    RunNow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleState {
    pub loading: bool,
    pub info: Option<Box<ScheduleInfo>>,
    pub on: Vec<bool>,
    pub weekly: bool,
    pub keep: u32,
    pub local: bool,
    /// The local copy's folder (the setting, else the export default)
    pub local_dir: PathBuf,
    pub focus: usize,
    /// A change is being applied
    pub busy: bool,
}

impl ScheduleState {
    pub fn new(default_local: PathBuf) -> Self {
        let mut on = vec![false; TARGETS.len()];
        on[0] = true;
        ScheduleState {
            loading: true,
            info: None,
            on,
            weekly: false,
            keep: crate::settings::DEFAULT_KEEP,
            local: false,
            local_dir: default_local,
            focus: 0,
            busy: false,
        }
    }

    pub fn rows(&self) -> Vec<Row> {
        let mut r: Vec<Row> = (0..TARGETS.len()).map(Row::Target).collect();
        r.extend([Row::Every, Row::Keep, Row::AlsoLocal, Row::Apply]);
        if self.info.as_ref().is_some_and(|i| i.status.installed) {
            r.extend([Row::Disable, Row::RunNow]);
        }
        r
    }

    pub fn spec(&self) -> Spec {
        // "All" covers the rest: never back the same app up twice
        let targets: Vec<String> =
            if self.on[0] { vec!["all".into()] } else { TARGETS.iter().zip(&self.on).filter(|(_, on)| **on).map(|(t, _)| t.to_string()).collect() };
        Spec { targets, every: if self.weekly { "weekly" } else { "daily" }.into(), keep: self.keep }
    }

    fn installed(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.status.installed)
    }
}

fn state(app: &mut App) -> Option<&mut ScheduleState> {
    match &mut app.screen {
        Screen::Schedule(s) => Some(s),
        _ => None,
    }
}

pub fn open(app: &mut App) -> Option<Command> {
    let dir = crate::tui::screens::export::default_dir(&app.header.home, app.dir_exists);
    app.screen = Screen::Schedule(Box::new(ScheduleState::new(PathBuf::from(dir))));
    Some(match crate::tui::screens::drive::check_if_unknown(app) {
        Some(check) => Command::Many(vec![Command::LoadSchedule, check]),
        None => Command::LoadSchedule,
    })
}

pub fn on_info(app: &mut App, info: ScheduleInfo) {
    let Some(st) = state(app) else { return };
    st.loading = false;
    st.busy = false;
    // What's set up wins; else the last choice; else the defaults
    let spec = info
        .status
        .spec
        .clone()
        .or_else(|| info.targets.clone().map(|targets| Spec { targets, every: info.every.clone().unwrap_or_default(), keep: info.keep }));
    if let Some(s) = spec {
        st.on = TARGETS.iter().map(|t| s.targets.iter().any(|x| x == t)).collect();
        if !st.on.iter().any(|b| *b) {
            st.on[0] = true;
        }
        st.weekly = s.every == "weekly";
        st.keep = s.keep;
    } else {
        st.keep = info.keep;
    }
    if let Some(d) = &info.also_local {
        st.local = true;
        st.local_dir = d.clone();
    }
    st.info = Some(Box::new(info));
    st.focus = st.focus.min(st.rows().len() - 1);
}

pub fn on_done(app: &mut App, result: Result<String, String>) -> Option<Command> {
    match result {
        Ok(msg) => app.toast(ToastKind::Ok, msg),
        Err(e) => app.toast(ToastKind::Error, e),
    }
    let st = state(app)?;
    st.busy = false;
    st.loading = true;
    Some(Command::LoadSchedule)
}

pub fn key(app: &mut App, k: KeyEvent) -> Option<Command> {
    let signed_in = app.drive.as_deref().is_some_and(|d| d.signed_in());
    let st = state(app)?;
    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
        app.screen = Screen::Menu;
        return None;
    }
    if st.loading || st.busy {
        return None;
    }
    let rows = st.rows();
    let n = rows.len();
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => st.focus = st.focus.checked_sub(1).unwrap_or(n - 1),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => st.focus = (st.focus + 1) % n,
        KeyCode::Char('r') => {
            st.loading = true;
            return Some(Command::LoadSchedule);
        }
        KeyCode::Left | KeyCode::Char('-') if rows[st.focus] == Row::Keep => st.keep = st.keep.saturating_sub(1).max(1),
        KeyCode::Right | KeyCode::Char('+') if rows[st.focus] == Row::Keep => st.keep = (st.keep + 1).min(99),
        KeyCode::Char(' ') | KeyCode::Enter => match rows[st.focus] {
            Row::Target(i) => {
                st.on[i] = !st.on[i];
                // "All" and single apps exclude each other
                if i == 0 && st.on[0] {
                    st.on.iter_mut().skip(1).for_each(|b| *b = false);
                } else if i > 0 && st.on[i] {
                    st.on[0] = false;
                }
            }
            Row::Every => st.weekly = !st.weekly,
            Row::Keep => {}
            Row::AlsoLocal => st.local = !st.local,
            Row::Apply => {
                let spec = st.spec();
                if let Err(e) = spec.validate() {
                    app.toast(ToastKind::Error, e);
                    return None;
                }
                let local = st.local.then(|| st.local_dir.clone());
                st.busy = true;
                if !signed_in {
                    app.toast(ToastKind::Info, "Not signed in to Proton Drive: the runs fail until you sign in (Export → Proton Drive)");
                }
                return Some(Command::ScheduleApply(spec, local));
            }
            Row::Disable => {
                st.busy = true;
                return Some(Command::ScheduleDisable);
            }
            Row::RunNow => {
                st.busy = true;
                return Some(Command::ScheduleRun);
            }
        },
        _ => {}
    }
    None
}

pub fn view(f: &mut Frame, body: Rect, app: &App, st: &ScheduleState) {
    let theme = &app.theme;
    let wide = body.width >= 110;
    let rows = st.rows();
    let list_h = rows.len() as u16 + 2;
    let (list_area, panel) = if wide {
        let [l, p] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
        (l, p)
    } else {
        let [l, p] = Layout::vertical([Constraint::Length(list_h.min(body.height.saturating_sub(6))), Constraint::Min(4)]).areas(body);
        (l, p)
    };
    let check = |on: bool| if on { theme.sym.on } else { theme.sym.off };
    let label = |t: &str| match t {
        "all" => "All apps & desktop settings (no face model)".to_string(),
        t => super::export::TARGETS.iter().find(|(k, _)| *k == t).map(|(_, l)| l.to_string()).unwrap_or_else(|| t.to_string()),
    };
    let installed = st.installed();
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| match *r {
            Row::Target(i) => ListItem::new(format!("{} {}", check(st.on[i]), label(TARGETS[i]))),
            Row::Every => ListItem::new(format!("How often:  {}  (Space switches)", if st.weekly { "weekly" } else { "daily" })),
            Row::Keep => ListItem::new(format!("Keep on Drive:  ‹ {} ›  per kind (←/→)", st.keep)),
            Row::AlsoLocal => ListItem::new(format!("{} Also keep a copy in {}", check(st.local), st.local_dir.display())),
            Row::Apply => {
                ListItem::new(Line::styled(format!("{} {}", theme.sym.start, if installed { "Save and keep it on" } else { "Turn it on" }), theme.ok()))
            }
            Row::Disable => ListItem::new(Line::styled("■ Turn it off", theme.warn())),
            Row::RunNow => ListItem::new("▷ Run a backup now"),
        })
        .collect();
    let list = List::new(items)
        .block(block(theme, " Scheduled backup to Proton Drive "))
        .highlight_style(theme.selected())
        .highlight_symbol(format!("{} ", theme.sym.pointer));
    let mut ls = ListState::default().with_selected(Some(st.focus.min(rows.len() - 1)));
    f.render_stateful_widget(list, list_area, &mut ls);

    let mut lines: Vec<Line> = Vec::new();
    let bold = |k: &'static str, v: String| Line::from(vec![Span::styled(format!("{k:<11}"), theme.bold()), Span::raw(v)]);
    let spin = theme.sym.spinner[app.frame % theme.sym.spinner.len()];
    match st.info.as_deref() {
        _ if st.loading => lines.push(Line::styled(format!("{spin} Reading the timer{}", theme.sym.ellipsis), theme.dim())),
        None => {}
        Some(i) => {
            let s = &i.status;
            let state = match (s.installed, s.enabled && s.active) {
                (false, _) => Span::styled("off (not set up)", theme.dim()),
                (true, true) => Span::styled("on", theme.ok()),
                (true, false) => Span::styled("set up, but the timer isn't active", theme.warn()),
            };
            lines.push(Line::from(vec![Span::styled(format!("{:<11}", "Status"), theme.bold()), state]));
            if let Some(p) = &s.spec {
                lines.push(bold("Backs up", format!("{} · {} · keeps {}", p.targets.join(", "), p.every, p.keep)));
            }
            if let Some(n) = &s.next {
                lines.push(bold("Next run", n.clone()));
            }
            match &s.last {
                Some(l) => {
                    let style = if l.result == "failed" { theme.error() } else { theme.ok() };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<11}", "Last run"), theme.bold()),
                        Span::raw(format!("{} · ", l.at)),
                        Span::styled(l.result.clone(), style),
                    ]));
                    for u in &l.uploaded {
                        lines.push(Line::raw(format!("{:<11}{}", "", u.rsplit('/').next().unwrap_or(u))));
                    }
                    if !l.skipped_apps.is_empty() {
                        lines.push(Line::styled(format!("{:<11}{} (were running)", "Left out", l.skipped_apps.join(", ")), theme.warn()));
                    }
                    if let Some(e) = &l.error {
                        lines.push(Line::styled(format!("{:<11}{e}", "Last error"), theme.error()));
                    }
                }
                None if s.installed => lines.push(bold("Last run", "not yet".into())),
                None => {}
            }
            if let Some(p) = &s.stale {
                lines.push(Line::raw(""));
                lines.push(Line::styled(format!("The timer runs {p}, which isn't this kast: \"Save\" points it here."), theme.warn()));
            }
        }
    }
    if st.busy {
        lines.push(Line::styled(format!("{spin} Working{}", theme.sym.ellipsis), theme.dim()));
    }
    lines.push(Line::raw(""));
    let drive = match app.drive.as_deref() {
        Some(d) if d.signed_in() => format!("Uploads to {} on Proton Drive (signed in).", d.host_folder()),
        Some(_) => "Proton Drive isn't ready (see Export → Proton Drive): runs fail until it is.".into(),
        None => "Checking Proton Drive…".into(),
    };
    lines.push(Line::raw(drive));
    for note in [
        "Runs as you, while you're logged in (your keyring is unlocked then). A run missed while the computer was off happens at your next login.",
        "Apps that are running are left out, never closed: their data may be mid-write. Each skip shows here.",
        "The Howdy face model isn't included: it needs sudo.",
        "Nothing is uploaded when nothing changed. A failure shows a notification.",
    ] {
        lines.push(Line::styled(note, theme.dim()));
    }
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(theme, " Status ")), panel);
}

pub const HINTS: [(&str, &str); 5] = [("↑/↓", "move"), ("Space", "toggle"), ("←/→", "keep"), ("r", "refresh"), ("Esc", "back")];

pub const HELP: [(&str, &str); 6] = [
    ("↑/↓  j/k", "Move"),
    ("Space  Enter", "Toggle a target or option; run a button"),
    ("←/→  -/+", "How many backups to keep on Proton Drive"),
    ("r", "Read the timer's state again"),
    ("Esc  q", "Back to the menu"),
    ("", "The timer is a systemd user unit: kast-backup.timer"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::LastRun;
    use crate::tui::app::tests::{app, key, render};
    use crate::tui::app::{Event, update};
    use crate::tui::event::Loaded;

    fn st(a: &App) -> &ScheduleState {
        match &a.screen {
            Screen::Schedule(s) => s,
            _ => panic!("not on the schedule screen"),
        }
    }

    fn info(installed: bool) -> ScheduleInfo {
        let status = if installed {
            Status {
                installed: true,
                enabled: true,
                active: true,
                next: Some("Sun 2026-10-18 00:21:07 CEST".into()),
                spec: Some(Spec { targets: vec!["all".into()], every: "daily".into(), keep: 5 }),
                stale: None,
                last: Some(LastRun {
                    at: "2026-10-17 00:12".into(),
                    result: "uploaded".into(),
                    uploaded: vec!["/my-files/kast-backups/archbox/apps_backup_2026-10-17_001203.zip".into()],
                    skipped_apps: vec!["zen".into(), "beeper".into()],
                    error: None,
                    next_run: None,
                }),
            }
        } else {
            Status::default()
        };
        ScheduleInfo { status, targets: None, every: None, keep: 5, also_local: None }
    }

    fn opened(w: u16, h: u16, i: ScheduleInfo) -> App {
        let mut a = app(w, h);
        a.drive = Some(Box::new(super::super::drive::tests::info(Some(crate::proton::Status::SignedIn))));
        assert!(matches!(update(&mut a, key(KeyCode::Char('s'))), Some(Command::LoadSchedule)));
        update(&mut a, Event::Loaded(Loaded::Schedule(Box::new(i))));
        a
    }

    #[test]
    fn snapshots() {
        insta::assert_snapshot!("schedule_off_80x24", render(&opened(80, 24, info(false))));
        insta::assert_snapshot!("schedule_on_140x40", render(&opened(140, 40, info(true))));
    }

    #[test]
    fn choose_and_turn_on() {
        let mut a = opened(80, 24, info(false));
        assert_eq!(st(&a).spec(), Spec { targets: vec!["all".into()], every: "daily".into(), keep: 5 });
        // A single app turns "all" off, and the other way round
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Char(' ')));
        assert_eq!(st(&a).spec().targets, ["beeper", "zen"]);
        let at = |a: &App, r: Row| st(a).rows().iter().position(|x| *x == r).unwrap();
        let i = at(&a, Row::Every);
        if let Screen::Schedule(s) = &mut a.screen {
            s.focus = i;
        }
        update(&mut a, key(KeyCode::Char(' ')));
        update(&mut a, key(KeyCode::Down));
        update(&mut a, key(KeyCode::Left));
        update(&mut a, key(KeyCode::Left));
        let i = at(&a, Row::Apply);
        if let Screen::Schedule(s) = &mut a.screen {
            s.focus = i;
        }
        let Some(Command::ScheduleApply(spec, local)) = update(&mut a, key(KeyCode::Enter)) else { panic!("no apply") };
        assert_eq!(spec, Spec { targets: vec!["beeper".into(), "zen".into()], every: "weekly".into(), keep: 3 });
        assert_eq!(local, None);
        assert!(update(&mut a, key(KeyCode::Enter)).is_none(), "busy until it's done");
        assert!(matches!(update(&mut a, Event::Loaded(Loaded::ScheduleDone(Ok("on".into())))), Some(Command::LoadSchedule)));
        // Nothing picked: caught before anything runs
        let mut a = opened(80, 24, info(false));
        update(&mut a, key(KeyCode::Char(' ')));
        let i = st(&a).rows().iter().position(|x| *x == Row::Apply).unwrap();
        if let Screen::Schedule(s) = &mut a.screen {
            s.focus = i;
        }
        assert!(update(&mut a, key(KeyCode::Enter)).is_none());
        assert_eq!(a.toast.as_ref().unwrap().text, "Pick at least one thing to back up");
    }

    #[test]
    fn when_on_it_can_be_turned_off_or_run() {
        let mut a = opened(80, 24, info(true));
        let rows = st(&a).rows();
        assert!(rows.contains(&Row::Disable) && rows.contains(&Row::RunNow));
        if let Screen::Schedule(s) = &mut a.screen {
            s.focus = rows.len() - 1;
        }
        assert!(matches!(update(&mut a, key(KeyCode::Enter)), Some(Command::ScheduleRun)));
    }
}
