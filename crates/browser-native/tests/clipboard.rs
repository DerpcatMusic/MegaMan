#![cfg(all(target_os = "linux", feature = "native-x11-tests"))]
use browser_native::x11::{ClipboardService, Config, Error, Outcome, Startup};
use browser_transfer::{CopyIntent, FileBatch};
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    lines: Receiver<String>,
    stderr: Arc<Mutex<String>>,
}
impl Process {
    fn spawn(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("native qualification tool must be installed");
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        let capture = stderr.clone();
        let error_stream = child.stderr.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(error_stream).lines().map_while(Result::ok) {
                let mut log = capture.lock().unwrap();
                let remaining = 4096usize.saturating_sub(log.len());
                log.extend(line.chars().take(remaining / 4));
                if log.len() < 4096 {
                    log.push('\n');
                }
            }
        });
        let (sender, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if sender.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            child,
            input,
            lines,
            stderr,
        }
    }
    fn line(&self) -> String {
        self.lines
            .recv_timeout(Duration::from_secs(8))
            .unwrap_or_else(|error| {
                panic!(
                    "native peer output {error}: {}",
                    self.stderr.lock().unwrap()
                )
            })
    }
    fn send(&mut self, value: &str) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
    }
    fn success(&mut self) {
        assert!(
            self.child.wait().unwrap().success(),
            "{}",
            self.stderr.lock().unwrap()
        );
    }
    fn terminate(&mut self) {
        if self.child.try_wait().unwrap().is_some() {
            return;
        }
        // Graceful termination lets Xvfb remove its own standard protocol socket/lock.
        let _ = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status();
        for _ in 0..100 {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.terminate();
    }
}
struct Server {
    _process: Process,
    display: String,
}
impl Server {
    fn start() -> Self {
        let process = Process::spawn(Command::new("Xvfb").args([
            "-displayfd",
            "1",
            "-screen",
            "0",
            "640x480x24",
            "-nolisten",
            "tcp",
        ]));
        let number = process.line();
        assert!(number.bytes().all(|b| b.is_ascii_digit()));
        Self {
            display: format!(":{number}"),
            _process: process,
        }
    }
    fn service(&self, mut config: Config) -> ClipboardService {
        config.display = Some(self.display.clone());
        let service = ClipboardService::open(config).unwrap();
        wait(|| match service.startup() {
            Startup::Ready { .. } => Some(()),
            Startup::Failed(error) => panic!("startup: {error:?}"),
            _ => None,
        });
        service
    }
    fn owner(&self, scenario: &str, paths: &[PathBuf]) -> Process {
        let mut command = Command::new("python3");
        command
            .arg(peer_script())
            .args(["own", &self.display, scenario]);
        for path in paths {
            command.arg(hex(path.as_os_str().as_bytes()));
        }
        let peer = Process::spawn(&mut command);
        assert!(peer.line().starts_with("READY "));
        peer
    }
    fn reader(&self, paused: bool) -> Process {
        Process::spawn(
            Command::new("python3")
                .arg(peer_script())
                .args([if paused { "read-paused" } else { "read" }, &self.display]),
        )
    }
}
fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/x11_peer.py")
}
fn wait<T>(mut predicate: impl FnMut() -> Option<T>) -> T {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(value) = predicate() {
            return value;
        }
        assert!(Instant::now() < until, "native state timeout");
        thread::sleep(Duration::from_millis(2));
    }
}
fn result(service: &mut ClipboardService, id: u64) -> Result<Outcome, Error> {
    wait(|| {
        let events = service.poll();
        if events.is_empty() {
            None
        } else {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].request, id);
            Some(events.into_iter().next().unwrap().result)
        }
    })
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn unhex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u8::from_str_radix(std::str::from_utf8(b).unwrap(), 16).unwrap())
        .collect()
}
static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);
struct Fixture {
    root: PathBuf,
    paths: Vec<PathBuf>,
}

