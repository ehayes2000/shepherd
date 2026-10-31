//! Test support shared by every crate: fakes, contract suites, and builders.
//!
//! Depend on this crate only under `[dev-dependencies]`.
//!
//! `shepherd-core` may use it only from integration tests in `crates/core/tests/`, never from `#[cfg(test)]` unit
//! tests: a core unit test would link two copies of `shepherd-core`, and their types would not match.

/// Builders for domain values (`AppBuilder`, `SessionBuilder`, ...), so tests don't construct large structs inline.
pub mod builders {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use serde_json::{Value, json};
    use shepherd_core::acpp::{
        Dir, EntryId, Envelope, Frame, FrameDraft, Seq, Source, SourceKind, Stamp, Timestamp,
    };
    use shepherd_core::client::{Client, ClientCommand, ClientEvent, Intent};
    use shepherd_core::config::{AgentKind, WorkspaceHooks};
    use shepherd_core::host::sizes::Sizes;
    use shepherd_core::host::{
        Host, HostCommand, HostEvent, NewSessionSpec, PostCreate, ReplyTo, ResumeSpec, WorkspaceAsk,
    };
    use shepherd_core::ids::{MachineId, SessionId};
    use shepherd_core::ports::commands::{CommandExit, CommandOutput, CommandSpec};
    use shepherd_core::ports::pty::{PtySize, SpawnSpec};
    use shepherd_core::ports::worktrees::{
        BaseUsed, BranchName, Created, WorktreeError, WorktreePath,
    };
    use shepherd_core::wire::{
        ClientMessage, ClientNotification, CloseWorktree, HostMessage, HostNotification, LogEntry,
        PROTOCOL_VERSION, RawResponse, Request, WireError,
    };

    /// Seals frames into envelopes for one session log, as the host's append path would: `seq` starts at 1 and
    /// increments with every envelope built.
    ///
    /// ```text
    /// let mut log = EnvelopeBuilder::new();
    /// let env = log.to_agent(json!({ "jsonrpc": "2.0", "id": 1, "method": "session/prompt", "params": { .. } }));
    /// ```
    #[derive(Debug, Clone)]
    pub struct EnvelopeBuilder {
        next_seq: u64,
        session: SessionId,
        source: Source,
    }

    impl Default for EnvelopeBuilder {
        fn default() -> Self {
            Self::new()
        }
    }

    impl EnvelopeBuilder {
        /// A log for session `s_test`, with frames from a `hook` source for `claude-code`.
        pub fn new() -> Self {
            Self {
                next_seq: 1,
                session: SessionId::new("s_test"),
                source: Source::new(SourceKind::Hook, Some("claude-code".to_owned())),
            }
        }

        pub fn session(mut self, session: SessionId) -> Self {
            self.session = session;
            self
        }

        pub fn source(mut self, source: Source) -> Self {
            self.source = source;
            self
        }

        /// The seq the next envelope will get.
        pub fn next_seq(&self) -> Seq {
            Seq::new(self.next_seq)
        }

        /// Seals `frame` (JSON-RPC 2.0 JSON) with direction `dir` at the next seq. Panics on a malformed frame.
        pub fn envelope(&mut self, dir: Dir, frame: Value) -> Envelope {
            let frame = Frame::from_value(frame)
                .unwrap_or_else(|e| panic!("EnvelopeBuilder: malformed frame: {e}"));
            let seq = self.next_seq;
            self.next_seq += 1;
            let ts = Timestamp::parse("2026-10-29T12:00:00.000Z")
                .unwrap_or_else(|e| panic!("EnvelopeBuilder: bad timestamp: {e}"));
            let stamp = Stamp {
                id: EntryId::new(format!("e{seq}")),
                ts,
                machine: MachineId::new("m_test"),
            };
            FrameDraft::new(frame, self.source.clone(), dir, Some(self.session.clone()))
                .seal(Seq::new(seq), stamp)
        }

        pub fn to_agent(&mut self, frame: Value) -> Envelope {
            self.envelope(Dir::ToAgent, frame)
        }

        pub fn from_agent(&mut self, frame: Value) -> Envelope {
            self.envelope(Dir::FromAgent, frame)
        }

        pub fn internal(&mut self, frame: Value) -> Envelope {
            self.envelope(Dir::Internal, frame)
        }
    }

