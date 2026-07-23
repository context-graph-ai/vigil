//! Freezes the HTTP route surface a Home Assistant automation, dashboard
//! card, or browser script may bind to directly: `GET /events`,
//! `GET /why/<id>`, `POST /correction`, the `GET /media/<name>` prefix on
//! the in-process review data plane, and the separate liveness probe
//! `GET /health` the Home Assistant Supervisor watchdog polls.
//!
//! The frozen names come straight from the compiled library —
//! `vigil::{EVENTS_ROUTE, WHY_ROUTE_PREFIX, CORRECTION_ROUTE,
//! MEDIA_ROUTE_PREFIX, HEALTH_PATH}` — never scanned out of a file. Request
//! dispatch in production matches on these exact constants (see
//! `http_data_plane.rs` and `health.rs`), so reading them here pins what
//! production actually routes on, not merely a name that looks right.
//! Renaming any one of these paths is a deliberate, reviewed transition to a
//! published identifier, not a routine refactor.

use vigil::{CORRECTION_ROUTE, EVENTS_ROUTE, HEALTH_PATH, MEDIA_ROUTE_PREFIX, WHY_ROUTE_PREFIX};

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/contract-goldens/http_route_contract.golden"
);

const FIXED_UNKNOWN_ID: &str = "00000000-0000-0000-0000-000000000001";
const FIXED_MEDIA_NAME: &str = "golden-fixture.png";

/// Renders the currently-published HTTP route surface by reading the real
/// production constants (compiled symbols, not scanned text) plus two
/// constructed examples built from fixed inputs — this is the "current
/// truth" the golden fixture pins.
fn render_current_route_surface() -> Vec<String> {
    let mut lines = vec![
        format!("correction_route={CORRECTION_ROUTE}"),
        format!("events_route={EVENTS_ROUTE}"),
        format!("health_route={HEALTH_PATH}"),
        format!("media_route_prefix={MEDIA_ROUTE_PREFIX}"),
        format!("why_route_prefix={WHY_ROUTE_PREFIX}"),
        format!("constructed_media_example={MEDIA_ROUTE_PREFIX}{FIXED_MEDIA_NAME}"),
        format!("constructed_why_example={WHY_ROUTE_PREFIX}{FIXED_UNKNOWN_ID}"),
    ];
    lines.sort();
    lines
}

fn read_golden() -> String {
    std::fs::read_to_string(GOLDEN)
        .unwrap_or_else(|error| panic!("read golden fixture {GOLDEN}: {error}"))
}

#[test]
fn published_http_route_surface_matches_frozen_registry() {
    let rendered = render_current_route_surface().join("\n") + "\n";
    let frozen = read_golden();
    assert_eq!(
        rendered, frozen,
        "the published HTTP route surface (the paths a Home Assistant automation, dashboard \
         card, or watchdog binds to directly) no longer matches the checked-in registry at \
         {GOLDEN}. Renaming a published route is a deliberate, reviewed transition to a \
         published identifier, not a side effect of a refactor — if this rename is intentional, \
         update the golden fixture as part of a reviewed change; otherwise restore the original \
         route string."
    );
}
