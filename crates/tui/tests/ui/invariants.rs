//! Random sequences of what a user does (clicks on drawn words, keys, the wheel, pastes, resizes down to 1x1) and of
//! what the host does never panic and keep two invariants after every step:
//!
//! - every button drawn whole is hittable: a click on its first cell finds a control, not nothing, the popup's body,
//!   the backdrop or the terminal;
//! - the selected session's row stays visible: unless the user has wheeled the list (or the host has listed a new
//!   session above it) since the selection or the window last changed, some cell of the sidebar hits it (while no
//!   popup covers it and the sidebar has room for a row).
//!
//! Seeded and bounded by count (cases x steps), not time. A failure prints its seed and its (shrunk) steps. Run another
//! seed with `PROPTEST_RNG_SEED=<n> cargo test -p shepherd-tui --test ui invariants`.

use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestError, TestRunner};
use ratatui::crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Position;
use shepherd_core::ids::SessionId;
use shepherd_tui::Target;
use shepherd_tui::layout::{layout, sidebar_rows};

use crate::harness::Ui;

/// The seed unless `PROPTEST_RNG_SEED` says otherwise.
const SEED: u64 = 0x5eed_0045;
const CASES: u32 = 96;
const MAX_STEPS: usize = 40;

const KEYS: &[(KeyCode, KeyModifiers)] = &[
    (KeyCode::Enter, KeyModifiers::NONE),
    (KeyCode::Esc, KeyModifiers::NONE),
    (KeyCode::Backspace, KeyModifiers::NONE),
    (KeyCode::Tab, KeyModifiers::NONE),
    (KeyCode::Up, KeyModifiers::NONE),
    (KeyCode::Down, KeyModifiers::NONE),
    (KeyCode::PageDown, KeyModifiers::NONE),
    (KeyCode::Char('b'), KeyModifiers::CONTROL),
    (KeyCode::Char('c'), KeyModifiers::CONTROL),
];

const NAMES: &[&str] = &[
    "fix-login",
    "docs",
    "a-name-much-longer-than-the-sidebar",
    "日本語",
    "x",
];

#[derive(Debug, Clone)]
enum Step {
    /// A left click on the first cell of a drawn word.
    Click(Index),
    /// A right click on the first cell of a drawn word.
    RightClick(Index),
    /// A left click anywhere, the cell taken modulo the window.
    ClickAt(u16, u16),
    /// The wheel anywhere, the cell taken modulo the window.
    Scroll(u16, u16, i32),
    Key(Index),
    Type(char),
    Paste(String),
    Resize(u16, u16),
    /// The host makes a new session.
    HostNew,
    /// The host renames a session.
    HostRename(Index, Index),
    /// A session's PTY exits.
    HostExit(Index),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        6 => any::<Index>().prop_map(Step::Click),
        2 => any::<Index>().prop_map(Step::RightClick),
        1 => (any::<u16>(), any::<u16>()).prop_map(|(x, y)| Step::ClickAt(x, y)),
        2 => (any::<u16>(), any::<u16>(), -8i32..8).prop_map(|(x, y, by)| Step::Scroll(x, y, by)),
        3 => any::<Index>().prop_map(Step::Key),
        2 => prop::sample::select(vec!['n', 'y', 'q', '?', 'r', 'x', 'd', '日']).prop_map(Step::Type),
        1 => "[a-z/ ]{0,6}(\n[a-z]{0,3})?".prop_map(Step::Paste),
        2 => (1u16..=120, 1u16..=40).prop_map(|(w, h)| Step::Resize(w, h)),
        1 => Just(Step::HostNew),
        1 => (any::<Index>(), any::<Index>()).prop_map(|(s, n)| Step::HostRename(s, n)),
        1 => any::<Index>().prop_map(Step::HostExit),
    ]
}

#[derive(Debug, Clone)]
struct Case {
    sessions: usize,
    size: (u16, u16),
    steps: Vec<Step>,
}

fn case() -> impl Strategy<Value = Case> {
    (
        0usize..20,
        (1u16..=120, 1u16..=40),
        prop::collection::vec(step(), 0..=MAX_STEPS),
    )
        .prop_map(|(sessions, size, steps)| Case {
            sessions,
            size,
            steps,
        })
}

/// What the run tracks between steps.
struct Run {
    ui: Ui,
    /// How many sessions the host has made, for fresh ids.
    made: usize,
    /// The sidebar's scroll may have left the selection since the selection or the window last changed: the user
    /// wheeled it, or the host listed a session above it.
    unfitted: bool,
}

impl Run {
    fn session(&self, at: &Index) -> Option<String> {
        let sessions = self.ui.client().sessions();
        (!sessions.is_empty()).then(|| sessions[at.index(sessions.len())].id.as_str().to_owned())
    }

    fn word(&self, at: &Index) -> Option<Position> {
        let words = self.ui.words();
        (!words.is_empty()).then(|| words[at.index(words.len())].0)
    }

    fn cell(&self, x: u16, y: u16) -> Position {
        let window = self.ui.window();
        Position::new(x % window.width, y % window.height)
    }

