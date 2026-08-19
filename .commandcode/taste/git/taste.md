# Taste — git

- Commit messages in Portuguese, conventional style with scope (e.g. `fix(core): ...`, `refactor(clipsyncd): ...`). Multi-line with body for complex changes is fine. Confidence: 1.0
- Subagent commits include `Co-authored-by: CommandCodeBot <noreply@commandcode.ai>` trailer; orchestrator commits do not. Confidence: 0.9
- Develops each issue in its own worktree with a dedicated branch (e.g. `fix-N-<slug>`); issues touching overlapping files are grouped into a single agent to minimize merge conflicts. Confidence: 0.9
- Uses a `develop` branch as integration target; worktrees branch off develop, merges land there first, then a single PR goes to main. Confidence: 0.9
- Always base new worktrees on the latest integration branch (`git fetch` before creating). Confidence: 0.9
- Merges happen sequentially with `git merge --no-ff`, validating (fmt/clippy/build/test) after each merge; when merging a branch based on a pre-protocol-change commit, update harness helpers to the new protocol, and keep both test sets when branches added different tests to the same file. Confidence: 0.9
- After a wave: close issues with a "Resolvido no PR #N — <resumo>" comment, remove worktrees with `--force`, delete branches locally and on remote, and clean up the worktrees directory. Confidence: 0.9