// Independent native protocol arbiter. It uses no worker internals or codec implementation.
// A tight native poll is needed to intercept the clock event before the service's next request.
enum RaceControl {
    Release,
    Arm,
    Steal(u32),
    Restore,
    Probe,
}
struct NativeRacePeer {
    observer: x11rb::rust_connection::RustConnection,
    clipboard: u32,
    actions: std::sync::mpsc::Sender<RaceControl>,
    claims: Receiver<(u32, u32)>,
    armed: Receiver<()>,
    losses: Receiver<(u32, u32, u32)>,
    probes: Receiver<bool>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}
impl NativeRacePeer {
    fn start(display: &str, service_window: u32) -> Self {
        Self::with_clock_watch(display, service_window, true)
    }
    fn with_clock_watch(display: &str, service_window: u32, watch_clock: bool) -> Self {
        use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
        let (observer, _) = x11rb::connect(Some(display)).unwrap();
        let clipboard = observer
            .intern_atom(false, b"CLIPBOARD")
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        let (actions, commands) = mpsc::channel();
        let (send_claim, claims) = mpsc::channel();
        let (send_ready, armed) = mpsc::channel();
        let (send_loss, losses) = mpsc::channel();
        let (send_probe, probes) = mpsc::channel();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let address = display.to_owned();
        let join = thread::spawn(move || {
            use x11rb::protocol::{
                Event,
                xfixes::{self, ConnectionExt as _},
                xproto::*,
            };
            let (conn, screen) = x11rb::connect(Some(&address)).unwrap();
            let window = conn.generate_id().unwrap();
            conn.create_window(
                0,
                window,
                conn.setup().roots[screen].root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
            conn.change_window_attributes(
                service_window,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )
            .unwrap()
            .check()
            .unwrap();
            let clock = conn
                .intern_atom(false, b"_MEGAMAN_CLIPBOARD_CLOCK")
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            conn.xfixes_query_version(2, 0).unwrap().reply().unwrap();
            conn.xfixes_select_selection_input(
                window,
                clipboard,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER,
            )
            .unwrap()
            .check()
            .unwrap();
            let targets = conn
                .intern_atom(false, b"TARGETS")
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            let property = conn
                .intern_atom(false, b"_NATIVE_ARBITER_PROBE")
                .unwrap()
                .reply()
                .unwrap()
                .atom;
            let mut enabled = watch_clock;
            let mut last_time = 0;
            send_ready.send(()).unwrap();
            while !stopped.load(Ordering::Acquire) {
                while let Ok(command) = commands.try_recv() {
                    match command {
                        RaceControl::Arm => {
                            enabled = true;
                            send_ready.send(()).unwrap();
                        }
                        RaceControl::Release => {}
                        RaceControl::Steal(time) => {
                            // Called only AFTER the parent consumed a confirmed Set completion.
                            conn.set_selection_owner(window, clipboard, time)
                                .unwrap()
                                .check()
                                .unwrap();
                            assert_eq!(
                                conn.get_selection_owner(clipboard)
                                    .unwrap()
                                    .reply()
                                    .unwrap()
                                    .owner,
                                window
                            );
                            let deadline = Instant::now() + Duration::from_secs(5);
                            loop {
                                assert!(Instant::now() < deadline);
                                if let Some(Event::XfixesSelectionNotify(event)) =
                                    conn.poll_for_event().unwrap()
                                    && event.owner == window
                                    && event.selection_timestamp == time
                                {
                                    last_time = time;
                                    send_loss
                                        .send((event.timestamp, event.selection_timestamp, window))
                                        .unwrap();
                                    break;
                                }
                                std::hint::spin_loop();
                            }
                        }
                        RaceControl::Restore => {
                            conn.set_selection_owner(service_window, clipboard, last_time)
                                .unwrap()
                                .check()
                                .unwrap();
                            assert_eq!(
                                conn.get_selection_owner(clipboard)
                                    .unwrap()
                                    .reply()
                                    .unwrap()
                                    .owner,
                                service_window
                            );
                            send_ready.send(()).unwrap();
                        }
                        RaceControl::Probe => {
                            // Foreign assignment routes ConvertSelection to the assigning client,
                            // so directly deliver a native request to the service's window.
                            let event = SelectionRequestEvent {
                                response_type: SELECTION_REQUEST_EVENT,
                                sequence: 0,
                                time: x11rb::CURRENT_TIME,
                                owner: service_window,
                                requestor: window,
                                selection: clipboard,
                                target: targets,
                                property,
                            };
                            conn.send_event(false, service_window, EventMask::NO_EVENT, event)
                                .unwrap()
                                .check()
                                .unwrap();
                            let deadline = Instant::now() + Duration::from_secs(5);
                            loop {
                                assert!(Instant::now() < deadline);
                                if let Some(Event::SelectionNotify(event)) =
                                    conn.poll_for_event().unwrap()
                                    && event.requestor == window
                                    && event.target == targets
                                {
                                    send_probe.send(event.property == x11rb::NONE).unwrap();
                                    break;
                                }
                                std::hint::spin_loop();
                            }
                        }
                    }
                }
                let Some(event) = conn.poll_for_event().unwrap() else {
                    std::hint::spin_loop();
                    continue;
                };
                if let Event::PropertyNotify(event) = event
                    && enabled
                    && event.atom == clock
                    && event.state == Property::NEW_VALUE
                {
                    conn.grab_server().unwrap();
                    conn.set_selection_owner(window, clipboard, event.time)
                        .unwrap()
                        .check()
                        .unwrap();
                    assert_eq!(
                        conn.get_selection_owner(clipboard)
                            .unwrap()
                            .reply()
                            .unwrap()
                            .owner,
                        window
                    );
                    send_claim.send((event.time, window)).unwrap();
                    assert!(matches!(commands.recv().unwrap(), RaceControl::Release));
                    conn.ungrab_server().unwrap().check().unwrap();
                    conn.flush().unwrap();
                    enabled = false;
                }
            }
        });
        armed.recv_timeout(Duration::from_secs(8)).unwrap();
        Self {
            observer,
            clipboard,
            actions,
            claims,
            armed,
            losses,
            probes,
            stop,
            join: Some(join),
        }
    }
    fn claim(&self) -> (u32, u32) {
        self.claims.recv_timeout(Duration::from_secs(8)).unwrap()
    }
    fn release(&self) {
        self.actions.send(RaceControl::Release).unwrap();
    }
    fn arm(&self) {
        self.actions.send(RaceControl::Arm).unwrap();
        self.armed.recv_timeout(Duration::from_secs(8)).unwrap();
    }
    fn steal(&self, time: u32) -> (u32, u32, u32) {
        self.actions.send(RaceControl::Steal(time)).unwrap();
        self.losses.recv_timeout(Duration::from_secs(8)).unwrap()
    }
    fn restore(&self) {
        self.actions.send(RaceControl::Restore).unwrap();
        self.armed.recv_timeout(Duration::from_secs(8)).unwrap();
    }
    fn probe_empty(&self) -> bool {
        self.actions.send(RaceControl::Probe).unwrap();
        self.probes.recv_timeout(Duration::from_secs(8)).unwrap()
    }
    fn owner(&self) -> u32 {
        use x11rb::protocol::xproto::ConnectionExt;
        self.observer
            .get_selection_owner(self.clipboard)
            .unwrap()
            .reply()
            .unwrap()
            .owner
    }
}
impl Drop for NativeRacePeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.actions.send(RaceControl::Release);
        self.join.take().unwrap().join().unwrap();
    }
}
impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join(format!(
                ".owned-x11-{}-{}",
                std::process::id(),
                FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        let file = root.join(OsString::from_vec(b"native-\xff\n#%".to_vec()));
        fs::write(&file, b"source remains untouched").unwrap();
        let directory = root.join("directory space 漢字");
        fs::create_dir(&directory).unwrap();
        Self {
            root,
            paths: vec![file, directory],
        }
    }
    fn batch(&self) -> FileBatch {
        FileBatch {
            intent: CopyIntent::Cut,
            paths: self.paths.clone(),
        }
    }
    fn intact(&self) {
        assert_eq!(
            fs::read(&self.paths[0]).unwrap(),
            b"source remains untouched"
        );
        assert!(self.paths[1].is_dir());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn native_export_matches_independent_peer_and_preserves_sources() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config::default());
    let request = service.set_files(fixture.batch()).unwrap();
    assert!(matches!(
        result(&mut service, request.id),
        Ok(Outcome::Set {
            cancelled_after_commit: false,
            ..
        })
    ));
    let mut peer = server.reader(false);
    let uri = unhex(peer.line().strip_prefix("URI ").unwrap());
    let gnome = unhex(peer.line().strip_prefix("GNOME ").unwrap());
    assert!(uri.ends_with(b"\r\n") && !gnome.ends_with(b"\n"));
    assert_eq!(peer.line(), "KDE 31");
    for path in &fixture.paths {
        assert_eq!(
            peer.line(),
            format!("PATH {}", hex(path.as_os_str().as_bytes()))
        );
    }
    let window = match service.startup() {
        Startup::Ready { window } => window,
        _ => unreachable!(),
    };
    assert_eq!(peer.line(), format!("OWNER {window}"));
    peer.success();
    fixture.intact();
}