    /// Runs a [`Host`] the way the runtime does: each event goes through its [`Sizes`], and each [`HostCommand::Append`]
    /// that `handle` returns is sealed at its log's next seq and passed to `apply` before the next event. Every other
    /// command is only returned. Each entry's `ts` is 1 ms after the previous one's, unless pinned with
    /// [`HostDriver::pin_ts`].
    ///
    /// ```text
    /// let mut host = HostDriver::new();
    /// host.new_session("s1");
    /// host.rename("s1", "api");
    /// assert_eq!(host.host().sessions()[0].name, "api");
    /// ```
    #[derive(Debug, Clone, Default)]
    pub struct HostDriver {
        host: Host,
        sizes: Sizes,
        log: Vec<Envelope>,
        /// The last seq of each log: the host log (`None`) and one per session.
        seqs: BTreeMap<Option<SessionId>, u64>,
        next_reply: u64,
        pinned_ts: Option<Timestamp>,
        /// The machine later entries are stamped with, when not `m_test`.
        machine: Option<MachineId>,
    }

    impl HostDriver {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn host(&self) -> &Host {
            &self.host
        }

        /// Every envelope appended so far, in append order.
        pub fn log(&self) -> &[Envelope] {
            &self.log
        }

        /// Stamps every later entry with `ts` instead of the advancing clock, until [`HostDriver::unpin_ts`].
        /// Panics on a malformed timestamp.
        pub fn pin_ts(&mut self, ts: &str) {
            let ts = Timestamp::parse(ts).unwrap_or_else(|e| panic!("HostDriver: {e}"));
            self.pinned_ts = Some(ts);
        }

        pub fn unpin_ts(&mut self) {
            self.pinned_ts = None;
        }

        /// Stamps every later entry with machine `machine` instead of `m_test`, e.g. `local`, as hosts did before machines
        /// had ids.
        pub fn stamp_machine(&mut self, machine: &str) {
            self.machine = Some(MachineId::new(machine));
        }

        /// The `ts` the next entry gets: the pinned one, else `2026-10-29T12:00:00.000Z` plus 1 ms per entry
        /// appended so far.
        pub fn now(&self) -> Timestamp {
            self.pinned_ts
                .clone()
                .unwrap_or_else(|| ts_at(self.log.len()))
        }

        /// A fresh token for a request event.
        pub fn reply_to(&mut self) -> ReplyTo {
            self.next_reply += 1;
            ReplyTo::new(self.next_reply)
        }

        /// Handles `event`, appends and applies its `Append`s, and returns all its commands.
        pub fn handle(&mut self, event: HostEvent) -> Vec<HostCommand> {
            let commands = self.sizes.handle(&self.host, event);
            for command in &commands {
                if let HostCommand::Append(draft) = command {
                    self.seal((**draft).clone());
                }
            }
            commands
        }

        /// Seals and applies a frame from outside the host (a hook, an agent) at its log's next seq, as the host's
        /// append path would. Panics on a malformed frame.
        pub fn append(&mut self, session: Option<&str>, source: Source, frame: Value) -> Envelope {
            let frame = Frame::from_value(frame)
                .unwrap_or_else(|e| panic!("HostDriver: malformed frame: {e}"));
            let draft = FrameDraft::new(frame, source, Dir::Internal, session.map(SessionId::new));
            self.seal(draft)
        }

        /// Seals and applies `draft`, e.g. one a watch beside the host decided on (`quiet`, `pr`), as the runtime's
        /// append path would.
        pub fn append_draft(&mut self, draft: FrameDraft) -> Envelope {
            self.seal(draft)
        }

        fn seal(&mut self, draft: FrameDraft) -> Envelope {
            let stamp = Stamp {
                id: EntryId::new(format!("e{}", self.log.len() + 1)),
                ts: self.now(),
                machine: self
                    .machine
                    .clone()
                    .unwrap_or_else(|| MachineId::new("m_test")),
            };
            let seq = self.seqs.entry(draft.session.clone()).or_insert(0);
            *seq += 1;
            let env = draft.seal(Seq::new(*seq), stamp);
            self.host.apply(&env);
            self.log.push(env.clone());
            env
        }

        /// A `NewSession` for `id`, with [`shell_session_spec`].
        pub fn new_session(&mut self, id: &str) -> Vec<HostCommand> {
            self.new_session_with(id, shell_session_spec())
        }

