# Recognition first light — pre-registered acceptance contract

Pre-registered before implementation. Any later edit to this file is a product-scope change and
must be surfaced, not slipped in. The live-smoke section is the acceptance gate for this work in
both repos it spans (vigil and the context-graph memory library it embeds).

## Acceptance criteria

Enrollment and matching:

- **AC1 — Enroll from a real event.** A person detection event can be marked with a name (e.g.
  "Roshan") from the Home Assistant card's corrections overlay. The same operation is available as
  a CLI command; both ride one shared API. First enrollment of a name creates a named entity in
  the site's memory graph, scoped to the site context, holding the enrolled reference vector(s).
- **AC2 — Later sightings match, site-wide.** After enrollment, a new detection of the same
  subject on ANY covered camera at the site produces an event that carries the entity name — in
  the MQTT event payload and in the card's event rows. Matching is per-site, not per-camera.
- **AC3 — Unknown stays unknown, loudly.** A detection that matches no enrolled reference above
  the acceptance threshold is reported as "unknown <class>" — never a name. Below-threshold
  near-matches do not name the event.
- **AC4 — Every match is memory, with provenance.** Each match (and enrolled-class non-match) is
  recorded as an observation against the entity in the site memory graph, carrying the
  precomputed vector. `vigil why <event>` for a matched event shows: which enrolled reference
  matched, the similarity score, and the correction that enrolled it.
- **AC5 — All classes, one mechanism.** The same enroll-and-match mechanism works for a non-person
  class (the dog → "Max"). Nothing in the path is person-specific.
- **AC6 — Deletion removes references.** Deleting an enrolled entity removes its enrolled
  reference vectors from the site store; subsequent detections of that subject report unknown.

The vision embedder:

- **AC7 — Crop → vector on CPU.** Each detection of a covered class yields a crop from the
  detection's bounding box, embedded to a fixed-dimension vector by a locally-running,
  permissively-licensed (Apache-2.0/MIT, weights included) vision embedder. No cloud call, no
  egress — the recognition path is deterministic vectors end to end, with no LLM anywhere.
- **AC8 — Packaging holds.** The embedder compiles into the existing single static musl binary
  for both release targets. GPU acceleration is a backend selection on the same code path
  (named backend, off in this slice), not a rearchitecture.

The memory-library surface (context-graph):

- **AC9 — The match door is public.** An external, non-test crate in a normal build can: register
  an embedding space, enroll a reference vector against an entity, and ask "which enrolled entity
  does this vector match?" — receiving entity id, label, and score. No test-only cfg seam, no
  crate-internal type in the signature.
- **AC10 — No free-text search door.** The newly public surface accepts only callers that already
  hold a vector. No public method added by this work accepts free text for search; the
  library's full verification gate passes in its own environment before vigil consumes it.

Honest consequences (stated, not hidden):

- Without cross-frame tracking, one physical visit can produce several events; each is matched
  independently. One-event-per-object is out of scope here.
- Recognition raises the per-detection observation write volume on covered classes; the measured
  delta (observations written per detection, before vs after) is reported with the change.

## Owner live smoke — the acceptance gate (assertions frozen)

Named inputs: the owner's real farm cameras; the owner in frame ("Roshan"); one un-enrolled
visitor; the dog ("Max"). Scoring: binary pass/fail per assertion; raw command/event output is
recorded, not summarized scores.

- **S1** A real person event appears; the owner enrolls it as "Roshan" from the card overlay.
- **S2** The owner walks back into frame; the next person event on a covered camera carries
  "Roshan" in the MQTT payload and the card event rows.
- **S3** `vigil why` on that event shows the matched reference, the score, and the enrolling
  correction.
- **S4** An un-enrolled visitor in frame yields "unknown person" — no name.
- **S5** The Max leg: enroll the dog as "Max" from a real event; a later sighting names it "Max";
  `vigil why` shows its match provenance.
- **S6** The recognition path runs with no outbound network traffic (embed, enroll, match all
  local).

A skipped assertion fails the gate unless the owner explicitly accepts the skip, quoted in the
ship record.

## Identity-depth measurement protocol (frozen before any data is seen)

Purpose: decide joint-embedder-only vs adding a face-verifier stage — an owner decision made on
measured numbers, not assumption.

- Enroll the actual household members and Max from real events.
- Over one real day of events on covered cameras, with owner-labeled ground truth per event,
  count per class: correct match / miss (enrolled subject, no name) / false match (wrong name, or
  any name on an un-enrolled subject).
- Report match rate and false-match rate per class. A false match is worse than a miss; the
  false-match rate is the headline number.
- The scoring method above is fixed across any embedder/threshold variants tried; variants are
  compared only under this same count.
