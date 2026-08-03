# CLI reference

The current binary accepts `run`, `events`, `why`, `stats`, `enroll`, `forget`, `doctor acceleration`, and the internal `detector-probe` command. The top-level `--help` output currently under-reports this surface and lists only `run`; this page records commands that are actually dispatched and covered by current tests.

<!-- vigil-unenforced: classification=documentation-gap; reason=`Top-level help under-reports dispatched commands and lacks one exact inventory contract.` -->

## Run

```console
vigil run [OPTIONS]
```

Starts the camera runtime, local store, health server, review server, control socket, and configured Home Assistant and recognition tasks.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No startup contract binds this complete list of run-owned services and tasks.` -->

Common options:

| Option | Meaning | Default |
|---|---|---|
| `--config PATH` | Read a TOML configuration file | none |
| `--data-dir PATH` | Runtime data root | `./vigil-data` |
| `--store-path PATH` | Context Graph store | `<data-dir>/store.contextgraph` |
| `--health-port PORT` | Health HTTP port | `8099` |
| `--review-port PORT` | Review HTTP port | `8098` |
| `--site-name NAME` | Site/context name | `site-1` |
| `--camera-name NAME` | Single-camera name | `camera-1` |
| `--rtsp-url URL` | Detection stream | none |
| `--live-rtsp-url URL` | Separate Home Assistant live stream | detection URL |
| `--usb-device IDENTITY` | Legacy single-camera USB source | none |
| `--csi-module IDENTITY` | Legacy single-camera CSI source | none |
| `--mjpeg-url URL` | Legacy single-camera MJPEG source | none |
| `--rtsp-username USER` | RTSP username outside the URL | none |
| `--rtsp-password PASSWORD` | RTSP password outside the URL | none |
| `--detector-model-id ID` | Model identity written to provenance | `yolox-tiny-burn-cpu` |
| `--detector-model-path PATH` | Detector weights path | artifact/config dependent |
| `--detector-confidence-threshold FLOAT` | Keep detections at or above this value | `0.5` |
| `--detector-sample-frames N` | Frames sampled per segment | `5` |
| `--detector-stationary-interval-secs N` | Sampling interval for stationary scenes | `30` |
| `--recognition-weights-dir PATH` | Enable recognition with local weights | disabled |
| `--hardware-decoding BOOL` | Request hardware-decode probing | `true` |
| `--accelerated-detection BOOL` | Request accelerated-detector probing | `true` |
| `--fabric-ticket TICKET` | Join an existing configured fabric | none |
| `--fabric-hub BOOL` | Start the node as a fabric join point | `false` |
| `--fabric-allow-frame-offload BOOL` | Permit this node's detector work to move | `true` |
| `--fabric-worker-lease-ms MS` | Fabric worker lease setting | `300000` |
| `--fabric-fallback-horizon-ms MS` | Remote-result wait setting | `5000` |

<!-- vigil-claim: `vigil.docs-cli.run-options-and-current-defaults` -->
<!-- enforced by: `vigil::config::tests::review_port_cli_override_is_documented_and_loaded` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_falls_back_to_detection_rtsp_url_for_single_stream_cameras` -->

> **Developer-preview fabric warning:** default builds omit fabric entirely. In a
> fabric-enabled build, remote or rescued detector jobs can currently return empty detection
> content, and the two tuning fields are not both production-wired. Do not treat these flags as an
> operational distributed-compute surface.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Fabric jobs can return empty detection content and tuning remains incompletely wired.` -->

The confidence threshold must be between `0.0` and `1.0`; sampled frames must be between 1 and 64; booleans accept `true` or `false`. Multi-camera lists, MQTT, recognition class coverage, and service identity are configured through TOML, add-on options, or environment variables rather than repeated CLI flags.

<!-- vigil-claim: `vigil.docs-cli.the-confidence-threshold-must-be-between-00` -->
<!-- enforced by: `vigil::config::tests::review_port_cli_override_is_documented_and_loaded` -->
<!-- enforced by: `vigil-bin::first_light_loop::detector_confidence_threshold_filters_detector_output` -->

## Events

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil events
```

Lists up to 100 recent landed detection events, newest first. It does not list motion-only segments or correction observations.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent mapping proves the 100-row limit and exclusion of motion/corrections.` -->

`/var/lib/vigil` must match the running daemon's `data_dir`. Review commands do not read its
`--config` TOML; without `VIGIL_DATA_DIR` or an exact `VIGIL_STORE_PATH`, they use
`./vigil-data` and can silently read a different store.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The current events test selects a store explicitly but does not prove config omission, environment precedence, or wrong-store fallback.` -->

With its local store selected explicitly, `vigil events` reads landed events and orders them newest
observed first.

<!-- vigil-claim: `vigil.docs-cli.varlibvigil-must-match-the-running-daemons-datadir` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_events_lists_recent_events_newest_first` -->

## Why

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil why <detection-id>
VIGIL_DATA_DIR=/var/lib/vigil vigil why --latest
```

`vigil why --latest` selects the newest observed detection and walks its camera, site, detector
Decision, baseline Intention, and evidence. An unknown UUID returns a clean not-found error.

<!-- vigil-claim: `vigil.docs-cli.walks-a-detection-through-camera-site-detector` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_latest_walks_the_newest_event` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_on_unknown_event_id_errors_cleanly` -->

Omitting the `why` argument currently behaves as `--latest`, while malformed detection IDs are
intended to return an error. The mapped tests do not exercise either CLI form directly.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct CLI witness binds omitted-argument fallback or malformed detection-ID rejection.` -->

## Stats

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil stats
```

