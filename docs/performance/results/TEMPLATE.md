Kind: `<official | community>` · Status: `<descriptive only | acceptance rows reported below>`

# Performance result: <workload> on <hardware label>, <run date>

Copy this file to `docs/performance/results/<unique-id>.md` and replace every `<…>` from reviewed
values only. See [the guide](../README.md#publishing-a-result-manual).

Rules for filling it in:

- **The first line is mandatory.** Kind and Status come before anything else, and the same Kind
  and Status open this entry's line in the guide's index.
- **Public only.** Never paste raw receipts, logs, absolute paths, hostnames, user names, camera
  addresses, hardware serials, cgroup paths or secrets. Raw receipts stay private.
- **Never invent a value.** If a field was not recorded, write `unknown`; if it does not apply,
  write `not applicable` and say why.
- **Descriptive is not acceptance.** Numbers go in the descriptive table, which has no threshold
  and no verdict. Only a row quoted verbatim from
  [the guide's acceptance bars](../README.md#acceptance-bars) goes in the acceptance table. A
  passing acceptance test or a completed run is never a performance acceptance verdict.
- **Unknown is not a pass.** An acceptance row that was not run is `NOT RUN`; one whose run does
  not match the row's scope, or whose provenance is partly unknown, is `NOT EVIDENCE`.
- **Failed and partial runs are worth recording**, with the limitation stated.

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry filling rules are a manual maintainer procedure applied at review.` -->

## Classification

- Kind: `<official | community>`
- Measured by: `<public name or handle>`; reviewed by: `<maintainer>`
- Run date: `<YYYY-MM-DD>`
- Run status: `<completed | failed | incomplete | not measured>` — failed: the build or test
  exited non-zero; incomplete: stopped, timed out or only partly kept.

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry classification fields are filled and reviewed by a maintainer.` -->

## Source

- Paired comparison: `<yes | no>`
- Baseline Vigil revision: `<full SHA>`, or `not available: <reason>`
- Candidate Vigil revision: `<full SHA>`
- Context Graph revision: `<full SHA from the dependency pins>`
- ContextDB revision: `<full SHA from the dependency pins>`
- Both receipts passed `verify-receipt`: `<yes | no: which>`

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry source fields are filled and reviewed by a maintainer.` -->

## Workload and configuration

- Workload: `<synthetic | physical>`
- Acceptance test: `<exact test name from the manifest>`
- Fixtures: `<path and SHA-256 for each fixture in the manifest>`
- Physical inputs: `<not applicable (synthetic) | ambient minutes and hard-event window length; never the camera address>`
- Build command and features: `<as recorded in the receipt>`
- Test command (verbatim, local paths replaced by placeholders): `<…>`
- Fixture tools: `<ffmpeg, ffprobe, mediamtx versions from the receipt>`

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry workload fields are filled and reviewed by a maintainer.` -->

## Environment

- Hardware (public label): `<CPU model, cores, memory; no serials>`
- Filesystem and storage (public): `<filesystem type and storage kind; never mount paths>`
- OS / kernel: `<…>`
- Toolchain: `<rustc version>`
- Accelerator in use: `<none | name and backend>`
- Resource limits: `<cgroup cpu/memory/pids limits from the receipt, or "none">`
- Isolation: `<what else was stopped during the runs>`

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry environment fields are filled and reviewed by a maintainer.` -->

## Scope

- Workloads selected: `<list>`; not run: `<list or "none">`
- A single-workload run is a partial, descriptive measurement of that workload, never a global
  performance pass.

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry scope fields are filled and reviewed by a maintainer.` -->

## Acceptance (quoted acceptance bars only)

Vigil currently publishes no numeric performance threshold. Unless
[the guide's acceptance bars](../README.md#acceptance-bars) quote one, write "None reported." and
remove the table.

<!-- vigil-unenforced: classification=product-decision; reason=`Publishing no numeric performance threshold is a product position without an executable absence proof.` -->

| Acceptance row (quoted verbatim) | Verdict | Evidence and coverage |
|---|---|---|
| `<"…" — the exact row text>` | `<PASS / FAIL / NOT RUN / NOT EVIDENCE>` | `<what was measured, statistic and unit, how it matches the row's scope, what was not covered>` |

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry acceptance table is filled and reviewed by a maintainer.` -->

## Descriptive measurements (no threshold and no verdict)

| Step or statistic | Unit | Baseline | Candidate | Change |
|---|---|---|---|---|
| `<build wall time / test wall time / test peak memory / workload statistic>` | `<…>` | `<value, or "not available">` | `<value, or "not measured">` | `<value, or "not comparable">` |

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry descriptive table is filled and reviewed by a maintainer.` -->

Notes (one line for any row that is not a straightforward paired value):

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry notes are filled and reviewed by a maintainer.` -->

- `<row>`: `<e.g. no baseline, command failed, host facts differ>`

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry notes are filled and reviewed by a maintainer.` -->

## Missing metadata

Tick what is not known; if any box is ticked, provenance is partly unknown and no acceptance row
may be `PASS` on the strength of this entry.

<!-- vigil-unenforced: classification=external-procedure; reason=`Result-entry missing-metadata checklist is filled and reviewed by a maintainer.` -->

- [ ] Baseline revision
- [ ] Candidate revision
- [ ] Dependency revisions
- [ ] Run date
- [ ] Fixture identity
- [ ] Hardware, OS or toolchain
- [ ] Resource limits or isolation
- [ ] Exact command

Provenance: `<complete | partly unknown: list above>`

## Limitations

- `<anything that could affect the comparison>`

Raw receipts kept privately by: `<who>`
