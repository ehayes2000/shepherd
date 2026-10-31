# Conversation archive
milestone: M3+
goal: Every agent conversation is kept on this device, indexed by repo and commit, so cleanup never loses context and a cleaned-up session can be restored and resumed

Background and design: `docs/proposals/conversation-archive.md` (the user's proposal). Decision 0020 records the choices
below. On-device only: no account, no network, nothing pushed or uploaded. Transcripts can hold secrets: files are 0600 in a
0700 root.

## Tracker

Status is one of: todo, ready, building, review, fixing, merging, blocked, parked, done. Only the PM edits it
(`scripts/tasks.py set`). Rounds and notes record review rounds and notable findings.

| Task | Title | Status | Branch / merge | Rounds | Notes |
|---|---|---|---|---|---|
| T38 | Archive crate and blob store | done | 597d476 | 3 | decompression bound untested; put didn't heal a corrupt blob; reservation clamp untested (forged header aborted) |
| T39 | Archive index | done | 8d11fb1 | 4 | refusing a newer WAL index modified it; untested rollback/newtypes; side-file peek raced with closes |
| T40 | Capture transcripts from Claude Code and Codex | done | 4380131 | 2 | subagent non-regular-file refusal untested (FIFO would hang capture) |
| T41 | Restore transcripts for a target cwd | done | f810144 | 2 | round 2 APPROVE; minor: RestoreError::Changed doc still says 'stopped resolving to itself' (fix in T42). Flags for T44: >200-char and non-ASCII cwds refused; cwd used as given |
| T42 | Archive port and host capture | done | 9a91698 | 3 | round 3 APPROVE |
| T42b | Capture across `/clear` and compressed Codex rollouts | todo |  |  |  |
| T43 | `shepherd archive` CLI | merging | task/t43-archive-cli @ a208f0a | 2 | round 2 APPROVE; argv faithful-check test → T44 |
| T44 | Real-agent end-to-end suite | todo |  |  |  |

## Specs

### T38: Archive crate and blob store
depends: T00
touches: crates/archive (new), Cargo.toml (workspace), scripts/check-arch.sh
goal: A new independent crate `shepherd-archive` stores transcript bytes content-addressed, compressed and private.
acceptance:
- `crates/archive` exists as `shepherd-archive`, depends on no internal crate, and check-arch enforces that (and that only the
  `shepherd` crate depends on it).
- `BlobStore::open(root)` creates `root` 0700 (refusing a symlinked or wrong-mode existing root, like private_fs) and
  `blobs/`; `put(bytes) -> BlobId` writes `blobs/<2 hex>/<sha256 hex>.zst` (zstd) atomically: temp file in the same dir,
  fsync, rename, fsync dir; files 0600. Identical content is stored once (second put writes nothing: count it).
- `get(id)` decompresses and verifies the sha256; a corrupt or truncated blob is an error naming it, never wrong bytes.
- Decompression is bounded (a blob that inflates past a cap is an error), so a hostile file can't exhaust memory.
- `list()` enumerates blob ids (for T39's verify).
scope: crates/archive, Cargo.toml, scripts/check-arch.sh
notes: (binding) Decision 0020: deps `zstd` and `sha2` (workspace) here; `rusqlite` arrives in T39. The crate does no clock,
no env and no network; it reads only paths it's given. Tests on tempdirs only. Crash-safety: a fault-injection feature
(decision 0015, inert in normal builds) kills between temp write and rename; no partial blob is ever visible under its id.

### T39: Archive index
depends: T38
touches: crates/archive
goal: A SQLite index maps repos, sessions, snapshots and commits to blobs, with every lookup the archive needs.
acceptance:
- `Archive::open(root)` opens `root/index.sqlite` (WAL, busy timeout, 0600) and migrates by `PRAGMA user_version`; a fixture
  v1 database migrates forward; a database from a newer version is refused, never modified.
- Schema as the proposal's sketch (repos keyed by the root commit hash; sessions; snapshots with reason
  `turn_end | close | before_remove | manual`; snapshot files with role `main | subagent | rollout`; session commits with
  patch-id and first snapshot). A snapshot row is inserted only after all its blobs exist (one transaction).
- Lookups: `find_by_commit`, `find_by_patch_id`, `find_by_branch(repo, branch)`, `find_by_pr(repo, n)`,
  `find_by_agent_session(agent, id)`, `sessions(repo)`, `snapshots(session)`, `read(snapshot)`; each tested, including
  "commit unknown" and a squash-merge case found by branch/PR.
- Two processes writing at once (two `Archive` handles in separate processes) both succeed; nothing is lost.
- `verify()` reports index rows whose blob is missing or corrupt, and orphan blobs; it never deletes anything.
- From T38's review: a maintenance step (under the index lock, not on `open`) removes `.tmp` files a crashed `put` left
  in shard dirs; it never touches a blob or a temp file another live writer may own (e.g. only temps older than the index lock
  holder's start, or take an exclusive lock).
- Times are parameters (`captured_at` etc. as `jiff::Timestamp`), never read from a clock inside the crate.
scope: crates/archive, Cargo.toml
notes: (binding) Decision 0020: `rusqlite` with `bundled`. All SQL uses bound parameters; values are typed (commit hashes
and ids as validated newtypes, not free strings compared as text).

### T40: Capture transcripts from Claude Code and Codex
depends: T39, T15, T19
touches: crates/archive (capture), core::agents (hook payload field)
goal: Given an agent's hook payload, the archive snapshots exactly the transcript files that agent wrote.
acceptance:
- The `transcript_path` field of Claude Code and Codex hook payloads reaches the host (verify both agents' current hook
  schemas against their docs or source and cite them); capture reads that path, never one shepherd computed.
- Claude Code: the main `<session>.jsonl` plus `<session>/subagents/*.jsonl` when present. Codex: the rollout file.
- Capture refuses paths outside the agent's own transcript directory for that session (a payload pointing at `/etc/passwd`
  or a symlink out is an error, nothing archived), and bounds the bytes read.
- A capture of unchanged files adds a snapshot row but no new blobs (counted).
- Tests on fixtures recorded from the real agents (checked in under `crates/archive/tests/fixtures`, scrubbed of anything
  personal), including hook payloads with `transcript_path`.
scope: crates/archive, crates/core (hook payload parsing), crates/testkit
notes: (binding) Builders never run the real `claude`/`codex` and never read the user's `~/.claude*` or `~/.codex`. The
first fixtures are hand-built from the documented formats and marked provisional; T44's opt-in run re-records them.
The agents' homes (Claude's config dir, `CODEX_HOME`) are parameters to capture, like T41's `agent_home`; the crate never
reads `$HOME` or env. "The agent's own transcript directory" is resolved under the given home after canonicalizing, and
symlinks are refused on every component below it. Hook payload parsing in core stays pure (the path is data).

