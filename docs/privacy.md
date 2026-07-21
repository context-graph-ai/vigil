# Privacy and access

Vigil's OSS runtime is local-first: camera ingest, detection, evidence storage, recognition, review,
and corrections run on machines the operator controls. "Local-first" does not mean "no network"—a
camera system necessarily connects to cameras and may connect to Home Assistant, MQTT, and Vigil
nodes the operator explicitly enrolls.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This paragraph defines local-first scope without asserting a separately testable endpoint.` -->

This page separates what the current code proves from privacy behavior that is approved but not yet
implemented.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence states the page’s editorial separation of code and approved work.` -->

## What stays local

A landed detection references one local, durable, decodable clip and one local detector image.

<!-- vigil-claim: `vigil.docs-privacy.detection-clips-detector-images-context-graph-memory` -->
<!-- enforced by: `vigil::first_light_loop::detection_produces_observation_referencing_clip` -->
<!-- enforced by: `vigil::first_light_loop::observation_never_references_undurable_clip` -->

Context Graph memory, camera-disabled markers, and runtime statistics are intended to stay below
the configured data directory or store path. The mapped detection-evidence tests do not bind that
complete on-disk layout.

<!-- vigil-unenforced: classification=documentation-gap; reason=`The evidence durability witnesses do not prove paths for Context Graph memory, camera-disabled markers, and runtime statistics.` -->

`vigil events`, `vigil why`, and `vigil stats` read local state and make no outbound network call.
Opening the local store keeps the text embedder disabled, so review does not fetch a model.

<!-- vigil-claim: `vigil.docs-privacy.vigil-events-vigil-why-and-vigil-stats` -->
<!-- enforced by: `vigil::first_light_loop::review_and_stats_surfaces_make_no_network_call` -->
<!-- enforced by: `vigil::first_light_loop::store_opens_with_text_embedder_disabled_no_model_fetch` -->

The first-light camera loop's network trace permits only the explicitly configured RTSP source; it
does not contact a model host, telemetry collector, or other service on that path.

<!-- vigil-claim: `vigil.docs-privacy.the-firstlight-camera-loops-network-trace-permits` -->
<!-- enforced by: `vigil::first_light_loop::first_light_loop_makes_no_network_call_beyond_rtsp` -->

## Expected local-network traffic

Depending on configuration, Vigil communicates with:

<!-- vigil-unenforced: classification=non-contract-context; reason=`This is only an introduction to the network-endpoint list that follows.` -->

- the RTSP endpoint for each configured camera;
- Home Assistant Supervisor to discover MQTT credentials and register Generic Camera entries when
  running as an add-on;
- the configured MQTT broker for discovery, service/camera state, detection metadata, the latest
  detector image, control commands, and corrections;
- another explicitly enrolled Vigil node when distributed compute is enabled.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No one network contract binds camera, Supervisor, MQTT, and enrolled-node traffic inventory.` -->

MQTT is gated off when no broker is configured. A broker failure cannot erase a correction that
was already durably recorded in the local store.

<!-- vigil-claim: `vigil.docs-privacy.mqtt-is-gated-off-when-no-broker` -->
<!-- enforced by: `vigil::ha_mqtt_broker::mqtt_gated_off_when_no_broker_configured` -->
<!-- enforced by: `vigil::ha_mqtt_broker::broker_drop_does_not_affect_durable_cg_record` -->

Vigil does not currently ship a hosted control-plane client or default telemetry exporter. Do not
turn that into an absolute "never communicates off box" claim: the operator-supplied endpoints
above are network surfaces, and an MQTT broker or camera can itself be remote.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This caution prevents an overbroad off-box claim rather than promising new behavior.` -->

## Access boundary

The current Vigil HTTP services have no login and bind to all interfaces.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`HTTP listeners have no login and bind every network interface.` -->

The review service does not opt browsers into cross-origin access.

<!-- vigil-claim: `vigil.docs-privacy.review-service-does-not-opt-browsers-into-cross-origin-access` -->
<!-- enforced by: `vigil::http_data_plane::review_data_plane_does_not_enable_cross_origin_access` -->

Anyone who can reach the review port can still call its HTTP endpoints directly to read events and
media or submit corrections. Do not expose ports 8098 or 8099 to the public internet. Put the
current build on a trusted, firewalled network.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Direct port reachability grants event/media reads and correction writes without authentication.` -->

This is a pre-OSS implementation blocker. The approved release boundary is:
<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence introduces the separately classified approved access-boundary decision list.` -->

- Home Assistant Ingress supplies the authenticated session on HAOS;
- a standalone deployer supplies its own authenticated reverse proxy or deliberately chooses a
  trusted-LAN deployment;
- Vigil gains configurable listen interfaces and serves its UI same-origin;
- whoever is past that boundary has full read/write access—Vigil will not add a second login.

<!-- vigil-unenforced: classification=product-decision; reason=`Ingress/proxy/trusted-LAN ownership and no second login are approved access-boundary choices.` -->

No existing test enforces that future boundary, so this section intentionally has no enforcement
tag.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No acceptance test enforces the approved future deployment boundary.` -->

## Media serving

The review data plane serves event media from the data directory and rejects path traversal. A
large clip byte-range response returns only the requested bytes. Missing or pruned media returns a
clean not-found response while the event can remain in the audit trail.

<!-- vigil-claim: `vigil.docs-privacy.the-review-data-plane-serves-event-media` -->
<!-- enforced by: `vigil::http_data_plane::media_path_traversal_rejected_serves_no_out_of_tree_bytes` -->
<!-- enforced by: `vigil::http_data_plane::clip_range_streams_from_large_file_without_whole_file_transfer` -->
<!-- enforced by: `vigil::http_data_plane::pruned_media_returns_clean_not_found_while_event_still_lists_and_walks_back` -->

## Retention

Automatic retention is **not implemented yet**. Current detection evidence remains until it is
removed outside the normal runtime path. Do not rely on the release defaults until retention work
lands.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Evidence has no automatic retention and can grow until externally removed.` -->

