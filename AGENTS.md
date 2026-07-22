# Vigil Development Contract

This file is the standing verification and integration contract for people and coding agents working
in this repository. Feature plans name product behavior and the affected feature shape; this file
owns how edits are verified and integrated.

## Use the repository verifier

Run `scripts/verify --help` and use its three explicit tiers. The launcher acquires the shared lease
before compiling the verifier itself, so its bootstrap cost is included in the receipt. Do not
substitute a handwritten chain of Cargo, nextest, Docker, or cross-build commands for these tiers.

- `change` is the frequent RED/GREEN loop. Select the exact test or focused family and the feature
  shape named by the work. RED must be a matched test failure, never a compile failure, timeout,
  harness failure, or zero-test success. Re-run the same selection for GREEN. Do not launch the full
  CI or release build for intermediate test and implementation commits.
- `dev-closeout` runs once on the reviewed final commit before it enters `dev`. It is manually
  dispatched from trusted `dev`, while its inputs name the exact feature and dependency commits to
  test. Any source/tree change invalidates the result. A successful candidate enters `dev` only by
  fast-forward.
- `release` is the owner's clean `main` qualification. It consumes exact known-good dependency
  commits and creates the complete amd64/arm64 release artifacts. It is not a development gate.

Never describe work as done or ready to merge from a focused `change` receipt alone. Never repeat a
successful exact-commit closeout merely because that commit was fast-forwarded into `dev`.

## Resource limit

Assume the normal workstation has only 10 GiB of free RAM and that other agents may be working in
parallel. Every compile-, link-, slow-test-, or image-heavy verifier lane must acquire the shared
Vigil resource lease. Let the verifier queue visibly; do not bypass it by launching Cargo directly,
moving build targets to tmpfs, or increasing Cargo jobs/test threads. Separate worktrees keep
separate target directories; compiler caches may be shared.

The temporary availability of a larger machine does not change this contract. Receipts derive cache
state from the application build outputs present before each command and must state the runner,
queue time, wall time, memory, and disk use before a budget is claimed. A caller cannot label a CI
run warm or cold.

## Commit and closeout sequence

Each RED commit must have a valid local `scripts/verify change ... --expect red` receipt for the
plan's exact selection. Each implementation commit runs the same selection with `--expect green`.
Keep the intentionally RED commit local, then push the RED/GREEN pair to a draft pull request
targeting `dev`; GitHub runs one cheap default-shape gate at the green checkpoint. Later green
checkpoints may be pushed while work continues. The full feature matrix, slow archive, containers,
and release artifacts do not run per commit or per push.

After implementation and independent review, make the tree clean, settle commit history, update the
branch against the current `dev`, and record exact Vigil, Context Graph, and ContextDB commits.
Dispatch `dev-closeout.yml` with `--ref dev` and those exact inputs. After it succeeds, dispatch
`integrate-dev.yml` with `--ref dev`, the same tuple, and the closeout run ID. The trusted integration
workflow verifies the receipts, confirms `dev` has not moved, and performs a non-forcing ref update.
Any later commit or dependency change requires a new review and closeout.

During pre-release development, GitHub's default branch is `dev`; this makes trusted manual workflow
dispatch independent of `main` and makes new pull requests target the integration line by default.
`dev` protection must enforce administrators, require linear history and the app-bound exact-head
status `vigil/dev-closeout`, reject force pushes, and permit the receipt-bound workflow's checked
fast-forward. The integration workflow refuses to update `dev` when that protection is absent.
`main` remains the release line.

A feature branch can modify the pull-request workflow that tests it, so a PR check is development
feedback, not integration authority. Only the manual workflows dispatched from `dev` can publish
the required status or update `dev`. Normal feature integration rejects changes to that trusted
control plane, including workflows, verifier/guard code, protected nextest configuration, and
`AGENTS.md`. Such changes use the separately reviewed owner bootstrap path. This initial
compartmentalization is that one-time bootstrap: fast-forward it into `dev` before enabling the new
protection. It never requires `main`.

Merging `dev` to `main`, tagging, publishing, and promoting artifacts remain owner-authorized release
actions. Development closeout never grants that authority.

## Test-estate rules

Retries are forbidden. Fix nondeterminism at its cause or record an explicitly owned quarantine with
its impact and removal condition. Tests synchronize on observable state, counters, or events—not
sleep duration or elapsed-time ratios. Every test deletion or consolidation needs identical
mutation/planted-fault detection evidence, and documentation claim bindings remain mechanically
checked.

Keep each registered feature shape unless evidence proves the product no longer supports it. Save
time by changing frequency and removing duplicate compilation/enumeration, not by silently dropping
distinct behavior.

## Dependency and repository boundaries

During development Vigil consumes the sibling `dev` checkouts of Context Graph and ContextDB. Vigil
uses storage and memory only through Context Graph's public consumer API; add a missing generic
capability at its owning upstream layer instead of creating a Vigil-side shadow.

Do not perform Git operations in external planning directories or include internal planning paths,
plan identifiers, or authoring-process shorthand in product source and user documentation. Plain
commit messages only; no co-author trailers.
