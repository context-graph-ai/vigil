# Why this fired

Vigil records enough context with a detection to answer a specific question: which camera observation fired, which detector configuration produced it, why that configuration existed, and which local media supports it. This is provenance, not a generated explanation.

<!-- vigil-unenforced: classification=non-contract-context; reason=`This is explanatory framing for provenance rather than a new endpoint contract.` -->

## The chain

The current chain is:

<!-- vigil-unenforced: classification=non-contract-context; reason=`This sentence only introduces the chain displayed on the following line.` -->

`Site Context → baseline watch Intention → detector-config Decision → Camera Entity → detection Observation → clip and detector-image EvidenceRefs`

<!-- vigil-unenforced: classification=documentation-gap; reason=`The exact six-part provenance chain lacks an adjacent first-light acceptance mapping.` -->

The detection Observation carries the detected class, confidence, bounding box, frame index, event time, and the detector Decision ID. The Decision carries the model ID and threshold and serves the baseline watch Intention. The camera Entity and Site Context provide the configured names.

<!-- vigil-claim: `vigil.docs-why-this-fired.the-detection-observation-carries-the-detected-class` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_walks_observation_to_clip_to_decision_to_context` -->

This chain is created from the running configuration. Current Vigil does not let a user author the Intention text or attach a free-form rationale to the Decision.

<!-- vigil-unenforced: classification=future-surface; reason=`User-authored intentions and free-form decision rationale are not implemented.` -->

## Find a detection ID

Run:

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil events
```

The command lists recent landed detections newest first. Motion-only segments and correction rows are excluded. Each row includes the Observation ID used by the other review surfaces.

<!-- vigil-claim: `vigil.docs-why-this-fired.the-command-lists-recent-landed-detections-newest` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_events_lists_recent_events_newest_first` -->
<!-- enforced by: `vigil-bin::correction_core_paths::detection_only_events_excludes_corrections` -->

Home Assistant detection events carry the same value as `detection_id`. The review HTTP API exposes it as `observation_id` in `GET /events`.

<!-- vigil-claim: `vigil.docs-why-this-fired.home-assistant-detection-events-carry-the-same` -->
<!-- enforced by: `vigil-ha::ha_discovery::tests::detection_event_payload_carries_current_contract_without_future_zone_field` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->

## Walk the chain at the CLI

Run either a named walk or the newest detection walk:

```console
VIGIL_DATA_DIR=/var/lib/vigil vigil why <detection-id>
VIGIL_DATA_DIR=/var/lib/vigil vigil why --latest
```

Replace `/var/lib/vigil` with the running daemon's `data_dir`. These commands do not read its TOML
configuration; without `VIGIL_DATA_DIR` or an exact `VIGIL_STORE_PATH`, they use `./vigil-data`
and can silently inspect a different store.

<!-- vigil-unenforced: classification=documentation-gap; reason=`No adjacent CLI contract binds config omission and wrong-store fallback for why commands.` -->

The result includes the detection and event time, camera and site, class, confidence, bounding box
and frame, model and threshold, Intention and Decision IDs, and evidence references. An unknown UUID
returns a clean error instead of substituting another event.

<!-- vigil-claim: `vigil.docs-why-this-fired.the-result-includes-the-detection-and-event` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_walks_observation_to_clip_to_decision_to_context` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_on_unknown_event_id_errors_cleanly` -->

A malformed detection ID is refused as a malformed ID rather than reported as a missing event: the
answer names what was typed and the form a detection ID takes, so an operator who mistyped one is
sent to their own typing rather than to a recording nobody ever named.

<!-- vigil-claim: `vigil.docs-why-this-fired.a-malformed-detection-id-is-refused-as` -->
<!-- enforced by: `vigil-bin::why_refuses_an_id_it_cannot_serve::a_malformed_detection_id_is_refused_as_a_malformed_id_and_not_as_a_missing_event` -->

When the daemon owns the open store, the CLI asks that process for the answer over the store's own owner channel — the store path is the whole address, and there is no separate socket to configure or clean up. If no runtime is holding the store, the CLI reads the same local store directly. It does not send the review request to a cloud service.

<!-- vigil-claim: `vigil.docs-why-this-fired.when-the-daemon-owns-the-open-store` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_served_by_the_store_owner_while_the_store_is_locked` -->
<!-- enforced by: `vigil-bin::first_light_loop::review_and_stats_surfaces_make_no_network_call` -->

## Use the local review data

The Rust data plane provides `GET /events`, `GET /why/{detection-id}`, and local `/media/{file}`
routes. The why response deliberately omits the camera RTSP URL, so review clients do not receive
camera credentials embedded in a source URL.