        /// A `NewSession` for `id`, with `spec`.
        pub fn new_session_with(&mut self, id: &str, spec: NewSessionSpec) -> Vec<HostCommand> {
            let reply = self.reply_to();
            self.handle(HostEvent::NewSession {
                reply,
                id: SessionId::new(id),
                at: self.now(),
                spec,
            })
        }

        /// Appends a hook-sourced `_shepherd/agent/started` from `agent` to session `id`'s log, as the hook translators
        /// do on `SessionStart`.
        pub fn agent_started(
            &mut self,
            id: &str,
            agent: &str,
            agent_session: Option<&str>,
        ) -> Envelope {
            let mut params = json!({ "agent": agent });
            if let Some(agent_session) = agent_session {
                params["agent_session_id"] = json!(agent_session);
            }
            let frame =
                json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/started", "params": params });
            let source = Source::new(SourceKind::Hook, Some(agent.to_owned()));
            self.append(Some(id), source, frame)
        }

        /// Like [`Self::agent_started`], naming the agent session and the transcript path its hook payload gave.
        pub fn agent_started_with_transcript(
            &mut self,
            id: &str,
            agent: &str,
            agent_session: &str,
            transcript_path: &str,
        ) -> Envelope {
            let params = json!({
                "agent": agent,
                "agent_session_id": agent_session,
                "transcript_path": transcript_path,
            });
            let frame =
                json!({ "jsonrpc": "2.0", "method": "_shepherd/agent/started", "params": params });
            let source = Source::new(SourceKind::Hook, Some(agent.to_owned()));
            self.append(Some(id), source, frame)
        }

        /// A `Resume` of `id`, with `spec`.
        pub fn resume(&mut self, id: &str, spec: ResumeSpec) -> Vec<HostCommand> {
            let reply = self.reply_to();
            self.handle(HostEvent::Resume {
                reply,
                session: SessionId::new(id),
                spec,
            })
        }

        pub fn rename(&mut self, id: &str, name: &str) -> Vec<HostCommand> {
            let reply = self.reply_to();
            self.handle(HostEvent::Rename {
                reply,
                session: SessionId::new(id),
                name: name.to_owned(),
            })
        }

        /// A `Close` of `id` that removes its worktree if that loses nothing, in a workspace with no hooks.
        pub fn close(&mut self, id: &str) -> Vec<HostCommand> {
            self.close_with(id, CloseWorktree::IfSafe, WorkspaceHooks::default())
        }

        /// A `Close` of `id` doing `worktree` with its worktree, its workspace's hooks being `hooks`.
        pub fn close_with(
            &mut self,
            id: &str,
            worktree: CloseWorktree,
            hooks: WorkspaceHooks,
        ) -> Vec<HostCommand> {
            let reply = self.reply_to();
            self.handle(HostEvent::Close {
                reply,
                session: SessionId::new(id),
                worktree,
                hooks,
            })
        }

        /// A user's select of workspace `path`, its probe answering that `path` is a repo. Returns the commands of
        /// the probe's answer.
        pub fn select_workspace(&mut self, path: &str) -> Vec<HostCommand> {
            let ask = WorkspaceAsk::Select(self.reply_to());
            self.handle(HostEvent::SelectWorkspace {
                ask,
                path: PathBuf::from(path),
            });
            self.handle(HostEvent::WorkspaceProbed {
                ask,
                repo: Ok(PathBuf::from(path)),
            })
        }

        /// The worktree result for session `id`, as the runtime feeds it back, with [`shell_spawn`] as its spawn.
        pub fn worktree_result(
            &mut self,
            id: &str,
            result: Result<Created, WorktreeError>,
        ) -> Vec<HostCommand> {
            self.handle(HostEvent::WorktreeCreated {
                session: SessionId::new(id),
                spawn: shell_spawn(),
                hooks: PostCreate::default(),
                result,
            })
        }

        /// A worktree made for session `id` at `/wt/<id>`, on branch `<id>` from `origin/main`.
        pub fn worktree_ready(&mut self, id: &str) -> Vec<HostCommand> {
            self.worktree_result(
                id,
                Ok(Created {
                    path: WorktreePath::new(format!("/wt/{id}")),
                    branch: BranchName::new(id),
                    base: BaseUsed::Origin {
                        branch: BranchName::new("main"),
                    },
                }),
            )
        }

