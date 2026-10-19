//! Owned-fixture directory-model measurement, not native UI latency.
use browser_core::{
    selection::{Action, Selection},
    Browser, EntryKind, LoadState,
};
use std::{
    error::Error,
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

fn wait(browser: &mut Browser) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        browser.poll();
        match browser.state() {
            LoadState::Complete => return Ok(()),
            LoadState::Failed(message) => return Err(message.clone().into()),
            LoadState::Loading if Instant::now() >= deadline => {
                return Err("directory delivery exceeded 60-second deadline".into());
            }
            LoadState::Loading => thread::sleep(Duration::from_micros(100)),
        }
    }
}

fn name(index: usize) -> String {
    format!("entry-{index:08}.dat")
}

// Outside the timed interval: a fast count-only result must not count as a
// correct catalog. Check every ordered native name, full path, type and size.
fn verify(browser: &Browser, fixture: &Path, count: usize) -> Result<(), Box<dyn Error>> {
    if browser.entries().len() != count || browser.path() != fixture {
        return Err("catalog count or destination mismatch".into());
    }
    for (index, entry) in browser.entries().iter().enumerate() {
        let expected = name(index);
        if entry.name != expected.as_str()
            || entry.path != fixture.join(&expected)
            || entry.kind != EntryKind::File
            || entry.size.is_some()
        {
            return Err(format!("catalog identity/type mismatch at {index}").into());
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let source = option_env!("MEGAMAN_BENCH_SOURCE")
        .ok_or("build with MEGAMAN_BENCH_SOURCE set to the full source commit")?;
    if source.len() != 40 || !source.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("MEGAMAN_BENCH_SOURCE must be a full hexadecimal commit".into());
    }
    let mut args = std::env::args_os().skip(1);
    let fixture = PathBuf::from(args.next().ok_or("expected new absolute fixture path")?);
    let entries: usize = args
        .next()
        .ok_or("expected entry count")?
        .to_str()
        .ok_or("entry count is not UTF-8")?
        .parse()?;
    let samples: usize = args
        .next()
        .ok_or("expected sample count")?
        .to_str()
        .ok_or("sample count is not UTF-8")?
        .parse()?;
    if args.next().is_some()
        || !fixture.is_absolute()
        || !(1..=1_000_000).contains(&entries)
        || !(3..=100).contains(&samples)
    {
        return Err(
            "usage: measure_directory NEW_ABSOLUTE_FIXTURE ENTRIES(1..1000000) SAMPLES(3..100)"
                .into(),
        );
    }
    // Never adopt or clear an existing directory. Preserve the generated
    // fixture even on failure for inspection; the caller owns its cleanup.
    fs::create_dir(&fixture)?;
    for index in 0..entries {
        fs::File::create_new(fixture.join(name(index)))?;
    }

    let start = Instant::now();
    let mut browser = Browser::new(fixture.clone());
    wait(&mut browser)?;
    let initial_ns = start.elapsed().as_nanos();
    verify(&browser, &fixture, entries)?;
    let mut rescan_ns = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        // Same-path navigation advances the location revision and enumerates
        // again, unlike set_filter, which can reuse worker catalog records.
        browser.navigate(fixture.clone());
        wait(&mut browser)?;
        let elapsed = start.elapsed().as_nanos();
        verify(&browser, &fixture, entries)?;
        rescan_ns.push(elapsed);
    }

    let snapshot = browser.snapshot().ok_or("missing accepted snapshot")?;
    let mut selection = Selection::new(snapshot.revision().clone(), entries);
    const PAIRS: usize = 100_000;
    let mut selection_ns = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        for _ in 0..PAIRS {
            black_box(selection.apply(black_box(snapshot.revision()), Action::Clear)?);
            black_box(selection.apply(black_box(snapshot.revision()), Action::SelectAll)?);
            black_box(selection.count());
        }
        let elapsed = start.elapsed().as_nanos();
        let ranges = selection.ranges(snapshot.revision())?;
        if selection.count() != entries || ranges.len() != 1 || ranges[0] != (0..entries) {
            return Err("Select All did not produce the one exact full range".into());
        }
        selection_ns.push(elapsed);
    }
    println!(
        "{{\"schema_version\":1,\"source_commit\":\"{source}\",\"entries\":{entries},\"samples\":{samples},\"correctness\":\"passed\",\"poll_sleep_microseconds\":100,\"initial_worker_spawn_and_catalog_ns\":{initial_ns},\"same_path_rescan_to_polled_delivery_ns\":{rescan_ns:?},\"selection_clear_select_all_pairs_per_sample\":{PAIRS},\"selection_pairs_total_ns\":{selection_ns:?}}}"
    );
    Ok(())
}
