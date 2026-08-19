# Taste — workflow

- Subagents never close issues, merge, push to main, or run `cargo clean` — those actions are reserved for the orchestrator. Confidence: 1.0
- Always creates an issue for any finding (bug, refactor, code-quality); issue titles carry type/severity prefixes like `bug HIGH:`, `security CRITICAL:`, `refactor MEDIUM:`, `test MEDIUM:`. Confidence: 1.0
- Mandatory validation before committing: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`, `cargo test --workspace`; only fix lints introduced by one's own change. Confidence: 1.0
- Likes subagent briefs to include: restriction to the worktree path, root cause with exact files/lines, a suggested strategy, acceptance criteria, and a return report (changed files, strategy, validation results, commit hashes). Confidence: 0.8
- Orchestrator drives each wave end-to-end on a direct go-ahead: analyze open issues with the `gh` CLI, define groups by file overlap, then launch and delegate to subagents in parallel. Confidence: 0.8
- PR review comments (Copilot or human) are addressed in a single commit on the PR branch: fix all points, run full validation suite, push, and wait for CI green before considering resolved. Confidence: 0.9
- After merging a batch of refactors, runs a thermo-nuclear code quality review as a background agent; its findings become GitHub issues with severity labels (HIGH/MEDIUM). Confidence: 0.9
