# Taste — git

- One-line commit messages in Portuguese, conventional style with scope (e.g. `fix(core): ...`, `refactor(clipsyncd): ...`); never add AI-attribution trailers. Confidence: 1.0
- Develops each issue in its own worktree with a dedicated branch (e.g. `fix-N-<slug>`); issues touching overlapping files are grouped into a single agent to minimize merge conflicts. Confidence: 0.9
- Always base new worktrees on an up-to-date main (`git fetch` before creating). Confidence: 0.9
- Merges happen sequentially with `git merge --no-ff`, validating (fmt/clippy/build/test) after each merge; when merging a branch based on a pre-protocol-change commit, update harness helpers to the new protocol, and keep both test sets when branches added different tests to the same file. Confidence: 0.9
- After a wave: close issues with a "Resolvido em <hash>: <resumo>" comment, remove worktrees with `--force`, delete branches locally and on remote, and `rm -rf` the whole worktrees directory since disk space is limited. Confidence: 0.9
