# Project rules

## Commit when work is finished

When a unit of work is complete (a bug fix, feature, or requested change has
been implemented and verified), commit it without waiting to be asked again.

- If this is a *new* piece of finished work (unrelated to the tip commit, or
  the tip commit has already been pushed), create a new commit.
- If this is a *continuation* of the work in the current local, unpushed tip
  commit (e.g. addressing review feedback on the same change), amend that
  commit instead of stacking a new one.
- Never amend a commit that has already been pushed to a shared remote —
  create a new commit instead, even if the change is a continuation.
- Only stage files that are actually part of the finished work. Pre-existing
  unrelated modifications or untracked files in the working tree should be
  left alone unless the user asks for them too.

## Never kill or restart a running server

The user plays live hands against the running server while work happens in
this session. If you find the server (or any long-lived dev process) already
running, leave it running — do not stop, restart, or rebuild-and-relaunch it
to test a change, since that would drop the user's live session.

- Validate changes with the test suite, `cargo check`/`cargo build`, or a
  separate instance on a different port instead of touching the live process.
- If restarting the server is genuinely required to proceed, stop and ask the
  user first rather than doing it unilaterally.

## Always fix failing tests when encountered

If running the test suite (for any reason — verifying unrelated work, a full
run, CI) turns up a failing test, investigate and fix it as part of the
current session rather than just reporting it and moving on, even if it's
unrelated to the task at hand.

- Fix the root cause in the code under test; only change the test itself if
  the test's expectation is actually wrong (e.g. stale after a deliberate
  behavior change).
- If a failure looks flaky (passes in isolation, fails under parallel
  execution) or the fix is genuinely ambiguous/risky, say so and ask before
  proceeding rather than guessing.
- Still respect "never kill or restart a running server" above — fixing a
  test never requires touching the live process.
