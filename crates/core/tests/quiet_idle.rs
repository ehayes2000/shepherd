//! The quiet-output idle fallback: a turn whose PTY goes quiet is inferred idle, through `core::host::quiet`.

use serde_json::{Value, json};
use shepherd_core::acpp::{
    Dir, EntryId, Envelope, Frame, FrameDraft, IdleInference, Seq, ShepherdFrame, Source,
    SourceKind, Stamp, Timestamp,
};
use shepherd_core::config::{Config, RawConfig};
use shepherd_core::host::HostCommand;
use shepherd_core::host::quiet::{QuietEvent, QuietWatch};
use shepherd_core::ids::{MachineId, SessionId};
use shepherd_core::session::{Activity, SessionState};
use shepherd_core::wire::InputKind;

const SESSION: &str = "s_q";

/// `2026-10-29T12:00:00Z` plus `ms`.
fn t(ms: i64) -> Timestamp {
    let base: jiff::Timestamp = "2026-10-29T12:00:00Z"
        .parse()
        .unwrap_or_else(|e| panic!("{e}"));
    let at = base
        .checked_add(jiff::SignedDuration::from_millis(ms))
        .unwrap_or_else(|e| panic!("{e}"));
    Timestamp::parse(at.to_string()).unwrap_or_else(|e| panic!("{e}"))
}

fn session() -> SessionId {
    SessionId::new(SESSION)
}

// ---------- frames ----------

fn rpc(method: &str, id: Option<i64>, params: Value) -> Value {
    let mut frame = json!({ "jsonrpc": "2.0", "method": method, "params": params });
    if let Some(id) = id {
        frame["id"] = json!(id);
    }
    frame
}

fn prompt(id: i64) -> (Dir, Value) {
    let params = json!({ "sessionId": "a1", "prompt": [{ "type": "text", "text": "go" }] });
    (Dir::ToAgent, rpc("session/prompt", Some(id), params))
}

fn prompt_done(id: i64) -> (Dir, Value) {
    let frame = json!({ "jsonrpc": "2.0", "id": id, "result": { "stopReason": "end_turn" } });
    (Dir::FromAgent, frame)
}

fn tool_call(tool_id: &str) -> (Dir, Value) {
    let update = json!({ "sessionUpdate": "tool_call", "toolCallId": tool_id, "title": "Bash",
                         "kind": "execute", "status": "in_progress" });
    let params = json!({ "sessionId": "a1", "update": update });
    (Dir::FromAgent, rpc("session/update", None, params))
}

fn permission(id: i64, tool_id: &str) -> (Dir, Value) {
    let params = json!({ "sessionId": "a1", "toolCall": { "toolCallId": tool_id, "title": "Bash" },
                         "options": [] });
    (
        Dir::FromAgent,
        rpc("session/request_permission", Some(id), params),
    )
}

fn shepherd(method: &str, params: Value) -> (Dir, Value) {
    (Dir::Internal, rpc(method, None, params))
}

fn exited() -> (Dir, Value) {
    shepherd("_shepherd/agent/exited", json!({ "code": 0 }))
}

fn closed() -> (Dir, Value) {
    shepherd("_shepherd/session/closed", json!({}))
}

// ---------- driver ----------

/// One session's log and a [`QuietWatch`], run the way the host runtime runs them: every entry appended is applied
/// to the watch, and so is every idle a tick appends.
struct Driver {
    watch: QuietWatch,
    log: Vec<Envelope>,
}

impl Driver {
    /// A watch on the default config with a session of `agent` running `profile` created at `t(0)`.
    fn new(agent: &str, profile: &str) -> Self {
        Self::with_config(RawConfig::default(), agent, profile)
    }

    fn with_config(raw: RawConfig, agent: &str, profile: &str) -> Self {
        let config = Config::resolve(Some(raw), None).unwrap_or_else(|e| panic!("{e}"));
        let mut driver = Self {
            watch: QuietWatch::new(&config),
            log: Vec::new(),
        };
        let created = json!({ "name": "q", "agent": agent, "profile": profile, "cwd": "/w" });
        driver.append(0, shepherd("_shepherd/session/created", created));
        driver
    }

    fn claude() -> Self {
        Self::new("claude-code", "claude")
    }

