# Automation guidance

Read [the workflow map](CI.md), [canonical download guide](../docs/download.md)
and [manual performance guide](../docs/performance/README.md) before automation changes.

The context-graph-ai shared standard keeps correctness in `ci.yml`, development
binary delivery in `build.yml`, manual controlled performance separate, and stable
release qualification separate. Preserve every existing required check and native
HAOS qualification job. Only the explicitly guarded same-repository `dev` push
caller after `change-gate` is auxiliary to the correctness aggregate.

`build.yml` supports reusable admission plus qualified manual retry. Trusted
metadata reads committed PUBLIC pins into outputs before the local same-commit
HAOS call. Missing pins refuse before network/build; no private fallback, inherited
secrets, fork publication or visibility change. Keep both manual UIs and the HAOS
build recipe, current-tuple checks and full exact-package Docker CLI smoke.

Builds are read-only. Only the no-checkout publisher writes contents; trusted
control code comes from the exact source SHA, never an artifact. Preserve the
stationary default-head bootstrap tag, authenticated paginated draft discovery,
write-once target/source/run/attempt groups before stable files, actual asset-byte
readback, JSON last, and tip checks before every stable mutation. Prune only complete
older run groups after success; retain the newest three plus the current stable
receipt group. A name collision refuses before any asset mutation.

The publisher job alone has `queue: max`, a fixed repository group and cancellation
disabled. `scripts/ci-workflow-lint.py` validates that exact placement and value,
then removes that single line for actionlint 1.7.12 compatibility. Every other
lint check remains active; GitHub job-level queue acceptance awaits the pilot.

Run `scripts/ci-checks --portable` (Python 3.11+, PyYAML, actionlint, ShellCheck).
It runs build/routing, release API-state and packaged-smoke controls offline;
fixture bytes are never executed. Full native verification remains the root
guide's `scripts/verify` and `cargo xtask test-estate-check`, subject to the task's
execution restrictions. Neither portable checks nor CLI smoke certify hardware,
HAOS deployment, performance or hosted downloads.