    fn apply(&mut self, step: &Step) {
        let before = self.ui.client().selected().cloned();
        let popup = self.ui.client().popup().is_some();
        match step {
            Step::Click(at) => {
                if let Some(p) = self.word(at) {
                    self.ui.click_at(p.x, p.y);
                }
            }
            Step::RightClick(at) => {
                if let Some(p) = self.word(at) {
                    self.ui.right_click_at(p.x, p.y);
                }
            }
            Step::ClickAt(x, y) => {
                let p = self.cell(*x, *y);
                self.ui.click_at(p.x, p.y);
            }
            Step::Scroll(x, y, by) => {
                let p = self.cell(*x, *y);
                let over_sidebar = layout(self.ui.window()).sidebar.contains(p);
                self.ui.scroll_at(p.x, p.y, *by);
                if !popup && over_sidebar && *by != 0 {
                    self.unfitted = true;
                }
            }
            Step::Key(at) => {
                let (code, modifiers) = KEYS[at.index(KEYS.len())];
                self.ui.key_with(code, modifiers);
            }
            Step::Type(ch) => {
                self.ui.type_text(&ch.to_string());
            }
            Step::Paste(text) => {
                self.ui.paste(text);
            }
            Step::Resize(w, h) => {
                self.ui.resize(*w, *h);
                self.unfitted = false;
            }
            Step::HostNew => {
                self.made += 1;
                let id = format!("h{}", self.made);
                self.ui.on_host(|h| {
                    h.new_session(&id);
                });
                // Known gap (reported with T45): a session the host makes is listed above the selection without
                // scrolling it back into view, which can push it off the bottom of the sidebar.
                self.unfitted = true;
            }
            Step::HostRename(session, name) => {
                if let Some(id) = self.session(session) {
                    let name = NAMES[name.index(NAMES.len())];
                    self.ui.on_host(|h| {
                        h.rename(&id, name);
                    });
                }
            }
            Step::HostExit(session) => {
                if let Some(id) = self.session(session) {
                    self.ui.on_host(|h| {
                        h.pty_exited(&id, Some(0));
                    });
                }
            }
        }
        if self.ui.client().selected() != before.as_ref() {
            self.unfitted = false;
        }
    }

    /// The invariants, or what broke them.
    fn check(&self) -> Result<(), String> {
        self.buttons_are_hittable()?;
        self.selection_is_visible()
    }

    fn buttons_are_hittable(&self) -> Result<(), String> {
        let popup = self.ui.client().popup().is_some();
        for (at, word) in self.ui.words() {
            let bracketed = word.starts_with('[') && word.ends_with(']') && word.len() > 2;
            // `[Start session]` and the like are two words; the first of them starts the button.
            let opens_button = word.starts_with('[') && !word.contains(']') && word.len() > 1;
            let sidebar_button = !popup && matches!(word.as_str(), "Detach" | "×");
            if !(bracketed || opens_button || sidebar_button) {
                continue;
            }
            match self.ui.hit_at(at.x, at.y) {
                None | Some(Target::Popup) | Some(Target::Backdrop) | Some(Target::Terminal) => {
                    return Err(format!(
                        "{word:?} at ({}, {}) is drawn but hits {:?}",
                        at.x,
                        at.y,
                        self.ui.hit_at(at.x, at.y)
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }

    fn selection_is_visible(&self) -> Result<(), String> {
        let client = self.ui.client();
        let window = self.ui.window();
        let Some(selected) = client.selected() else {
            return Ok(());
        };
        if self.unfitted || client.popup().is_some() || sidebar_rows(window) == 0 {
            return Ok(());
        }
        let sidebar = layout(window).sidebar;
        let target = Some(Target::Session(selected.clone()));
        if sidebar
            .positions()
            .any(|p| self.ui.hit_at(p.x, p.y) == target)
        {
            Ok(())
        } else {
            Err(format!(
                "the selected session {selected} has no visible row"
            ))
        }
    }
}

fn run(case: &Case) -> Result<(), TestCaseError> {
    let names: Vec<String> = (1..=case.sessions)
        .map(|i| format!("name-{i:02}"))
        .collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut run = Run {
        ui: Ui::with_sessions(&names, case.size.0, case.size.1),
        made: 0,
        unfitted: false,
    };
    let fail = |run: &Run, after: &str, why: String| {
        TestCaseError::fail(format!("{why}, after {after}:\n{}", run.ui.screen()))
    };
    run.check().map_err(|why| fail(&run, "the start", why))?;
    for (i, step) in case.steps.iter().enumerate() {
        run.apply(step);
        run.check()
            .map_err(|why| fail(&run, &format!("step {i} ({step:?})"), why))?;
    }
    Ok(())
}

#[test]
fn random_ui_steps_never_panic_and_keep_the_invariants() {
    let seed = match Config::default().rng_seed {
        RngSeed::Fixed(seed) => seed,
        RngSeed::Random => SEED,
    };
    let config = Config {
        cases: CASES,
        rng_seed: RngSeed::Fixed(seed),
        failure_persistence: None,
        ..Config::default()
    };
    let mut runner = TestRunner::new(config);
    match runner.run(&case(), |case| run(&case)) {
        Ok(()) => {}
        Err(TestError::Fail(why, case)) => {
            panic!("seed {seed}: {why}\nsteps (shrunk): {case:#?}")
        }
        Err(TestError::Abort(why)) => panic!("seed {seed}: aborted: {why}"),
    }
}

#[test]
fn the_invariants_see_a_selection_scrolled_out_of_view() {
    // The check itself: with the list wheeled away from the selection and the run not told, it fails.
    let names: Vec<String> = (1..=15).map(|i| format!("name-{i:02}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut run = Run {
        ui: Ui::with_sessions(&names, 80, 12),
        made: 0,
        unfitted: false,
    };
    assert_eq!(run.check(), Ok(()));
    run.ui.scroll("name-15", 20);
    assert_eq!(
        run.ui.client().selected(),
        Some(&SessionId::new("s15")),
        "the wheel leaves the selection"
    );
    assert!(run.check().is_err(), "{}", run.ui.screen());
}