### T41: Restore transcripts for a target cwd
depends: T40
touches: crates/archive (restore)
goal: An archived session's transcripts go back where each agent's resume looks for them, for a given cwd.
acceptance:
- `restore(session, target_cwd, agent_home) -> Resume { agent, agent_session_id, cwd }` writes the files under the given
  agent home (Claude: the project dir for `target_cwd`; Codex: its sessions tree), creating dirs 0700 and files 0600.
- An existing different file at a target path is an error naming it, never overwritten; identical content is a no-op.
- Round-trip tests on the fixtures: capture, restore into a different cwd, the restored files are what the agent would
  look for there (per its documented layout).
- Whether Claude needs the `cwd` fields inside the JSONL rewritten for a different cwd is not assumed: implement no rewrite
  unless the agent's docs or source say resume checks it (cite either way); T44 settles it against the real agent.
scope: crates/archive
notes: (binding) `agent_home` is always a parameter (tests pass tempdirs); the crate never reads `$HOME`.

### T42: Archive port and host capture
depends: T39, T40, T25, T25b, T29
touches: ports::archive, core::host, shepherd::host::runtime, testkit
goal: The host snapshots every agent conversation on turn end, on close and before any worktree removal.
acceptance:
- An `Archive` port (trait in core, stateful fake in testkit, per-case contract suite run against the fake and the real
  `shepherd-archive` through an adapter in the `shepherd` crate).