#[test]
fn native_import_negotiates_gnome_and_uri_kde_with_exact_bytes() {
    for scenario in [
        "normal",
        "uri-kde",
        "incr",
        "lying-incr",
        "gnome-copy",
        "uri-copy",
    ] {
        let server = Server::start();
        let fixture = Fixture::new();
        let _peer = server.owner(scenario, &fixture.paths);
        let mut service = server.service(Config::default());
        let request = service.read_files().unwrap();
        let mut expected = fixture.batch();
        if scenario.ends_with("-copy") {
            expected.intent = CopyIntent::Copy;
        }
        assert_eq!(
            result(&mut service, request.id),
            Ok(Outcome::Read(expected)),
            "{scenario}"
        );
        fixture.intact();
    }
}

#[test]
fn malformed_oversized_and_uncooperative_native_owners_fail_whole_reads() {
    for (scenario, expected) in [
        ("malformed", "payload"),
        ("bad-escape", "payload"),
        ("oversize-direct", "large"),
        ("oversize-chunk", "large"),
        ("incr-empty", "payload"),
        ("bad-type", "protocol"),
        ("unsupported", "unsupported"),
        ("timeout", "timeout"),
        ("change-owner", "owner"),
        ("owner-exit", "owner"),
        ("incr-overall", "large"),
        ("incr-large-hint", "large"),
        ("incr-short", "protocol"),
        ("targets-huge", "large"),
        ("bad-format", "protocol"),
        ("incr-timeout", "timeout"),
        ("timestamp-change", "owner"),
    ] {
        let server = Server::start();
        let fixture = Fixture::new();
        let _peer = server.owner(scenario, &fixture.paths);
        let mut config = Config {
            timeout: Duration::from_millis(200),
            chunk_bytes: 128,
            ..Config::default()
        };
        config.limits.max_payload_bytes = 1024;
        let mut service = server.service(config);
        let request = service.read_files().unwrap();
        let err = result(&mut service, request.id).unwrap_err();
        assert!(
            match expected {
                "payload" => matches!(err, Error::InvalidPayload(_)),
                "large" => err == Error::TooLarge,
                "protocol" => matches!(err, Error::Protocol(_)),
                "unsupported" => err == Error::UnsupportedTargets,
                "timeout" => err == Error::Timeout,
                "owner" => err == Error::OwnerChanged,
                _ => false,
            },
            "{scenario}: {err:?}"
        );
        fixture.intact();
    }
}

