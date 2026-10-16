//! App state and the pure `update` / `view` pair. `update` never does I/O: anything slow or
//! side-effecting comes back as a `Command` for the runtime in `mod.rs` to carry out.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};

use super::event::Event;
use super::screens::help;
use super::screens::menu::{self, Action, ITEMS, MenuState};
use super::theme::Theme;
use super::widgets::{self, MIN_H, MIN_W};

/// Who kas runs for, shown in the header bar (copied out of `ctx()` so drawing stays pure).
#[derive(Clone, Debug, Default)]
pub struct Header {
    pub user: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub host: String,
    pub root: bool,
    pub log_file: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Ok,
    Error,
    Info,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    /// `App::now_ms` after which it disappears
    pub until_ms: u64,
}

#[derive(Debug)]
pub enum Screen {
    Menu,
}

/// Side effects requested by `update`.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Run a menu action
    Run(Action),
}

pub struct App {
    pub header: Header,
    pub theme: Theme,
    pub size: (u16, u16),
    /// Wall clock (seconds) and a millisecond clock, set by the runtime
    pub now: i64,
    pub now_ms: u64,
    /// Spinner frame, advanced on ticks
    pub frame: usize,
    pub screen: Screen,
    pub menu: MenuState,
    pub help: bool,
    pub toast: Option<Toast>,
    pub quit: bool,
    last_click: Option<(usize, u64)>,
}

const DOUBLE_CLICK_MS: u64 = 400;

pub fn is_ctrl_c(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'C'))
}

impl App {
    pub fn new(header: Header, theme: Theme, size: (u16, u16)) -> Self {
        App {
            header,
            theme,
            size,
            now: 0,
            now_ms: 0,
            frame: 0,
            screen: Screen::Menu,
            menu: MenuState::default(),
            help: false,
            toast: None,
            quit: false,
            last_click: None,
        }
    }

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast { text: text.into(), kind, until_ms: self.now_ms + 4000 });
    }

    /// Whether something on screen animates (the runtime only ticks then).
    pub fn animating(&self) -> bool {
        self.toast.is_some()
    }

    fn too_small(&self) -> bool {
        self.size.0 < MIN_W || self.size.1 < MIN_H
    }

    fn body_area(&self) -> Rect {
        let [_, body, _] = areas(Rect::new(0, 0, self.size.0, self.size.1));
        body
    }

    fn help_keys(&self) -> (&'static str, &'static [(&'static str, &'static str)]) {
        match self.screen {
            Screen::Menu => ("Main menu", &menu::HELP),
        }
    }
}

fn areas(area: Rect) -> [Rect; 3] {
    Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(area)
}

pub fn update(app: &mut App, ev: Event) -> Option<Command> {
    match ev {
        Event::Resize(w, h) => {
            app.size = (w, h);
            None
        }
        Event::Tick => {
            app.frame = app.frame.wrapping_add(1);
            if app.toast.as_ref().is_some_and(|t| app.now_ms >= t.until_ms) {
                app.toast = None;
            }
            None
        }
        Event::Paste(_) => None,
        Event::Key(k) if k.kind == KeyEventKind::Release => None,
        Event::Key(k) => on_key(app, k),
        Event::Mouse(m) => on_mouse(app, m),
    }
}

fn on_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    if is_ctrl_c(&k) {
        app.quit = true;
        return None;
    }
    if app.help {
        app.help = false;
        return None;
    }
    if app.too_small() {
        if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
            app.quit = true;
        }
        return None;
    }
    if k.code == KeyCode::Char('?') {
        app.help = true;
        return None;
    }
    match app.screen {
        Screen::Menu => menu_key(app, k),
    }
}

fn menu_key(app: &mut App, k: KeyEvent) -> Option<Command> {
    match k.code {
        KeyCode::Up | KeyCode::Char('k') => app.menu.up(),
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => app.menu.down(),
        KeyCode::Home | KeyCode::Char('g') => app.menu.selected = 0,
        KeyCode::End | KeyCode::Char('G') => app.menu.selected = ITEMS.len() - 1,
        KeyCode::Enter => return activate(app, app.menu.action()),
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char(c @ '1'..='9') => {
            let idx = c as usize - '1' as usize;
            app.menu.selected = idx;
            return activate(app, ITEMS[idx].action);
        }
        _ => {}
    }
    None
}

fn activate(app: &mut App, action: Action) -> Option<Command> {
    if action == Action::Quit {
        app.quit = true;
        return None;
    }
    Some(Command::Run(action))
}