    /// Seals `frame` at `ms` and applies it.
    fn append(&mut self, ms: i64, (dir, frame): (Dir, Value)) -> Seq {
        let frame = Frame::from_value(frame).unwrap_or_else(|e| panic!("{e}"));
        let source = Source::new(SourceKind::Hook, Some("claude-code".to_owned()));
        self.seal(ms, FrameDraft::new(frame, source, dir, Some(session())))
    }

    fn seal(&mut self, ms: i64, draft: FrameDraft) -> Seq {
        let seq = Seq::new(self.log.len() as u64 + 1);
        let stamp = Stamp {
            id: EntryId::new(format!("e{}", seq.get())),
            ts: t(ms),
            machine: MachineId::new("m_test"),
        };
        let env = draft.seal(seq, stamp);
        self.watch.apply(&env);
        self.log.push(env);
        seq
    }

    fn output(&mut self, ms: i64) {
        let commands = self.watch.handle(QuietEvent::Output {
            session: session(),
            at: t(ms),
        });
        assert_eq!(commands, vec![], "output never appends");
    }

    /// A client's Enter key written to the session's PTY at `ms`: an answer, if a dialog is open.
    fn input(&mut self, ms: i64) {
        self.typed(ms, InputKind::Keys, b"\r");
    }

    /// A client's input of `kind` written to the session's PTY at `ms`.
    fn typed(&mut self, ms: i64, kind: InputKind, bytes: &[u8]) {
        let commands = self.watch.handle(QuietEvent::Input {
            session: session(),
            at: t(ms),
            kind,
            bytes: bytes.to_vec(),
        });
        assert_eq!(commands, vec![], "input never appends");
    }

    /// Ticks at `ms`, appends what the watch asks for, and returns how each idle it appended was inferred.
    fn tick(&mut self, ms: i64) -> Vec<IdleInference> {
        let commands = self.watch.handle(QuietEvent::Tick { at: t(ms) });
        let mut idles = Vec::new();
        for command in commands {
            let HostCommand::Append(draft) = command else {
                panic!("a tick only appends, got {command:?}");
            };
            assert_eq!(draft.session, Some(session()));
            assert_eq!(draft.source.kind, SourceKind::Shepherd);
            let Frame::Shepherd(m) = &draft.frame else {
                panic!("expected a shepherd frame, got {:?}", draft.frame);
            };
            let ShepherdFrame::AgentIdle(idle) = m.body() else {
                panic!("expected agent/idle, got {:?}", m.body());
            };
            idles.push(
                idle.inferred
                    .clone()
                    .unwrap_or_else(|| panic!("an inferred idle says so")),
            );
            self.seal(ms, *draft);
        }
        idles
    }

    fn deadline(&self) -> Option<jiff::Timestamp> {
        self.watch.deadline()
    }

    /// The session's state as a client folds it from the log.
    fn state(&self) -> SessionState {
        let mut state = SessionState::default();
        for env in &self.log {
            state.apply(env);
        }
        state
    }
}

fn quiet_config(profile: &str, ms: u64) -> RawConfig {
    let raw = json!({ "agents": { profile: { "idle_after_quiet_ms": ms } } });
    serde_json::from_value(raw).unwrap_or_else(|e| panic!("{e}"))
}

// ---------- working ----------

#[test]
fn a_working_session_whose_pty_stays_quiet_goes_idle_inferred_quiet() {
    let mut d = Driver::claude();
    d.append(1000, prompt(1));
    assert_eq!(d.tick(3999), vec![]);
    assert_eq!(d.tick(4000), vec![IdleInference::Quiet]);
    assert_eq!(d.state().activity(), &Activity::Idle);
}

#[test]
fn a_prompt_the_host_typed_that_no_agent_signal_follows_goes_idle_after_the_quiet_period() {
    let mut d = Driver::claude();
    d.append(
        1000,
        shepherd(
            "_shepherd/session/prompted",
            json!({ "text": "go", "by": "cli" }),
        ),
    );
    assert_eq!(d.state().activity(), &Activity::Working { tool: None });
    assert_eq!(d.tick(3999), vec![]);
    assert_eq!(d.tick(4000), vec![IdleInference::Quiet]);
    assert!(d.state().at_prompt());
}

#[test]
fn output_resets_the_quiet_timer() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.output(2000);
    assert_eq!(d.tick(3000), vec![]);
    d.output(4500);
    assert_eq!(d.tick(7499), vec![]);
    assert_eq!(d.tick(7500), vec![IdleInference::Quiet]);
}

