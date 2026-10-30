# Proposal: conversation archive

status: proposal, for the PM to turn into `docs/features/conversation-archive.md`
date: 2026-10-30

## Why

Automatic cleanup (see the cleanup discussion behind T25/T25b) has to be able to delete worktrees and stop agents without
losing what the agent knew. Agents keep their transcripts in places shepherd doesn't own: Claude Code under
`~/.claude/projects/<encoded cwd>/` (and deletes them after `cleanupPeriodDays`, 30 by default), Codex under
`~/.codex/sessions/`. Once a worktree and its transcript are gone, the "why" behind a commit is gone too.

The archive keeps a copy of every agent conversation on the device, indexed by repo and commit, so that:

1. cleanup can remove anything, because the conversation is archived first;
2. you can ask "which conversation produced this commit?" (via `git blame` → commit → session);
3. a cleaned-up session can be restored: the transcript is put back where the agent looks for it and the agent resumes.

## Constraints

- **On-device, no account, no network.** Nothing in the archive is ever pushed or uploaded. Checked existing tools
  (Entire, git-ai, SpecStory, git-memento): each either stores inside the repo (dies with the clone, pushed by default) or
  is tied to a hosted product. None fits, so we build a small one.
- **An independent crate**, `crates/archive` (`shepherd-archive`). It depends on no internal crate, so it is reusable and
  testable alone. The `shepherd` binary wires it to core through a port.
- **Thoroughly tested**, including end-to-end tests against the real Claude Code and Codex.

## Design

### Storage

```
~/.shepherd/archive/            (0700)
  index.sqlite                  WAL mode, busy timeout, PRAGMA user_version for migrations
  blobs/ab/abcdef….zst          content-addressed: name = sha256 of the uncompressed bytes, body = zstd
```

- Blob writes are atomic: temp file in the same dir → fsync → rename. A blob is written before any index row refers to it.
  A read verifies the hash. Identical content is stored once.
- Files are 0600. Transcripts can contain secrets; they stay on this machine.
- One archive per machine. Each host archives its own sessions. Cross-machine lookup is a later concern (over the
  multi-machine link), not part of this feature.

### Index (sketch, final shape is A2's call within these rules)

```sql
repos(id TEXT PRIMARY KEY,            -- hash of the repo's root commit: stable across clones and machines
      origin_url TEXT)                -- label only, never a key
sessions(id TEXT PRIMARY KEY,         -- shepherd session id
         repo_id TEXT, machine TEXT, agent TEXT,          -- agent: claude | codex
         agent_session_id TEXT, name TEXT, branch TEXT, pr INTEGER,
         base_commit TEXT, cwd TEXT, created_at INTEGER, archived_at INTEGER)
snapshots(id INTEGER PRIMARY KEY, session_id TEXT, captured_at INTEGER,
          head_commit TEXT, reason TEXT)                  -- reason: turn_end | close | before_remove | manual
snapshot_files(snapshot_id INTEGER, role TEXT,            -- role: main | subagent | rollout
               original_path TEXT, blob TEXT, bytes INTEGER)
session_commits(session_id TEXT, commit_hash TEXT, patch_id TEXT, first_snapshot INTEGER,
                PRIMARY KEY (session_id, commit_hash))
-- indexes: session_commits(commit_hash), session_commits(patch_id), sessions(repo_id, pr),
--          sessions(repo_id, branch), sessions(agent, agent_session_id)
```

- A session has many snapshots over time (transcripts are append-only, so content addressing makes repeats cheap).
  `session_commits.first_snapshot` gives "the conversation as of this commit".
- Lookup by commit tries the exact hash, then `git patch-id` (survives rebase), then branch and PR number (survives a
  squash merge, where GitHub makes a commit no session ever made).
