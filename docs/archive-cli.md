# `shepherd archive`

The conversation archive (decision 0020) from the command line: list, inspect, find, restore and verify the agent
conversations this machine archived under `~/.shepherd/archive/`.

```
shepherd archive list                                  [--json]
shepherd archive show <session>                        [--json]
shepherd archive find <commit|branch|#pr>              [--json]
shepherd archive restore <session> [--cwd <dir>]       [--json]
shepherd archive verify                                [--json]
```

- The archive is opened read-only: the commands work while the host is writing it, never make the host wait, and never
  change the index (when no host has it open, SQLite gives the reader an empty `-wal` and a `-shm`, which change
  nothing). An index from an older shepherd is refused until `shepherd host` has migrated it; one from a newer shepherd
  is refused.
- Human output is display-safe (control characters in names, branches and paths are dropped). `--json` output is one
  pretty-printed JSON object on stdout, with the shapes below.
- Errors go to stderr as `shepherd: <why>`, with exit status 1. With no archive yet, `list` and `find` answer with
  nothing (exit 0); `show`, `restore` and `verify` fail saying there is no archive.

## Shapes

These are stable: fields may be added, never renamed, removed or retyped. Times are RFC 3339 UTC
(`2026-10-31T12:00:00Z`). Paths are strings (a path that isn't UTF-8 is shown lossily). Hashes are lowercase hex.

### Session

```json
{
  "id": "01JBX…",                    // shepherd session id
  "repo": "<root commit hash>",      // the repo, named by its root commit
  "machine": "<machine id>",
  "agent": "claude" | "codex",
  "agent_session_id": "<the agent's own session id>",
  "name": "add a parser" | null,
  "branch": "parser" | null,
  "pr": 7 | null,
  "base_commit": "<hash>" | null,
  "cwd": "/path/to/the/worktree",    // where the session ran
  "created_at": "<time>",
  "archived_at": "<time>" | null
}
```

### `list --json`

Every session, oldest first. `{"sessions": []}` when there is no archive yet.

```json
{ "sessions": [ Session, … ] }
```

### `show <session> --json`

```json
{
  "session": Session,
  "snapshots": [                     // oldest first
    {
      "id": 12,
      "captured_at": "<time>",
      "reason": "turn_end" | "close" | "before_remove" | "manual",
      "head_commit": "<hash>" | null,
      "files": [
        { "role": "main" | "subagent" | "rollout", "original_path": "/…", "blob": "<sha256>", "bytes": 1234 }
      ]
    }
  ]
}
```

### `find <query> --json`

```json
{
  "query": "<the query as given>",
  "matches": [
    {
      "how": "commit" | "patch_id" | "branch" | "pr",
      "commit": "<hash>" | null,       // commit and patch_id: the archived commit that matched
      "patch_id": "<hash>" | null,
      "first_snapshot": 12 | null,     // the session's first snapshot that saw that commit
      "session": Session
    }
  ]
}
```

How a query is looked up, in order:

1. `#N` is pull request `N` of the current directory's repo (outside a repo it is an error).
2. Anything else is resolved by git in the current directory's repo: an abbreviated hash (what `git blame` prints), a
   full hash, a branch, `HEAD`, any revision. Outside a repo only a full hash works. The commit is looked up exactly
   (`how: "commit"`); if no session made it, by its patch-id (`how: "patch_id"`: the same change, rebased or
   cherry-picked), computed exactly as capture computes it.
3. A query that is a valid branch name is also looked up as a branch of the current repo (`how: "branch"`), for a
   squash-merged change no session made, or a branch git no longer has. A session already found by commit isn't
   listed again.

No match is `{"query": …, "matches": []}` (exit 0).

### `restore <session> --json`

Puts the session's latest snapshot back where its agent's resume looks for it, for `--cwd <dir>` (relative to the
current directory), or by default the session's recorded worktree if that directory still exists (otherwise `--cwd` is
required). The agent's home is `CLAUDE_CONFIG_DIR` / `CODEX_HOME`, else `~/.claude` / `~/.codex`, and must exist. A
different file already at a target path is never overwritten (an error naming it); identical files are left as they
are, so restoring twice is fine.

```json
{
  "session": "<shepherd session id>",
  "agent": "claude" | "codex",
  "agent_session_id": "<id>",
  "cwd": "/dir/to/resume/in",
  "command": ["claude", "--resume", "<id>"]   // or ["codex", "resume", "<id>"]: run it in `cwd`
}
```

### `verify --json`

Checks every snapshot file's blob and looks for blobs nothing refers to. Changes nothing. Exit status 1 when it finds a
problem (after printing the report).

```json
{
  "ok": true | false,
  "bad_rows": [
    {
      "snapshot": 12,
      "role": "main" | "subagent" | "rollout",
      "original_path": "/…",
      "blob": "<sha256>",
      "problem": "missing" | "corrupt" | "too_large" | "unreadable",
      "error": "<why, as text>"
    }
  ],
  "orphans": [ "<sha256>", … ]
}
```
