# Claude Code hook fixtures

Hand-written hook payloads (stdin JSON) for `tests/claude_hooks.rs`. They were not recorded from a real `claude` run.
Each follows the schema in the Claude Code hooks reference:

- Source: <https://code.claude.com/docs/en/hooks> (also `hooks.md`), read 2026-10-29.

## Documented vs inferred

Documented means the field name, type and meaning are in the reference, and its value is the reference's example
value or one the reference lists. Inferred means a value we picked that fits the documented type.

| fixture | documented | inferred |
|---|---|---|
| all | common fields `session_id`, `transcript_path`, `cwd`, `hook_event_name`, and `prompt_id`, `permission_mode`, `effort` where they appear | the `session_id`, path and `prompt_id` values, and which optional common fields each fixture carries |
| `SessionStart.{startup,compact,clear}` | `source` (`startup`, `compact`, `clear`), `model` | `def456` as the new session id after `/clear` |
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
| `CwdChanged` | `old_cwd`, `new_cwd` (stands in for any hook without a mapping) | |

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
