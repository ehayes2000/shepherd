# Inbox

Write anything here: a feature, a change to how something should work, a bug, a "this feels wrong". Plain words are
fine. No need to know which task, milestone or doc it belongs to.

The PM reads this file, files each item where it belongs (a new task, a change to an existing spec, the manifesto, or a
decision), and moves it to "Filed" with a pointer to where it went.

## New

-

## Filed

<!-- PM moves items here: "- <item> → <where it went>" -->
- Conversation archive (proposal `docs/proposals/conversation-archive.md`) → `docs/features/conversation-archive.md`
  (T38–T44) and decision 0020.
- Prompt agents from a CLI (manager/subagents), manage the UI from a CLI (splits, pages), test complex UI (several
  pages) → `docs/features/automation.md` (T45-T50) and decision 0022.
- TUI screens from the mocks (`docs/mocks/shepherd-tui.html`): status page → `docs/features/status-page.md` (T57-T59);
  project manager → `docs/features/project-manager.md` (T60-T61), backed by the MCP server and answer/screen tasks in
  `automation.md` (T51-T53); subagents → `docs/features/subagents.md` (T54-T56); machines → `multi-machine.md` (T62-T65,
  T32b latency). "Loops watching" dropped until loops exist.