#[test]
fn the_deadline_is_the_end_of_the_quiet_period_and_clears_once_idle() {
    let mut d = Driver::claude();
    assert_eq!(d.deadline(), None, "not working yet");
    d.append(0, prompt(1));
    assert_eq!(d.deadline(), Some(t(3000).instant()));
    d.output(2000);
    assert_eq!(d.deadline(), Some(t(5000).instant()));
    d.tick(5000);
    assert_eq!(d.deadline(), None);
}

#[test]
fn a_frame_in_the_turn_resets_the_quiet_timer_too() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(2000, tool_call("t1"));
    assert_eq!(d.tick(4999), vec![]);
    assert_eq!(d.tick(5000), vec![IdleInference::Quiet]);
}

#[test]
fn esc_during_a_tool_leaves_it_working_until_the_quiet_period() {
    // No `PostToolUse` or `Stop` follows an interrupted tool.
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, tool_call("t1"));
    assert_eq!(d.tick(3100), vec![IdleInference::Quiet]);
    assert_eq!(d.state().activity(), &Activity::Idle);
}

#[test]
fn an_already_idle_session_gets_no_second_idle() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    assert_eq!(d.tick(3000), vec![IdleInference::Quiet]);
    assert_eq!(d.tick(6000), vec![]);
    assert_eq!(d.tick(60_000), vec![]);
}

#[test]
fn a_session_the_agent_said_is_idle_gets_no_inferred_idle() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, prompt_done(1));
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(60_000), vec![]);
}

#[test]
fn output_after_an_inferred_idle_does_not_make_it_working_again() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.tick(3000);
    d.output(4000);
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(60_000), vec![]);
    assert_eq!(d.state().activity(), &Activity::Idle);
}

#[test]
fn a_prompt_after_an_inferred_idle_starts_a_turn_that_can_go_quiet_again() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.tick(3000);
    d.append(10_000, prompt(2));
    assert_eq!(d.state().activity(), &Activity::Working { tool: None });
    assert_eq!(d.tick(12_999), vec![]);
    assert_eq!(d.tick(13_000), vec![IdleInference::Quiet]);
}

#[test]
fn an_inferred_idle_starts_a_wait_that_wants_attention() {
    let mut d = Driver::claude();
    let prompt_seq = d.append(0, prompt(1));
    assert!(!d.state().attention(prompt_seq));
    d.tick(3000);
    assert!(d.state().attention(prompt_seq));
}

#[test]
fn a_profiles_quiet_period_comes_from_its_config() {
    let mut d = Driver::with_config(quiet_config("claude", 1000), "claude-code", "claude");
    d.append(0, prompt(1));
    assert_eq!(d.tick(999), vec![]);
    assert_eq!(d.tick(1000), vec![IdleInference::Quiet]);
}

#[test]
fn a_session_whose_profile_is_gone_uses_the_default_quiet_period() {
    let mut d = Driver::new("claude-code", "removed");
    d.append(0, prompt(1));
    assert_eq!(d.tick(2999), vec![]);
    assert_eq!(d.tick(3000), vec![IdleInference::Quiet]);
}

#[test]
fn codex_sessions_go_quiet_idle_too() {
    let mut d = Driver::new("codex", "codex");
    d.append(0, prompt(1));
    assert_eq!(d.tick(3000), vec![IdleInference::Quiet]);
}

#[test]
fn a_shell_session_is_never_inferred_idle() {
    // Hooks from an agent the shell runs can make it working, but a shell's quiet means nothing.
    let mut d = Driver::new("shell", "shell");
    d.append(0, prompt(1));
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(60_000), vec![]);
}

#[test]
fn an_exited_session_is_never_inferred_idle() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, exited());
    assert_eq!(d.tick(60_000), vec![]);
}

#[test]
fn a_closed_session_is_forgotten() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, closed());
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(60_000), vec![]);
}

#[test]
fn output_for_a_session_it_does_not_know_is_ignored() {
    let config = Config::resolve(None, None).unwrap_or_else(|e| panic!("{e}"));
    let mut watch = QuietWatch::new(&config);
    let output = QuietEvent::Output {
        session: SessionId::new("s_other"),
        at: t(0),
    };
    assert_eq!(watch.handle(output), vec![]);
    assert_eq!(watch.deadline(), None);
    assert_eq!(watch.handle(QuietEvent::Tick { at: t(60_000) }), vec![]);
}