- The host emits a capture on Stop (turn end) and on close, and records `base..HEAD` commits with patch-ids for the
  session's worktree (through the Worktrees port, which gains `commits_since(base) -> Vec<(Commit, PatchId)>`).
- Before any worktree removal (T25 close, T25b cleanup, and any future automatic cleanup), the host captures first; if that
  capture fails, the worktree is kept and the notice says why. Mutation-check the ordering.
- Captures run off the runtime thread (F27 worker), bounded, and never block input; shutdown waits for them (bounded) like
  creates (F37).
- Sessions without an agent transcript (plain shells) capture nothing and never block removal.
scope: crates/core, crates/testkit, crates/adapters, crates/shepherd
- From T41's review: `RestoreError::Changed`'s doc comment (crates/archive/src/restore.rs ~112) still says "stopped
  resolving to itself"; it now means "is no longer the same directory (device and inode)". Fix the wording.
notes: (binding) The archive root is `~/.shepherd/archive/` (resolved once at host start). `machine` in rows is T29's
`MachineId`. Times come from the host's Tick, never a clock in core. `/clear` and Codex `.jsonl.zst` rollouts are T42b.
Port shape (PM decision; core can't see `shepherd-archive`'s types, so core has its own and the adapter converts):
```rust
pub trait Archive {
    /// Records (or updates) the session that captures belong to. Idempotent.
    fn put_session(&self, session: &ArchivedSession) -> Result<(), ArchiveError>;
    /// Snapshots the session's transcripts. Blocking: the host calls it on its worker, never the runtime thread.
    fn capture(&self, capture: &CaptureRequest) -> Result<(), ArchiveError>;
}
pub struct ArchivedSession { pub id: SessionId, pub repo: RepoRoot /* the repo's root commit */, pub machine: MachineId,
    pub agent: AgentKind, pub agent_session_id: String, pub name: Option<String>, pub branch: Option<BranchName>,
    pub base: Option<CommitHash>, pub cwd: PathBuf, pub created_at: jiff::Timestamp }
pub struct CaptureRequest { pub session: SessionId, pub at: jiff::Timestamp, pub head: Option<CommitHash>,
    pub reason: CaptureReason /* TurnEnd | Close | BeforeRemove | Manual */, pub commits: Vec<(CommitHash, Option<PatchId>)>,
    pub agent_home: PathBuf, pub transcript_path: PathBuf }
pub enum ArchiveError { Refused(String) /* capture refused the path or limits */, Missing(String) /* no transcript file:
    nothing to capture, removal proceeds (amended after T42's review) */, Failed(String) /* IO, index */ }
```
`CommitHash`, `PatchId` and `RepoRoot` are validated hex newtypes in core (not free strings). `Worktrees` gains
`commits_since(&self, path: &WorktreePath, base: &CommitHash) -> Result<Vec<(CommitHash, Option<PatchId>)>, WorktreeError>`
and `root_commit(&self, repo: &RepoRef) -> Result<RepoRoot, WorktreeError>`, with contract cases. The agent homes (Claude:
`CLAUDE_CONFIG_DIR` or `~/.claude`; Codex: `CODEX_HOME` or `~/.codex`) are resolved once at host start in the `shepherd`
crate and passed in; core and the archive crate never read env. Tests never touch the real `~/.shepherd`, `~/.claude` or
`~/.codex`.

### T42b: Capture across `/clear` and compressed Codex rollouts
depends: T42
touches: core::host, shepherd::host (archive adapter), crates/archive (capture)
goal: A conversation is never lost because the agent started a new session in place or Codex compressed an old rollout.
acceptance:
- From T40: on `/clear` (a new agent session in the same shepherd session) the host captures the old transcript before it
  replaces the path, then `put_session`s with the new agent session id before the next capture (capture checks against the
  indexed id). Scenario test with a fake agent that switches session ids.
