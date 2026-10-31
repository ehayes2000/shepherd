# Claude Code hook fixtures

Hand-written hook payloads (stdin JSON) for `tests/claude_hooks.rs` and `tests/subagents.rs`. They were not recorded
from a real `claude` run.
Each follows the schema in the Claude Code hooks reference:

- Source: <https://code.claude.com/docs/en/hooks> (also `hooks.md`), read 2026-10-29; the subagent hooks
  (`SubagentStart`, `SubagentStop`), the `Agent` and `AskUserQuestion` tool inputs, re-read 2026-10-31.

## Documented vs inferred

Documented means the field name, type and meaning are in the reference, and its value is the reference's example
value or one the reference lists. Inferred means a value we picked that fits the documented type.

| fixture | documented | inferred |
|---|---|---|
| all | common fields `session_id`, `transcript_path`, `cwd`, `hook_event_name`, and `prompt_id`, `permission_mode`, `effort` where they appear | the `session_id`, path and `prompt_id` values, and which optional common fields each fixture carries |
| `SessionStart.{startup,compact,clear}` | `source` (`startup`, `compact`, `clear`), `model` | `def456` as the new session id after `/clear`, and its transcript `def456.jsonl` (the documented layout names a transcript by its session: `projects/<project>/<session>.jsonl`) |
| `UserPromptSubmit`, `.second` | `prompt` | prompt texts |
| `PreToolUse.Bash` | `tool_name`, `tool_input` (Bash: `command`, `description`, `timeout`, `run_in_background`), `tool_use_id` | `toolu_01ABC123` |
| `PreToolUse.Read` | `tool_input.file_path` (always absolute) | path, `toolu_02DEF456` |
| `PermissionRequest.Bash` | `tool_name`, `tool_input`, `permission_suggestions`, and no `tool_use_id` | that its `tool_input` equals the matching `PreToolUse`'s (the translator relies on this to name the tool) |
| `PostToolUse.Bash` | `tool_response` (Bash: `stdout`, `stderr`, `interrupted`, `isImage`), `tool_use_id`, `duration_ms` | output values |
| `PostToolUseFailure.Bash` | `error`, `is_interrupt`, `duration_ms`, `tool_use_id` | |
| `PermissionDenied.Bash` | `tool_use_id`, `reason` | that it shares a `tool_use_id` with a `PreToolUse` (the reference says denials fire `PreToolUse`) |
| `Notification.*` | `message`, `title` (optional), `notification_type` (`permission_prompt`, `idle_prompt`, `auth_success`) | message texts |
| `Stop` | `stop_hook_active`, `last_assistant_message`, `background_tasks`, `session_crons` | |
| `StopFailure` | `error` (`rate_limit`), `error_details`, `last_assistant_message` | |
| `PreCompact` | `trigger`, `custom_instructions` (`null` when none given) | |
| `PostCompact` | `trigger`, `compact_summary` | |
| `SessionEnd.{other,clear}` | `reason` | |
| `Stop.continued` | `stop_hook_active: true` (Claude is continuing because a Stop hook blocked the previous `Stop`) | |
| `PreToolUse.Grep.subagent`, `PermissionRequest.Grep.subagent` | `agent_id`, `agent_type` (present when the hook fires inside a subagent), Grep `pattern` | `agent-7f3a`, `Explore`, `path`, `toolu_03GHI789` |
| `CwdChanged` | `old_cwd`, `new_cwd` (stands in for any hook without a mapping) | |
| `PreToolUse.Agent`, `PostToolUse.Agent` | tool `Agent` ("Spawns a subagent"), `tool_input` `prompt`, `description`, `subagent_type`; a foreground call's `tool_response` `status: completed`, `agentId`, `content`, `resolvedModel`, `totalTokens`, `totalDurationMs`, `totalToolUseCount`, `usage` (the reference's example values) | the texts, `toolu_04JKL012`; that `agentId` is the same id as the subagent's hooks' `agent_id` (the reference calls it the "identifier for the subagent run") |
| `PostToolUse.Agent.background` | a background launch's `tool_response`: `status: async_launched`, `agentId`, `description`, `prompt`, `outputFile`, `resolvedModel`, and no usage fields; it returns "immediately" | the `outputFile` path; that this `PostToolUse` can come before the subagent's `SubagentStart` |
| `PostToolUseFailure.Agent` | `error`, `is_interrupt` | the error text |
| `PreToolUse.Agent.second`, `PostToolUse.Agent.second`, `SubagentStart.second` | as above: a second `Explore` call in parallel | `agent-8b4c`, `toolu_07STU901`, the texts |
| `PreToolUse.Agent.default`, `SubagentStart.default` | `prompt` alone (`description` and `subagent_type` left out) | that the `Agent` tool then runs `general-purpose`; `agent-a1b2` |
| `SubagentStart` | `agent_id`, `agent_type` (the matcher's value: `general-purpose`, `Explore`, `Plan`, custom names); no task field | `agent-7f3a` (the id the subagent fixtures already use; the reference's own examples use both `agent-abc123` and `def456`) |
| `SubagentStop` | `stop_hook_active`, `agent_id`, `agent_type`, `agent_transcript_path` (in a nested `subagents/` folder), `last_assistant_message`, `background_tasks`, `session_crons` | the transcript file name `agent-7f3a.jsonl` |
| `SubagentStop.internal` | internal agents (prompt suggestions, `/btw`) fire `SubagentStop` with `agent_type` `""` when the session runs without `--agent` | `agent-9c1d` |
| `SubagentStart.internal` | | that an internal agent's `SubagentStart`, if it fires, also has an empty `agent_type` (the reference only says so of `SubagentStop`) |
| `PostToolUse.Grep.subagent` | as `PreToolUse.Grep.subagent`; `tool_response` | the response shape |
| `PreToolUse.AskUserQuestion.subagent` | tool `AskUserQuestion`, `tool_input.questions[]` with `question`, `header`, `options`, `multiSelect` (the reference's example) | that a subagent can ask one, and its `PreToolUse` fires as the question appears |
| `PreToolUse.Agent.hostile`, `SubagentStart.hostile` | | escape sequences, NUL, CR/LF, bidi override and zero-width text in `subagent_type`/`agent_type` and `description` |

## Facts from the reference the translator relies on

- `Stop` "does not run if the stoppage occurred due to a user interrupt". No hook fires on Esc. After an interrupt
  the turn ends at the next `UserPromptSubmit`, or at `Notification` `idle_prompt`, which fires about 60 s after Claude
  finishes responding if the user hasn't typed. The reference doesn't say whether an interrupted turn counts as having
  finished responding.
- `PermissionRequest` fires when the dialog appears. The `permission_prompt` notification follows about 6 s later,
  and only if the user hasn't typed. A sandboxed command's network request fires only the notification.
- `SessionStart` fires with `source: compact` after compaction, inside the same process. `/clear` fires `SessionEnd`
  (`reason: clear`) and then `SessionStart` (`source: clear`) with a new `session_id`.
- API errors end the turn with `StopFailure` instead of `Stop`.
- A Stop hook can block `Stop` (or `/goal` can), and Claude then keeps working and fires `Stop` again with
  `stop_hook_active: true`, up to 8 consecutive continuations.
- A hook can block `PreCompact`, and then no `PostCompact` follows.
- Subagents: `SubagentStart` fires "when a subagent is spawned", `SubagentStop` "when a subagent finishes". Hooks
  inside a subagent carry its `agent_id` and `agent_type`. `SubagentStart` has no task, so the translator pairs it
  with the `Agent` tool call that spawned it: exactly, by the `agentId` the call's `PostToolUse` names (a foreground
  call's when the subagent is done, a background launch's at once); before that, by type when only one open call
  has it (inferred: that the call's `PreToolUse` comes before the subagent's `SubagentStart`). A `SubagentStop` hook can keep a subagent from stopping (exit code 2).
- `idle_prompt` doesn't fire while a background subagent is still running.