#[test]
fn cancellation_and_late_reply_cannot_poison_the_next_request() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut peer = server.owner("late-reply", &fixture.paths);
    let mut service = server.service(Config::default());
    let first = service.read_files().unwrap();
    assert!(peer.line().starts_with("REQUEST "));
    first.cancel();
    assert_eq!(result(&mut service, first.id), Err(Error::Cancelled));
    peer.send("normal");
    while peer.line() != "NORMAL" {}
    let second = service.read_files().unwrap();
    assert_eq!(
        result(&mut service, second.id),
        Ok(Outcome::Read(fixture.batch()))
    );
}

#[test]
fn invalid_set_preserves_native_old_offer_and_reports_ownership_loss() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config::default());
    let first = service.set_files(fixture.batch()).unwrap();
    assert!(result(&mut service, first.id).is_ok());
    let invalid = service
        .set_files(FileBatch {
            intent: CopyIntent::Copy,
            paths: vec![PathBuf::from("relative")],
        })
        .unwrap();
    assert!(matches!(
        result(&mut service, invalid.id),
        Err(Error::InvalidPayload(_))
    ));
    let read = service.read_files().unwrap();
    assert_eq!(
        result(&mut service, read.id),
        Ok(Outcome::Read(fixture.batch()))
    );
    let generation = service.ownership().generation;
    let _peer = server.owner("normal", &fixture.paths);
    wait(|| {
        let state = service.ownership();
        (!state.is_ours && state.owner != 0 && state.generation > generation).then_some(())
    });
    fixture.intact();
}