`vigil stats` prints the runtime's local persisted snapshot of pipeline counters and health.

<!-- vigil-claim: `vigil.docs-cli.prints-the-runtimes-local-persisted-snapshot-of` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_stats_reports_live_pipeline_counters` -->

If a fabric is configured, shared fabric status and join instructions are intended to appear on this
surface as well; the local-counter witness does not exercise a fabric-configured CLI invocation.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No mapped CLI acceptance invokes stats with fabric configured and verifies shared status and join instructions.` -->

## Enroll

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil enroll <detection-id> <name>
```

The underlying enrollment operation creates a named site-local recognition reference from a
detection and writes its linked correction. The detection must carry the stored recognition probe;
when it does not, enrollment fails without a stranded correction row.

<!-- vigil-claim: `vigil.docs-cli.enrolls-the-subject-in-a-detection-as` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->
<!-- enforced by: `vigil::recognition_slice::enroll_failure_writes_no_correction_row` -->

The `vigil enroll` command is dispatched to this operation, but the current deterministic witnesses
invoke the operation directly rather than through the CLI.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct CLI acceptance proves that vigil enroll routes its arguments through the tested enrollment operation.` -->

## Forget

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil forget <name>
```

The underlying forget operation removes site-local recognition references for the named subject;
later sightings then stop matching that subject.

<!-- vigil-claim: `vigil.docs-cli.removes-recognition-references-for-the-named-subject` -->
<!-- enforced by: `vigil::recognition_slice::forget_removes_references_and_the_subject_reverts_to_unknown` -->

The `vigil forget` command is dispatched to this operation, but the current deterministic witness
invokes the operation directly rather than through the CLI.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct CLI acceptance proves that vigil forget routes its argument through the tested forget operation.` -->

## Doctor acceleration

```console
vigil doctor acceleration [--service-user USER] [RUN_OPTIONS]
```

The acceleration report renders decode and detection receipts in a fixed order with configured,
status, attempted-backend, active-backend, and failure-code fields. Without `sudo`, it reports the
current process's usable software and CPU fallback state.

<!-- vigil-claim: `vigil.docs-cli.inspects-the-current-host-and-prints-structured` -->
<!-- enforced by: `vigil::doctor_acceleration::no_sudo_reports_current_process_truth_only` -->
<!-- enforced by: `vigil::doctor_acceleration::doctor_renders_receipts_in_fixed_format` -->

The command is intended to be read-only, include an operator action for each fallback, and use the
same relevant probes as feature-enabled runtime startup. The current mapped tests call the report
logic directly and do not bind all three CLI-level assertions.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct doctor CLI acceptance jointly proves read-only execution, rendered operator actions, and reuse of runtime startup probes.` -->

## Fabric ticket

```console
vigil fabric ticket [--data-dir PATH]
```

Prints this node's fabric enrollment ticket to stdout, on explicit invocation only. The ticket is a
credential, held to a two-tier contract. On every displayed or served surface — logs, `vigil
stats`, `vigil doctor`, the `/health` HTTP body — it is deliberately absent, with exactly two
sanctioned retrieval paths: `vigil run`'s own startup log, which still prints it as a sanctioned
operator-facing instruction, and this command, which falls back to the ticket the running node
cached the last time it bound (since a live rebind opens the same on-disk fabric ledger the
running node's own `Database::open` already holds, which fails fast with a typed database-locked
error before this command ever reaches its own endpoint bind step). Separately, it is expected to
sit in exactly two private, tight-permissioned (owner-read/write only) persisted stores: the
`fabric/own-ticket` cache this command falls back to, and the fabric ledger
(`fabric/fabric-ledger.db`), whose
peer-directory table is the fleet's own coordination mechanism — reading either already requires
filesystem access to this node's data directory.

<!-- vigil-claim: `vigil.docs-cli.prints-this-nodes-fabric-enrollment-ticket-to` -->
<!-- enforced by: `vigil-bin::fabric_ticket_command::fabric_ticket_command_prints_the_same_ticket_the_running_hub_advertised` -->
<!-- enforced by: `vigil-bin::fabric_ticket_command::fabric_ticket_command_prints_the_same_ticket_while_the_hub_node_is_still_running` -->
<!-- enforced by: `vigil-bin::fabric_ticket_single_retrieval_path::fabric_enrollment_ticket_appears_on_no_surface_except_the_two_sanctioned_retrieval_paths` -->
<!-- enforced by: `vigil-bin::binary_cli_surface_smoke::help_flag_names_the_fabric_ticket_command` -->

## Detector probe

`vigil detector-probe` is an internal subprocess command used by the detection-probe machinery. It is not an operator workflow and its wire arguments are intentionally omitted here.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph labels an internal subprocess command and intentionally omits its wire protocol.` -->

## Global options

| Option | Meaning |
|---|---|
| `--version` | Print the Vigil package version |
| `--help`, `-h` | Print the current built-in help |

<!-- vigil-unenforced: classification=documentation-gap; reason=`No exact help/version test binds this global-options table to the binary surface.` -->

## Unavailable commands

The current binary has no `status`, `health`, `config check`, `config validate`, `correct`, `state-at`, `why-flagged`, `scan onvif`, `scan rtsp`, or `support-bundle` command. Health is an HTTP endpoint, corrections use Home Assistant/MQTT or the review HTTP API, and configuration validation happens while `vigil run` loads its inputs. These names remain unavailable until implemented and tested.

<!-- vigil-unenforced: classification=future-surface; reason=`These named status, validation, scan, replay, and support commands do not exist.` -->
