#![cfg(target_os = "linux")]
use browser_native::x11::{ClipboardService, Config, Error, Startup};
use std::{
    thread,
    time::{Duration, Instant},
};
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn startup_failure_is_asynchronous_and_never_a_memory_clipboard_success() {
    let mut service = ClipboardService::open(Config {
        display: Some("invalid-display-name".into()),
        ..Config::default()
    })
    .unwrap();
    wait(|| matches!(service.startup(), Startup::Failed(_)));
    assert!(service.read_files().is_err());
    assert!(!service.ownership().is_ours);
    assert!(service.poll().is_empty());
}

#[test]
fn policy_failure_is_reported_by_worker_before_any_native_capability() {
    let mut service = ClipboardService::open(Config {
        timeout: Duration::ZERO,
        ..Config::default()
    })
    .unwrap();
    wait(|| service.startup() == Startup::Failed(Error::InvalidConfig));
    assert_eq!(service.read_files().unwrap_err(), Error::InvalidConfig);
    assert!(!service.ownership().is_ours);
    assert!(service.poll().is_empty());
}
