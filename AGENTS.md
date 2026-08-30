# AGENTS.md — Vigil repository guide

Where a change lives in this repository, and the invariants a change must not break. This file
carries no product intent: it states what the code in this checkout already does, and names the
guard that holds each statement. Read it before adding a module, a command, or a second way of
doing something this repository already does once.

## Crates

Three crates, one binary.

| Crate | Owns |
|---|---|
| `crates/vigil` | The runtime library. Configuration and the settings store, model, projection, domains and application (`config.rs`, `settings*.rs`); the Context Graph store handle (`store.rs`); the camera, media and detection pipeline (`camera_hub.rs`, `camera_track.rs`, `media_pipeline.rs`, `decode*.rs`, `encode.rs`, `detector*.rs`, `detection_*.rs`, `yolox_detector.rs`); recognition and corrections; the health and review HTTP planes; the command dispatch and the `--help` inventory (`lib.rs`); runtime-user adoption (`privilege.rs`); the owner-channel read path (`live_read.rs`); the fabric surface (`fabric.rs`). |
| `crates/vigil-ha` | The Home Assistant adapter and nothing else: MQTT discovery, entity state, and command handling (`ha_discovery.rs`, `ha_mqtt_tasks.rs`). Home Assistant discovery and transport vocabulary belongs here; `crates/vigil/tests/home_assistant_vocabulary_scan.rs` refuses it in the core crate. |
| `crates/vigil-bin` | The composition root: `src/main.rs` wires the adapter onto the core and is the `vigil` binary, and the suites under `tests/` drive that compiled binary as a real process. |

Everything outside those three: `addons/vigil/` holds the Home Assistant add-on manifest, its
`Dockerfile`, `provision-runtime-root.sh` and `runtime-packages.yaml`; the root `Dockerfile` and
`Dockerfile.hardware` build the standalone images; `xtask/` holds `verify`, the closeout impact
tool, and the test-estate guards that read `.config/*.toml`.

## Invariants

**The live-command family is closed, and it answers over the store owner channel.**
`commands_that_ask_the_running_deployment` in `crates/vigil/src/lib.rs` names exactly `events`,
`why`, `stats`, `settings`, `enroll` and `forget`. Each is answered by the process that owns this
deployment's store, through Context Graph's authenticated owner channel — the store path is the
whole address — and reads the store file directly only when no runtime is holding it. Vigil has no
control socket, and none is to be reintroduced:
`crates/vigil-bin/tests/control_socket_override_is_a_migration_error.rs` refuses a socket path
module, a socket transport call, and the retired `VIGIL_CONTROL_SOCKET` variable, and
`crates/vigil/tests/every_command_that_reaches_the_owner_channel_is_in_the_family.rs` refuses a
dispatch arm that reaches the channel from outside the family list.

**Locations are resolved once, then the runtime identity is adopted before anything is opened.**
`configured_locations` resolves the deployment directory and the exact store file at the entry
point, through the same rule the daemon resolves them with, and that answer is carried from there —
never re-read part-way through an operation, never rebuilt by joining a default filename onto
whatever directory is in hand. The dispatch then runs
`privilege::adopt_runtime_user_for_live_command` before the owner channel, the store, or anything
else is touched, because that channel authorizes on the peer's operating-system user and nothing
else.

**A live command creates and chowns nothing.** `runtime_user_plan_for_live_command` is identity
work only and is handed no store path; a read or an edit brings no deployment into existence.
Asking a deployment that has never started what it would run at leaves its data directory exactly
as it was — `crates/vigil-bin/tests/settings_live_operator_visibility.rs` for the settings shapes,
and `crates/vigil-bin/tests/a_read_of_a_never_started_deployment_creates_nothing.rs` for the whole
family, each command in a directory of its own.

**A question and a change take two different doors, and neither asks whether the store is there.**
Every command that only ASKS opens read-only. The settings shapes — the listing, `settings list`,
`settings find`, the read-only `settings identity`, and the consequence an unconfirmed
`settings identity change` states — go through `SettingsStore::open_to_read`, and `why` and
`events` go through `store::open_for_review_read` and the typed graph reads on
`ConsumerReader::graph()`. Both are Context Graph's `ConsumerReader`: one no-create read session
each, so operators asking at the same moment coexist and the store's bytes are identical
afterwards. Every command that CHANGES something opens writable and existing-only — `settings set`,
`settings reset` and a confirmed `settings identity change` through
`SettingsStore::open_existing_for_change`, `enroll` and `forget` through
`store::open_existing_for_edit` — which refuses a store that is not there instead of creating one
and leaves the directory byte-for-byte as it found it.

