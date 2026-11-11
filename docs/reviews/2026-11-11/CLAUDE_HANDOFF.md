# Harness review handoff for Claude

All 12 assigned harness findings were reviewed independently against main at `e31f3626ce06726cf8cfe654e6699b1d5c3b1401`. Nine are fixed, two were already fixed, and one valid finding remains blocked on its authorization design. Start with the [complete finding ledger](FINDING_LEDGER.md), then the [validation summary](VALIDATION.md) and [commit mapping](COMMIT_MAPPING.md).

This work is published on `codex/harness-findings-20261111` for final review. Main remains unchanged. No PR, merge, auto-merge, or deployment was performed. Keep the Agent Workspace organizer paused and its tasks unchanged.

## Review directly from GitHub

[Branch](https://github.com/Mischa-dev/harness/tree/codex/harness-findings-20261111).
[Implementation-only comparison](https://github.com/Mischa-dev/harness/compare/e31f3626ce06726cf8cfe654e6699b1d5c3b1401...fe2d68239c0341b39e97e98705333ec798d19164).

The implementation head is `fe2d68239c0341b39e97e98705333ec798d19164`. This review-documentation commit follows it and changes only Markdown in this directory. To inspect the branch locally in an existing clone:

```sh
git fetch origin codex/harness-findings-20261111
git switch --detach origin/codex/harness-findings-20261111
git log --oneline e31f3626ce06726cf8cfe654e6699b1d5c3b1401..HEAD
git diff e31f3626ce06726cf8cfe654e6699b1d5c3b1401..fe2d68239c0341b39e97e98705333ec798d19164
```

For a new checkout, clone `https://github.com/Mischa-dev/harness.git` with `--branch codex/harness-findings-20261111`. No cloud-local files or bundle imports are required.

## Implementation commits

1. [21ef792](https://github.com/Mischa-dev/harness/commit/21ef79289137f1a20951ec62f76f0b42d50da21a) fixes dot-root and unresolved-symlink path containment. Resolution errors fail closed.
2. [e034565](https://github.com/Mischa-dev/harness/commit/e034565fd57f261b5b741bc9881d1a0c21b3f7d0) preserves provider reasoning details, accepts null tool lists, explicitly rejects unsupported legacy function calls, and routes malformed tool arguments through normal executor feedback. Wrong-type OpenAI argument objects cannot become executable arguments.
3. [fe2d682](https://github.com/Mischa-dev/harness/commit/fe2d68239c0341b39e97e98705333ec798d19164) rejects blank scenario event names and non-file state objects before changing permissions.

The earlier uncompiled provider patch was reused selectively after source review and failing-before/passing-after tests. Provider fixtures are synthetic, following this repository's fixture convention. No live provider calls or credentials were used. The lockfile is unchanged.

## Remaining decisions and testing limits

H07 is valid and unfixed. memory_write and journal_write declare only a memory-path write but also run Git against history outside workdir. That separation intentionally prevents generic tools from editing Git configuration. Simply adding WritePath(history) would deny ordinary memory writes. A complete fix needs a narrowly scoped runtime-state effect or persistence authorization service, declared Git process effects, and tests proving denied Git never starts. See the ledger for evidence and suggested scope. No fix was accepted without settling that design.

Review the deliberate legacy-protocol choice: responses containing deprecated function_call now fail clearly instead of silently losing the action. Full legacy request/result support was not added. Also assess null as the invalid sentinel for wrong-type OpenAI argument fields.

The normal full suite reports 343 passed. The strict sandbox suite reports nine failures, all reproduced on clean main. The cloud host blocks bubblewrap's NETLINK_ROUTE socket. Re-run the strict suite where bubblewrap works before approving a merge. Real-provider, deployment, Windows, and Rust 1.88 testing were not run. See VALIDATION.md for every check, baseline comparison, and scenario score.

Review all three implementation commits and the blocked finding before choosing what to merge. No implementation changes are hidden in this documentation commit. The full multi-repository source handoff, unrelated private information, large logs, archives, and binaries are not included here.
