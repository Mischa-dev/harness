# Harness validation summary

Tested implementation head: `fe2d68239c0341b39e97e98705333ec798d19164`. Its complete file tree is identical to the tested local head `2acdc67756b5b98894fac3e05ab0e87283b4a577`. The later review-documentation commit changes Markdown only.

Official Rust stable was installed in this task's isolated directories as authorized. rustc is 1.99.1; The toolchain was rustc 1.99.1 and cargo from the same stable installation. The lockfile was unchanged. Full final checks used RUSTFLAGS=-D warnings.

| Check | Outcome |
| --- | --- |
| Full baseline suite on pristine base | 331 passed, 0 failed, 2 ignored helper tests |
| Added regressions before production fixes | 10 failing tests reproduced the nine accepted findings |
| Affected suites after fixes | 217 passed, 0 failed, 1 ignored helper test |
| cargo fmt --all --check | passed |
| cargo check --workspace --locked | passed |
| cargo clippy --workspace --all-targets --locked -- -D warnings | passed |
| cargo test --workspace --locked --no-fail-fast | 343 passed, 0 failed, 2 ignored helper tests |
| HARNESS_REQUIRE_SANDBOX=1 cargo test --workspace --locked --no-fail-fast | 334 passed, 9 failed, 2 ignored; all nine fail identically on clean base |
| cargo test -p harness-providers --no-default-features --locked | 34 passed |
| RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --locked | passed |
| git diff --check | passed |
| Bundle and patch imports | both verified in a separate clean checkout; identical final tree |

The two ignored helpers run in subprocesses from their non-ignored parent tests. The normal suite also returns early from nine sandbox tests when bubblewrap is unusable. The strict suite exposes these as failures. A direct bubblewrap probe failed with `loopback: Failed to create NETLINK_ROUTE socket: Operation not permitted`. Clean-base comparisons reproduce all eight tools failures and the daemon shell failure, with the same diagnostic. No tests were disabled or edited to hide these failures. The exact nine matching failures are listed below.

The three fake-provider exam runs pass:

- quiet-week: 22 wakes, 1 message; filter 4 looks and 3 dropped events.
- busy-group-chat: 5 wakes, 2 messages; filter 13 looks and 11 dropped events.
- outage-in-channel: 4 wakes, 0 messages; filter 5 looks, 1 early wake and 3 dropped events.

Real-provider calls, deployment tests, Windows testing, and an additional Rust 1.88 minimum-version run were not run. No source prompts, loop implementation, wake filter, or state view changed. The loop changes are regression tests only.

## Strict sandbox failures reproduced on clean main

All failed with `bubblewrap isn't working here`:

- `shell::tests::a_command_that_runs_too_long_is_stopped`
- `shell::tests::a_protected_path_that_is_a_link_stops_the_command`
- `shell::tests::commands_change_only_the_work_area`
- `shell::tests::credentials_are_hidden_at_any_depth`
- `shell::tests::long_output_keeps_its_start_and_end`
- `shell::tests::protected_paths_are_read_only_even_before_they_exist`
- `shell::tests::runs_a_program_and_reports_what_happened`
- `shell::tests::the_environment_holds_nothing_from_the_runtime`
- `the_shell_works_in_the_work_area_and_leaves_memory_alone`

The unchanged base comparison commands were `HARNESS_REQUIRE_SANDBOX=1 RUSTFLAGS="-D warnings" cargo test -p harness-tools --lib --locked` and the same environment with `cargo test -p harness-daemon --test agent --locked`. They returned exit 101 with the same eight tools failures and one daemon failure.

No approval denial caused these failures. The host prevented bubblewrap from creating a NETLINK_ROUTE socket. Re-run the strict full suite on a host where bubblewrap works before approving a merge. Real-provider, deployment, Windows, and Rust 1.88 minimum-version testing remain unperformed.

## Reproduce the checks from the branch

```sh
export RUSTFLAGS='-D warnings'
cargo fmt --all --check
cargo check --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
HARNESS_REQUIRE_SANDBOX=1 cargo test --workspace --locked --no-fail-fast
cargo test -p harness-providers --no-default-features --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked
cargo run -p sim --locked -- run quiet-week --provider fake
cargo run -p sim --locked -- run busy-group-chat --provider fake
cargo run -p sim --locked -- run outage-in-channel --provider fake
```

No large logs, binaries, or full source handoff are stored with these review documents. Validation after the documentation-only commit consists of checking the changed-file set and reading each published Markdown file back. Implementation tests are tied to the unchanged implementation tree above.