fn on_mouse(app: &mut App, m: MouseEvent) -> Option<Command> {
    if app.help || app.too_small() {
        return None;
    }
    match app.screen {
        Screen::Menu => match m.kind {
            MouseEventKind::ScrollUp => app.menu.up(),
            MouseEventKind::ScrollDown => app.menu.down(),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = menu::row_at(app.body_area(), m.column, m.row)?;
                let double = app.last_click.is_some_and(|(r, t)| r == row && app.now_ms.saturating_sub(t) <= DOUBLE_CLICK_MS);
                app.menu.selected = row;
                if double {
                    app.last_click = None;
                    return activate(app, ITEMS[row].action);
                }
                app.last_click = Some((row, app.now_ms));
            }
            _ => {}
        },
    }
    None
}

pub fn view(app: &App, f: &mut Frame) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        widgets::too_small(f, area, &app.theme);
        return;
    }
    let [top, body, bottom] = areas(area);
    widgets::header(f, top, &app.header, &app.theme);
    match app.screen {
        Screen::Menu => {
            menu::view(f, body, app);
            widgets::footer(f, bottom, &menu::HINTS, &app.theme);
        }
    }
    if app.help {
        let (title, keys) = app.help_keys();
        help::view(f, area, &app.theme, title, keys);
    }
    widgets::toast(f, body, app);
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyEventState;

    pub fn header() -> Header {
        Header {
            user: "p3ter".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/p3ter".into(),
            host: "archbox".into(),
            root: true,
            log_file: "/var/log/kas.log".into(),
        }
    }

    pub fn app(w: u16, h: u16) -> App {
        App::new(header(), Theme::new(true, false), (w, h))
    }

    pub fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent { code, modifiers: KeyModifiers::NONE, kind: KeyEventKind::Press, state: KeyEventState::NONE })
    }

    pub fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    pub fn render(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
        t.draw(|f| view(app, f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn menu_navigation_wraps() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Up));
        assert_eq!(a.menu.selected, ITEMS.len() - 1);
        update(&mut a, key(KeyCode::Down));
        assert_eq!(a.menu.selected, 0);
        update(&mut a, key(KeyCode::Char('j')));
        update(&mut a, key(KeyCode::Char('j')));
        update(&mut a, key(KeyCode::Char('k')));
        assert_eq!(a.menu.selected, 1);
        update(&mut a, key(KeyCode::End));
        assert_eq!(a.menu.action(), Action::Quit);
        update(&mut a, key(KeyCode::Home));
        assert_eq!(a.menu.selected, 0);
    }

    #[test]
    fn digits_and_enter_run_actions() {
        let mut a = app(80, 24);
        assert_eq!(update(&mut a, key(KeyCode::Char('3'))), Some(Command::Run(Action::Repo)));
        assert_eq!(a.menu.selected, 2);
        assert_eq!(update(&mut a, key(KeyCode::Char('9'))), Some(Command::Run(Action::Info)));
        update(&mut a, key(KeyCode::Home));
        assert_eq!(update(&mut a, key(KeyCode::Enter)), Some(Command::Run(Action::Auto)));
        assert!(!a.quit);
    }

    #[test]
    fn quit_keys() {
        for ev in [key(KeyCode::Char('q')), key(KeyCode::Esc), ctrl('c')] {
            let mut a = app(80, 24);
            assert_eq!(update(&mut a, ev), None);
            assert!(a.quit);
        }
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::End));
        update(&mut a, key(KeyCode::Enter));
        assert!(a.quit);
    }

    #[test]
    fn help_opens_and_any_key_closes() {
        let mut a = app(80, 24);
        update(&mut a, key(KeyCode::Char('?')));
        assert!(a.help);
        assert_eq!(update(&mut a, key(KeyCode::Char('q'))), None);
        assert!(!a.help && !a.quit);
    }

    #[test]
    fn double_click_runs() {
        let mut a = app(120, 30);
        let click = |row| Event::Mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: 5, row, modifiers: KeyModifiers::NONE });
        // Body starts at y=1, list border at y=1, first item at y=2
        assert_eq!(update(&mut a, click(4)), None);
        assert_eq!(a.menu.selected, 2);
        a.now_ms += 100;
        assert_eq!(update(&mut a, click(4)), Some(Command::Run(Action::Repo)));
        a.now_ms += 5000;
        assert_eq!(update(&mut a, click(4)), None);
    }

    #[test]
    fn snapshot_menu_80x24() {
        insta::assert_snapshot!(render(&app(80, 24)));
    }

    #[test]
    fn snapshot_menu_140x40() {
        let mut a = app(140, 40);
        update(&mut a, key(KeyCode::Char('j')));
        insta::assert_snapshot!(render(&a));
    }

    #[test]
    fn snapshot_too_small() {
        let mut a = app(50, 15);
        insta::assert_snapshot!(render(&a));
        update(&mut a, key(KeyCode::Char('q')));
        assert!(a.quit);
    }

    #[test]
    fn snapshot_help_ascii() {
        let mut a = App::new(header(), Theme::new(false, true), (80, 24));
        update(&mut a, key(KeyCode::Char('?')));
        insta::assert_snapshot!(render(&a));
    }
}
