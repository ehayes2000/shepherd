# Archive fixtures

## `index-v1.sql`

A version-1 index, for the migration tests (T39).

## `claude/`, `codex/`: capture fixtures (PROVISIONAL)

**Provisional.** These were written by hand from the agents' documentation and source, not recorded from a real
`claude` or `codex` run: builders never run the real agents or read anyone's `~/.claude` / `~/.codex`. T44's opt-in
`scripts/e2e-agents.sh` re-records them from the real agents (scrubbed) and replaces this notice.

Each has `home/`, a scrubbed agent home (Claude Code's config dir, or `CODEX_HOME`) holding one session's
transcripts, and `hooks/`, hook payloads (stdin JSON) for that session. In the payloads, `${AGENT_HOME}` stands for
wherever the test installs `home/`; tests substitute it. Nothing personal: the cwd is `/work/app`, ids are made up.

Capture treats transcript content as opaque bytes, so only the layout and the payload fields are load-bearing.

### Sources

- Claude Code hooks reference: <https://code.claude.com/docs/en/hooks>, read 2026-10-30. Every hook's input has
  `session_id` and `transcript_path` ("Path to conversation JSON"); `SubagentStop` adds `agent_transcript_path`, "the
  subagent's own transcript stored in a nested `subagents/` folder" (example
  `~/.claude/projects/.../abc123/subagents/agent-def456.jsonl`, next to `transcript_path`
  `~/.claude/projects/.../abc123.jsonl`).
- Claude Code `.claude` directory reference: <https://code.claude.com/docs/en/claude-directory>, "Application data",
  read 2026-10-30: `projects/<project>/<session>.jsonl` is the transcript, `projects/<project>/<session>/subagents/`
  holds subagent transcripts "in the same format", `projects/<project>/<session>/tool-results/` large tool outputs.
  With `CLAUDE_CONFIG_DIR` set, all of it lives under that directory instead of `~/.claude`.
- Claude Code sessions reference: <https://code.claude.com/docs/en/sessions>, read 2026-10-30, used by restore (T41):
  "Where transcripts are stored" gives `projects/<project>/<session-id>.jsonl`, `<project>` being "your working
  directory path with non-alphanumeric characters replaced by `-`", truncated with an unspecified hash past 200
  characters; "Resume by session ID or name": `--resume <id>` searches the current project dir, then every other one.
  Nothing there says resume checks the `cwd` inside a transcript. Undocumented, so restore refuses them until T44
  checks the real agent: a cwd that isn't ASCII, a project dir name over 200 characters. Also for T44: restore
  encodes the cwd as given, not resolved (macOS `/tmp/x` vs `/private/tmp/x`).
- Codex, openai/codex at `rust-v0.166.0`:
  - `codex-rs/hooks/src/schema.rs`: `SessionStartCommandInput` and `StopCommandInput` have `session_id: String` and
    `transcript_path: NullableString` (string or null); `codex-rs/hooks/src/events/session_start.rs`: the
    `session_id` is the `ThreadId`, `transcript_path` an `Option<PathBuf>`.
  - `codex-rs/rollout/src/lib.rs`: `SESSIONS_SUBDIR = "sessions"`, `ARCHIVED_SESSIONS_SUBDIR = "archived_sessions"`.
  - `codex-rs/rollout/src/recorder.rs`: rollouts go in `sessions/<year>/<MM>/<DD>/`.
  - `codex-rs/rollout/src/rollout_file_name.rs`: the name is `rollout-<YYYY-MM-DDTHH-MM-SS>-<thread id>.jsonl`, or
    `...-<thread id>_<rollout id>.jsonl` for a reverted thread.
  - `codex-rs/rollout/src/compression.rs`: rollouts older than 7 days may be compressed in place to `.jsonl.zst`.
    Capture doesn't accept those (a live session's rollout is plain); see the T40 report.
  - `codex-rs/rollout/src/list.rs`: `find_thread_path_by_id_str` (resume by id) walks `sessions/` matching rollout
    file names by thread id, with no cwd filter; only the listing (picker) filters by the `session_meta` cwd.

### Documented vs inferred

| fixture | documented | inferred |
|---|---|---|
| `claude/hooks/*` | `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `permission_mode`; `SessionStart` `source`, `model`; `Stop`/`SubagentStop` `stop_hook_active`, `last_assistant_message`, `background_tasks`, `session_crons`; `SubagentStop` `agent_id`, `agent_type`, `agent_transcript_path` | the ids, the project dir name `-work-app` |
| `claude/home/projects/-work-app/<session>.jsonl` | its place and name | its lines (shaped like Claude Code's JSONL: one JSON object per line with `type`, `message`, `uuid`, `parentUuid`, `sessionId`, `timestamp`, ...) |
| `claude/home/.../<session>/subagents/agent-*.jsonl` | the directory, `agent-<id>.jsonl` (from the `SubagentStop` example) | the lines (`isSidechain`, `agentId`) |
| `claude/home/.../<session>/tool-results/` | the directory | the file in it (capture must not take it) |
| `codex/hooks/*` | `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `model`, `permission_mode`, `source`; `Stop` `turn_id`, `stop_hook_active`, `last_assistant_message` | the ids |
| `codex/home/sessions/2026/10/30/rollout-...jsonl` | its place and name | its lines (`session_meta` first, as `recorder.rs` writes it; the other item shapes are guesses) |
