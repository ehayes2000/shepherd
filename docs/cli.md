# Shepherd from the command line

Commands for scripts and agents (decision 0022): see every session, start one, prompt them, answer them, stop them,
arrange the screen and see it drawn. Each command is a short-lived client of this machine's host (started if it isn't
running), and reaches the paired machines through it, as the TUI does. An agent gets the same commands as MCP tools
from [`shepherd mcp`](#shepherd-mcp).

```
shepherd sessions                                                        [--json]
shepherd new [--profile P] [--machine M] [--cwd DIR] [--name N]          [--json]
shepherd prompt <session> <text | -> [--wait] [--queue] [--timeout T]    [--json]
shepherd reply <session>                                                 [--json]
shepherd wait <session> [--timeout T]                                    [--json]
shepherd ui get                                                          [--json]
shepherd ui page new [NAME]                                              [--json]
shepherd ui page select|close <page>                                     [--json]
shepherd ui page rename <page> <name>                                    [--json]
shepherd ui split <pane> right|down [--ratio R] [--session S]            [--json]
shepherd ui put <pane> <session|view>                                    [--json]
shepherd ui close <pane>                                                 [--json]
shepherd ui ratio <pane> R                                               [--json]
shepherd ui screenshot [--size WxH] [--page P]                           [--json]
shepherd screen <session> [--lines N]                                    [--json]
shepherd answer <session> <yes | always | no | N> [--asked SEQ]          [--json]
shepherd stop <session> [--force]                                        [--json]
shepherd mcp
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

MCP tool `status`: input `{}`; output [`sessions --json`](#sessions---json). A session's `question` names what its
agent waits on the user for, by the seq `answer` takes.

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

MCP tool `new_session`: input `{ profile?: string, machine?: string, cwd?: string, name?: string }`, each its flag (a
relative `cwd` is relative to where `shepherd mcp` runs); output [`new --json`](#new---json).

## `shepherd prompt`

Has the session's host type `<text>` into its agent as one prompt (`-` reads the text from stdin), and prints
`prompted <name>` once it is typed. The host types it, as a paste and then Enter, so every window showing the session
sees it, and records it in the session's log (`_shepherd/session/prompted`, with `by: "cli"`; from the MCP tool,
[`mcp:<session>`](#shepherd-mcp)). Newlines are kept;
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

MCP tool `send`: input `{ session: string, text: string, wait?: bool, queue?: bool, timeout?: integer }` (`timeout` in
seconds); output [`prompt --json`](#prompt---json-reply---json-wait---json). A timeout is an error, as for the command.

## `shepherd reply`

Prints the session's agent's last reply, without prompting it, or `<name> has no reply yet`.

## `shepherd wait`

Waits until the session's agent is at its prompt, waits on the user, or exits, and prints its status
(`wants-user: <what it asked>` when it asked something).

## `shepherd ui`

The screen's arrangement (decision 0022): pages of split panes, owned by this machine's host and shown by every
window on it, each at its own size. `ui get` prints it; every other `ui` command but `screenshot` applies one change
through the host and prints the layout the host answers with. A change the host refuses (an unknown pane, a pane that
isn't split, a page name that is blank or too long, too many pages or panes) is the error, and changes nothing.

- `<pane>` is a pane's id, as `ui get` prints it. `<page>` is a page's id, or its name when no page has that id and
  exactly one has that name (default page names are numbers, so an id wins).
- `page new` adds a page after the others, with one empty pane, and shows it; unnamed, it is numbered. `page select`
  shows a page; `page close` closes it and everything on it (closing the last page leaves a fresh one).
- `split` divides a pane in two: it stays as the first half (`a`, left or top), taking `R` of the space (0.5 by
  default), and the new second half (`b`) shows `--session S`, or nothing. `ratio` sets a split's `R`. A ratio is a
  fraction from 0 to 1; the host keeps it within 0.05..0.95.
- `put` shows something in a pane that isn't split: `empty`, `selected` (each window's own sidebar selection, what the
  default layout's pane shows), a view (`status`, `machines`, `pm-panel`, `cleanup`, `archive`), or else a session by
  [id or name](#naming-sessions-and-machines). A view's name wins over a session's: name such a session by its id.
- A session is pinned for every window. One on a paired machine is pinned with its machine.
- `close` closes a pane and everything in it: its sibling takes its place. Closing a page's last pane leaves one empty
  pane.

The human form is one line per page, `page <id>: <name>` (`page <id> (shown): <name>` for the shown one), and under it
its panes, indented by depth: `split <id> right|down <R>`, then its two halves; `pane <id>: <content>`, where content is
`empty`, `selected`, `view <name>` or `session <name> (<id>) on <machine>`.

### `shepherd ui screenshot`

Draws the window a user would see at `--size` (120x40 by default; each from 1 up to 1000x500), with the TUI's own
renderer, headless, and prints it as text, one line per row without trailing blanks, display-safe, concealed text
(SGR 8) blank. It shows the shown page, or `--page P` instead (the host's shown page stays as it is). It changes
nothing: no session is resized.

- The sidebar is a newly opened window's: it selects the first session it lists (this machine's newest). A
  `selected` pane shows that session.
- Each session's terminal is the host's current snapshot of it, at its own size, cut to the pane: rows from the top
  as a terminal made shorter cuts them (the cursor's row stays in sight), columns from the right, never rewrapped. A
  terminal over 400x200 isn't shown whole. A session that has printed nothing, or has exited, shows what the TUI shows
  for it.

## `shepherd screen`

Prints what the session's terminal shows now, as text, the way a user would read it: its host reads it from its own
terminal emulator, at the session's own size, so colours and other control sequences are gone, a wide character is
one character (never followed by a blank for the cell it covers), concealed text (SGR 8) is blank, and each line is
display-safe and has no trailing blanks. `--lines N` adds up to `N` lines of scrollback above the screen (none on an
agent's full-screen view, which keeps none). The human form leaves out the blank rows below the last one drawn;
`--json` gives every row.

Bounded, so asking can't make a host build a huge answer: at most 1000 lines of scrollback (as many as the host keeps;
asking for more gives what there is), and at most 1 MiB of text, past which the oldest lines are left out
(`truncated: true`). Reading a screen is allowed to paired machines, as reading a reply is: they already see the
session's screen when they show it. A session that isn't running (it exited, or its worktree is still being made) is
refused.

MCP tool `read_pane`: input `{ session: string, lines?: integer }`; output [`screen --json`](#screen---json).

## `shepherd answer`

Answers the permission prompt or question the session's agent (or a subagent of its, whose dialog is in the session)
is blocked on, without attaching, and prints `answered <name> <choice>: <what it asked>`. The host types the agent's
own keys for the choice into the session, so every window sees the dialog close, and records it in the session's log
(`_shepherd/session/answered`, with `by: "cli"`; from the MCP tool, [`mcp:<session>`](#shepherd-mcp)).

- A permission takes `yes`, `always` (yes, and don't ask again) or `no`, where that dialog offers them. A choice the
  dialog doesn't offer, or that shepherd can't tell it offers, is refused rather than guessed: Claude Code takes
  `always` only on a file prompt (Edit, Write); Codex takes `always` only on a patch, and `no` on a patch or a command.
  For a Codex command, `no` cancels: Codex stops its turn and waits for you to tell it what to do differently (its
  command dialog offers no "no, but carry on").
  `yes` is always offered.
- A question takes the number of one of its options, from 1 in the order the agent lists them, or `no` to cancel it.
  A question that takes several picks, or asks several questions at once, can only be answered in the pane.

The answer names the question it means: the seq of its `_shepherd/agent/asked` entry in the session's log, as this
command last saw it, or `--asked SEQ`. Only that question, still pending as the host sees it when the answer arrives,
is answered. Otherwise nothing is typed and it is refused with the session's status: `<name> has nothing to answer
(<status>)`. That covers an agent that asked nothing, one that has moved on (or asked something else since), one whose
question the user is answering in the pane (any key typed into the session after it asked, including moving around
the dialog, takes the question away from shepherd), and one that opened a second dialog while another was up (the
agents queue them, so keys could land in the first). The last two stay `wants-user`, unanswerable here, until the
agent's activity shows the dialogs closed (or, for a dialog about no tool the agent named, until its turn ends). The
agent and a subagent asking about the same command are two dialogs. A choice the question doesn't take (`yes` for a
question, a number past its last option) is refused too, saying why.

MCP tool `answer`: input `{ session: string, choice: "yes" | "always" | "no" | integer, asked?: integer }` (`choice`
an option's number from 1; `asked` the question's seq, as `status` shows it in the session's `question`, like
`--asked`); output [`answer --json`](#answer---json). Pass `asked` to be sure the answer lands on the question it was
chosen for.

## `shepherd stop`

Stops a session, as the sidebar's Close does: its host kills its process at once and closes it. Like the Close dialog,
it first asks the host what removing the session's worktree would lose: its uncommitted changes, and its commits that
are neither pushed nor merged. Nothing: the session is closed and the worktree removed (after its pre-remove hooks;
should it have something to lose by then, it is kept). Something, or the host can't tell: nothing is closed, and it is
refused saying what (`<name> has unsaved work in its worktree (uncommitted changes): it was not stopped; …`). With
`--force` it is stopped anyway and its worktree **kept**: `stop` never throws work away (the cleanup view can remove
the worktree later). A session with no worktree (or one still being made) is just closed. It prints `stopped <name>`,
saying what happens to the worktree, once the host has started closing it; the session leaves the list once its
worktree is dealt with.

MCP tool `stop`: input `{ session: string, force?: bool }`; output [`stop --json`](#stop---json).

## `shepherd mcp`

Serves the commands above to an agent as MCP tools, over stdin and stdout (newline-delimited JSON-RPC, MCP's stdio
transport), until the agent hangs up. Configure it as a stdio MCP server whose command is `shepherd mcp`. Its tools
mirror the commands one to one, and run the commands' own code: each call is a short-lived client of this machine's
host (started if it isn't running), as a command is, so a tool can do nothing its command can't.

| tool | command | input | output |
|---|---|---|---|
| `status` | `sessions` | `{}` | [`sessions --json`](#sessions---json) |
| `read_pane` | `screen` | `{ session, lines? }` | [`screen --json`](#screen---json) |
| `send` | `prompt` | `{ session, text, wait?, queue?, timeout? }` | [`prompt --json`](#prompt---json-reply---json-wait---json) |
| `answer` | `answer` | `{ session, choice, asked? }` | [`answer --json`](#answer---json) |
| `new_session` | `new` | `{ profile?, machine?, cwd?, name? }` | [`new --json`](#new---json) |
| `stop` | `stop` | `{ session, force? }` | [`stop --json`](#stop---json) |

- `tools/list` gives each tool's input and output as JSON Schema (generated from the types the commands print, so
  they can't drift from the shapes below). Arguments are each command's flags by name; unknown arguments, or a
  missing required one, are refused as invalid params (`-32602`), as is an unknown tool.
- A call's result is its command's `--json` output, as `structuredContent` and as text. A call its command would fail
  is a tool error (`isError: true`) whose text is the command's error (what it would print after `shepherd: `),
  timeouts included.
- Calls are served one at a time, in order: a `send` that waits holds later calls until it returns.
- Who asked: what `send` and `answer` type is recorded in the session's log with `by: "mcp:<session>"`, the session
  the server runs in, named by `SHEPHERD_SESSION` (shepherd sets it, with `SHEPHERD_SOCK`, in every session it
  spawns), or `by: "mcp"` when the server wasn't started from a session. `new_session` and `stop` record no origin
  (the host's session-new and session-close requests don't carry one).

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
  "created_at": "<time>",
  "question": Question | null              // what its agent waits on the user for, that `answer` can answer
}
```

Question:

```json
{
  "seq": 12,                               // its `_shepherd/agent/asked` entry's seq: `answer --asked` takes it
  "text": "what it asks",                  // the tool and its main argument, or the question
  "choices": ["one", "two"] | null         // a question's options in order (answered by number, from 1);
                                           // null for a permission, or a question only the pane can answer
}
```

`question` is `null` when nothing is pending that `answer` could answer, including while the user is answering in the
pane: a session can be `wants-user` with no `question`.

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
  "asked": "what it asked the user" | null   // with wants-user: the permission's title, the agent's question, or
                                             // the subagent's question
}
```

### Layout

Pages in page-bar order; `current` is the shown page's id. Ids are whole numbers, unique among pages and panes.

```json
{ "current": 1, "pages": [ Page, … ] }
```

Page: `{ "id": 1, "name": "1", "root": Pane }`.

Pane, one of:

```json
{ "kind": "split", "id": 3, "dir": "right" | "down", "ratio": 0.5, "a": Pane, "b": Pane }
{ "kind": "leaf", "id": 2, "content": Content }
```

`right`: `a` on the left, `b` on the right. `down`: `a` above, `b` below. `ratio` is `a`'s share, 0.05..0.95.

Content, one of:

```json
{ "kind": "empty" }
{ "kind": "selected" }                     // each window's own sidebar selection
{ "kind": "session", "id": "s_01jbx…", "name": "api" | null, "machine": "studio", "machine_id": "<machine id>" }
{ "kind": "view", "view": "status" | "machines" | "pm-panel" | "cleanup" | "archive" | "<a view this build doesn't know>" }
```

A session's `name` is `null` when it isn't listed (its machine can't be reached). `machine` and `machine_id` name the machine whose host
runs it; for one of this machine's own they are this machine's, or `""` (empty) if its host hasn't said which machine it
is, which a command that has reached it never sees.

### `ui … --json`

Every `ui` command but `screenshot`:

```json
{ "layout": Layout }
```

### `ui screenshot --json`

```json
{
  "cols": 120,
  "rows": 40,
  "page": 1,                               // the page drawn
  "panes": [                               // each pane of it, where it is drawn on the window, its border included
    { "id": 2, "content": Content, "selected": true, "x": 24, "y": 0, "cols": 96, "rows": 40 }
  ],
  "lines": [ "…", … ]                      // the screen, one string per row, as the human form prints it
}
```

A pane's `content` is what it shows: for a pane showing the selection (`selected: true`), the session selected (or
`empty` with none).

### `screen --json`

```json
{
  "session": "s_01jbx…",
  "cols": 80,                              // the session's terminal size
  "rows": 24,
  "scrollback": 3,                         // how many of `lines` are scrollback, before the screen's rows
  "lines": ["…", …],                       // scrollback (oldest first), then every row of the screen, top to bottom
  "truncated": false                       // past 1 MiB of text the oldest lines are left out, so `lines` may
                                           // start below the screen's top row
}
```

### `answer --json`

```json
{
  "session": "s_01jbx…",
  "choice": "yes" | "always" | "no" | 2,     // as given: a word, or an option's number from 1
  "asked": "what it asked the user" | null,  // as this command's client last saw it; the host decided
  "question": 12 | null                      // the seq of the `agent/asked` entry it named (--asked, or as seen)
}
```

A refused answer is an error (stderr, exit status 1). On the wire it is `_shepherd/session/answer { session, choice,
asked?, by? }`, refused with error code 1003 and `data: { session, status }` when the question isn't pending.

### `stop --json`

```json
{
  "session": "s_01jbx…",
  "worktree": "remove" | "keep" | null,    // what happens to its worktree; null: it has none
  "unsaved": ["uncommitted changes", "2 commits not pushed or merged"]
                                           // with --force: what the kept worktree holds; otherwise []
}
```

A refused stop is an error (stderr, exit status 1), and closes nothing. On the wire it is
`_shepherd/worktree/status { session }`, then `_shepherd/session/close { session, worktree: "keep"? }`.
