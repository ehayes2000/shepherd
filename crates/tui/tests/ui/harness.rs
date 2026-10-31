//! [`Ui`]: a real [`Client`] driven the way a user drives it, against a scripted fake host, drawn by the real
//! [`render`] on ratatui's [`TestBackend`].
//!
//! Every step goes through what the runtime does with a terminal event ([`map_event`] with one [`Pointer`], each
//! intent handled by the client) and redraws, so the screen a test reads and clicks is always the one the user would
//! see. The host is a [`HostDriver`] the test scripts ([`Ui::on_host`]) plus raw [`HostMessage`]s ([`Ui::host`]); the
//! client's requests are answered only when the test says so ([`Ui::answer`]). Nothing here sleeps or reads a clock.
//!
//! ```text
//! let mut ui = Ui::with_sessions(&["fix-login", "docs"], 80, 24);
//! ui.click("+ New session");
//! assert!(ui.contains("New session"));
//! insta::assert_snapshot!(ui.screen());
//! ```
//!
//! Text is found cell by cell on the drawn screen: a wide character is one cell's symbol, and the cell it hides is
//! skipped, so a label matches the way it reads.

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::{Buffer, CellWidth};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::Value;
use shepherd_core::acpp::RequestId;
use shepherd_core::client::{Client, ClientCommand, ClientEvent, Intent};
use shepherd_core::ids::SessionId;
use shepherd_core::wire::{
    ClientMessage, ClientRequest, HostMessage, HostNotification, Method, PtyOutput, RawResponse,
    Request, WireError,
};
use shepherd_term::Screen;
use shepherd_testkit::builders::{ClientDriver, HostDriver};
use shepherd_tui::{Pointer, Screens, Target, hit, layout, map_event, render};

/// A headless window onto a client. See the module docs.
pub struct Ui {
    driver: ClientDriver,
    screens: Screens,
    pointer: Pointer,
    terminal: Terminal<TestBackend>,
    /// Every command the client has returned, in order.
    sent: Vec<ClientCommand>,
    answered: Vec<RequestId>,
}

impl Ui {
    /// A `cols` x `rows` window onto `driver`'s client, told its size as the runtime tells it at start.
    pub fn new(driver: ClientDriver, cols: u16, rows: u16) -> Self {
        let terminal = Terminal::new(TestBackend::new(cols, rows)).unwrap_or_else(|e| match e {});
        let mut ui = Self {
            driver,
            screens: Screens::new(),
            pointer: Pointer::default(),
            terminal,
            sent: Vec::new(),
            answered: Vec::new(),
        };
        ui.event(Event::Resize(cols, rows));
        ui
    }

    /// A connected client with one session per name, `s1` (oldest, named `names[0]`) to `s<n>` (newest, selected), in
    /// a `cols` x `rows` window.
    pub fn with_sessions(names: &[&str], cols: u16, rows: u16) -> Self {
        let ids: Vec<String> = (1..=names.len()).map(|i| format!("s{i}")).collect();
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut driver = ClientDriver::with_sessions(&ids);
        for (id, name) in ids.iter().zip(names) {
            driver.host().rename(id, name);
        }
        driver.sync();
        Self::new(driver, cols, rows)
    }

    // ---------- steps ----------

    /// A terminal event, as the runtime handles it: mapped by the TUI with the selected session's input modes, each
    /// intent handled by the client in order. Then redraws.
    pub fn event(&mut self, event: Event) -> &mut Self {
        if let Event::Resize(cols, rows) = event {
            self.terminal.backend_mut().resize(cols, rows);
        }
        let modes = self
            .client()
            .selected()
            .and_then(|id| self.screens.get(id))
            .map(Screen::modes)
            .unwrap_or_default();
        let intents = map_event(
            &event,
            self.driver.client(),
            &modes,
            self.window(),
            &mut self.pointer,
        );
        for intent in intents {
            self.user(intent);
        }
        self.draw();
        self
    }

    /// An intent straight to the client, as if the TUI had mapped it. For setting up state no click reaches yet;
    /// prefer the user's steps.
    pub fn user(&mut self, intent: Intent) -> &mut Self {
        if let Intent::Resize(size) = &intent {
            for screen in self.screens.values_mut() {
                screen.resize(size.cols, size.rows);
            }
        }
        let commands = self.driver.user(intent);
        self.sent.extend(commands);
        self.draw();
        self
    }

