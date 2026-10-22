# Vigil automation

The context-graph-ai standard separates required correctness, extended native
qualification, development binary delivery, manual performance and stable release
qualification. Existing coverage and product bars remain enforced; ordinary suite
duration is report-only.

| Path | Entrypoint | Local command/profile |
| --- | --- | --- |
| Required correctness | `ci.yml` → `default-checks.yml` | `scripts/ci-checks`; `scripts/ci-checks --portable` for offline controls and lint |
| Extended integration | `dev-closeout.yml` | `scripts/ci-integration dev-closeout OPTIONS...` |
| Development binaries | `build.yml` → `dev-haos-artifact.yml` | Existing `scripts/verify release --lane artifact-amd64` recipe |
| Manual performance | Dormant `performance.yml` | [Manual performance guide](../docs/performance/README.md) |
| Stable qualification | `release-qualification.yml` | `scripts/ci-publication release OPTIONS...`; no publication implied |

The [canonical download guide](../docs/download.md) explains authentication,
exact-source identity, both checksum checks and interrupted-download fallback.
The same-commit `dev` CI caller runs only after the existing required aggregate;
manual retry also requires same-source successful correctness. Native HAOS jobs,
`integrate-dev.yml`, stable publishing and repository visibility stay as before.
A missing or malformed committed public dependency pin blocks real packaging before
fetch or build. No hosted success or available assets are asserted.

Only the isolated publisher has contents write permission. It never checks out,
unpacks or executes artifacts, and emits an allowlisted JSON rather than raw
private receipts. Job-level `queue: max` has narrow lint compatibility validation;
GitHub acceptance and the complete private download journey await a hosted pilot.
See [agent automation rules](AGENTS.md) before editing workflows.
