//! A scripted host on a [`FakeNetwork`], standing in for the host runtime (T11) in client scenarios. Its state is
//! a real `core::host` driven through [`HostDriver`]; its "PTYs" echo what they are sent.
//!
//! It handles events only when pumped, from the test's thread, so a scenario is deterministic.

use std::sync::Arc;
use std::sync::mpsc::Receiver;

use serde_json::json;
use shepherd_core::acpp::{Source, SourceKind};
use shepherd_core::config::AgentKind;
use shepherd_core::host::HostCommand;
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::ports::{ConnId, Link, LinkEvent};
use shepherd_core::wire::{
    BuildId, ClientMessage, ClientNotification, ClientRequest, ExeDigest, HelloOk,
    HostNotification, HostResponse, HostResult, LogEntry, Notify, ProfileInfo, PtyOutput, Request,
    SessionNew, SessionResize, UpgradeAnswer, decode_client_message, encode_host_notification,
    encode_response, negotiate,
};
use shepherd_testkit::builders::{HostDriver, claude_resume_spec};
use shepherd_testkit::contracts::link::ChannelSink;
use shepherd_testkit::fakes::link::{FakeLink, FakeNetwork};

pub struct FakeHost {
    link: FakeLink,
    events: Receiver<LinkEvent>,
    driver: HostDriver,
    subscribers: Vec<ConnId>,
    sessions: u64,
    /// Every input the host was sent, in order.
    inputs: Vec<(SessionId, Vec<u8>)>,
    /// Every resize the host was sent, in order.
    resizes: Vec<SessionResize>,
    /// What its hello answer says about `[notify] bell`.
    bell: bool,
    /// The `cwd` of every hello it was sent, in order.
    hello_cwds: Vec<Option<String>>,
    /// The profiles and default its hello answer offers. None by default, like a host older than profiles.
    profiles: Vec<ProfileInfo>,
    default_profile: Option<String>,
    /// Every session/new it was sent, in order.
    session_news: Vec<SessionNew>,
    /// The build its hello answer says it runs, and what it says is installed at its path. None by default, like a
    /// host older than builds.
    build: Option<BuildId>,
    installed: Option<ExeDigest>,
    /// How it answers `_shepherd/host/upgrade`, in order; once they run out, like a host older than updating in
    /// place (method not found).
    upgrade_answers: std::collections::VecDeque<UpgradeAnswer>,
    /// How many upgrade requests it was sent.
    upgrades: usize,
}

impl FakeHost {
    /// Starts listening on `net`. Panics if a host already is.
    pub fn start(net: &FakeNetwork) -> Self {
        let (tx, events) = std::sync::mpsc::channel();
        let link = net
            .listen(Arc::new(ChannelSink(tx)))
            .unwrap_or_else(|e| panic!("FakeHost: {e}"));
        Self {
            link,
            events,
            driver: HostDriver::new(),
            subscribers: Vec::new(),
            sessions: 0,
            inputs: Vec::new(),
            resizes: Vec::new(),
            bell: false,
            hello_cwds: Vec::new(),
            profiles: Vec::new(),
            default_profile: None,
            session_news: Vec::new(),
            build: None,
            installed: None,
            upgrade_answers: std::collections::VecDeque::new(),
            upgrades: 0,
        }
    }

    /// One that answers its next upgrade requests with `answers`, in order.
    pub fn with_upgrade_answers(self, answers: impl IntoIterator<Item = UpgradeAnswer>) -> Self {
        Self {
            upgrade_answers: answers.into_iter().collect(),
            ..self
        }
    }

    /// How many `_shepherd/host/upgrade` requests it was sent.
    pub fn upgrades(&self) -> usize {
        self.upgrades
    }

    /// One whose hello answer says it runs `build`, with `installed` at its path.
    pub fn with_build(self, build: BuildId, installed: ExeDigest) -> Self {
        Self {
            build: Some(build),
            installed: Some(installed),
            ..self
        }
    }

    /// One whose hello answer offers `profiles`, the first the default.
    pub fn with_profiles(self, profiles: &[(&str, AgentKind)]) -> Self {
        Self {
            default_profile: profiles.first().map(|(name, _)| (*name).to_owned()),
            profiles: profiles
                .iter()
                .map(|(name, agent)| ProfileInfo {
                    name: (*name).to_owned(),
                    agent: *agent,
                })
                .collect(),
            ..self
        }
    }

    /// Every session/new it was sent, in order.
    pub fn session_news(&self) -> &[SessionNew] {
        &self.session_news
    }

    /// One whose config has `[notify] bell = true`.
    pub fn with_bell(self) -> Self {
        Self { bell: true, ..self }
    }

    /// The agent in `session` goes idle: its hook's frame is logged and sent to subscribers.
    pub fn agent_idle(&mut self, session: &str) {
        let hook = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
        let frame = json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/idle", "params": {} });
        let envelope = self.driver.append(Some(session), hook, frame);
        self.broadcast(&HostNotification::LogEntry(Box::new(LogEntry { envelope })));
    }

    pub fn inputs(&self) -> &[(SessionId, Vec<u8>)] {
        &self.inputs
    }

