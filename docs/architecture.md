# Architecture

## One edge process

Vigil serves health and the review HTTP data plane from the same Rust process.

<!-- vigil-claim: `vigil.docs-architecture.vigils-edge-runtime-is-one-rust-binary` -->
<!-- enforced by: `vigil-bin::http_data_plane_binary_coexistence::health_liveness_still_serves_alongside_data_plane_in_single_binary` -->

The edge application is packaged as one Rust binary and is intended to own configuration, camera
ingest, media decode, motion gating, object detection, event evidence, Context Graph writes, the
local owner channel the CLI reaches it on, Home Assistant MQTT tasks, and optional recognition
without a separate database server. The current suite does not bind that complete process inventory in one direct
acceptance.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct acceptance binds the complete one-process subsystem inventory and absence of a separate database server.` -->

The Home Assistant add-on, container, and local-service forms package that same application. Mosquitto and Home Assistant are integrations at the product boundary, not hidden Vigil storage or inference services.
<!-- vigil-unenforced: classification=product-decision; reason=`Packaging one application and keeping Home Assistant/Mosquitto at the boundary defines architecture.` -->

## Storage stack

The in-process dependency direction is:

```text
Vigil → context-graph → contextdb
```

Vigil owns camera, detector, evidence, review, correction, and Home Assistant behavior. Context Graph supplies typed Contexts, Intentions, Decisions, Entities, Observations, EvidenceRefs, graph relationships, and audit semantics. Contextdb supplies the embedded persistent database beneath Context Graph.

The default data directory is `./vigil-data` for a standalone process and `/data` in the add-on.
Unless overridden, the store is `store.contextgraph` beneath the data directory. Event clips and
detector images are intended to live in the data directory's `clips` tree and be referenced from
Context Graph rather than stored as opaque video blobs in the graph row.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The store-reopen tests do not prove the standalone/add-on defaults or the complete store-and-media path layout.` -->

Registered camera data and landed event history survive closing and reopening the local store.

<!-- vigil-claim: `vigil.docs-architecture.the-default-data-directory-is-vigildata-for` -->
<!-- enforced by: `vigil-bin::first_light_loop::camera_survives_store_reopen` -->
<!-- enforced by: `vigil-bin::first_light_loop::event_history_survives_store_reopen` -->

## Detection pipeline

The intended per-camera pipeline captures and decodes RTSP media, motion-gates frames, samples
detector inputs, runs YOLOX through Burn, applies confidence filtering and non-maximum suppression,
finalizes evidence, and records at most the selected event for a segment. No single adjacent test
currently binds that complete sequence.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The current witnesses do not bind every stage of the complete capture-to-event pipeline stated here.` -->

Vigil preserves confidence produced by the detector instead of manufacturing it from the configured
threshold, and an empty or undecodable stream produces no event.

<!-- vigil-claim: `vigil.docs-architecture.for-each-configured-rtsp-camera-vigil-captures` -->
<!-- enforced by: `vigil-bin::first_light_loop::confidence_is_detector_output_not_threshold_derived` -->
<!-- enforced by: `vigil-bin::first_light_loop::empty_or_undecodable_stream_lands_no_event` -->

The portable static artifact is intended to use software decode and CPU detection, while native
feature builds may add GStreamer hardware decode and Burn/WGPU detection. The current runtime tests
do not build and inspect both shipped artifact forms.

<!-- vigil-unenforced: classification=documentation-gap; reason=`Runtime fallback witnesses do not prove the portable-static and native artifact feature composition.` -->

In a CPU-only build, requesting unavailable acceleration records a backend-not-compiled fallback;
CPU fallback keeps the watchdog alive when detection falls behind.

<!-- vigil-claim: `vigil.docs-architecture.the-portable-static-artifact-uses-software-decode` -->
<!-- enforced by: `vigil::acceleration_receipts::cpu_only_artifact_with_accel_true_reports_backend_not_compiled_fallback` -->
<!-- enforced by: `vigil::health_watchdog_liveness::detector_behind_on_cpu_fallback_stays_alive_for_the_watchdog` -->

## Provenance write

Startup maintains four configuration nodes: Site Context, Camera Entity, baseline watch Intention, and detector-config Decision. A landed detection adds the Observation with video and detector-image evidence, producing the five-node explanation path users see through `vigil why`.

<!-- vigil-claim: `vigil.docs-architecture.startup-maintains-four-configuration-nodes-site-context` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_walks_observation_to_clip_to_decision_to_context` -->

