# Development binary downloads

Vigil's development channel is an access-controlled GitHub prerelease named
`dev-latest`, for the Linux amd64 Alpine HAOS production binary. Repository access
and GitHub authentication are required while Vigil is private. This workflow does
not change repository visibility or stable/crates publishing. No existing hosted
asset, successful hosted build, or working download is claimed here. The committed
public Context Graph and ContextDB revisions are recorded in
`scripts/ci-dependency-pins.json`; real packaging refuses before any dependency
fetch or heavy build if either is missing or not a full commit SHA.
<!-- vigil-unenforced: classification=external-procedure; reason=`Hosted availability and repository access require actual external qualification.` -->

A trusted push to `dev` calls `build.yml` from the same commit after every required
correctness check succeeds. Manual retry uses **Build development binaries** on
`dev`, with that exact current Vigil SHA and its committed public Context Graph
and ContextDB pins. It requires a push CI run at that same SHA with all correctness
jobs and the aggregate successful; a delivery-only failure is retryable. Builds
and package smoke run read-only; only the isolated publisher can write releases.
The original **Dev HAOS deploy artifact** manual workflow remains available on
`dev`/`dev-next` with its existing exact-current-tuple checks and Actions artifacts.
<!-- vigil-unenforced: classification=external-procedure; reason=`Workflow admission is subject to actual hosted execution; no current success is asserted.` -->

## Download and verify before unpacking

Sign in with `gh auth login` using an account that can read Vigil. On the repository
Releases page, inspect the `dev-latest` notes and copy the full source SHA. The tag
is stationary at the default-branch head used when the channel was bootstrapped;
it is **not** the binary source. Source identity comes from the JSON receipt and
run-named assets in the notes. The channel is a prerelease, not a stable install.
<!-- vigil-unenforced: classification=external-procedure; reason=`Authentication, release discovery and source inspection are operator procedures.` -->

