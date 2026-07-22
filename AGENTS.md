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
- `dev-closeout` runs once on the reviewed final commit before it enters `dev`. The manual workflow
  must name exact 40-character Vigil, Context Graph, and ContextDB commits. Any source/tree change
  invalidates the result. A successful candidate enters `dev` only by fast-forward.
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

## Exact closeout sequence

Before dispatching closeout, finish code and review, make the tree clean, settle commit history, and
update the feature branch against the current `dev` base. Dispatch the manual closeout workflow on
that exact branch commit with exact sibling dependency commits. Preserve its receipt and independent
review verdict.

After it succeeds, dispatch the receipt-bound `integrate-dev.yml` workflow from the exact current
`dev` ref. It re-checks the three commits, source/impact/final receipts, current `dev`, and ancestry,
then performs a non-forcing ref update. If `dev` moved, fast-forward is impossible, or any commit
changed, it stops and a new closeout is required. Direct `dev` updates outside that workflow are not
the closeout path. Merging `dev` into a feature branch after qualification, rebasing, amending, or
making a cleanup edit all invalidate the qualification.

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