        /// Like [`HostDriver::worktree_ready`], for a session whose workspace has `hooks` as its post-create hooks:
        /// the first of them runs next.
        pub fn worktree_ready_with_hooks(&mut self, id: &str, hooks: &[&str]) -> Vec<HostCommand> {
            self.handle(HostEvent::WorktreeCreated {
                session: SessionId::new(id),
                spawn: shell_spawn(),
                hooks: PostCreate {
                    hooks: hooks.iter().map(|h| (*h).to_owned()).collect(),
                    ..PostCreate::default()
                },
                result: Ok(Created {
                    path: WorktreePath::new(format!("/wt/{id}")),
                    branch: BranchName::new(id),
                    base: BaseUsed::Origin {
                        branch: BranchName::new("main"),
                    },
                }),
            })
        }

        /// Session `id`'s post-create hook `script`, its last, exited with `code` and no output.
        pub fn hook_exited(&mut self, id: &str, script: &str, code: i32) -> Vec<HostCommand> {
            let cwd = PathBuf::from(format!("/wt/{id}"));
            self.handle(HostEvent::HookFinished {
                session: SessionId::new(id),
                command: CommandSpec {
                    script: script.to_owned(),
                    cwd: cwd.clone(),
                    env: Vec::new(),
                    timeout: PostCreate::default().timeout,
                },
                rest: PostCreate::default(),
                spawn: SpawnSpec {
                    cwd,
                    ..shell_spawn()
                },
                result: Ok(CommandOutput {
                    exit: CommandExit::Code(code),
                    tail: Vec::new(),
                }),
            })
        }

        /// Session `id`'s PTY was carried over from the previous image at `size`, as the runtime records on adopt.
        pub fn adopted(&mut self, id: &str, size: PtySize) {
            self.sizes.adopted(SessionId::new(id), size);
        }

        /// A client's resize of session `id`'s terminal to `cols`x`rows`.
        pub fn resize(&mut self, id: &str, cols: u16, rows: u16) -> Vec<HostCommand> {
            self.handle(HostEvent::Resize {
                session: SessionId::new(id),
                size: PtySize { cols, rows },
            })
        }

        pub fn pty_exited(&mut self, id: &str, code: Option<i32>) -> Vec<HostCommand> {
            self.handle(HostEvent::PtyExited {
                session: SessionId::new(id),
                code,
            })
        }

        /// Appends a hook-sourced `session/prompt` with `text` to session `id`'s log, as the Claude Code translator
        /// does on `UserPromptSubmit`, with `_meta.shepherd.continuation` set if `continuation`. Only appends: see
        /// [`HostDriver::user_prompt`] for the whole hook.
        pub fn prompt_frame(&mut self, id: &str, text: &str, continuation: bool) -> Envelope {
            let mut params = json!({
                "sessionId": "c1",
                "prompt": [{ "type": "text", "text": text }],
            });
            if continuation {
                params["_meta"] = json!({ "shepherd": { "continuation": true } });
            }
            let frame = json!({
                "jsonrpc": "2.0",
                "id": format!("p{}", self.log.len() + 1),
                "method": "session/prompt",
                "params": params,
            });
            let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
            self.append(Some(id), source, frame)
        }

        /// A user prompt hook, as the runtime runs it: the translated `session/prompt` is appended, then
        /// `HookTranslated` is handled. Returns that event's commands.
        pub fn user_prompt(&mut self, id: &str, text: &str) -> Vec<HostCommand> {
            self.prompt_frame(id, text, false);
            self.hook_translated(id)
        }

        /// A `HookTranslated` for `id`: the runtime has appended a hook's frames.
        pub fn hook_translated(&mut self, id: &str) -> Vec<HostCommand> {
            self.handle(HostEvent::HookTranslated {
                session: SessionId::new(id),
            })
        }
    }

    /// Drives a [`Client`] against a [`HostDriver`]: sessions are made on the host, and [`ClientDriver::sync`] delivers
    /// the host's new log entries to the client as `_shepherd/log/entry` notifications.
    ///
    /// ```text
    /// let mut c = ClientDriver::with_sessions(&["s1", "s2"]);
    /// c.user(Intent::NextSession);
    /// assert_eq!(c.client().selected(), Some(&SessionId::new("s1")));
    /// ```
    #[derive(Debug, Clone)]
    pub struct ClientDriver {
        client: Client,
        host: HostDriver,
        /// How many of the host's log entries the client has been sent.
        delivered: usize,
    }

