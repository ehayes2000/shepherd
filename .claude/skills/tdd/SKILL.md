---
name: tdd
description: The red/green/refactor loop for shepherd and where each kind of test lives (core, render, contract, scenario). Use whenever implementing or changing behavior.
---

# TDD on shepherd

## The loop
1. **Pick one behavior** from the acceptance criteria. Keep it small.
2. **Red.** Write a test for it and run only that test (`cargo test -p <crate> <name>`). It must fail, and it
   must fail *for the reason you expect*: an assertion failure or a missing item, not a typo or an unrelated
   panic. Save the test name and failure reason for your report.
3. **Green.** Write the least code that passes. Hard-coding is fine if the next test will force generality.
4. **Refactor.** With the test green, clean up names, duplication and structure. Rerun the tests.
5. Repeat. Run `scripts/check.sh` before committing.

If you write code before its test, delete the code and start from the test.

## Which layer? Pick the lowest that can see the behavior.

**Core (default).** Domain decisions, state transitions, what to do next.
```rust
let mut app = AppBuilder::new().with_workspace("~/code/shepherd").build();
let cmds = app.handle(Event::NewSession { at: t(0) });
assert_eq!(cmds, vec![Command::CreateWorktree { repo: repo("shepherd"), base: BranchSpec::OriginDefault }]);
assert_eq!(app.sessions().len(), 1);
```
Assert on returned `Command`s and on public state accessors. Never call adapters from core tests.

**Render.** What the user sees. Build an `App` in a known state, render it to `TestBackend`, and snapshot it.
```rust
let app = AppBuilder::new().with_sessions([running("fix-login"), idle("docs")]).build();
insta::assert_snapshot!(render_to_string(&app, 40, 12));
```
Name snapshots after the state they show. Check the new `.snap` by eye before accepting it with
`cargo insta review`. Never accept a snapshot diff you can't explain.

**Contract.** How a port behaves. Add cases to `shepherd-testkit::contracts::<port>::suite` so the fake and
the real adapter both run them. If the real adapter can't satisfy a case, the trait or the fake is wrong.
Fix the design, don't special-case the test.

**Scenario.** A user flow across event → command → port → event. Use the harness in
`shepherd/tests` with all-fake ports. Write one per feature, covering the happy path plus the most important
failure path. Edge cases belong in core tests.

## What makes a test meaningful
- It fails if the behavior breaks. Imagine deleting the key line of the implementation. Would this test catch it?
- It describes behavior in its name: `closing_session_with_dirty_worktree_asks_for_confirmation`.
- It survives refactors. If renaming a private function breaks it, it's testing implementation.
- One reason to fail per test. Use several small tests, not one test with 12 asserts across different behaviors.
- Use testkit builders and fakes. If you're constructing a 15-field struct by hand, add a builder method instead.

## Never
- `#[ignore]`, `sleep`, the real clock, or the network in core and tui tests.
- Editing an existing test to make your change pass without saying why in the commit and report.
- Mocking frameworks. Extend the fake instead.
