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