#[test]
fn outbound_incr_retains_original_bytes_across_ownership_loss() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config {
        chunk_bytes: 32,
        ..Config::default()
    });
    let first = service.set_files(fixture.batch()).unwrap();
    assert!(result(&mut service, first.id).is_ok());
    let mut reader = server.reader(true);
    assert_eq!(reader.line(), "INCR_READY");
    let _new_owner = server.owner("normal", &fixture.paths);
    wait(|| (!service.ownership().is_ours).then_some(()));
    reader.send("continue");
    let uri = unhex(reader.line().strip_prefix("URI ").unwrap());
    let expected = browser_transfer::encode_uri_list(&fixture.paths, &Default::default()).unwrap();
    assert_eq!(uri, expected);
    reader.success();
    fixture.intact();
}

#[test]
fn request_slots_are_reserved_until_poll_and_close_is_asynchronous() {
    let server = Server::start();
    let fixture = Fixture::new();
    let peer = server.owner("timeout", &fixture.paths);
    let mut service = server.service(Config {
        timeout: Duration::from_millis(400),
        ..Config::default()
    });
    let requests: Vec<_> = (0..8).map(|_| service.read_files().unwrap()).collect();
    assert_eq!(service.read_files().unwrap_err(), Error::Busy);
    assert!(peer.line().starts_with("REQUEST "));
    for request in &requests {
        request.cancel();
    }
    let mut events = Vec::new();
    wait(|| {
        events.extend(service.poll());
        (events.len() == 8).then_some(())
    });
    assert!(events.iter().all(|e| e.result == Err(Error::Cancelled)));
    let request = service.read_files().unwrap();
    service.close();
    assert_eq!(result(&mut service, request.id), Err(Error::Cancelled));
    assert_eq!(service.read_files().unwrap_err(), Error::Closed);
}

#[test]
fn bounded_outbound_leases_expire_and_capacity_is_reusable() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config {
        chunk_bytes: 32,
        timeout: Duration::from_millis(150),
        ..Config::default()
    });
    let set = service.set_files(fixture.batch()).unwrap();
    assert!(result(&mut service, set.id).is_ok());
    let mut peer = Process::spawn(
        Command::new("python3")
            .arg(peer_script())
            .args(["lease-probe", &server.display]),
    );
    assert_eq!(peer.line(), "FULL");
    thread::sleep(Duration::from_millis(200));
    peer.send("continue");
    assert_eq!(peer.line(), "REUSED");
    peer.success();
}