Evidence is durable before the Observation is written. If clip or image persistence fails, Vigil writes no event that points at missing evidence and removes orphaned material from the failed path.

<!-- vigil-claim: `vigil.docs-architecture.evidence-is-durable-before-the-observation-is` -->
<!-- enforced by: `vigil-bin::first_light_loop::observation_never_references_undurable_clip` -->
<!-- enforced by: `vigil-bin::first_light_loop::disk_full_on_clip_write_surfaces_and_drops_no_evidence` -->

## Review paths

The daemon exposes two local review paths over the same store-backed functions:

- the CLI (`vigil events`, `vigil why`, `vigil stats`, and `vigil settings`), routed to whichever process owns the store, over that store's own owner channel — there is no separate control socket to configure, publish or clean up, and no socket path anywhere on this surface;
- the HTTP review data plane on port `8098` by default;

<!-- vigil-unenforced: classification=documentation-gap; reason=`No combined test binds the CLI and HTTP review-path inventory.` -->

A companion Home Assistant integration and review card are planned to proxy the HTTP contract
through Home Assistant authentication, but they are not a currently shipped Vigil surface.

<!-- vigil-unenforced: classification=external-procedure; reason=`The companion integration and review card are not distributed as a public Vigil release.` -->

The HTTP server offers `GET /events`, `GET /why/{id}`, `GET /media/{file}`, and `POST /correction`. Media paths reject traversal, clips support byte ranges, and why responses exclude the source RTSP URL.

<!-- vigil-claim: `vigil.docs-architecture.the-http-server-offers-get-events-get` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->
<!-- enforced by: `vigil::http_data_plane::why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction` -->
<!-- enforced by: `vigil::http_data_plane::media_path_traversal_rejected_serves_no_out_of_tree_bytes` -->
<!-- enforced by: `vigil::http_data_plane::clip_range_streams_from_large_file_without_whole_file_transfer` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->

## Home Assistant paths

The MQTT connection carries Home Assistant discovery, retained camera state, detection events,
snapshot bytes, camera-control commands, and correction commands. Live video is configured through
Home Assistant's Generic Camera flow, using `live_rtsp_url` when present and otherwise the detection
RTSP URL.

<!-- vigil-claim: `vigil.docs-architecture.the-mqtt-connection-is-a-control-and` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::discovery_registers_device_and_per_camera_subdevice` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::camera_enabled_switch_state_is_retained_and_updates_on_control_commands` -->
<!-- enforced by: `vigil-bin::mqtt_composed_product::correction_commands_via_mqtt_land_in_cg_through_the_composed_binary` -->
<!-- enforced by: `vigil-bin::correction_core_paths::detection_publishes_event_to_real_broker` -->
<!-- enforced by: `vigil-ha::ha_mqtt_broker::discovery_published_to_real_broker_on_start` -->
<!-- enforced by: `vigil-bin::mqtt_composed_product::snapshot_command_publishes_the_latest_detector_evidence_through_the_composed_binary` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_falls_back_to_detection_rtsp_url_for_single_stream_cameras` -->
<!-- enforced by: `vigil::runtime::tests::generic_camera_url_prefers_live_rtsp_url_over_detection_rtsp_url` -->
<!-- enforced by: `vigil::supervisor::tests::build_generic_camera_flow_step_payload_nests_advanced_fields` -->

The review plane is separate from MQTT because a rich event client needs ordered rows, provenance,
and streamed media. A future companion Home Assistant integration may proxy that data, but no
currently shipped integration or card owns corrections or duplicates Vigil's store.

<!-- vigil-unenforced: classification=external-procedure; reason=`The planned companion integration and review card are not currently shipped.` -->

## Recognition

Recognition is intended to remain off unless a weights directory is configured, crop covered-class
subjects when enabled, and allow a detection to land with an explicit report when recognition
fails. The current matching witnesses do not bind those enablement, crop-selection, and failure
behaviors.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The mapped matching tests do not prove recognition enablement defaults, covered-class crop selection, or detection survival after recognition failure.` -->

