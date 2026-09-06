//! The crash log is the only channel out of a killed process. If it loses its
//! last entry, it loses exactly the fact the test was about.

use recovery_conformance::child::{CrashLog, Observation};

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "recovery-conformance-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    path
}

#[test]
fn every_record_is_readable_without_a_clean_close() {
    let path = temp_path("roundtrip");
    let log = CrashLog::create(&path).unwrap();

    log.record(&Observation::PostDispatched {
        url: "https://chain.example/shielded-vote/v1/delegate-vote".to_string(),
    });
    log.record(&Observation::StageReached {
        stage: "before-broadcast".to_string(),
    });

    // Deliberately not dropped or flushed by hand: `abort` would run neither,
    // so every record must already be on disk by the time it returns.
    let observations = CrashLog::read(&path).unwrap();
    assert_eq!(observations.len(), 2);
    assert!(matches!(
        observations[1],
        Observation::StageReached { ref stage } if stage == "before-broadcast"
    ));

    std::fs::remove_file(&path).ok();
}

#[test]
fn a_torn_trailing_line_does_not_discard_the_records_before_it() {
    // A process can be killed mid-write by something other than our own abort.
    // Losing the whole log to one partial line would turn a real recovery
    // finding into an unexplained harness error.
    let path = temp_path("torn");
    let log = CrashLog::create(&path).unwrap();
    log.record(&Observation::StageReached {
        stage: "after-proof".to_string(),
    });
    drop(log);

    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("{\"kind\":\"stage_rea");
    std::fs::write(&path, text).unwrap();

    let observations = CrashLog::read(&path).unwrap();
    assert_eq!(observations.len(), 1);

    std::fs::remove_file(&path).ok();
}

#[test]
fn a_log_with_no_records_reads_as_empty_rather_than_missing() {
    let path = temp_path("empty");
    CrashLog::create(&path).unwrap();
    assert!(CrashLog::read(&path).unwrap().is_empty());
    std::fs::remove_file(&path).ok();
}