    pub fn resizes(&self) -> &[SessionResize] {
        &self.resizes
    }

    /// The `cwd` of every hello it was sent, in order.
    pub fn hello_cwds(&self) -> &[Option<String>] {
        &self.hello_cwds
    }

    /// The open sessions' ids.
    pub fn sessions(&self) -> Vec<SessionId> {
        self.driver
            .host()
            .sessions()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    /// Handles every event queued so far. Returns how many there were.
    pub fn pump(&mut self) -> usize {
        let mut handled = 0;
        while let Ok(event) = self.events.try_recv() {
            handled += 1;
            match event {
                LinkEvent::Connected { .. } => {}
                LinkEvent::Line { conn, line } => self.line(conn, &line),
                LinkEvent::Disconnected { conn, .. } => self.subscribers.retain(|c| *c != conn),
            }
        }
        handled
    }

    fn line(&mut self, conn: ConnId, line: &[u8]) {
        match decode_client_message(line) {
            Ok(ClientMessage::Request(request)) => self.request(conn, request),
            Ok(ClientMessage::Notification(ClientNotification::SessionInput(input))) => {
                let echo = PtyOutput {
                    session: input.session.clone(),
                    bytes: input.data.clone(),
                };
                self.inputs.push((input.session, input.data));
                self.broadcast(&HostNotification::PtyOutput(echo));
            }
            Ok(ClientMessage::Notification(ClientNotification::SessionResize(resize))) => {
                self.resizes.push(resize);
            }
            Err(e) => {
                if let Some(response) = e.into_response() {
                    self.send(conn, &encode_response(&response));
                }
            }
        }
    }

    fn request(&mut self, conn: ConnId, Request { id, body }: Request) {
        let before = self.driver.log().len();
        let result = match body {
            ClientRequest::Hello(hello) => {
                self.hello_cwds.push(hello.cwd.clone());
                negotiate(&hello, &MachineId::new("m_test")).map(|ok| {
                    HostResult::Hello(HelloOk {
                        notify: Notify { bell: self.bell },
                        profiles: self.profiles.clone(),
                        default_profile: self.default_profile.clone(),
                        build: self.build.clone(),
                        installed: self.installed,
                        ..ok
                    })
                })
            }
            ClientRequest::Subscribe(sub) => {
                for env in self.driver.log().to_vec() {
                    let from = match env.session() {
                        None => sub.from.host,
                        Some(s) => sub.from.sessions.get(s).copied(),
                    };
                    if from.is_none_or(|from| env.seq() > from) {
                        let entry =
                            HostNotification::LogEntry(Box::new(LogEntry { envelope: env }));
                        self.send(conn, &encode_host_notification(&entry));
                    }
                }
                self.subscribers.push(conn);
                Ok(HostResult::Ack)
            }
            ClientRequest::SessionNew(new) => {
                self.session_news.push(new);
                self.sessions += 1;
                let id = format!("s{}", self.sessions);
                reply_of(self.driver.new_session(&id))
            }
            ClientRequest::SessionRename(r) => {
                reply_of(self.driver.rename(r.session.as_str(), &r.name))
            }
            ClientRequest::SessionClose(c) => reply_of(self.driver.close_with(
                c.session.as_str(),
                c.worktree,
                shepherd_core::config::WorkspaceHooks::default(),
            )),
            ClientRequest::HookFrame(_) => Ok(HostResult::Ack),
            ClientRequest::SessionResume(r) => {
                reply_of(self.driver.resume(r.session.as_str(), claude_resume_spec()))
            }
            ClientRequest::WorkspaceSelect(w) => reply_of(self.driver.select_workspace(&w.path)),
            // The scripted host makes no worktrees: any it is asked about is gone.
            ClientRequest::WorktreeStatus(_) => Ok(HostResult::WorktreeStatus(
                shepherd_core::wire::WorktreeStatusOk::default(),
            )),
            ClientRequest::HostUpgrade(_) => {
                self.upgrades += 1;
                match self.upgrade_answers.pop_front() {
                    Some(answer) => Ok(HostResult::Upgrade(answer)),
                    None => Err(shepherd_core::wire::WireError::MethodNotFound(
                        "_shepherd/host/upgrade".into(),
                    )),
                }
            }
        };
        let fresh = self.driver.log()[before..].to_vec();
        for env in fresh {
            let entry = HostNotification::LogEntry(Box::new(LogEntry { envelope: env }));
            self.broadcast(&entry);
        }
        self.send(conn, &encode_response(&HostResponse { id, result }));
    }

    fn broadcast(&mut self, note: &HostNotification) {
        let line = encode_host_notification(note);
        for conn in self.subscribers.clone() {
            self.send(conn, &line);
        }
    }

    fn send(&mut self, conn: ConnId, line: &[u8]) {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let _ = self.link.send(conn, line);
    }
}

fn reply_of(commands: Vec<HostCommand>) -> Result<HostResult, shepherd_core::wire::WireError> {
    commands
        .into_iter()
        .find_map(|c| match c {
            HostCommand::Reply { result, .. } => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("FakeHost: the host didn't reply"))
}
