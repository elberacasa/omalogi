//! A mouse Omalogi does not list: it is opened only when it answers, with reads only,
//! that it is a mouse with onboard profiles in a layout Omalogi reads.

mod support;

use omalogi::{
    check::Outcome,
    device::{CLI_SOFTWARE_ID, Session},
    hidraw::unlisted,
};
use support::FakeG502x;

async fn session(device: FakeG502x) -> Session {
    let mut session = Session::connect(
        device,
        unlisted(0xC09B, "PRO X SUPERLIGHT 2"),
        "emulated".to_owned(),
        CLI_SOFTWARE_ID,
    )
    .await
    .expect("session starts");
    session.set_consent_path(None);
    session
}

fn superlight() -> FakeG502x {
    FakeG502x::new()
        .with_product_id(0xC09B)
        .with_device_name("PRO X SUPERLIGHT 2")
}

#[tokio::test]
async fn an_unlisted_mouse_with_a_layout_omalogi_reads_is_opened_as_untested() {
    let device = superlight();
    let state = device.state();
    let mut session = session(device).await;
    assert!(session.is_editable_mouse().await);
    assert!(
        state.lock().expect("state").committed.is_empty(),
        "asking writes nothing"
    );
    assert_eq!(
        session.model().name,
        "PRO X SUPERLIGHT 2",
        "the name it reports"
    );

    let support = session.support().await.expect("support");
    assert!(!support.listed);
    assert!(!support.verified);
    assert!(
        !support.accepted,
        "editing it asks first, like any untested mouse"
    );
    assert!(support.editable);

    let report = session.check().await.expect("check");
    assert_eq!(report.outcome, Outcome::Pass, "{:#?}", report.findings);
}

#[tokio::test]
async fn a_keyboard_with_onboard_profiles_is_left_alone() {
    let mut session = session(superlight().with_device_type(0)).await;
    assert!(!session.is_editable_mouse().await);
}

#[tokio::test]
async fn a_mouse_with_a_layout_omalogi_does_not_read_is_left_alone() {
    let mut session = session(superlight().with_profile_format(9)).await;
    assert!(!session.is_editable_mouse().await);
    assert_eq!(
        session.model().name,
        "PRO X SUPERLIGHT 2",
        "keeps the name it was found by"
    );
}

#[tokio::test]
async fn a_listed_mouse_keeps_its_listed_name() {
    let mut session = Session::connect(
        FakeG502x::new().with_device_name("G502 X LIGHTSPEED"),
        omalogi::hidraw::SUPPORTED_DEVICES[0],
        "emulated".to_owned(),
        CLI_SOFTWARE_ID,
    )
    .await
    .expect("session starts");
    assert!(session.is_editable_mouse().await);
    assert_eq!(session.model().name, "G502 X");
}