The approved pre-OSS contract is:

- continuous recordings: 7 days by default;
- event clips: 14 days by default;
- snapshots: 7 days by default;
- one per-machine space budget and shared pool, with one retention clock and per-camera overrides;
- oldest-first reclamation, including startup/emergency reclamation when the disk is already full;
- small audit memory retained while bulky evidence follows its retention clock;
- every automatic adjustment recorded so it can be reviewed.

<!-- vigil-unenforced: classification=product-decision; reason=`Retention periods, budget pooling, reclamation, and audit rules are owner-approved choices.` -->

These are product decisions, not observed behavior. Enforcement tags will be added only after tests
exercise real pruning and disk reclamation.
<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence labels the preceding retention list and explains why no tests bind it yet.` -->

## Recognition data

The recognition seam stores a crop-derived vector and match score in an observation anchored to the
sighting detection and matched entity. Enrolling a sighting creates a named site-local reference;
the underlying forget operation removes those references so later sightings revert to unknown.

<!-- vigil-claim: `vigil.docs-privacy.when-recognition-is-enabled-vigil-stores-a` -->
<!-- enforced by: `vigil::recognition_slice::enroll_correction_creates_named_entity_and_later_sightings_match` -->
<!-- enforced by: `vigil::recognition_slice::forget_removes_references_and_the_subject_reverts_to_unknown` -->
<!-- enforced by: `vigil::recognition_slice::match_records_observation_against_the_matched_entity_with_vector_and_score` -->

The `vigil forget NAME` CLI is intended to route to that operation, but the mapped recognition tests
invoke the store operation directly rather than the command surface.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No direct CLI acceptance proves vigil forget argument and store routing into the tested reference-removal operation.` -->

The current implementation stores sighting vectors inline in durable observations, so they do not
age out automatically. Before OSS release, raw sighting vectors move to a prunable evidence lane and
default to the 14-day event-clip window. Enrolled reference vectors remain until explicitly
forgotten; small history such as the matched name, score, and camera remains as audit memory. After
the sighting window, forgetting the enrolled references is intended to be irreversible.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Durable inline sighting vectors have no automatic expiry or prunable evidence lane.` -->

That future retention behavior is not yet enforced by a test and therefore has no tag.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No retention test enforces sighting expiry, reference survival, or irreversible forgetting.` -->

## Corrections and audit history

Corrections are stored in the local Context Graph store, tied to the named detection, and survive a
daemon restart. The current HTTP, MQTT, and CLI correction paths do not yet record the complete
author/channel trust information required for release.

<!-- vigil-claim: `vigil.docs-privacy.corrections-are-stored-in-the-local-context` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_survives_daemon_restart` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_anchored_to_named_detection_only` -->

Before OSS release, each correction records its channel and an honestly classified author: a
front-door assertion for HTTP, a claim for MQTT, and the local CLI origin. Anonymous remains a
fallback, not the approved release default.

<!-- vigil-unenforced: classification=product-decision; reason=`Correction channel and honestly classified author provenance are approved release behavior.` -->

## Credentials and logs

RTSP credentials embedded in a URL are stripped from the runtime session/log surface, and separate
credentials authenticate without putting user information in the URL.

<!-- vigil-claim: `vigil.docs-privacy.rtsp-credentials-embedded-in-a-url-are` -->
<!-- enforced by: `vigil::first_light_loop::credentialed_rtsp_url_authenticates_and_redacts_runtime_surface` -->
<!-- enforced by: `vigil::first_light_loop::separate_rtsp_credentials_authenticate_without_url_userinfo` -->

The current binary still parses a password command-line flag. It is forbidden for release because
process arguments can be visible to other software on the host. Add-on options, TOML, and an
environment variable avoid argv exposure but still hold a plain string; they are not secret vaults.
Restrict access to those inputs and `/data/options.json`. The final configuration work removes
passwords from argv and uses a non-printable secret type.

<!-- vigil-unenforced: classification=implementation-blocker; reason=`Password argv parsing exposes secrets and the non-printable secret path is unfinished.` -->

## Distributed compute and media movement

Distributed compute is off unless a node is enrolled. When enabled and allowed, an ephemeral
compressed clip can move directly to another same-tenant Vigil node under detector pressure; it is
not routed through a hosted hub. `fabric_allow_frame_offload = false` prevents this node from
sending its camera clips while still allowing it to join and accept work.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent privacy contract binds enrolled-node media movement and offload disable semantics.` -->

Do not enable this as a release feature yet. Work movement and worker-death recovery have been
proven, but offloaded/rescued jobs can currently return empty detection content. That is a release
implementation blocker.
<!-- vigil-unenforced: classification=implementation-blocker; reason=`Offloaded and rescued jobs can return empty detection content despite proven transport.` -->

## Support bundles and deletion

There is no `vigil support-bundle` command and no user-facing event-erasure command today. Do not
promise either surface. The release work must define bundle contents/redaction and test exactly
what event deletion removes versus what audit metadata remains.

<!-- vigil-unenforced: classification=future-surface; reason=`Support-bundle and user-facing event-erasure commands are not implemented.` -->
