//! `omalogi verify` against the emulated G502 X: the whole hardware self-test, as an owner
//! runs it, without the hardware.

mod support;

use std::{
    error::Error,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

use omalogi::{
    check::Outcome,
    device::{CLI_SOFTWARE_ID, Session},
    hidraw::SUPPORTED_DEVICES,
    verify::{VerifyOptions, verify},
};
use support::{FakeG502x, fixture_sectors};

fn test_dir(test: &str) -> PathBuf {
    std::env::temp_dir().join(format!("omalogi-verify-{}-{test}", std::process::id()))
}

fn backup_in(test: &str) -> Result<PathBuf, Box<dyn Error>> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    Ok(test_dir(test).join(format!("backup-{n}.json")))
}

async fn session(device: FakeG502x, test: &str) -> Session {
    let mut session = Session::connect(
        device,
        SUPPORTED_DEVICES[0],
        "emulated".to_owned(),
        CLI_SOFTWARE_ID,
    )
    .await
    .expect("session starts");
    session.set_consent_path(Some(test_dir(test).join("untested.json")));
    session
}

#[tokio::test]
async fn the_self_test_passes_on_a_g502x_and_puts_everything_back() {
    let _ = std::fs::remove_dir_all(test_dir("pass"));
    let device = FakeG502x::new();
    let state = device.state();
    let in_use = state.lock().expect("state").current_profile;
    let mut phases = Vec::new();
    let report = verify(
        session(device, "pass").await,
        &VerifyOptions::default(),
        |_| backup_in("pass"),
        &mut |line: &str| phases.push(line.to_owned()),
    )
    .await
    .expect("the self-test runs");

    assert_eq!(report.failures, Vec::<String>::new());
    assert_eq!(report.outcome, Outcome::Pass);
    assert!(report.passed > 1000, "{} checks", report.passed);
    assert_ne!(
        report.profile,
        usize::from(in_use),
        "never the profile in use"
    );
    assert!(report.backup.exists());
    for phase in ["[1/6]", "[2/6]", "[3/6]", "[4/6]", "[5/6]", "[6/6]"] {
        assert!(
            phases.iter().any(|line| line.starts_with(phase)),
            "{phase} ran: {phases:?}"
        );
    }

    let state = state.lock().expect("state");
    let original = fixture_sectors();
    for (sector, data) in &original {
        assert_eq!(&state.sectors[sector], data, "sector {sector:04x}");
    }
    assert_eq!(
        state.current_profile, in_use,
        "the profile in use is in use again"
    );
    let _ = std::fs::remove_dir_all(test_dir("pass"));
}

#[tokio::test]
async fn the_self_test_does_not_write_to_a_mouse_that_fails_the_check() {
    let _ = std::fs::remove_dir_all(test_dir("check"));
    let device = FakeG502x::new();
    let state = device.state();
    state
        .lock()
        .expect("state")
        .sectors
        .get_mut(&2)
        .expect("sector 2")[10] ^= 0xFF;
    let result = verify(
        session(device, "check").await,
        &VerifyOptions::default(),
        |_| backup_in("check"),
        &mut |_: &str| {},
    )
    .await;
    let error = result.expect_err("refused").to_string();
    assert!(error.contains("`omalogi check` does not pass"), "{error}");
    assert!(state.lock().expect("state").committed.is_empty());
}

#[tokio::test]
async fn the_self_test_never_runs_on_the_profile_in_use() {
    let _ = std::fs::remove_dir_all(test_dir("active"));
    let device = FakeG502x::new();
    let state = device.state();
    let in_use = usize::from(state.lock().expect("state").current_profile);
    let result = verify(
        session(device, "active").await,
        &VerifyOptions {
            profile: Some(in_use),
        },
        |_| backup_in("active"),
        &mut |_: &str| {},
    )
    .await;
    let error = result.expect_err("refused").to_string();
    assert!(
        error.contains(&format!("profile {in_use} is in use")),
        "{error}"
    );
    assert!(state.lock().expect("state").committed.is_empty());
}

#[tokio::test]
async fn a_write_that_reads_back_wrong_fails_the_test_and_everything_is_put_back() {
    let _ = std::fs::remove_dir_all(test_dir("corrupt"));
    let device = FakeG502x::new();
    let state = device.state();
    // Well into the bindings phase, after the test profile is set up.
    state.lock().expect("state").corrupt_write_number = Some(20);
    let report = verify(
        session(device, "corrupt").await,
        &VerifyOptions::default(),
        |_| backup_in("corrupt"),
        &mut |_: &str| {},
    )
    .await
    .expect("the self-test runs to its end");

    assert_eq!(report.outcome, Outcome::Fail);
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.contains("read back")),
        "{:#?}",
        report.failures
    );
    let state = state.lock().expect("state");
    for (sector, data) in &fixture_sectors() {
        assert_eq!(&state.sectors[sector], data, "sector {sector:04x}");
    }
    let _ = std::fs::remove_dir_all(test_dir("corrupt"));
}
