---
name: fix-bug
description: Fix a bug in shepherd by first writing a failing regression test at the lowest layer that reproduces it. Use for any bug report, crash, or misbehavior, including ones found mid-task.
---

# Fixing a bug

1. **Reproduce in a test before touching the code.** Pick the lowest layer that can see the bug (see `tdd`):
   - wrong decision or state → core test via `App::handle`
   - wrong pixels → render snapshot
   - adapter misbehaves vs. what core assumed → add a case to the port's **contract suite**. That way both
     the fake and the real adapter are held to it, and the fake can't hide the bug again.
   - only shows up across components → scenario test, then try to push it down a layer
2. Run it and **watch it fail for the bug's reason**. If you can't make it fail, you haven't found the bug yet.
   Keep digging and don't fix blind.
3. Name it after the bug's behavior: `sidebar_keeps_selection_when_session_above_is_removed`.
4. Fix the cause with the smallest change that does it. Don't patch the symptom in the layer where it showed up.
5. Run `scripts/check.sh`. The regression test is never deleted later.

Commit as `fix: <behavior that was wrong>`. In the report, give the test name, the root cause in one sentence,
and why the existing tests didn't catch it. The last part is how the PM improves the system.

If the bug was found during another task and fixing it is outside that task's scope: still write the
failing test if it's cheap, don't fix it, and report it to the PM as a follow-up.
