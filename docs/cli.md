# CLI reference

Nine public commands, and `vigil --help` names every one of them:

| Command | What it does |
|---|---|
| `vigil run` | Starts the camera runtime, the local store, the health and review servers, and the owner channel every command below is answered on |
| `vigil events` | Lists this deployment's recent landed detection events, newest first |
| `vigil why` | Walks one detection back through its camera, site, detector Decision, baseline Intention and evidence |
| `vigil stats` | Prints this deployment's own persisted snapshot of pipeline counters and health |
| `vigil settings` | Reads and changes what this deployment is configured to run at, and reports what it is running at now |
| `vigil enroll` | Names the subject in a detection, so later sightings match that name |
| `vigil forget` | Removes this site's recognition references for a named subject |
| `vigil doctor acceleration` | Reports the decode and detection acceleration this host actually offers |
| `vigil fabric ticket` | Prints this node's fabric enrollment ticket, on explicit invocation only |

<!-- vigil-claim: `vigil.docs-cli.the-current-binary-dispatches-ten-commands-nine` -->
<!-- enforced by: `vigil-bin::help_names_every_public_command::help_prints_exactly_the_declared_public_inventory` -->
<!-- enforced by: `vigil-bin::help_names_every_public_command::the_declared_inventory_is_exactly_the_nine_public_commands` -->
<!-- enforced by: `vigil-bin::help_names_every_public_command::help_names_no_internal_subcommand` -->
<!-- enforced by: `vigil-bin::help_names_every_public_command::every_advertised_command_is_dispatched_by_the_binary` -->