- Times come from the caller (the host's clock port), never read inside the crate's logic, so tests pass them in.

### Capture

Hooks already give us the agent's session id. Both agents' hook payloads carry a `transcript_path`; capture reads that,
never a path we computed. (A3 verifies this for both agents against current docs/source and records fixtures.)

- Claude Code: the main `<session>.jsonl`, plus subagent transcripts next to it (`<session>/subagents/*.jsonl`) if
  present.
- Codex: the rollout file `~/.codex/sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`.

The host captures a snapshot on turn end (Stop), on close, and **before any worktree removal** (T25 close, T25b cleanup,
and future automatic cleanup). If the before-removal capture fails, the worktree is kept and the notice says why.
On each capture the host records the commits in `base..HEAD` with their patch-ids.

### Restore

Given a session (or a commit that leads to one), restore:

1. puts the transcript files back where the agent's own resume finds them **for the target cwd** (the restored
   worktree's path may differ from the original). For Claude this means the project dir for the new cwd; whether the
   `cwd` fields inside the JSONL must be rewritten is verified by the e2e test, not assumed;
2. never overwrites an existing different file at the target (error naming it; identical content is a no-op);
3. returns what the caller needs to resume (agent kind, agent session id, cwd).

Recreating the worktree from the branch or an archive ref is the cleanup feature's job; restore takes the cwd as input.

### Interfaces

- **Crate API** (`shepherd-archive`): `Archive::open(root)`, `capture(...)`, `record_commits(...)`, `find_by_commit`,
  `find_by_patch_id`, `find_by_branch`, `find_by_pr`, `find_by_agent_session`, `sessions(repo)`, `read(snapshot)`,
  `restore(session, target_cwd, agent_home)`, `verify()` (index rows whose blob is missing or corrupt, orphan blobs).
- **Core port** `Archive` (trait in core, fake in testkit, contract suite run against the fake and the real crate via
  an adapter in the `shepherd` crate).
- **CLI**: `shepherd archive list | show <session> | find <commit|branch|#pr> | restore <session> [--cwd <dir>] | verify`,
  with `--json` for agents (and later the shepherd MCP).

## Testing

| Layer | What | Where | In the gate |
|---|---|---|---|
| Unit / API | blob store (atomicity, hash check, dedupe, perms), index (every lookup, migrations from a fixture v1 DB, concurrent writers in WAL), restore conflicts | `crates/archive/tests` on tempdirs | yes |
| Fixture | capture and restore of transcripts **recorded from real agents** (Claude main + subagent files, Codex rollout), including hook payloads with `transcript_path` | `crates/archive/tests/fixtures` | yes |
| Contract | the `Archive` port suite against the fake and the real adapter | testkit + `crates/shepherd/tests` | yes |
| Host scenario | turn end → snapshot; close → snapshot; cleanup removal → snapshot first; failed capture keeps the worktree | `crates/shepherd/tests` with fake agents | yes |
| Crash | kill between blob write and index insert → no dangling row; `verify` reports the orphan blob | archive tests (fault-injection feature per decision 0015) | yes |
| **Real-agent e2e** | for each of `claude` and `codex`: start the real agent in a temp git repo, have it create and commit a file, capture, delete the worktree **and** the agent's transcript, restore into a new cwd, resume with the agent's own resume flag, ask what file it created, assert the answer names it. Also: `find` by the commit's hash, and by patch-id after a rebase | `crates/archive/e2e` (opt-in), `scripts/e2e-agents.sh` | **no** (see open question 1) |

The real-agent run also re-records the fixtures, so the gated fixture tests track the agents' current formats.

## Tasks

Ids are for the PM to assign. Ordered by dependency; A3 and A4 can run in parallel after A2.

- **A1 — crate and blob store.** New `crates/archive`, check-arch rules for it, decision entry for the new deps. Content-
  addressed zstd blobs with atomic writes, hash-verified reads, dedupe, 0600/0700 perms.
- **A2 — index.** SQLite schema with migrations (`user_version`), sessions/snapshots/commits, every lookup, WAL
  concurrency (two processes writing), `verify`.
- **A3 — capture from Claude Code and Codex.** Read `transcript_path` from hook payloads, include Claude subagent files,
  record fixtures from the real agents, capture tests against the fixtures.
- **A4 — restore.** Put transcripts back for a target cwd for both agents, conflict rules, round-trip tests on fixtures.
- **A5 — core port + host wiring.** `Archive` port, fake, contract suite, adapter in `shepherd`. Host captures on turn
  end, close and before worktree removal (T25/T25b paths), records `base..HEAD` commits with patch-ids. A failed
  before-removal capture keeps the worktree.
- **A6 — CLI.** `shepherd archive list/show/find/restore/verify` with `--json`.
- **A7 — real-agent e2e suite.** The e2e scenario above for both agents, `scripts/e2e-agents.sh`, fixture re-recording.
- Later, not in this feature: transcript reader (parse to messages) and full-text search (SQLite FTS5); archive
  retention; cross-machine lookup; the shepherd MCP tools that expose this to agents.

## Open questions for the PM

1. **Real agents in tests.** CLAUDE.md rule 7 keeps network out of core/tui tests, and the existing `claude.rs`/`codex.rs`
   tests never run the real agents. Real-agent e2e needs network, auth and costs tokens. Proposal: keep it out of the
   gate, run it via `scripts/e2e-agents.sh` before merging A3/A4/A7 and whenever an agent updates, and let it re-record
   the fixtures the gate uses. Needs a decision entry.
2. **New deps.** `rusqlite` (with `bundled`, so no system SQLite) and `zstd` for `shepherd-archive`; `sha2`, `serde`,
   `serde_json`, `jiff`, `thiserror` already in the workspace. Needs allowlist + decision.
3. **Crate placement.** An independent crate (no internal deps) wired through an adapter in the `shepherd` crate,
   rather than living in `shepherd-adapters`. Needs a `check-arch.sh` rule for `shepherd-archive` and a decision.
4. **Claude restore across cwds.** If Claude's resume refuses a transcript whose `cwd` fields don't match, A4 rewrites
   them. A7 settles it; A4's spec should say "verified by e2e" rather than guess.
