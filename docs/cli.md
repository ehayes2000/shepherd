# Shepherd from the command line

Commands for scripts and agents (decision 0022): see every session, start one, and (in later tasks) prompt, answer and
arrange them. Each command is a short-lived client of this machine's host (started if it isn't running), and reaches
the paired machines through it, as the TUI does.

```
shepherd sessions                                                        [--json]
shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N]          [--json]
```

- Human output is display-safe (control characters in names, branches and paths are dropped). `--json` output is one
  pretty-printed JSON object on stdout, with the shapes below.
- Errors go to stderr as `shepherd: <why>`, with exit status 1 and nothing on stdout.
- A command first waits until this machine's host and every paired machine it can reach have sent their sessions. A
  machine that can't be reached is left out (and listed as such), never waited for.

## Naming sessions and machines

Every command that acts on a session takes it by **id**, or by **name** when no other session (on any machine) has
that name. An id wins over a name. An ambiguous name is an error listing the sessions it could mean, with their ids and
machines; an unknown one lists every session.

`--machine` takes a machine's id or its name (`shepherd peers` lists them; this machine's name is its host's). An
unknown or ambiguous name is an error listing the machines.

## `shepherd sessions`

Every session on this machine and on each paired machine the host can reach, newest first. The human form is one line
per session: `<id>  <name>  <status>  <agent>  <machine>  <branch>  <#pr>  <workspace>` (`-` for what it hasn't), then
`(<machine> is unreachable: <why>)` for each machine that couldn't be reached.

## `shepherd new`

Starts a session on `--machine` (this one by default) with `--profile` (the host's default profile by default), and
prints its id once the host has made it. Without `--cwd` the session gets a managed worktree in that host's current
workspace (or runs in its home directory when it has none); with `--cwd` it runs in that directory and gets no
worktree. A relative `--cwd` is relative to the current directory. `--name` names it (and its worktree's branch), and
the name stays: it is never replaced by one taken from the first prompt.

It fails, making nothing, when the profile is unknown, the machine is unknown or can't be reached, or the directory
doesn't exist. It returns once the session exists; its worktree may still be being made (`starting: true`), and a
session whose worktree then can't be made shows `status: "exited"` with the reason in `failed`.

## Shapes

These are stable: fields may be added, never renamed, removed or retyped. Times are RFC 3339. Paths are strings (a
path that isn't UTF-8 is shown lossily).

### Session

```json
{
  "id": "s_01jbx…",
  "name": "api",
  "machine": "studio",                     // what the machine is called
  "machine_id": "<machine id>",
  "agent": "claude-code" | "codex" | "shell" | null,   // null: from a host older than recording it
  "status": "running" | "idle" | "wants-user" | "exited",
  "workspace": "/path/to/repo" | null,     // the workspace it was started in
  "worktree": "/path/to/worktree" | null,
  "branch": "api" | null,
  "pr": { "number": 7, "state": "open" | "closed" | "merged", "checks": "none" | "pending" | "passing" | "failing" } | null,
  "starting": false,                       // its worktree (or its post-create hooks) is still being made
  "failed": "why it couldn't start" | null,
  "created_at": "<time>"
}
```

`status`:
- `running`: its process runs and isn't known to be waiting: the agent is in a turn or still starting, or it runs no
  agent (a shell).
- `idle`: the agent is at its prompt with nothing to do.
- `wants-user`: the agent is waiting on the user (it asked permission or a question).
- `exited`: its process ended, or it never started.

### Machine

```json
{
  "id": "<machine id>",
  "name": "laptop",
  "local": false,                          // true for this machine
  "status": "connected" | "unreachable" | "incompatible",
  "why": "why it can't be reached" | null
}
```

### `sessions --json`

This machine first, then the paired machines by name; sessions newest first.

```json
{ "machines": [ Machine, … ], "sessions": [ Session, … ] }
```

### `new --json`

```json
{ "session": Session }
```