The permanent latest-development download locations are
[archive](https://github.com/context-graph-ai/vigil/releases/download/dev-latest/vigil-haos-production-amd64.tar.gz),
[checksum](https://github.com/context-graph-ai/vigil/releases/download/dev-latest/vigil-haos-production-amd64.tar.gz.sha256)
and [receipt](https://github.com/context-graph-ai/vigil/releases/download/dev-latest/vigil-haos-production-amd64.tar.gz.json).
These paths stay the same across builds and require repository access; use the
authenticated CLI below while Vigil is private.

In a fresh directory, download all three stable files, then verify both the
checksum file and the JSON archive digest. Set `EXPECTED_SOURCE` to the full
40-character source commit from the release notes that you intend to use.

<!-- vigil-unenforced: classification=external-procedure; reason=`Download verification is an operator procedure run against published release assets, with no repository test.` -->

```bash
set -euo pipefail
export EXPECTED_SOURCE='REPLACE_WITH_FULL_SOURCE_SHA'
gh release download dev-latest --repo context-graph-ai/vigil \
  --pattern 'vigil-haos-production-amd64.tar.gz' \
  --pattern 'vigil-haos-production-amd64.tar.gz.sha256' \
  --pattern 'vigil-haos-production-amd64.tar.gz.json'
export ARCHIVE='vigil-haos-production-amd64.tar.gz'
sha256sum --check "$ARCHIVE.sha256"
python3 - <<'PY'
import hashlib, json, os, re
from pathlib import Path
archive = Path(os.environ['ARCHIVE'])
receipt = json.loads(Path(str(archive) + '.json').read_text())
expected = os.environ['EXPECTED_SOURCE']
assert re.fullmatch('[0-9a-f]{40}', expected), 'full expected source SHA required'
assert set(receipt) == {'source', 'context_graph', 'contextdb', 'version',
                        'toolchain', 'target', 'archive_sha256', 'run_id',
                        'run_attempt'}, 'unexpected receipt'
assert receipt['source'] == expected, 'wrong source commit'
assert all(type(receipt[k]) is int and receipt[k] > 0
           for k in ('run_id', 'run_attempt')), 'invalid run identity'
assert all(re.fullmatch('[0-9a-f]{40}', receipt[k])
           for k in ('context_graph', 'contextdb')), 'invalid dependency tuple'
assert receipt['target'] == 'x86_64-alpine-linux-musl', 'wrong target'
assert receipt['archive_sha256'] == hashlib.sha256(archive.read_bytes()).hexdigest(), 'JSON checksum mismatch'
print(json.dumps(receipt, indent=2))
PY
# Proceed only if BOTH checks above succeeded and the displayed tuple is intended.
tar -xzf "$ARCHIVE"
```

The archive contains only `binary/vigil`. This is an Alpine binary with runtime
dependencies, not a universal static executable, add-on package or container image.
The complete CLI smoke runs `--help` and `--version` on the unpacked exact archive
in the matching Alpine builder with networking disabled before publication.
It does not qualify runtime startup, video detection, native hardware, production
performance or HAOS deployment acceptance.
<!-- vigil-unenforced: classification=external-procedure; reason=`The verification recipe does not certify any unexecuted artifact or deployment.` -->

## Interrupted replacement and immutable run fallback

Publication replaces archive, checksum and then JSON receipt. Downloads can race
an interrupted replacement, so **stop on either checksum failure or a source
mismatch**. Never unpack after a failed check. Use the newest complete run-named
set in a fresh directory, verifying all three files with the commands above.
Run groups are named
`vigil-haos-production-amd64-x86_64-alpine-linux-musl-<sha12>-r<run_id>a<run_attempt>.tar.gz`,
with `.sha256` and `.json` appended for the checksum and receipt. For example, set
`ARCHIVE='vigil-haos-production-amd64-x86_64-alpine-linux-musl-aaaaaaaaaaaa-r123a2.tar.gz'`
using the actual source/run/attempt in the release assets, then download exactly
`$ARCHIVE`, `$ARCHIVE.sha256` and `$ARCHIVE.json` with `gh release download`.
Set `EXPECTED_SOURCE` to that group's full intended source SHA before verification.
Release notes name the last successful group; a newer complete fallback may exist
if stable replacement was interrupted before the notes update.

<!-- vigil-unenforced: classification=external-procedure; reason=`Interrupted-replacement handling is an operator procedure against published release assets, with no repository test.` -->

When notes are stale, list the actual assets with authenticated access:

```bash
release_id=$(gh api repos/context-graph-ai/vigil/releases/tags/dev-latest --jq '.id')
gh api --paginate "repos/context-graph-ai/vigil/releases/$release_id/assets" \
  --jq '.[] | [.name, .created_at] | @tsv'
```

Choose the newest run group with all three assets, then download it in a fresh
directory (replace the example archive name with the selected asset name):

```bash
export ARCHIVE='vigil-haos-production-amd64-x86_64-alpine-linux-musl-aaaaaaaaaaaa-r123a2.tar.gz'
gh release download dev-latest --repo context-graph-ai/vigil \
  --pattern "$ARCHIVE" --pattern "$ARCHIVE.sha256" --pattern "$ARCHIVE.json"
python3 -c 'import json, os; print(json.load(open(os.environ["ARCHIVE"] + ".json"))["source"])'
```

Copy the receipt's full 40-character `source` into `EXPECTED_SOURCE`, confirm that
it is the source commit you intend to use, and run both verification checks above
with this `ARCHIVE` before unpacking. The asset name carries only a source prefix;
stale notes do not supply the fallback group's exact source identity.

<!-- vigil-unenforced: classification=external-procedure; reason=`Immutable run fallback is an operator procedure against published release assets, with no repository test.` -->

Every publication writes a new immutable run group and refuses any name collision.
The publisher downloads and hashes all three run assets before stable files change.
Only after a successful stable update and notes update may pruning delete complete
older groups. It retains the newest three publications plus any group whose receipt
matches the current stable receipt; incomplete and unrelated groups survive.
Raw private build receipts and log paths are never release assets; the curated JSON
contains only source, dependency tuple, version, toolchain, target, archive hash,
run ID and attempt. The publisher never unpacks or executes downloaded payloads.
<!-- vigil-unenforced: classification=external-procedure; reason=`Retry and retention describe the prepared publisher; hosted end-to-end validation is pending.` -->

The native publisher queue keeps at most 100 pending jobs. It rechecks the current
`dev` tip before each stable asset mutation and skips stale sources. The first
hosted pilot must confirm job-level `queue: max`, built-in-token stationary-tag
bootstrap, draft retry, actual private asset access and this full download journey.
Controlled measurements follow [the manual performance guide](performance/README.md).
<!-- vigil-unenforced: classification=external-procedure; reason=`Provider acceptance, assets and measurement require separately authorised real execution.` -->