A tenth command is dispatched and deliberately kept off that list: `vigil detector-probe` belongs
to the detection-probe machinery rather than to an operator, and is described under
[Detector probe](#detector-probe). This page documents each public command and the answers it
gives.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph orients the reader to the page and points at the internal subprocess command's own section.` -->

## Run

```console
vigil run [OPTIONS]
```

Starts the camera runtime, local store, health server, review server, the owner channel the CLI reaches this process on, and configured Home Assistant and recognition tasks.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No startup contract binds this complete list of run-owned services and tasks.` -->

If another process is reading this deployment's store when the runtime starts, the runtime waits
for it rather than failing. It says on its startup output that it is waiting and what it is waiting
for, and it starts normally the moment the reader finishes. There is no timeout on that wait and no
setting that shortens it: a reader that is genuinely working is not a fault, and a runtime that
gave up on one would leave a healthy node down for a condition that was about to clear.

<!-- vigil-claim: `vigil.docs-cli.if-another-process-is-reading-this-deployments` -->
<!-- enforced by: `vigil-bin::hydrating_reader_does_not_degrade_a_starting_runtime::a_hydrating_reader_does_not_permanently_degrade_a_starting_runtime` -->
<!-- enforced by: `vigil-bin::hydrating_reader_does_not_degrade_a_starting_runtime::a_startup_open_refused_by_readers_names_the_readers_holding_the_store` -->
<!-- enforced by: `vigil-bin::a_waiting_runtime_still_stops_when_asked::a_runtime_waiting_for_a_reader_stops_when_it_is_asked_to` -->

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

The confidence threshold must be between `0.0` and `1.0`; sampled frames must be between 1 and 64; booleans accept `true` or `false`. Multi-camera lists and MQTT are configured through TOML or add-on options rather than repeated CLI flags. Recognition class coverage (`recognition_covered_classes`) is configured through TOML or add-on options; it is not an environment variable. Service identity is not a TOML or add-on field at all — an ordinary `vigil settings set service_identity` is refused, and moving it is its own deliberate operation, `vigil settings identity change <identifier> --confirm` (see [Configuration](configuration.md#site-and-service-identity-fields)).

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

An empty result is an empty answer, not a refusal and not a manufactured row. On a deployment
nothing has run in, `vigil events` prints nothing at all and exits `0` — there are no events, and
that is what was asked. `vigil why --latest` is the opposite case on the same deployment: it could
not serve the request, because there is no event to walk, so it refuses on standard error with exit
status `2`.

<!-- vigil-claim: `vigil.docs-cli.an-empty-result-is-an-empty-answer` -->
<!-- enforced by: `vigil-bin::a_read_of_a_never_started_deployment_creates_nothing::the_review_commands_answer_a_never_started_deployment_honestly` -->
<!-- enforced by: `vigil-bin::a_read_of_a_never_started_deployment_creates_nothing::no_live_command_brings_a_never_started_deployment_into_existence` -->

Where a running owner serves that empty answer instead, the whole of it is the
`served-by=af_unix` line with nothing beneath it.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No mapped test drives vigil events against an owner-served store that holds no events, so the marker-only shape of that answer is stated from observation rather than bound.` -->

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

A malformed detection ID is refused as a malformed ID and never reported as a missing event: the
answer names what was typed and the form a detection ID takes, so an operator who mistyped one is
not sent looking for a recording nobody ever named.

<!-- vigil-claim: `vigil.docs-cli.a-malformed-detection-id-is-refused-as` -->
<!-- enforced by: `vigil-bin::why_refuses_an_id_it_cannot_serve::a_malformed_detection_id_is_refused_as_a_malformed_id_and_not_as_a_missing_event` -->

Omitting the `why` argument currently behaves as `--latest`. The mapped tests do not exercise that
CLI form directly.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct CLI witness binds the omitted-argument fallback.` -->

## Stats

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil stats
```

`vigil stats` prints the runtime's local persisted snapshot of pipeline counters and health.

<!-- vigil-claim: `vigil.docs-cli.prints-the-runtimes-local-persisted-snapshot-of` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_stats_reports_live_pipeline_counters` -->

Those figures come from this deployment's own snapshot file, `<data-dir>/runtime-stats.json`, and
`vigil stats` opens no store to read them. The answer opens with a `stats-provenance=` line whenever
the figures are not a reading of a running node. `stats-provenance=not-started` says nothing has run
in this data directory yet and names the directory it looked in, so an all-zeros answer from a
mistyped `VIGIL_DATA_DIR` is not read as a quiet node; `stats-provenance=stopped-run` says these are
the last figures this deployment's own run wrote before it ended, carried under a line saying they
are a record and not a reading. A live run's figures carry no such line, and that absence is what
says they are current. A snapshot whose writer cannot be established at all — truncated, unreadable,
or carrying no ownership stamp — is refused rather than served as either.

<!-- vigil-claim: `vigil.docs-cli.those-figures-come-from-this-deployments-own-snapshot` -->
<!-- enforced by: `vigil-bin::detector_queue_capacity_does_not_survive_its_variable::a_stats_read_on_a_never_started_directory_answers_zeros_under_a_not_started_line` -->
<!-- enforced by: `vigil-bin::detector_queue_capacity_does_not_survive_its_variable::a_stats_read_of_this_deployments_own_stopped_run_serves_its_last_figures` -->
<!-- enforced by: `vigil-bin::detector_queue_capacity_does_not_survive_its_variable::a_stats_read_of_a_truncated_snapshot_refuses_instead_of_claiming_nothing_ran` -->
<!-- enforced by: `vigil-bin::detector_queue_capacity_does_not_survive_its_variable::a_stats_read_of_an_unreadable_snapshot_refuses_instead_of_claiming_nothing_ran` -->
<!-- enforced by: `vigil-bin::detector_queue_capacity_does_not_survive_its_variable::a_stats_read_with_no_owner_says_no_runtime_owns_the_data_directory` -->

If a fabric is configured, shared fabric status and join instructions are intended to appear on this
surface as well; the local-counter witness does not exercise a fabric-configured CLI invocation.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No mapped CLI acceptance invokes stats with fabric configured and verifies shared status and join instructions.` -->

## Settings

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil settings
VIGIL_DATA_DIR=/var/lib/vigil vigil settings list
VIGIL_DATA_DIR=/var/lib/vigil vigil settings find <TEXT>
VIGIL_DATA_DIR=/var/lib/vigil vigil settings set <SETTING> <VALUE>
VIGIL_DATA_DIR=/var/lib/vigil vigil settings reset <SETTING>
VIGIL_DATA_DIR=/var/lib/vigil vigil settings identity
VIGIL_DATA_DIR=/var/lib/vigil vigil settings identity change <IDENTIFIER> --confirm
```

Every one of these asks the process that owns this deployment's store, through Context Graph's
authenticated owner channel, and reads the store file directly only when no runtime is holding it.
Both routes render the same report; an answer the running owner served opens with a
`served-by=af_unix` line, and an answer read from an idle store file carries no such line. Neither
route brings anything into existence: asking a deployment that has never started what it would run
at leaves its data directory exactly as it was, with no store on disk afterwards.

<!-- vigil-claim: `vigil.docs-cli.every-one-of-these-asks-the-process` -->
<!-- enforced by: `vigil-bin::settings_live_operator_visibility::listing_answers_while_the_runtime_owns_the_store` -->
<!-- enforced by: `vigil-bin::settings_live_operator_visibility::listing_answers_by_direct_read_when_no_runtime_owns_the_store` -->
<!-- enforced by: `vigil-bin::settings_live_operator_visibility::listing_an_unstarted_deployment_leaves_no_store_on_disk` -->

`vigil settings` and `vigil settings list` are the same listing. It is six kinds of line —
`served-by`, `identity`, `setting`, `held`, `domain` and `secret` — and every field each kind
carries, with its value set, is in
[the settings listing's field glossary](configuration.md#the-settings-listing-line-by-line), beside
the table that says which surface may author each field.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph points at the configuration reference, which owns the field glossary and its authority table; it states no runtime promise of its own.` -->

`vigil settings find <TEXT>` searches the same listing and answers with every line containing
that text — setting lines, the `domain switch=` control lines and the explanations alike, because
the search is over the whole rendered answer and that breadth is deliberate. A search that matches
nothing succeeded and found nothing: the answer is empty and the exit status is `0`. A search with
no text names nothing to look for, so it is a usage error.

<!-- vigil-claim: `vigil.docs-cli.vigil-settings-find-text-searches-the-same` -->
<!-- enforced by: `vigil-bin::settings_find_is_the_search_verb::find_narrows_the_listing_to_every_line_carrying_the_text` -->
<!-- enforced by: `vigil-bin::settings_find_is_the_search_verb::a_search_that_matches_nothing_succeeded_and_found_nothing` -->
<!-- enforced by: `vigil-bin::settings_find_is_the_search_verb::a_search_with_no_text_is_a_usage_error` -->

`vigil settings set <SETTING> <VALUE>` records the value at this deployment through the
`vigil-settings` surface and then prints the listing back; a bare `true` or `false` is a switch, a
bare number is a number, a comma-separated value is a list, and anything else is text.
`vigil settings reset <SETTING>` withdraws this deployment's own record for that setting, states
what the withdrawal leaves in force, and prints the listing beneath it. A change made against the
running node takes effect on that node before the answer is rendered where the setting is one the
process can take on live; where only a start brings it into force, the line says so with
`applies=next-restart`.

<!-- vigil-claim: `vigil.docs-cli.vigil-settings-domain-domain-narrows-the-same` -->
<!-- enforced by: `vigil-bin::settings_live_application_while_running::a_rate_change_made_while_running_takes_effect_without_a_restart` -->
<!-- enforced by: `vigil-bin::settings_live_application_while_running::a_rate_withdrawn_while_running_is_taken_back_without_a_restart` -->
<!-- enforced by: `vigil::settings_application_timing_roster::the_values_the_take_back_promise_covers_are_declared_live` -->

`vigil settings identity` prints this node's service identifier and how it was arrived at, and it is
read-only. The identifier is not an ordinary setting: `vigil settings set service_identity` is
refused, naming the orphaned-history consequence, and so is a reset of it. Moving it is its own
deliberate operation. `vigil settings identity change <IDENTIFIER>` without `--confirm` states the
consequence and changes nothing, so the consequence is read before the identity moves; with
`--confirm` the change is recorded and takes effect at the next start.

<!-- vigil-claim: `vigil.docs-cli.vigil-settings-identity-prints-this-nodes-service` -->
<!-- enforced by: `vigil-bin::settings_operator_surface_fields::listing_shows_the_service_identifier_read_only_with_how_it_was_arrived_at` -->
<!-- enforced by: `vigil-bin::identity_refusal_through_settings_cli::setting_the_identity_through_the_cli_is_refused_with_the_consequence_and_changes_nothing` -->
<!-- enforced by: `vigil-bin::identity_refusal_through_settings_cli::resetting_the_identity_through_the_cli_is_refused_with_the_consequence_and_changes_nothing` -->
<!-- enforced by: `vigil::service_identity_persistence::the_deliberate_change_operation_states_the_consequence_before_it_takes_effect` -->

What each of these shapes answers when another process is reading the store, when nothing has ever
run in this directory, when another runtime owns the store, and when the store cannot be read at
all — and which stream and exit status each of those answers arrives on — is stated once, in
[When a read command cannot open the store](#when-a-read-command-cannot-open-the-store).

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph cross-references the section that owns the store-failure contract, so the contract has one home rather than two.` -->

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
running node's own `Database::open` already holds, which fails fast with a typed writer-held
refusal before this command ever reaches its own endpoint bind step). Separately, it is expected to
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

## When a read command cannot open the store

Four different things stop a read command opening this deployment's store, and they are four
different answers because they send you to four different places. Every one of them is decided on
the typed condition the layers below report, never on the wording of a message, and none of them
writes to your store.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph introduces the four conditions below; each one carries its own classification.` -->

**Somebody is reading it.** Another process — a second command, or this node's own reader — holds
the store while it hydrates. Nothing you only ASK notices: `vigil settings`, `vigil settings list`,
`vigil settings find <text>`, `vigil settings identity`, `vigil why` and `vigil events` all read
through a door that coexists with other readers, so they are served their ordinary answer and name
nobody. `vigil enroll` and `vigil forget` are the two that wait, because they change what this node
recognizes and that needs the store to itself. They say so on a `busy` line naming the store and
how many readers hold it, followed by a `reader` line per reader carrying that reader's process id
and process name, so you can go and look at the one that is in the way. The store is healthy,
nothing needs repairing, and the condition clears on its own: run the same command again once they
finish and it lands. It is not the unreadable answer — nothing tells you the store cannot be read,
and nothing calls this node unmanaged.

<!-- vigil-claim: `vigil.docs-cli.somebody-is-reading-it-another-process-a` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::an_edit_answers_that_the_store_is_busy_rather_than_unreadable` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::every_read_only_settings_shape_is_served_while_a_reader_holds_the_store` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::why_and_events_are_served_while_a_reader_holds_the_store` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::the_same_commands_answer_normally_once_the_reader_has_finished` -->

**Nothing has ever run here.** There is no store because no runtime has started in this directory.
Nothing is broken and nothing needs repairing, so no answer sends you to repair anything: `vigil
events` answers with no events, `vigil why` refuses because there is no event to walk, `vigil
stats` and the read-only settings shapes answer what this node would run at, and `vigil enroll`
and `vigil forget` refuse because there is nothing here yet to correct. Whichever you ask, asking
leaves the directory exactly as it was — no command in this family brings a deployment into
existence, so a command pointed at the wrong path does not turn that path into a deployment.

<!-- vigil-claim: `vigil.docs-cli.nothing-has-ever-run-here-there-is` -->
<!-- enforced by: `vigil-bin::a_read_of_a_never_started_deployment_creates_nothing::no_live_command_brings_a_never_started_deployment_into_existence` -->
<!-- enforced by: `vigil-bin::a_read_of_a_never_started_deployment_creates_nothing::the_review_commands_answer_a_never_started_deployment_honestly` -->

**Another runtime owns it.** A writer holds the store, and it keeps holding it until that runtime
lets go. That is one condition however it is reached: the same refusal whether the writer is this
process or another one, spelled by type rather than by matching text.

<!-- vigil-claim: `vigil.docs-cli.another-runtime-owns-it-a-writer-holds` -->
<!-- enforced by: `vigil::store_open_failure_classes::a_locked_store_still_refuses_a_second_runtime_naming_the_holder` -->
<!-- enforced by: `vigil-bin::fabric_ticket_command::fabric_ticket_command_prints_the_same_ticket_while_the_hub_node_is_still_running` -->

**It cannot be read at all.** A corrupt file or an unreadable volume is a storage problem. Here the
answer names the store and says this deployment's settings and this node's identity are not
available through this command, and where they are. What it deliberately does not do is print a
settings listing, a domain roster or an identity: this process never read your store, so every one
of those would be a value it made up — the artifact's own defaults standing where your
deployment's belong, with nothing on the line to say so.

<!-- vigil-claim: `vigil.docs-cli.it-cannot-be-read-at-all-a` -->
<!-- enforced by: `vigil-bin::storeless_command_answers_without_guessing::a_command_that_cannot_read_the_store_answers_without_inventing_this_deployments_state` -->
<!-- enforced by: `vigil-bin::storeless_command_answers_without_guessing::no_answer_shape_carries_an_identity_when_the_store_cannot_be_read` -->

`vigil stats` is the exception to all four conditions, because it never opens the store: its figures
come from `<data-dir>/runtime-stats.json`, so it answers normally at exit `0` — under its own
`stats-provenance=` line — however badly the store is faring. The condition it has of its own is a
snapshot whose writer cannot be established, which it refuses.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The storeless suite drives why, events and the settings shapes against an unreadable store but never stats, so stats' exemption from the four store conditions is stated from the read path in crates/vigil/src/lib.rs rather than bound by a mapped test.` -->

How each answer is delivered does not change with which of the conditions produced it, for every
condition a command actually meets. `vigil why` and `vigil events` could not serve your request, so
they refuse: the answer is on standard error and the exit status is `2`. The `vigil settings`,
`vigil settings list` and `vigil settings find <text>` listings were served — the explanation is the
answer — so they arrive on standard output. `vigil settings identity` is a failed request whichever
way it fails: an operator asked this node what it is called and no answer exists to give them, so it
leaves on standard error with exit status `2` rather than letting a script read a missing identity
as a successful lookup. A settings change refuses too, on the same terms, because a change nothing
recorded is not a change. The reader condition is the one no question ever meets: everything you
only ask reads through a door that coexists with other readers, so a store somebody is reading
answers with your settings, your node's identity and your review history exactly as an idle store
does.

<!-- vigil-claim: `vigil.docs-cli.how-each-answer-is-delivered-does-not` -->
<!-- enforced by: `vigil-bin::storeless_command_answers_without_guessing::each_read_shape_keeps_its_own_status_and_stream_when_the_store_cannot_be_read` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::an_edit_answers_that_the_store_is_busy_rather_than_unreadable` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::every_read_only_settings_shape_is_served_while_a_reader_holds_the_store` -->
<!-- enforced by: `vigil-bin::a_store_busy_with_readers_is_not_an_unreadable_store::why_and_events_are_served_while_a_reader_holds_the_store` -->

One refusal sits outside those four, because the store is not what is in the way. A running owner
serves a bounded number of these requests at a time — four, on the shipped limits — and a request
that arrives while that many are already in flight is refused rather than queued. The answer is
`the process holding the store at <path> is already serving as many requests as it admits`, on
standard error with exit status `2`, and that holds for every command in the family: it is a live
owner speaking for itself, so no command may go behind it and read the store instead. It is neither
the busy answer nor the unreadable one — nothing needs repairing, and the same command answers
normally once the requests in flight have landed — and it is the one condition under which a
read-only settings shape does not deliver its answer on standard output.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No mapped test drives more concurrent requests at one owner than its admitted concurrency, so the refusal's text, stream and exit status are stated from OwnerReadLimits::default (concurrency 4) and the OwnerAtCapacity route in crates/vigil/src/lib.rs rather than bound by a witness.` -->

## Global options

| Option | Meaning |
|---|---|
| `--version` | Print the Vigil package version |
| `--help`, `-h` | Print the current built-in help |

<!-- vigil-unenforced: classification=documentation-gap; reason=`No exact help/version test binds this global-options table to the binary surface.` -->

## Unavailable commands

The current binary has no `status`, `health`, `config check`, `config validate`, `correct`, `state-at`, `why-flagged`, `scan onvif`, `scan rtsp`, or `support-bundle` command. Health is an HTTP endpoint, corrections use Home Assistant/MQTT or the review HTTP API, and configuration validation happens while `vigil run` loads its inputs. These names remain unavailable until implemented and tested.

<!-- vigil-unenforced: classification=future-surface; reason=`These named status, validation, scan, replay, and support commands do not exist.` -->
