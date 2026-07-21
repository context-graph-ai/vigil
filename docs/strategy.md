# Product strategy and boundaries

## What Vigil is

Vigil is a local-first camera and NVR intelligence runtime for Home Assistant users. The product
destination is a direct Frigate replacement with an additional memory property: detections,
evidence, active configuration, operator corrections, and later recognition remain connected
instead of becoming separate logs and UI state.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This is product positioning and destination language, not present runtime behavior.` -->

The developer preview already proves that memory property on one direct RTSP path. It records a
clip-backed detection with its camera, site, watch intention, and detector decision, then reads the
chain back through `vigil why`.
<!-- vigil-claim: `vigil.docs-strategy.the-developer-preview-already-proves-that-memory` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

Vigil embeds Context Graph for typed operational memory, which in turn embeds ContextDB for local
persistence. These are libraries inside the Vigil process; the current direct-camera path does not
require a separate database server or a hosted understanding service.
<!-- vigil-claim: `vigil.docs-strategy.vigil-embeds-context-graph-for-typed-operational` -->
<!-- enforced by: `vigil-acceptance::acceptance::installable_substrate::vigil_standalone_first_start_opens_local_context_graph_store` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

## Who Vigil is for

### Home Assistant power users

The first audience already operates some combination of Home Assistant, MQTT, Frigate, RTSP
cameras, NVRs, and mixed vendors. They need a local system they can inspect and automate without
giving up the camera workflows their home, farm, or property depends on.

### Multi-property self-hosters

The second audience runs different camera stacks at different properties. In the release model,
each property keeps a local runtime and local media while the operator gets the same concepts for
cameras, detections, evidence, health, recognition, and corrections. A hosted cross-property
control plane is not part of Vigil OSS.
<!-- vigil-unenforced: classification=product-decision; reason=`Per-property local runtimes and exclusion of hosted cross-property control define OSS scope.` -->

## The product principles

### Home Assistant is the primary operator surface

MQTT discovery and control provide the entity and automation plane; the review data plane supplies
events, evidence, `why`, and corrections to the richer Home Assistant review surface.
<!-- vigil-claim: `vigil.docs-strategy.mqtt-discovery-and-control-provide-the-entity` -->
<!-- enforced by: `vigil::ha_mqtt_broker::discovery_published_to_real_broker_on_start` -->
<!-- enforced by: `vigil::ha_mqtt_broker::operator_action_command_effects_action` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->

The first public release also includes a Vigil-native same-origin machinery dashboard embedded in
Home Assistant and reachable directly in standalone deployments. That dashboard is pre-release
work, not part of the current HTTP review service.

<!-- vigil-unenforced: classification=future-surface; reason=`The same-origin Vigil machinery dashboard is pre-release implementation work.` -->

### Local operation is the default

The current first-light path performs decode, detection, evidence writing, provenance, and review
against the local process and store. Its acceptance test permits the configured RTSP connection and
rejects unrelated outbound network connections.
<!-- vigil-claim: `vigil.docs-strategy.the-current-firstlight-path-performs-decode-detection` -->
<!-- enforced by: `vigil-acceptance::acceptance::one_camera_acceptance::frigate_replacement_loop_runs_over_direct_rtsp_synthetic` -->

The release expands that local contract to recording retention, deletion, health, and all ordinary
HA automation behavior without a cloud dependency.
<!-- vigil-unenforced: classification=product-decision; reason=`Cloud-independent retention, deletion, health, and HA behavior are approved release scope.` -->

### Achieved state beats configured intent

Vigil reports the decode and detection backend that actually runs. A requested accelerator that
falls back to software is visibly different from an accelerator the operator disabled.
<!-- vigil-claim: `vigil.docs-strategy.vigil-reports-the-decode-and-detection-backend` -->
<!-- enforced by: `vigil::acceleration_receipts::accel_false_is_disabled_not_fallback_and_they_are_distinct` -->
<!-- enforced by: `vigil::acceleration_receipts::stats_show_active_decoder_per_stream_and_detector_backend` -->

The same rule governs future dashboard work: missing counters remain unreadable, lost analysis
becomes an explicit coverage statement, and a worker route cannot be shown as verified without a
real result receipt.
<!-- vigil-unenforced: classification=product-decision; reason=`Dashboard truthfulness requires explicit loss and real result receipts before verification.` -->

### Corrections belong to durable memory

MQTT and HTTP correction entry points converge on the local correction record. Restarting the
daemon or losing the broker does not turn a recorded correction into client-local state.
<!-- vigil-claim: `vigil.docs-strategy.mqtt-and-http-correction-entry-points-converge` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_survives_daemon_restart` -->
<!-- enforced by: `vigil::ha_mqtt_broker::broker_drop_does_not_affect_durable_cg_record` -->
<!-- enforced by: `vigil::http_data_plane::http_correction_post_lands_through_record_correction_seam` -->

## How Vigil relates to existing products

| Product | Established strength | Vigil's direction |
|---|---|---|
| Frigate | Local Home Assistant NVR, detection, recording, and review | Replace the runtime while retaining per-event provenance and durable corrections |
| Scrypted | Camera interoperability and polished streaming integrations | Keep the Vigil runtime and operational memory local and auditable |
| Blue Iris | Mature Windows NVR operation | Offer a Rust-based, Home Assistant-first local runtime |
| ZoneMinder | Long-running open-source surveillance stack | Provide a smaller modern runtime with typed event memory |
| Vendor NVR apps | Appliance integration with the vendor's own cameras | Treat mixed RTSP, ONVIF, and NVR estates as the normal case |

This table states positioning, not current feature parity. The developer preview does not yet match
the mature products' complete ingest, recording, retention, and review surfaces.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph limits a comparison table and makes no feature-parity promise.` -->

## What Vigil is not

Vigil OSS is not:
<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence only introduces the separately classified OSS product-exclusion list.` -->

- a Frigate sidecar whose core behavior depends on Frigate staying installed;
- a cloud camera service or a hosted-media product;
- a login and identity provider;
- a natural-language agent that autonomously invents surveillance policy;
- a promise that every configured accelerator is active;
- a multi-tenant learning network hidden inside an open-source edge binary.

<!-- vigil-unenforced: classification=product-decision; reason=`Sidecar, cloud-media, login-provider, agent, and multi-tenant exclusions define product scope.` -->

## Where OSS ends

The Vigil OSS release owns one complete self-hosted site: camera ingest, detection, recording,
review, HA automation, site-local recognition and corrections, health, privacy, and explicit
retention. Some of that surface remains under pre-release implementation, as listed in
[the promise](promise.md).
<!-- vigil-unenforced: classification=product-decision; reason=`One complete self-hosted site is the approved ownership boundary for Vigil OSS.` -->

Vigil Enterprise owns managed onboarding, billing and support operations, hosted fleet management,
cross-site control, reusable intent catalogs, learned cross-tenant patterns, and tuning across a
customer network. An OSS deployment does not need those services to run locally.

<!-- vigil-unenforced: classification=product-decision; reason=`Managed onboarding, fleet control, and cross-tenant learning define the Enterprise boundary.` -->

## Release discipline

The repository becomes an installable OSS product only when the documented replacement surface is
backed by code, deterministic tests, and real Home Assistant and camera acceptance. Until then, the
README keeps the developer-preview label and every unfinished capability stays in future tense.
<!-- vigil-unenforced: classification=product-decision; reason=`Installable release status requires code, deterministic tests, and real HA/camera acceptance.` -->