- Codex rollouts compressed in place after 7 days (`.jsonl.zst`) are captured too (decompress through the archive's bounded
  reader, or archive the `.zst` as is with a role/flag; say which), so a long-idle Codex session can still be removed.
  Fixture test with a compressed rollout; a hostile `.zst` that inflates past the cap is refused, nothing archived.
- From T42: a profile that sets its own `CLAUDE_CONFIG_DIR` / `CODEX_HOME` puts transcripts outside the host's agent home,
  so every capture is refused and that session's worktree can never be removed. Capture uses the agent home the session's
  agent actually ran with (its launch env), falling back to the host's; test with a profile that sets it.
- From T42: `session/closed` carries an optional `note` (e.g. "no transcript was found"), but the client doesn't show it
  yet. Show it as the close notice (core client + tui), with a test.
- From T42's review: an agent session whose cwd is not in a git repo fails `root_commit` on every turn end (a warning each
  time) and is never archived. Archive it under a repo-less key, or say clearly once that it can't be archived.
scope: crates/core, crates/archive, crates/shepherd

### T43: `shepherd archive` CLI
depends: T42, T41
touches: shepherd (cli)
goal: You (and later agents over MCP) can list, inspect, find, restore and verify archived conversations.
acceptance:
- `shepherd archive list | show <session> | find <commit|branch|#pr> | restore <session> [--cwd <dir>] | verify`, each with
  `--json` (stable, documented shape) and human output through `display_safe`.
- `find` tries the exact commit, then patch-id, then branch and PR.
- `restore` defaults the cwd to the session's recorded worktree if it exists, else requires `--cwd`; it never overwrites
  (T41's rule) and prints how to resume.
- Tests run the binary with a tempdir HOME and a fixture archive.
scope: crates/shepherd
notes: the patch-id for `find` comes from the same pinned code as capture (T42's `commits_since` path in the git adapter;
expose a single-commit variant on the Worktrees port if needed, with a contract case), never a second copy of the flags.
The CLI opens the archive while a host may be writing it (WAL): reads must not block the host or modify the index;
`restore` writes only through T41's `restore`. `--json` shapes are documented in `docs/` and covered by a test each.
PM-approved during the build: `Archive::all_sessions()` and `Archive::open_read_only(root)` in crates/archive (additive);
`Worktrees::patch_id(path, commit)` and `Worktrees::resolve_commit(path, rev)` (abbreviated hashes from `git blame`) with
contract cases.

### T44: Real-agent end-to-end suite
depends: T43
touches: crates/archive/e2e (new, opt-in), scripts/e2e-agents.sh
goal: Prove capture and restore against the real Claude Code and Codex, and keep the gate's fixtures current.
acceptance:
- For each of `claude` and `codex`: in a temp git repo, the real agent creates and commits a file; capture; delete the
  worktree and the agent's transcript; restore into a new cwd; resume with the agent's own resume flag; ask what file it
  created; the answer names it. Also `find` by the commit hash, and by patch-id after a rebase.
- The run re-records `crates/archive/tests/fixtures` (scrubbed) and settles T41's cwd-rewrite question (amend T41 if needed).
- Agent state goes to throwaway dirs (`CLAUDE_CONFIG_DIR`, `CODEX_HOME` or the agents' documented equivalents), never the
  user's real `~/.claude` / `~/.codex`; the script says up front that it uses the network and costs tokens.
- From T43's review: `archive restore`'s "print no shell line unless faithful" check on the resume argv
  (archive_cli.rs ~632) is untested; add a restore test whose agent session id holds U+202E and assert no `cd` line.
scope: crates/archive (e2e, behind a feature or separate package not built by the gate), scripts
notes: (binding) Decision 0020: outside the gate. Builders write it and dry-run everything except the agent calls; only the
user runs `scripts/e2e-agents.sh` (or the PM, with the user's go-ahead each time). How the throwaway agent dirs get
credentials is the user's choice at run time (e.g. an API key env var); the script never reads or copies the user's
credential files.