<!-- vigil-claim: `vigil.docs-why-this-fired.the-companion-vigil-review-card-reads-event` -->
<!-- enforced by: `vigil::http_data_plane::event_list_serves_full_review_row_fieldset` -->
<!-- enforced by: `vigil::http_data_plane::why_walk_back_serves_provenance_excludes_rtsp_url_and_lists_correction` -->
<!-- enforced by: `vigil::http_data_plane::media_reference_from_event_list_resolves_to_media_bytes` -->

A companion Home Assistant integration and review card are planned to consume these routes, but
they are not currently shipped.

<!-- vigil-unenforced: classification=external-procedure; reason=`The companion integration and review card are not currently distributed as a public Vigil surface.` -->

If media has been pruned, the event and provenance row can still exist while the media route returns not found. The UI must present that as missing media, not as a missing event.

<!-- vigil-claim: `vigil.docs-why-this-fired.if-media-has-been-pruned-the-event` -->
<!-- enforced by: `vigil::http_data_plane::pruned_media_returns_clean_not_found_while_event_still_lists_and_walks_back` -->

## Configuration as of the event

`vigil why` follows the Decision linked to the detection. If the detector configuration changes later, an older event still reports the model and threshold that produced that event rather than today's values.

<!-- vigil-claim: `vigil.docs-why-this-fired.vigil-why-follows-the-decision-linked-to` -->
<!-- enforced by: `vigil-bin::first_light_loop::vigil_why_reports_config_as_of_event_time_not_current` -->

That is event-level historical provenance. It is not a general `state_at(timestamp)` replay command.
<!-- vigil-unenforced: classification=product-decision; reason=`Event-specific provenance is deliberately distinct from general timestamp replay.` -->

## Corrections

A correction is a durable Observation anchored to one detection. Current correction types are:

<!-- vigil-unenforced: classification=documentation-gap; reason=`The current correction-type inventory lacks an adjacent wire-format contract.` -->

- **Identity** — confirms the event; an optional label can be retained.
- **WrongClass** — records the corrected class label.
- **FalseAlarm** — marks the detection as a false alarm.
- **Enroll** — names the detected subject and adds its sighting as a recognition reference.

The event list exposes the server-authoritative current correction, corrected label, confirmation state, and whether a negative correction was recorded. The why view lists the corrections anchored to that event.

<!-- vigil-claim: `vigil.docs-why-this-fired.the-event-list-exposes-the-serverauthoritative-current` -->
<!-- enforced by: `vigil::ha_correction_seam::review_events_rows_expose_current_correction_authority_fields` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_reads_back_via_review_why` -->

Corrections survive store reopen. Repeating the same detection ID, correction type, and label is idempotent; two genuinely different corrections on the same detection are both retained.

<!-- vigil-claim: `vigil.docs-why-this-fired.corrections-survive-store-reopen-repeating-the-same` -->
<!-- enforced by: `vigil::ha_correction_seam::correction_survives_daemon_restart` -->
<!-- enforced by: `vigil-bin::mqtt_composed_product::correction_commands_via_mqtt_land_in_cg_through_the_composed_binary` -->

Current corrections do not retrain the detector, adjust its threshold, or generate a rule. Enrollment affects later recognition matching; the other correction types remain durable review facts.

<!-- vigil-unenforced: classification=future-surface; reason=`Corrections do not yet tune detection or create persistent rules.` -->

## Recognition provenance

When a covered-class sighting matches an enrolled subject, the why view can show the subject name, similarity score, matched reference label, and the correction that enrolled that reference. A result below the configured threshold stays unknown.

<!-- vigil-claim: `vigil.docs-why-this-fired.when-a-coveredclass-sighting-matches-an-enrolled` -->
<!-- enforced by: `vigil::recognition_slice::why_view_shows_match_provenance_reference_score_and_enrolling_correction` -->
<!-- enforced by: `vigil::recognition_slice::below_threshold_sighting_stays_unknown` -->

## Replay

**Unavailable as a Vigil command or UI.** The current CLI has no `state-at` command, and the review server has no timestamp replay endpoint. Event-specific `why` walks preserve the configuration attached to that event, but they do not expose the site's entire worldview at an arbitrary time.

<!-- vigil-unenforced: classification=future-surface; reason=`Timestamp replay has no CLI command, HTTP endpoint, or full-worldview implementation.` -->

## Invalidations

**Unavailable in the Vigil runtime.** There is no `why-flagged` command, invalidation view, or Home Assistant invalidation notification today. Context Graph's substrate capability is not a shipped Vigil user surface until the Vigil rule/invalidation loop is implemented and tested.

<!-- vigil-unenforced: classification=future-surface; reason=`Invalidation commands, views, notifications, and the rule loop are absent.` -->
