//! Scenario tests: user flows across a host and client runtime, wired with testkit fakes.
//! One scenario per user-visible feature. Edge cases belong in core tests.

mod support {
    pub mod fake_host;
}

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use shepherd::client::{ClientError, ClientRuntime, Connector};
use shepherd_core::acpp::Timestamp;
use shepherd_core::client::{
    Connection, HELLO_TIMEOUT, Intent, MAX_CONNECT_ATTEMPTS, RETRY_INTERVAL,
};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::LaunchError;
use shepherd_term::Screen;
use shepherd_testkit::fakes::launcher::FakeHostLauncher;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};
use support::fake_host::FakeHost;

/// A host slot the fake launcher fills.
type HostSlot = Arc<Mutex<Option<FakeHost>>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A client runtime on a fake network whose launcher starts a [`FakeHost`], with a clock the test moves.
struct Harness {
    net: FakeNetwork,
    host: HostSlot,
    launcher: FakeHostLauncher,
    now: Arc<Mutex<jiff::Timestamp>>,
    runtime: ClientRuntime<FakeLink, Connector<FakeLink>, FakeHostLauncher>,
}

impl Harness {
    fn new() -> Self {
        Self::with_launcher(|net, host| {
            FakeHostLauncher::new().on_launch(move || {
                *lock(&host) = Some(FakeHost::start(&net));
            })
        })
    }

    fn with_launcher(make: impl FnOnce(FakeNetwork, HostSlot) -> FakeHostLauncher) -> Self {
        let net = FakeNetwork::new();
        let host: HostSlot = Arc::default();
        let launcher = make(net.clone(), host.clone());
        let connect_net = net.clone();
        let now = Arc::new(Mutex::new(
            "2026-10-29T12:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap_or_else(|e| panic!("{e}")),
        ));
        let clock = {
            let now = now.clone();
            move || Timestamp::parse(lock(&now).to_string()).unwrap_or_else(|e| panic!("{e}"))
        };
        let connect: Connector<FakeLink> =
            Box::new(move |sink| connect_net.connect(sink).map_err(|e| e.to_string()));
        let runtime =
            ClientRuntime::new(MachineId::new("m_client"), connect, launcher.clone(), clock);
        Self {
            net,
            host,
            launcher,
            now,
            runtime,
        }
    }

    /// Pumps host and client until neither has anything left to do.
    fn settle(&mut self) {
        loop {
            let host = lock(&self.host).as_mut().map_or(0, FakeHost::pump);
            if host + self.runtime.pump() == 0 {
                return;
            }
        }
    }

    /// Moves the clock past the retry interval and ticks.
    fn wait_retry(&mut self) {
        self.advance(RETRY_INTERVAL);
        self.runtime.tick();
        self.settle();
    }

    fn advance(&mut self, by: jiff::SignedDuration) {
        let mut now = lock(&self.now);
        *now = now.checked_add(by).unwrap_or_else(|e| panic!("{e}"));
    }

    fn user(&mut self, intent: Intent) {
        self.runtime.user(intent);
        self.settle();
    }

    fn selected(&self) -> Option<String> {
        self.runtime.client().selected().map(|s| s.to_string())
    }

    fn screen(&self, session: &str) -> String {
        self.runtime
            .screen(&SessionId::new(session))
            .map(first_line)
            .unwrap_or_default()
    }

    fn inputs(&self) -> Vec<(String, String)> {
        lock(&self.host)
            .as_ref()
            .map(|h| {
                h.inputs()
                    .iter()
                    .map(|(s, b)| (s.to_string(), String::from_utf8_lossy(b).into_owned()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn first_line(screen: &Screen) -> String {
    screen
        .row(0)
        .map(|c| c.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

#[test]
fn two_sessions_switching_between_them_routes_input_to_the_selected_one() {
    let mut h = Harness::new();
    // No host is running: the first connect fails, and the client launches one.
    h.runtime.start();
    h.settle();
    assert_eq!(h.launcher.launches(), 1);
    h.wait_retry();
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);

    h.user(Intent::NewSession);
    h.user(Intent::NewSession);
    assert_eq!(h.runtime.client().sessions().len(), 2);
    assert_eq!(
        h.selected().as_deref(),
        Some("s2"),
        "a new session is selected"
    );

    h.user(Intent::Input(b"two".to_vec()));
    h.user(Intent::NextSession);
    assert_eq!(h.selected().as_deref(), Some("s1"));
    h.user(Intent::Input(b"one".to_vec()));

    assert_eq!(
        h.inputs(),
        [("s2".into(), "two".into()), ("s1".into(), "one".into())]
    );
    // Each session's echo lands on its own screen.
    assert_eq!(
        (h.screen("s1"), h.screen("s2")),
        ("one".into(), "two".into())
    );
}

#[test]
fn a_host_that_cannot_be_started_is_a_clear_error() {
    let mut h = Harness::with_launcher(|_, _| {
        FakeHostLauncher::new().failing(LaunchError::SpawnFailed("no such file".into()))
    });
    let Err(ClientError::Fatal(why)) = h.runtime.run() else {
        panic!("run should fail");
    };
    assert!(why.contains("could not start the shepherd host"), "{why}");
    assert!(why.contains("no such file"), "{why}");
}

#[test]
fn a_host_that_never_listens_is_given_up_on_after_the_last_attempt() {
    // The launch "succeeds" but no host ever listens.
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    h.runtime.start();
    h.settle();
    for _ in 1..MAX_CONNECT_ATTEMPTS {
        assert!(!h.runtime.done());
        h.wait_retry();
    }
    assert!(h.runtime.done());
    assert_eq!(h.launcher.launches(), 1);
    let why = h.runtime.client().fatal().unwrap_or_default();
    assert!(
        why.contains("could not connect to the shepherd host"),
        "{why}"
    );
}

#[test]
fn a_host_slow_to_answer_hello_is_retried_and_the_old_links_events_are_ignored() {
    let mut h = Harness::with_launcher(|_, _| FakeHostLauncher::new());
    *lock(&h.host) = Some(FakeHost::start(&h.net));
    // Connect, but the host doesn't get to answer the hello before it times out.
    h.runtime.start();
    h.runtime.pump();
    h.advance(HELLO_TIMEOUT);
    h.runtime.tick();
    assert!(matches!(
        h.runtime.client().connection(),
        Connection::Waiting { .. }
    ));
    // The retry replaces the first link. Dropping it queues that link's `Disconnected` behind the new
    // connection, where it must not be taken for the new link going away.
    h.advance(RETRY_INTERVAL);
    h.runtime.tick();
    h.settle();
    assert_eq!(h.runtime.client().connection(), &Connection::Ready);
    h.user(Intent::NewSession);
    h.user(Intent::Input(b"hi".to_vec()));
    assert_eq!(h.inputs(), [("s1".into(), "hi".into())]);
}