    /// A left click on the first cell of `label`, which must be drawn exactly once.
    pub fn click(&mut self, label: &str) -> &mut Self {
        let at = self.at(label);
        self.click_at(at.x, at.y)
    }

    /// A left click on the first cell of `label`, which must be drawn exactly once inside `area`.
    pub fn click_in(&mut self, area: Rect, label: &str) -> &mut Self {
        let at = self.at_in(area, label);
        self.click_at(at.x, at.y)
    }

    /// A right click on the first cell of `label`, which must be drawn exactly once.
    pub fn right_click(&mut self, label: &str) -> &mut Self {
        let at = self.at(label);
        self.right_click_at(at.x, at.y)
    }

    /// A left press and release on cell `x`, `y`.
    pub fn click_at(&mut self, x: u16, y: u16) -> &mut Self {
        self.press(MouseButton::Left, x, y)
    }

    /// A right press and release on cell `x`, `y`.
    pub fn right_click_at(&mut self, x: u16, y: u16) -> &mut Self {
        self.press(MouseButton::Right, x, y)
    }

    fn press(&mut self, button: MouseButton, x: u16, y: u16) -> &mut Self {
        self.mouse(MouseEventKind::Down(button), x, y);
        self.mouse(MouseEventKind::Up(button), x, y)
    }