#[test]
fn an_idle_the_host_failed_to_append_is_retried_one_quiet_period_later() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    // The runtime couldn't append what the tick asked for: the session is still working.
    let asked = d.watch.handle(QuietEvent::Tick { at: t(3000) });
    assert_eq!(asked.len(), 1);
    assert_eq!(d.deadline(), Some(t(6000).instant()));
    assert_eq!(d.watch.handle(QuietEvent::Tick { at: t(5999) }), vec![]);
    assert_eq!(d.tick(6000), vec![IdleInference::Quiet]);
}

// ---------- permission ----------

#[test]
fn an_untouched_permission_dialog_stays_put_however_long_it_is_quiet() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(600_000), vec![]);
    assert!(matches!(
        d.state().activity(),
        Activity::NeedsPermission { .. }
    ));
}

#[test]
fn a_permission_dialog_that_only_redraws_stays_put() {
    // Its own first draw, and redraws after a resize, come after the request with nobody answering.
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.output(600);
    d.output(5000);
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(600_000), vec![]);
}

#[test]
fn a_permission_dialog_answered_and_redrawn_then_quiet_goes_idle_inferred_quiet_after_permission() {
    // Choosing "No" fires no hook: the user types, the dialog redraws, then the agent waits at its prompt.
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.output(600);
    d.input(900);
    d.output(1000);
    assert_eq!(d.tick(3999), vec![]);
    assert_eq!(d.tick(4000), vec![IdleInference::QuietAfterPermission]);
    assert_eq!(d.state().activity(), &Activity::Idle);
}

#[test]
fn an_answer_with_no_redraw_after_it_stays_put() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.output(600);
    d.input(900);
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(600_000), vec![]);
}

#[test]
fn input_before_the_permission_request_does_not_count_as_an_answer() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.input(400);
    d.append(500, permission(2, "t1"));
    d.output(1000);
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(600_000), vec![]);
}

#[test]
fn a_second_permission_request_needs_its_own_answer() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.input(900);
    d.output(1000);
    d.append(1500, permission(3, "t2"));
    d.output(1600);
    assert_eq!(d.tick(600_000), vec![]);
}

#[test]
fn a_permission_dialog_answered_with_a_signal_goes_back_to_the_working_rules() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.append(1000, tool_call("t1"));
    assert_eq!(d.tick(3999), vec![]);
    assert_eq!(d.tick(4000), vec![IdleInference::Quiet]);
}

/// Whether `kind` input `bytes` answers an open permission dialog: it does if, after it and a redraw, quiet makes the
/// session idle.
fn answers(kind: InputKind, bytes: &[u8]) -> bool {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.output(600);
    d.typed(900, kind, bytes);
    d.output(1000);
    d.tick(600_000) == vec![IdleInference::QuietAfterPermission]
}

