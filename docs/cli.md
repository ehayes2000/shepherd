# Shepherd from the command line

Commands for scripts and agents (decision 0022): see every session, start one, and (in later tasks) prompt, answer and
arrange them. Each command is a short-lived client of this machine's host (started if it isn't running), and reaches
the paired machines through it, as the TUI does.

```
shepherd sessions                                                        [--json]
shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N]          [--json]
shepherd prompt <session> <text | -> [--wait] [--queue] [--timeout T]    [--json]
shepherd reply <session>                                                 [--json]
shepherd wait <session> [--timeout T]                                    [--json]
```

- Human output is display-safe (control characters in names, branches and paths are dropped). `--json` output is one
  pretty-printed JSON object on stdout, with the shapes below.
- Errors go to stderr as `shepherd: <why>`, with exit status 1 and nothing on stdout. A command that gives up waiting
  at its `--timeout` exits with status 124 (as `timeout(1)` does), saying so the same way.
- `--timeout T` takes seconds (`90`) or a duration (`30s`, `5m`, `1h 30m`). Without it, a command waits as long as it
  takes.
- A command first waits until this machine's host and every paired machine it can reach have sent their sessions. A
  machine that can't be reached, or hasn't sent all its sessions within 20 seconds of being reached, is left out (and
  listed as unreachable, with why) rather than waited for; none of its sessions are listed, not even ones it sent
  before. A host speaking another protocol version than this `shepherd` fails the command at once, saying so.
- Where a command runs never matters to the host: unlike the TUI, it never makes the host select the repo it runs in as
  its workspace. Only `new --cwd` passes a directory.

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
worktree. A relative `--cwd` is relative to the current directory; on another machine `--cwd` must be absolute (a
path on that machine), and a relative one is refused. `--name` names it (and its worktree's branch), and
the name stays: it is never replaced by one taken from the first prompt.

It fails, making nothing, when the profile is unknown, the machine is unknown or can't be reached, or the directory
doesn't exist. It returns once the session exists; its worktree may still be being made (`starting: true`), and a
session whose worktree then can't be made shows `status: "exited"` with the reason in `failed`.

## `shepherd prompt`

Has the session's host type `<text>` into its agent as one prompt (`-` reads the text from stdin), and prints
`prompted <name>` once it is typed. The host types it, as a paste and then Enter, so every window showing the session
sees it, and records it in the session's log (`_shepherd/session/prompted`, with `by: "cli"`). Newlines are kept;
other control characters, and the invisible characters Claude Code would refuse to send, are left out.

Only an agent at its prompt is typed into: one that is working or waiting on the user is refused (`<name> is busy
(running)`), and so is a session that runs no agent or isn't running. Two prompts at once: the second is refused.
`--queue` instead waits for the agent to be at its prompt, then sends (a prompt someone else got in first is waited
out too).

With `--wait` it then waits for that prompt's turn to end and prints the agent's reply: its last assistant message of
the turn, read from its transcript by the session's host. If the agent stops to ask the user instead (a permission),
or a subagent of its does, it returns at once with `wants-user` and what it asked: `<name> wants the user: <what>`.
A turn that ends with no reply of its own (nothing written since the prompt) prints `<name> is idle, with no reply`;
an agent that exits, or a session closed, before its turn ends gives `exited`. `--timeout` bounds `--queue` and `--wait` together; when it runs out the
prompt (if it was sent) stays: nothing is withdrawn.

## `shepherd reply`

Prints the session's agent's last reply, without prompting it, or `<name> has no reply yet`.

## `shepherd wait`

Waits until the session's agent is at its prompt, waits on the user, or exits, and prints its status
(`wants-user: <what it asked>` when it asked something).

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
  "pr": PR | null,
  "starting": false,                       // its worktree (or its post-create hooks) is still being made
  "failed": "why it couldn't start" | null,
  "created_at": "<time>"
}
```

`status`:
- `running`: its process runs and isn't known to be waiting: the agent is in a turn or still starting, or it runs no
  agent (a shell).
- `idle`: the agent is at its prompt with nothing to do (a new agent is, once it reports that it started).
- `wants-user`: the agent is waiting on the user (it, or one of its subagents, asked permission or a question).
- `exited`: its process ended, or it never started.

### PR

```json
{
  "number": 7,
  "state": "open" | "closed" | "merged" | "other",
  "checks": "none" | "pending" | "passing" | "failing" | "other"
}
```

`other` is a state or checks value the host recorded that this `shepherd` doesn't know (from a newer host).

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

### `prompt --json`, `reply --json`, `wait --json`

```json
{
  "session": "s_01jbx…",
  "status": "running" | "idle" | "wants-user" | "exited",
  "reply": "the agent's last reply" | null,  // prompt without --wait, wait, and a turn without one: null
  "asked": "what it asked the user" | null   // with wants-user: the permission's title, or the subagent's question;
                                             // null for a question the agent asks in its own TUI (no hook says
                                             // what it is yet)
}
```
