# Performance measurement

This is the one home for measuring Vigil performance: what is measured, how to run a measurement,
how to compare two revisions and how to publish a reviewed result. Overview pages link here rather
than copying these steps.

## Current state

Performance measurement is **manual**. Nothing in GitHub Actions runs a Vigil performance workload,
and nothing publishes a result automatically. The performance workflow
(`.github/workflows/performance.yml`) is a dormant placeholder: it has no active trigger and cannot
produce a green run.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No executable witness registered yet that the performance workflow is dormant and publishes nothing.` -->

A maintainer runs the existing camera workloads on one controlled native host, keeps the raw
receipts privately, compares a baseline and a candidate by hand and publishes a reviewed summary from
[the blank template](results/TEMPLATE.md).

Measured results here are evidence about one revision, workload and machine. They are not a
marketing claim about any hardware.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence scopes how measured results may be read, not a runtime behavior promise.` -->

## Acceptance bars

Vigil has **no published numeric performance threshold** today. No root product page states a
latency, throughput, frame-rate or resource number that a measurement must meet.

<!-- vigil-unenforced: classification=product-decision; reason=`Publishing no numeric performance threshold is a product position without an executable absence proof.` -->

The one published performance statement is a scope limit, in [Detection](../detection.md):

> ARM hardware images are build-proven but do not carry a real-board acceleration performance
> promise yet.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This quotes the published Detection scope limit, which the Detection page owns.` -->

So no result entry may report an acceptance `PASS` for ARM acceleration performance, and every
numeric value a run produces is a descriptive measurement without a verdict.

<!-- vigil-unenforced: classification=product-decision; reason=`Forbidding an ARM acceleration PASS verdict follows the published scope limit and has no executable check.` -->

The real-camera workload (below) asserts its own pass conditions in the acceptance test
`tests/acceptance/one_camera_acceptance.rs`, such as an ambient window of at least 30 minutes, zero
dropped motion-positive frames and processing lag within the bound the runtime reports. Those are
test assertions about that workload, not published product performance thresholds. A result entry
records whether the test passed as the run status and reports the numbers descriptively.

<!-- vigil-unenforced: classification=documentation-gap; reason=`Acceptance test pass conditions are asserted in the acceptance test; this paragraph has no registered binding yet.` -->

When a product page publishes a numeric performance threshold, add it to this section verbatim
with its statistic and scope; only then can a result entry quote it in its acceptance table.

## Why not GitHub-hosted runners

Shared hosted runners vary in CPU, disk and neighbours from run to run, so their wall-clock numbers
cannot support a paired comparison. The real-camera workload also needs a reachable camera. The
helper refuses to run a workload when `CI` or `GITHUB_ACTIONS` is set.

## The journey

### 1. Choose a scope

The helper `scripts/ci-performance` measures existing workloads only. Choose one:

| Workload | `--workload` | What it runs | Needs |
|---|---|---|---|
| Synthetic camera loop | `synthetic` | `one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` | committed video/model fixtures, ffmpeg, ffprobe, mediamtx |
| Real-camera event | `physical` | `one_camera_acceptance::frigate_replacement_real_camera_event_lands_and_walks` (ignored by default) | a real RTSP camera, an ambient window and a hard-event time window |

<!-- vigil-unenforced: classification=documentation-gap; reason=`The workload table mirrors the performance helper manifest and has no registered binding yet.` -->

Each workload runs exactly one acceptance test in the release profile. A single-workload run is a
partial, descriptive measurement of that workload, never a global performance pass.

<!-- vigil-unenforced: classification=documentation-gap; reason=`Single-workload helper behavior has no registered documentation binding yet.` -->

### 2. See what a run will do

```sh
./scripts/ci-performance manifest --output performance-manifest.json
```

`manifest` writes the build command, the per-workload test commands, the fixture paths and their
SHA-256 hashes, the declared dependency pins and the required physical inputs. It builds and runs
nothing.

### 3. Prepare a clean pinned source tuple on one controlled host

The run requires the Vigil checkout and sibling `context-graph` and `contextdb` checkouts, all clean
and at the exact revisions in `scripts/ci-dependency-pins.json`. Prepare them with
`scripts/ci-public-dependencies checkout`, and check them with
`scripts/ci-public-dependencies verify-checkouts`.

A run fails this check if either pin is missing or is not a full commit SHA; that is the
intended refusal, not a step to work around.

<!-- vigil-unenforced: classification=documentation-gap; reason=`A missing dependency pin is a refusal state with no registered binding.` -->

Run baseline and candidate on the same machine, with the same toolchain, filesystem and resource
limits, and with no competing inference, builds or heavy I/O. Do not set `RUSTFLAGS`, profile
overrides or an external Cargo config that changes compiler or profile settings: the helper rejects
them because they would change the measured binary.

<!-- vigil-unenforced: classification=external-procedure; reason=`Same-host measurement conditions are an operator procedure the helper cannot verify.` -->

### 4. Run baseline, then candidate

For each revision, allocate a new receipt directory outside every source tree and Cargo target
directory, then run:

```sh
receipt_dir="$(mktemp -d "$HOME/vigil-performance-receipts.XXXXXXXX")/run"
./scripts/ci-performance run --operator-controlled --workload synthetic --receipt-dir "$receipt_dir"
./scripts/ci-performance verify-receipt "$receipt_dir"
```

For the real-camera workload use `--workload physical` and set `VIGIL_ACCEPTANCE_RTSP_URL`,
`VIGIL_ACCEPTANCE_AMBIENT_MINUTES`, `VIGIL_ACCEPTANCE_HARD_EVENT_START_EPOCH` and
`VIGIL_ACCEPTANCE_HARD_EVENT_END_EPOCH`. Without them the helper stops and the workload is
`NOT RUN`.

<!-- vigil-unenforced: classification=external-procedure; reason=`Physical workload inputs are an operator procedure on a real camera host.` -->

`run` builds the workspace with `cargo build --locked --release --workspace --features
first-light-acceptance,acceptance` into a fresh private target, copies the binary into the receipt,
then runs the selected acceptance test against that copy. The receipt records the build and test
commands, exit status, wall time and peak memory of each step, the measured binary's SHA-256, CPU
and memory facts, cgroup limits and the ffmpeg, ffprobe and mediamtx versions, with secrets and
local paths redacted. `verify-receipt` re-checks the receipt and re-hashes the saved binary.

Receipts are private raw evidence. Keep them off the repository.

### 5. Compare manually

The helper has no compare or publish mode. Compare the two verified receipts by hand: the same
workload, the same host facts and limits, the same fixtures, the pinned dependencies unchanged,
and only the Vigil revision different. Read the per-step wall time and peak memory, and the
statistics the workload reports. If anything other than the Vigil revision differs, the comparison
is not paired; say so in the result.

<!-- vigil-unenforced: classification=external-procedure; reason=`Manual receipt comparison is an operator procedure with no helper mode.` -->

## Publishing a result (manual)

1. Copy [the template](results/TEMPLATE.md) to `docs/performance/results/<unique-id>.md`.
2. Fill it from the verified receipts only. Never paste raw receipts, logs, absolute paths,
   hostnames, user names, camera addresses, hardware serials or secrets.
3. Have the maintainer review it, then add one line to the index below, opening with the entry's
   Kind and Status.

<!-- vigil-unenforced: classification=external-procedure; reason=`Publishing a result is a manual maintainer procedure.` -->

## Measured results

### Official

None yet.

### Community

None yet.

## Planned automation (not active)

A maintainer-dispatched performance run needs a maintainer-registered, dedicated, isolated
self-hosted runner and an admission rule that keeps untrusted fork code off it. Until both exist,
the performance workflow stays dormant and measurement stays manual.

<!-- vigil-unenforced: classification=future-surface; reason=`Maintainer-dispatched self-hosted performance automation is not implemented.` -->