#[test]
fn only_enter_printable_keys_and_pastes_answer_a_permission_dialog() {
    use InputKind::{Focus, Keys, Mouse, Other, Paste};
    let cases: &[(InputKind, &[u8], bool)] = &[
        // Enter picks the highlighted option.
        (Keys, b"\r", true),
        // A printable key picks an option by its number or letter.
        (Keys, b"1", true),
        (Keys, b"y", true),
        (Keys, "é".as_bytes(), true),
        // An old client may send several keys at once.
        (Keys, b"no\r", true),
        // Moving around the dialog doesn't answer it.
        (Keys, b"\x1b[A", false),
        (Keys, b"\x1bOB", false),
        (Keys, b"\x1b[1;2C", false),
        (Keys, b"\t", false),
        (Keys, b"\x1b[Z", false),
        (Keys, b"\x7f", false),
        (Keys, b"\x1b[5~", false),
        // Every other escape sequence (Esc, alt chords, kitty-encoded keys) isn't text either.
        (Keys, b"\x1b", false),
        (Keys, b"\x1b1", false),
        (Keys, b"\x1b[27u", false),
        (Keys, b"", false),
        // A paste is text, bracketed or not.
        (Paste, b"\x1b[200~yes\x1b[201~", true),
        (Paste, b"yes\r", true),
        // The terminal's reports never answer, whatever they contain.
        (Focus, b"\x1b[I", false),
        (Focus, b"\x1b[O", false),
        (Mouse, b"\x1b[<0;4;3M", false),
        (Mouse, b"\x1b[M !!", false),
        // A kind from a newer client this host doesn't know.
        (Other, b"\r", false),
    ];
    for (kind, bytes, expected) in cases {
        assert_eq!(
            answers(*kind, bytes),
            *expected,
            "{kind:?} {:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn a_key_the_client_marks_as_an_answer_answers_a_permission_dialog_whatever_its_encoding() {
    // Esc and Ctrl-C dismiss the dialog; in kitty keyboard modes every key may be `CSI … u`.
    let cases: &[&[u8]] = &[
        b"\x1b",
        b"\x03",
        b"\x1b[27u",
        b"\x1b[99;5u",
        b"\x1b[13u",
        b"\x1b[121u",
        b"y",
        b"\r",
    ];
    for bytes in cases {
        assert!(
            answers(InputKind::Answer, bytes),
            "{:?}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn navigating_a_permission_dialog_then_pressing_enter_answers_it() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.typed(700, InputKind::Keys, b"\x1b[B");
    d.output(800);
    assert_eq!(
        d.tick(600_000),
        vec![],
        "moving the highlight isn't an answer"
    );
    d.typed(600_100, InputKind::Keys, b"\r");
    d.output(600_200);
    assert_eq!(d.tick(603_200), vec![IdleInference::QuietAfterPermission]);
}

#[test]
fn a_focus_report_and_its_redraw_leave_a_permission_dialog_put() {
    // Switching back to the window: the app asked for focus reports and redraws on focus.
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(500, permission(2, "t1"));
    d.output(600);
    d.typed(5000, InputKind::Focus, b"\x1b[I");
    d.output(5050);
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(600_000), vec![]);
    assert!(matches!(
        d.state().activity(),
        Activity::NeedsPermission { .. }
    ));
}

// ---------- a blocked subagent ----------

fn subagent_started(id: &str) -> (Dir, Value) {
    shepherd(
        "_shepherd/subagent/started",
        json!({ "id": id, "agent_type": "Explore" }),
    )
}

fn subagent_blocked(id: &str, text: &str) -> (Dir, Value) {
    shepherd(
        "_shepherd/subagent/blocked",
        json!({ "id": id, "kind": "permission", "text": text }),
    )
}

fn subagent_working(id: &str) -> (Dir, Value) {
    shepherd("_shepherd/subagent/working", json!({ "id": id }))
}

#[test]
fn a_blocked_subagent_keeps_a_working_session_from_going_quiet_idle() {
    // The parent waits on the subagent, and the subagent's dialog is legitimately quiet.
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, subagent_started("a"));
    d.append(500, subagent_blocked("a", "Bash: ls"));
    d.output(600);
    assert_eq!(d.deadline(), None);
    assert_eq!(d.tick(600_000), vec![]);
    assert!(matches!(d.state().activity(), Activity::Working { .. }));
}

#[test]
fn a_subagent_block_answered_and_redrawn_then_quiet_goes_idle_inferred_quiet_after_permission() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, subagent_started("a"));
    d.append(500, subagent_blocked("a", "Bash: ls"));
    d.output(600);
    d.input(900);
    d.output(1000);
    assert_eq!(d.tick(3999), vec![]);
    assert_eq!(d.tick(4000), vec![IdleInference::QuietAfterPermission]);
}

#[test]
fn a_second_subagent_block_needs_its_own_answer() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, subagent_started("a"));
    d.append(500, subagent_blocked("a", "Bash: ls"));
    d.input(900);
    d.output(1000);
    d.append(1500, subagent_blocked("a", "Bash: rm"));
    d.output(1600);
    assert_eq!(d.tick(600_000), vec![]);
}

#[test]
fn a_subagent_working_again_lets_the_session_go_quiet_again() {
    let mut d = Driver::claude();
    d.append(0, prompt(1));
    d.append(100, subagent_started("a"));
    d.append(500, subagent_blocked("a", "Bash: ls"));
    d.append(1000, subagent_working("a"));
    assert_eq!(d.tick(3999), vec![]);
    assert_eq!(d.tick(4000), vec![IdleInference::Quiet]);
}