#[test]
fn replacing_offer_does_not_mutate_an_inflight_incr_snapshot() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config {
        chunk_bytes: 32,
        ..Config::default()
    });
    let first = service.set_files(fixture.batch()).unwrap();
    assert!(result(&mut service, first.id).is_ok());
    let mut reader = server.reader(true);
    assert_eq!(reader.line(), "INCR_READY");
    let mut replacement = fixture.batch();
    replacement.paths.reverse();
    replacement.intent = CopyIntent::Copy;
    let set = service.set_files(replacement.clone()).unwrap();
    assert!(result(&mut service, set.id).is_ok());
    reader.send("continue");
    let old_uri = unhex(reader.line().strip_prefix("URI ").unwrap());
    assert_eq!(
        old_uri,
        browser_transfer::encode_uri_list(&fixture.paths, &Default::default()).unwrap()
    );
    reader.success();
    let mut peer = server.reader(false);
    assert!(peer.line().starts_with("URI "));
    let gnome = unhex(peer.line().strip_prefix("GNOME ").unwrap());
    assert!(gnome.starts_with(b"copy\n"));
    assert_eq!(peer.line(), "KDE 30");
    for path in &replacement.paths {
        assert_eq!(
            peer.line(),
            format!("PATH {}", hex(path.as_os_str().as_bytes()))
        );
    }
    assert!(peer.line().starts_with("OWNER "));
    peer.success();
    let read = service.read_files().unwrap();
    assert_eq!(
        result(&mut service, read.id),
        Ok(Outcome::Read(replacement))
    );
}

#[test]
fn server_death_is_a_native_failure_and_empty_selection_is_not_success() {
    let mut server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config::default());
    let empty = service.read_files().unwrap();
    assert_eq!(result(&mut service, empty.id), Err(Error::NoOwner));
    let peer = server.owner("timeout", &fixture.paths);
    let read = service.read_files().unwrap();
    assert!(peer.line().starts_with("REQUEST "));
    let mut ids = vec![read.id];
    ids.extend((0..7).map(|_| service.read_files().unwrap().id));
    server._process.terminate();
    let mut completions = Vec::new();
    wait(|| {
        completions.extend(service.poll());
        (completions.len() == 8).then_some(())
    });
    assert!(
        completions
            .iter()
            .all(|event| matches!(event.result, Err(Error::Native(_))))
    );
    let mut actual: Vec<_> = completions.iter().map(|event| event.request).collect();
    actual.sort_unstable();
    ids.sort_unstable();
    assert_eq!(actual, ids);
    wait(|| matches!(service.startup(), Startup::Failed(_)).then_some(()));
    assert!(!service.ownership().is_ours);
    fixture.intact();
}

#[test]
fn a_queued_cancelled_set_cannot_acquire_clipboard_ownership() {
    let server = Server::start();
    let fixture = Fixture::new();
    let peer = server.owner("timeout", &fixture.paths);
    let mut service = server.service(Config::default());
    let read = service.read_files().unwrap();
    assert!(peer.line().starts_with("REQUEST "));
    let set = service.set_files(fixture.batch()).unwrap();
    set.cancel();
    read.cancel();
    let mut events = Vec::new();
    wait(|| {
        events.extend(service.poll());
        (events.len() == 2).then_some(())
    });
    assert!(events.iter().all(|e| e.result == Err(Error::Cancelled)));
    assert!(!service.ownership().is_ours);
    fixture.intact();
}