The implemented recognition seam records a same-site match with its vector and score, and honors
the configured recognition threshold without a hidden floor.

<!-- vigil-claim: `vigil.docs-architecture.recognition-is-off-unless-a-weights-directory` -->
<!-- enforced by: `vigil::recognition_slice::match_records_observation_against_the_matched_entity_with_vector_and_score` -->
<!-- enforced by: `vigil::recognition_slice::configured_recognition_threshold_is_honored_without_a_hidden_floor` -->

## Distributed detection

Distributed detector work is intended to compile only with Vigil's default-off `fabric` feature,
with no fabric transport or shared-work-ledger code linked into a build without that feature. The
current fabric-only runtime witnesses do not inspect a non-fabric binary's linkage.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No artifact witness inspects a non-fabric binary to prove transport and shared-ledger code are absent.` -->

In a fabric build, explicitly configured nodes can advertise detector capability, exchange detector
work, and fall back locally if a claimed remote worker dies before returning its result.

<!-- vigil-claim: `vigil.docs-architecture.distributed-detector-work-is-compiled-only-with` -->
<!-- enforced by: `vigil::fabric_config_defaults::all_offload_and_fabric_knobs_have_sane_defaults_and_work_unset` -->
<!-- enforced by: `vigil::detector_worker::worker_advertises_truthful_backend_claims_materializes_runs_records_once` -->
<!-- enforced by: `vigil::kill_worker_fallback::worker_death_midlease_falls_back_local_with_named_receipt` -->

A node serves the fleet as soon as its detector exists, not on a schedule. The worker loop starts
the moment a detector arrives — the first camera's, or, on a node with no camera at all, the one it
loads for itself — however long that took, and it starts exactly once however many detectors follow.
Nothing anywhere on that path watches a clock: there is no deadline on becoming ready and no setting
that adjusts one, because a slow cold start is not a failure and a node written off for one stays
not-serving for the rest of its life. A node that genuinely cannot serve says why with the real
reason — a model that is missing or will not load names the staging fix and advertises nothing, and
a node with no serving role says that instead — and an operator stopping the node cancels a start
that is still waiting rather than bringing a worker up on the way down.

<!-- vigil-claim: `vigil.docs-architecture.a-node-serves-the-fleet-as-soon` -->
<!-- enforced by: `vigil::fabric::worker_detector_slot_tests::a_detector_that_arrives_before_anything_waits_still_starts_exactly_one_worker` -->
<!-- enforced by: `vigil::fabric::worker_detector_slot_tests::an_operator_stop_settles_a_waiting_start_and_nothing_starts_after_it` -->
<!-- enforced by: `vigil-bin::cameraless_worker::detector_job_registers_as_vigil_detector_class_slot_populates_without_a_camera` -->
<!-- enforced by: `vigil-bin::cameraless_worker::cameraless_worker_without_a_loadable_model_does_not_advertise_and_reports_no_model` -->

Every capability delivery attempt ends on one greppable line saying it resolved and how — delivered,
refused, or never attempted because this node hosts the hub over its own store and has no hub to
dial. A delivery that simply worked used to print nothing at all, which left an operator with an
absence to guess from and no way to tell an attempt that succeeded from one that had not happened
yet. A refused delivery still prints its own named error line beside it.

<!-- vigil-claim: `vigil.docs-architecture.every-capability-delivery-attempt-ends-on-one` -->
<!-- enforced by: `vigil-bin::capability_delivery::failed_capability_push_emits_named_error` -->
<!-- enforced by: `vigil-bin::two_process_fabric::two_real_processes_render_remote_detectors_line_hub_first` -->

Fabric is not a cloud requirement and does not provide multi-site product management. Enrollment and frame movement must be deliberately configured; the ordinary single-node runtime works with no ticket and no hub.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No single-node acceptance binds the no-ticket, no-hub, no-cloud requirement stated here.` -->

## Deliberately absent

The current Vigil runtime has no user-authored rule engine, general timestamp replay endpoint, invalidation notification surface, voice/Assist integration, hosted understanding call, or required Vigil cloud connection. Multi-site management and a hosted control plane are outside the current OSS runtime.

<!-- vigil-unenforced: classification=future-surface; reason=`Rules, replay, invalidation, voice, and hosted control are deliberately absent surfaces.` -->