    /// One mouse event at cell `x`, `y`, with no modifiers.
    pub fn mouse(&mut self, kind: MouseEventKind, x: u16, y: u16) -> &mut Self {
        self.event(Event::Mouse(MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }))
    }

    /// The wheel over the first cell of `label` (drawn exactly once): `lines` notches, down for positive.
    pub fn scroll(&mut self, label: &str, lines: i32) -> &mut Self {
        let at = self.at(label);
        self.scroll_at(at.x, at.y, lines)
    }

    /// The wheel over cell `x`, `y`: `lines` notches, down for positive.
    pub fn scroll_at(&mut self, x: u16, y: u16, lines: i32) -> &mut Self {
        let kind = if lines < 0 {
            MouseEventKind::ScrollUp
        } else {
            MouseEventKind::ScrollDown
        };
        for _ in 0..lines.unsigned_abs() {
            self.mouse(kind, x, y);
        }
        self
    }

    /// Each char of `text` as a key press.
    pub fn type_text(&mut self, text: &str) -> &mut Self {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch));
        }
        self
    }

    /// One key press with no modifiers.
    pub fn key(&mut self, code: KeyCode) -> &mut Self {
        self.key_with(code, KeyModifiers::NONE)
    }

    /// One key press with `modifiers`.
    pub fn key_with(&mut self, code: KeyCode, modifiers: KeyModifiers) -> &mut Self {
        self.event(Event::Key(KeyEvent::new(code, modifiers)))
    }

    /// `ctrl-<ch>`.
    pub fn ctrl(&mut self, ch: char) -> &mut Self {
        self.key_with(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    /// A bracketed paste of `text`.
    pub fn paste(&mut self, text: &str) -> &mut Self {
        self.event(Event::Paste(text.to_owned()))
    }

    /// The window resized to `cols` x `rows`.
    pub fn resize(&mut self, cols: u16, rows: u16) -> &mut Self {
        self.event(Event::Resize(cols, rows))
    }

    /// A message from the host, as the runtime handles it: a session's PTY output is fed to its terminal (made at the
    /// client's size on first output) before the client sees it, and a log entry drops the terminals of sessions that
    /// are gone.
    pub fn host(&mut self, message: HostMessage) -> &mut Self {
        if let HostMessage::Notification(HostNotification::PtyOutput(out)) = &message {
            let (cols, rows) = self.client().size().map_or((80, 24), |s| (s.cols, s.rows));
            let screen = self
                .screens
                .entry(out.session.clone())
                .or_insert_with(|| Screen::new(cols, rows));
            screen.feed(&out.bytes);
            screen.take_replies();
        }
        let log_entry = matches!(
            message,
            HostMessage::Notification(HostNotification::LogEntry(_))
        );
        let commands = self.driver.handle(ClientEvent::Host(message));
        self.sent.extend(commands);
        if log_entry {
            self.drop_closed_screens();
        }
        self.draw();
        self
    }

    fn drop_closed_screens(&mut self) {
        let sessions = self.driver.client().sessions();
        self.screens
            .retain(|id, _| sessions.iter().any(|s| &s.id == id));
    }

    /// Session `id`'s PTY wrote `bytes`.
    pub fn pty(&mut self, id: &str, bytes: &[u8]) -> &mut Self {
        self.host(HostMessage::Notification(HostNotification::PtyOutput(
            PtyOutput {
                session: SessionId::new(id),
                bytes: bytes.to_vec(),
            },
        )))
    }

    /// Scripts the host (`script` runs on its [`HostDriver`]), then delivers the log entries that appended to the
    /// client, as a subscribed client gets them.
    pub fn on_host(&mut self, script: impl FnOnce(&mut HostDriver)) -> &mut Self {
        script(self.driver.host());
        let commands = self.driver.sync();
        self.sent.extend(commands);
        self.drop_closed_screens();
        self.draw();
        self
    }

    /// Answers the newest unanswered request for `method` with `result`. Panics, naming what was sent, if there is
    /// none.
    pub fn answer(&mut self, method: Method, result: Result<Value, WireError>) -> &mut Self {
        let id = self
            .requests_with_ids()
            .into_iter()
            .rev()
            .find(|r| r.body.method() == method && !self.answered.contains(&r.id))
            .map(|r| r.id.clone())
            .unwrap_or_else(|| {
                let sent: Vec<Method> = self.requests().iter().map(|r| r.method()).collect();
                panic!("Ui: no unanswered {method:?} request; requests sent: {sent:?}")
            });
        self.answered.push(id.clone());
        self.host(HostMessage::Response(RawResponse { id, result }))
    }

    fn draw(&mut self) {
        let (client, screens) = (self.driver.client(), &self.screens);
        self.terminal
            .draw(|frame| render(client, screens, frame))
            .unwrap_or_else(|e| match e {});
    }

    // ---------- what is on screen ----------

    /// The window's area.
    pub fn window(&self) -> Rect {
        self.buffer().area
    }

    /// What is drawn, without styles.
    pub fn buffer(&self) -> &Buffer {
        self.terminal.backend().buffer()
    }

    /// The screen as text: one line per row, trailing blanks trimmed.
    pub fn screen(&self) -> String {
        self.region(self.window())
    }

    /// The text of the cells in `area` (clipped to the window), as [`Ui::screen`] shows it.
    pub fn region(&self, area: Rect) -> String {
        let area = area.intersection(self.window());
        let lines: Vec<String> = (area.top()..area.bottom())
            .map(|y| {
                let line: String = self
                    .row(y)
                    .into_iter()
                    .filter(|(x, _)| (area.left()..area.right()).contains(x))
                    .map(|(_, symbol)| symbol)
                    .collect();
                line.trim_end().to_owned()
            })
            .collect();
        lines.join("\n")
    }

    /// Whether `text` is drawn anywhere, on one row.
    pub fn contains(&self, text: &str) -> bool {
        !self.find(text).is_empty()
    }

    /// Panics, showing the screen, unless `text` is drawn.
    pub fn assert_contains(&self, text: &str) -> &Self {
        assert!(
            self.contains(text),
            "{text:?} is not on screen:\n{}",
            self.screen()
        );
        self
    }

    /// Panics, showing the screen, if `text` is drawn.
    pub fn assert_lacks(&self, text: &str) -> &Self {
        assert!(
            !self.contains(text),
            "{text:?} is on screen:\n{}",
            self.screen()
        );
        self
    }

    /// Every cell `text` starts at, top to bottom and left to right.
    pub fn find(&self, text: &str) -> Vec<Position> {
        self.find_in(self.window(), text)
    }

    /// Every cell inside `area` that `text` starts at and fits in, top to bottom and left to right.
    pub fn find_in(&self, area: Rect, text: &str) -> Vec<Position> {
        let area = area.intersection(self.window());
        if text.is_empty() {
            return Vec::new();
        }
        let mut found = Vec::new();
        for y in area.top()..area.bottom() {
            let row: Vec<(u16, String)> = self
                .row(y)
                .into_iter()
                .filter(|(x, _)| (area.left()..area.right()).contains(x))
                .collect();
            for start in 0..row.len() {
                let mut rest = text;
                for (_, symbol) in &row[start..] {
                    match rest.strip_prefix(symbol.as_str()) {
                        Some(r) => rest = r,
                        None => break,
                    }
                    if rest.is_empty() {
                        break;
                    }
                }
                if rest.is_empty() {
                    found.push(Position::new(row[start].0, y));
                }
            }
        }
        found
    }

    /// Where `text` is drawn. Panics, showing the screen, unless it is drawn exactly once.
    pub fn at(&self, text: &str) -> Position {
        self.at_in(self.window(), text)
    }

    /// Where `text` is drawn inside `area`. Panics, showing the screen, unless it is drawn there exactly once.
    pub fn at_in(&self, area: Rect, text: &str) -> Position {
        match self.find_in(area, text)[..] {
            [at] => at,
            [] => panic!(
                "{text:?} is not on screen{}:\n{}",
                self.within(area),
                self.screen()
            ),
            ref all => panic!(
                "{text:?} is ambiguous{}: drawn at {}:\n{}",
                self.within(area),
                all.iter()
                    .map(|p| format!("({}, {})", p.x, p.y))
                    .collect::<Vec<_>>()
                    .join(", "),
                self.screen()
            ),
        }
    }

    /// The whole screen line `text` is drawn on (exactly once), for [`Ui::click_in`] on a control repeated per row.
    pub fn line_of(&self, text: &str) -> Rect {
        let at = self.at(text);
        Rect::new(0, at.y, self.window().width, 1)
    }

    /// The inside of the bordered box titled `title`: the one whose top-left corner reads `┌ <title>`, to its `┐` and
    /// `└`. Panics, showing the screen, unless that box is drawn whole, once.
    pub fn boxed(&self, title: &str) -> Rect {
        let corner = self.at(&format!("┌ {title}"));
        let row = self.row(corner.y);
        let right = row
            .iter()
            .find(|(x, s)| *x > corner.x && s == "┐")
            .map(|(x, _)| *x);
        let bottom = (corner.y + 1..self.window().bottom())
            .find(|y| self.buffer()[(corner.x, *y)].symbol() == "└");
        let (Some(right), Some(bottom)) = (right, bottom) else {
            panic!("the {title:?} box is not drawn whole:\n{}", self.screen());
        };
        Rect::new(
            corner.x + 1,
            corner.y + 1,
            right - corner.x - 1,
            bottom - corner.y - 1,
        )
    }

    fn within(&self, area: Rect) -> String {
        if area == self.window() {
            String::new()
        } else {
            format!(" within {area:?}")
        }
    }

    /// Every word drawn, top to bottom and left to right: each run of non-blank cells on a row, with where it starts.
    pub fn words(&self) -> Vec<(Position, String)> {
        let mut words = Vec::new();
        for y in self.window().top()..self.window().bottom() {
            let mut word: Option<(Position, String)> = None;
            for (x, symbol) in self.row(y) {
                if symbol.trim().is_empty() {
                    words.extend(word.take());
                } else {
                    word.get_or_insert_with(|| (Position::new(x, y), String::new()))
                        .1
                        .push_str(&symbol);
                }
            }
            words.extend(word);
        }
        words
    }

    /// Row `y`'s visible symbols with their columns: a cell hidden by a wide symbol to its left is left out.
    fn row(&self, y: u16) -> Vec<(u16, String)> {
        let buf = self.buffer();
        let mut cells = Vec::new();
        let mut skip: u16 = 0;
        for x in buf.area.left()..buf.area.right() {
            let cell = &buf[(x, y)];
            if skip == 0 {
                cells.push((x, cell.symbol().to_owned()));
            }
            skip = skip.max(cell.cell_width()).saturating_sub(1);
        }
        cells
    }

    /// The control under cell `x`, `y`, as a click there finds it.
    pub fn hit_at(&self, x: u16, y: u16) -> Option<Target> {
        hit(&layout(self.window()), self.client(), x, y)
    }

    // ---------- the client and what it sent ----------

    pub fn client(&self) -> &Client {
        self.driver.client()
    }

    /// Every command the client has returned so far, in order.
    pub fn sent(&self) -> &[ClientCommand] {
        &self.sent
    }

    /// Every request the client has sent so far, in order.
    pub fn requests(&self) -> Vec<&ClientRequest> {
        self.requests_with_ids()
            .into_iter()
            .map(|r| &r.body)
            .collect()
    }

    fn requests_with_ids(&self) -> Vec<&Request> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                ClientCommand::Send(ClientMessage::Request(r)) => Some(r),
                _ => None,
            })
            .collect()
    }
}