#[test]
fn stale_same_timestamp_loss_before_confirmed_claim_preserves_the_new_offer() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config::default());
    let initial = service.set_files(fixture.batch()).unwrap();
    assert!(result(&mut service, initial.id).is_ok());
    let window = match service.startup() {
        Startup::Ready { window } => window,
        _ => unreachable!(),
    };
    let peer = NativeRacePeer::start(&server.display, window);
    let mut observed_stale = false;
    for attempt in 0..64 {
        let replacement = FileBatch {
            intent: CopyIntent::Copy,
            paths: vec![fixture.paths[attempt % 2].clone()],
        };
        let request = service.set_files(replacement.clone()).unwrap();
        let (timestamp, peer_window) = peer.claim();
        peer.release();
        let outcome = result(&mut service, request.id);
        let owner = peer.owner();
        if owner == window {
            // The peer confirmed its own ownership under its server grab. Only the service's
            // later claim can explain this owner: stale same-time loss is actually observed.
            assert!(
                matches!(
                    outcome,
                    Ok(Outcome::Set {
                        cancelled_after_commit: false,
                        ..
                    })
                ),
                "{outcome:?}"
            );
            let state = service.ownership();
            assert!(state.is_ours);
            assert_eq!(
                (state.owner, state.selection_timestamp),
                (window, timestamp)
            );
            request.cancel(); // A late cancel cannot roll back an already confirmed publication.
            let read = service.read_files().unwrap();
            assert_eq!(
                result(&mut service, read.id),
                Ok(Outcome::Read(replacement))
            );
            assert!(service.poll().is_empty());
            observed_stale = true;
            break;
        }
        assert_eq!(owner, peer_window);
        assert!(
            matches!(outcome, Ok(Outcome::Set { .. }) | Err(Error::OwnerChanged)),
            "{outcome:?}"
        );
        wait(|| {
            let state = service.ownership();
            (!state.is_ours && state.owner == peer_window).then_some(())
        });
        peer.arm();
    }
    assert!(
        observed_stale,
        "native peer never observed the required pre-claim same-time loss"
    );
    fixture.intact();
}

#[test]
fn genuine_equal_timestamp_loss_after_sequence_wrap_clears_offers() {
    let server = Server::start();
    let fixture = Fixture::new();
    let mut service = server.service(Config::default());
    let first = service.set_files(fixture.batch()).unwrap();
    assert!(result(&mut service, first.id).is_ok());
    // Each served conversion requires at least GetSelectionOwner, ChangeProperty and SendEvent
    // on the SERVICE connection. 22,000 conversions cross the 65,536 wire sequence boundary.
    let mut flood = Process::spawn(
        Command::new("python3")
            .arg(peer_script())
            .args(["flood-targets", &server.display]),
    );
    assert_eq!(
        flood
            .lines
            .recv_timeout(Duration::from_secs(45))
            .expect("native sequence-wrap load timed out"),
        "FLOODED 22000"
    );
    flood.success();
    let window = match service.startup() {
        Startup::Ready { window } => window,
        _ => unreachable!(),
    };
    let peer = NativeRacePeer::with_clock_watch(&server.display, window, false);
    let mut observed_equal = false;
    for _ in 0..128 {
        let replacement = service.set_files(fixture.batch()).unwrap();
        // Poll without an artificial millisecond sleep: the actual notification time must
        // still equal the claim time, not merely carry an old client-supplied timestamp.
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            let events = service.poll();
            if !events.is_empty() {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].request, replacement.id);
                break events.into_iter().next().unwrap().result;
            }
            assert!(Instant::now() < deadline);
            std::hint::spin_loop();
        };
        assert!(matches!(
            outcome,
            Ok(Outcome::Set {
                cancelled_after_commit: false,
                ..
            })
        ));
        let state = service.ownership();
        assert!(state.is_ours);
        let (event_time, selection_time, peer_window) = peer.steal(state.selection_timestamp);
        assert_eq!(selection_time, state.selection_timestamp);
        wait(|| {
            let now = service.ownership();
            (!now.is_ours && now.owner == peer_window && now.generation > state.generation)
                .then_some(())
        });
        if event_time != state.selection_timestamp {
            continue;
        }
        // Both SelectionClear.time and XFixes.timestamp equal the acquired millisecond,
        // and the loss followed a fully consumed native-confirmed Set completion.
        replacement.cancel();
        peer.restore();
        wait(|| (service.ownership().owner == state.owner).then_some(()));
        assert!(
            !service.ownership().is_ours,
            "foreign assignment must not resurrect cleared offers"
        );
        assert!(peer.probe_empty());
        assert!(service.poll().is_empty());
        observed_equal = true;
        break;
    }
    assert!(
        observed_equal,
        "native peer never witnessed genuine equal-server-time loss after confirmation"
    );
    fixture.intact();
}