    impl Default for ClientDriver {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ClientDriver {
        /// A client for machine `m_client` that hasn't started connecting.
        pub fn new() -> Self {
            Self::from_client(Client::new(MachineId::new("m_client")))
        }

        /// Drives `client`, which hasn't started connecting.
        pub fn from_client(client: Client) -> Self {
            Self {
                client,
                host: HostDriver::new(),
                delivered: 0,
            }
        }

        /// A connected client that has been sent sessions `ids`, created in that order (so the last is newest).
        pub fn with_sessions(ids: &[&str]) -> Self {
            let mut driver = Self::new();
            driver.connect();
            for id in ids {
                driver.host.new_session(id);
            }
            driver.sync();
            driver
        }

        pub fn client(&self) -> &Client {
            &self.client
        }

        pub fn host(&mut self) -> &mut HostDriver {
            &mut self.host
        }

        pub fn handle(&mut self, event: ClientEvent) -> Vec<ClientCommand> {
            self.client.handle(event)
        }

        pub fn user(&mut self, intent: Intent) -> Vec<ClientCommand> {
            self.client.handle(ClientEvent::User(intent))
        }

        /// Starts the client, connects it, and answers its hello. Returns the commands the answer produced (the
        /// subscribe). Panics if the client doesn't follow that script.
        pub fn connect(&mut self) -> Vec<ClientCommand> {
            let answer = json!({ "version": PROTOCOL_VERSION, "machine": "m_test" });
            self.connect_answering(answer)
        }

        /// Like [`ClientDriver::connect`], with a host whose machine is called `name`.
        pub fn connect_named(&mut self, name: &str) -> Vec<ClientCommand> {
            let answer = json!({ "version": PROTOCOL_VERSION, "machine": "m_test", "name": name });
            self.connect_answering(answer)
        }

        /// Like [`ClientDriver::connect`], with a host whose config has `profiles` (name, agent), `default` the
        /// default one.
        pub fn connect_with_profiles(
            &mut self,
            profiles: &[(&str, AgentKind)],
            default: &str,
        ) -> Vec<ClientCommand> {
            let profiles: Vec<Value> = profiles
                .iter()
                .map(|(name, agent)| json!({ "name": name, "agent": agent }))
                .collect();
            let answer = json!({
                "version": PROTOCOL_VERSION,
                "machine": "m_test",
                "profiles": profiles,
                "default_profile": default,
            });
            self.connect_answering(answer)
        }

        /// Like [`ClientDriver::connect`], with a host whose config has `[notify] bell = bell`, and the subscribe
        /// answered, as the host does once the catch-up is sent: the client is live.
        pub fn connect_live_with_bell(&mut self, bell: bool) -> Vec<ClientCommand> {
            let answer = json!({
                "version": PROTOCOL_VERSION,
                "machine": "m_test",
                "notify": { "bell": bell },
            });
            let subscribe = self.connect_answering(answer);
            self.answer(&subscribe, Ok(json!({})))
        }

        fn connect_answering(&mut self, answer: Value) -> Vec<ClientCommand> {
            let started = self.handle(ClientEvent::Start);
            assert_eq!(started, vec![ClientCommand::Connect], "ClientDriver: start");
            let hello = self.handle(ClientEvent::Connected {
                at: ts("2026-10-29T12:00:00.000Z"),
            });
            self.answer(&hello, Ok(answer))
        }

        /// Sends the client every host-log entry it hasn't been sent yet, in append order.
        pub fn sync(&mut self) -> Vec<ClientCommand> {
            let fresh: Vec<Envelope> = self.host.log()[self.delivered..].to_vec();
            self.delivered = self.host.log().len();
            let mut commands = Vec::new();
            for env in fresh {
                commands.extend(self.client.handle(log_entry(env)));
            }
            commands
        }

        /// Answers the one request in `commands` with `result`. Panics unless there is exactly one.
        pub fn answer(
            &mut self,
            commands: &[ClientCommand],
            result: Result<Value, WireError>,
        ) -> Vec<ClientCommand> {
            let [request] = requests(commands)[..] else {
                panic!("ClientDriver: expected one request in {commands:?}");
            };
            let id = request.id.clone();
            self.handle(ClientEvent::Host(HostMessage::Response(RawResponse {
                id,
                result,
            })))
        }
    }

