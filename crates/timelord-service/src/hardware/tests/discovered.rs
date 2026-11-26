use crate::hardware::testing::{Rig, serving};
use timelord_core::ProfileMode;

#[tokio::test]
async fn empty_config_prepares_and_starts_three_discovered_automatic_boards() {
    let mut rig = Rig::unconfigured("discover", 3).await;
    assert!(
        rig.shared.config.hardware.boards.is_empty(),
        "first start must not require a pre-written board list"
    );
    assert!(
        rig.shared.native.is_none(),
        "empty Automatic list must not prepare capabilities at start"
    );

    rig.adopt_discovered_boards().await;
    assert_eq!(rig.shared.config.hardware.boards.len(), 3);
    assert!(rig.shared.native.is_some());
    assert!(
        rig.shared
            .config
            .hardware
            .boards
            .iter()
            .all(|board| board.profile == ProfileMode::Automatic && board.serial.is_some())
    );

    let ids: Vec<String> = rig
        .shared
        .config
        .hardware
        .boards
        .iter()
        .map(|board| board.id.clone())
        .collect();
    rig.start(Rig::fast());
    rig.until(15, "discovered boards start serving", move |state| {
        ids.iter().all(|id| serving(state, id))
    })
    .await;
    for location in 1..=3 {
        assert!(
            !rig.lines(&format!("launches-{location}")).is_empty(),
            "native client for USB location {location} should have started"
        );
    }
    rig.shutdown().await;
}