**No door is guarded by an existence check, and none is to be added.** `Path::exists()` answers
about a different moment than the open that follows it, and it answers `false` for a configured
store whose volume never mounted exactly as it does for a directory nobody has run anything in. The
engine's typed outcome is the whole authority: the store is not there, it needs one writable open
to settle its committed image, or it is there and cannot be read. `classify_store_open_at`
classifies what its own open returned rather than looking first, and `direct_read_local` answers a
never-started deployment from the same typed refusal. The middle outcome is the ONLY one a question
may answer by opening the store for writing — `SettingsStore::open_to_read` does exactly that and
nothing else, because a mutating open on damage nobody has diagnosed is how a store somebody could
still have recovered by hand stops being recoverable at all.

**Vigil declares Context Graph's own tables from Context Graph's own published specs.** The review
reads reach eleven cg-owned tables, and `store::REVIEW_READ_TABLES` names them against
`context_graph::schema::TABLES` rather than against a copy of the DDL text: the consumer door
verifies a declaration against what the store actually persisted and refuses a stale one by type,
so a copied `CREATE TABLE` string is a drift bomb with a date on it. Neither reader declares a
scope label. A label on a reader is a request to be NARROWED, read narrowing is the engine's
`SCOPE_LABEL_READ` vocabulary, and the pushed settings table's plain `SCOPE_LABEL ('server')`
constrains writes only — the node's `edge` label says what it may author, never what it may see.

**Help and the dispatched set are one statement.** `public_commands()` in `crates/vigil/src/lib.rs`
is the single list `print_help` renders, and
`crates/vigil-bin/tests/help_names_every_public_command.rs` holds the help output, that list and the
binary's own dispatch to each other. A subcommand an operator may run joins that list;
`detector-probe` is the one dispatched subcommand deliberately kept off it.

**Settings carry requested, running, pending and source truth.** The settings store is the
authority and the surfaces are authors that write records into it; a resolved line carries the value
requested, what the process is running, what closes any gap between them, when the value takes
effect, and the author, surface and scope that recorded it. Attribution is stored, not recomputed,
so a stopped deployment answers with the surface that actually authored a value. Add a setting
through the registry declaration and its timing declaration, never by reading a value straight from
the environment or a file at the point of use. That rule governs declaring a setting and resolving
what it is worth; the seam that CONSUMES the value stays in the module that owns the behavior. So a
change to how many frames the accelerated detector samples is detector work under the Burn
invariant even though `detector_sample_frames` is declared through the registry, and a change to
what the setting is worth, who may author it, or when it takes effect is registry work.

**Burn is the detector runtime.** `yolox-burn` on Burn is the detector, with `burn-wgpu` behind the
`detect-burn-wgpu` feature for the accelerated backend (`crates/vigil/Cargo.toml`). Detection work
belongs in `detector.rs`, `yolox_detector.rs`, `detection_accel.rs`, `detection_transition.rs` and
`detector_workclass.rs`; do not introduce a second inference runtime beside them.

**Every configured sampled frame gets its own proved inference.** `sampled_frame_indices` and
`sampled_detector_rgb` in `crates/vigil/src/media_pipeline.rs` turn a request for five sampled
frames into five frames handed to the detector. Collapsing a multi-frame sample to one inference is
a defect on both the software and the accelerated backend, not an optimization.

**A worker answer is settled on every exit.** A node that runs a camera settles its
worker-detector slot for every eligible camera and on every path out — an early return, an error, a
panic, and teardown — so nothing is left reporting that it is still starting. The failure text names
the model that was staged and what loading it actually said
(`detector_load_failure_reason` in `crates/vigil/src/runtime.rs`), because that line is the whole of
what the operator gets.

**Packaging changes go through the packaging surfaces.** An image, runtime-root, privilege, or
worker-stack change belongs in `addons/vigil/` (`config.yaml`, `Dockerfile`,
`provision-runtime-root.sh`, `runtime-packages.yaml`) or the root Dockerfiles, under the guards that
read them — `crates/vigil/tests/addon_config_surface.rs`,
`crates/vigil/tests/addon_least_supervisor_role.rs`,
`crates/vigil/tests/runtime_packages_manifest.rs`,
`crates/vigil/tests/artifact_profile_honesty.rs`. Do not compensate for a packaging gap in
application defaults.

## Never author a second path

This repository is a thin, surveillance-specific layer over Context Graph, which is itself a layer
over ContextDB. Anything generic — embedding, matching, memory, storage, transport, learning
machinery — belongs upstream and is consumed from there. Before adding a trait, a store, or a
transport here, check whether the layer below already owns that concept; if it does but its shape
does not fit, the fix is the small additive change upstream, never a Vigil-side shadow of it.

Within this repository the same rule applies to refusals and answers: one classifier, one
projection, one dispatch. A read command classifies the typed failure its own open returned rather
than opening the store again to ask
(`crates/vigil/tests/a_failed_open_is_classified_from_the_error_it_returned.rs`), and both routes of
a live command render the same projection.

## Before calling a change done

Run `scripts/verify` and `cargo xtask test-estate-check`. A new test file, a renamed test function,
and a changed documentation paragraph each need their registration in `.config/` — the check names
what is missing and prints the digests to record.