    /// `envelope` as the host pushes it to a subscribed client.
    pub fn log_entry(envelope: Envelope) -> ClientEvent {
        ClientEvent::Host(HostMessage::Notification(HostNotification::LogEntry(
            Box::new(LogEntry { envelope }),
        )))
    }

    /// The requests among `commands`.
    pub fn requests(commands: &[ClientCommand]) -> Vec<&Request> {
        commands
            .iter()
            .filter_map(|c| match c {
                ClientCommand::Send(ClientMessage::Request(r)) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// The notifications among `commands`.
    pub fn notifications(commands: &[ClientCommand]) -> Vec<&ClientNotification> {
        commands
            .iter()
            .filter_map(|c| match c {
                ClientCommand::Send(ClientMessage::Notification(n)) => Some(n),
                _ => None,
            })
            .collect()
    }

    /// Parses `ts`. Panics on a malformed timestamp.
    pub fn ts(ts: &str) -> Timestamp {
        Timestamp::parse(ts).unwrap_or_else(|e| panic!("{e}"))
    }

    /// What the M1 runtime spawns for a plain session: a shell in `/home/test`, 80x24.
    pub fn shell_spawn() -> SpawnSpec {
        SpawnSpec {
            command: "/bin/zsh".to_owned(),
            args: Vec::new(),
            cwd: PathBuf::from("/home/test"),
            env: Vec::new(),
            size: PtySize { cols: 80, rows: 24 },
        }
    }

    /// A new session of the `shell` profile with no name, running [`shell_spawn`].
    pub fn shell_session_spec() -> NewSessionSpec {
        NewSessionSpec {
            name: None,
            profile: "shell".to_owned(),
            agent: AgentKind::Shell,
            explicit_cwd: false,
            worktree_branch: None,
            spawn: shell_spawn(),
            hooks: WorkspaceHooks::default(),
        }
    }

    /// A new session of the `claude` profile (Claude Code) with no name, running `claude` in `/home/test`, 80x24.
    pub fn claude_session_spec() -> NewSessionSpec {
        NewSessionSpec {
            name: None,
            profile: "claude".to_owned(),
            agent: AgentKind::ClaudeCode,
            explicit_cwd: false,
            worktree_branch: None,
            spawn: SpawnSpec {
                command: "claude".to_owned(),
                ..shell_spawn()
            },
            hooks: WorkspaceHooks::default(),
        }
    }

    /// A resume of a [`claude_session_spec`] session, running `claude --resume abc123` in `/home/test`.
    pub fn claude_resume_spec() -> ResumeSpec {
        ResumeSpec {
            profile: "claude".to_owned(),
            agent: AgentKind::ClaudeCode,
            spawn: SpawnSpec {
                command: "claude".to_owned(),
                args: vec!["--resume".to_owned(), "abc123".to_owned()],
                ..shell_spawn()
            },
        }
    }

    /// `2026-10-29T12:00:00.000Z` plus `ms` milliseconds. Panics past the end of that day.
    fn ts_at(ms: usize) -> Timestamp {
        let (secs, ms) = (ms / 1000, ms % 1000);
        let (mins, secs) = (secs / 60, secs % 60);
        let (hours, mins) = (12 + mins / 60, mins % 60);
        Timestamp::parse(format!(
            "2026-10-29T{hours:02}:{mins:02}:{secs:02}.{ms:03}Z"
        ))
        .unwrap_or_else(|e| panic!("HostDriver: ran out of timestamps: {e}"))
    }
}

/// One shared suite per port: `pub fn suite<P: Port>(make: impl Fn(..) -> P)`, run against the fake and the real
/// adapter. `make` takes whatever the port is constructed with (e.g. an event sink).
pub mod contracts {
    pub mod commands;
    pub mod config;
    pub mod event_log;
    pub mod forge;
    pub mod identity;
    pub mod launcher;
    pub mod link;
    pub mod peers;
    pub mod pty;
    pub mod pty_handoff;
    pub mod worktrees;
}

pub mod stand_in;

/// One stateful in-memory fake per port. Each must pass its port's contract suite.
pub mod fakes {
    pub mod commands;
    pub mod config;
    pub mod event_log;
    pub mod forge;
    pub mod hold;
    pub mod identity;
    pub mod launcher;
    pub mod link;
    pub mod peers;
    pub mod pty;
    pub mod worktrees;
}
