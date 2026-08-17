//! `NoSiteChannel` is the explicit "no integration attached" choice a caller
//! passes to `run_cli_with_site_channel` (core's own tests and tooling). All
//! three of its trait methods must be a true no-op — never connecting,
//! listening, or announcing anything — so a caller that forgets to wire a
//! real adapter gets silence, not a half-wired integration.

use vigil::{ConnectionEndpoint, HealthState, NoSiteChannel, SiteAnnouncement, SiteChannelFactory};

fn endpoint() -> ConnectionEndpoint {
    ConnectionEndpoint {
        host: "127.0.0.1".to_string(),
        port: 1883,
        username: None,
        password: None,
    }
}

fn announcement() -> SiteAnnouncement {
    SiteAnnouncement {
        service_name: "Vigil".to_string(),
        service_id: "no-site-channel-test".to_string(),
        cameras: Vec::new(),
    }
}

/// Unfakeable because it drives the real trait method through the real
/// `SiteChannelFactory` interface a composition root calls — a build that
/// wired `NoSiteChannel::announce` to a real integration (or to
/// `unimplemented!()`, or to a `Some` carrying an inert handle) fails this
/// either by panicking or by returning `Some`.
#[test]
fn no_site_channel_announce_returns_none() {
    let factory = NoSiteChannel;
    let result = factory.announce(&endpoint(), announcement(), HealthState::new());
    assert!(
        result.is_none(),
        "NoSiteChannel is the explicit no-integration choice — announce must never hand back a \
         live presence connection"
    );
}

/// The other two legs of the same contract, pinned alongside `announce` so
/// the three cannot drift: a build that fixed only the leg cold review named
/// would leave the other two silently unchecked.
#[test]
fn no_site_channel_connect_and_listen_also_return_none() {
    let factory = NoSiteChannel;
    assert!(
        factory
            .connect(&endpoint(), "no-site-channel-test", HealthState::new())
            .is_none(),
        "NoSiteChannel must never hand back a live detection channel"
    );

    struct NoOpSiteControl;
    impl vigil::SiteControl for NoOpSiteControl {
        fn camera_enabled_states(&self) -> Vec<(String, bool)> {
            Vec::new()
        }
        fn set_camera_enabled(&self, _camera_id: &str, _enabled: bool) -> bool {
            false
        }
        fn latest_detection_image(&self, _camera_id: &str) -> Option<Vec<u8>> {
            None
        }
        fn submit_correction(
            &self,
            _request: vigil::CorrectionRequest,
        ) -> Result<(), vigil::SubmitCorrectionError> {
            Ok(())
        }
    }

    assert!(
        factory
            .listen(
                &endpoint(),
                announcement(),
                HealthState::new(),
                std::sync::Arc::new(NoOpSiteControl),
            )
            .is_none(),
        "NoSiteChannel must never hand back a live command listener"
    );
}
