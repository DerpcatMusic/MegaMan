//! Asynchronous X11 CLIPBOARD selection service. The worker owns every native object.
use browser_transfer::{self as codec, CopyIntent, FileBatch};
use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::{Connection, RequestConnection, SequenceNumber},
    protocol::{
        Event,
        xfixes::{self, ConnectionExt as _},
        xproto::{
            self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
            Property, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

const REQUEST_CAP: usize = 8;
const TARGET_CAP: usize = 64;
const INBOX_CAP: usize = 128;

#[derive(Clone, Debug)]
pub struct Config {
    /// Explicit display or normal X11 environment discovery, on the worker only.
    pub display: Option<String>,
    pub limits: codec::Limits,
    pub timeout: Duration,
    pub chunk_bytes: usize,
    pub max_outbound_transfers: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            display: None,
            limits: codec::Limits::default(),
            timeout: Duration::from_secs(5),
            chunk_bytes: 64 << 10,
            max_outbound_transfers: 8,
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    Busy,
    Closed,
    Unavailable,
    InvalidConfig,
    Cancelled,
    Timeout,
    NoOwner,
    OwnerChanged,
    /// A SetSelectionOwner request may have reached the server; no rollback is claimed.
    PublicationUncertain,
    UnsupportedTargets,
    TooLarge,
    Protocol(&'static str),
    InvalidPayload(codec::TransferError),
    Native(String),
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "X11 file clipboard {self:?}")
    }
}
impl std::error::Error for Error {}
pub type Result<T> = std::result::Result<T, Error>;
fn native<T, E: fmt::Display>(r: std::result::Result<T, E>) -> Result<T> {
    r.map_err(|e| Error::Native(e.to_string().chars().take(192).collect()))
}
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Startup {
    Connecting,
    Ready { window: u32 },
    Failed(Error),
    Closed,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Ownership {
    pub owner: u32,
    pub generation: u64,
    pub selection_timestamp: u32,
    pub is_ours: bool,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// Ownership was confirmed by the X server. Later ownership loss is a separate snapshot.
    Set {
        generation: u64,
        cancelled_after_commit: bool,
    },
    Read(FileBatch),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Completion {
    pub request: u64,
    pub result: Result<Outcome>,
}
/// Cancellation is a flag; it never performs native IO or clears a clipboard.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: u64,
    cancelled: Arc<AtomicBool>,
}
impl Request {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
struct Shared {
    shutdown: AtomicBool,
    // Serialize enqueue against the worker's final drain. Never held during native IO.
    accepting: Mutex<bool>,
    startup: Mutex<Startup>,
    ownership: Mutex<Ownership>,
    completions: Mutex<VecDeque<Completion>>,
}
struct Ticket {
    id: u64,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}
enum Command {
    Set(Ticket, FileBatch),
    Read(Ticket),
}
impl Command {
    fn ticket(&self) -> &Ticket {
        match self {
            Self::Set(t, _) | Self::Read(t) => t,
        }
    }
}
/// Polling and submission never connect to X11, encode paths, read files, or wait for server IO.
/// Drop/close requests asynchronous shutdown; a stalled X server may delay worker exit.
pub struct ClipboardService {
    sender: SyncSender<Command>,
    shared: Arc<Shared>,
    pending: usize,
    next_id: u64,
    timeout: Duration,
    max_items: usize,
}
impl ClipboardService {
    pub fn open(config: Config) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(REQUEST_CAP);
        let shared = Arc::new(Shared {
            shutdown: AtomicBool::new(false),
            accepting: Mutex::new(true),
            startup: Mutex::new(Startup::Connecting),
            ownership: Mutex::new(Ownership::default()),
            completions: Mutex::new(VecDeque::with_capacity(REQUEST_CAP)),
        });
        let worker_shared = shared.clone();
        let timeout = config.timeout;
        let max_items = config.limits.max_items.min(4096);
        thread::Builder::new()
            .name("file-clipboard-x11".into())
            .spawn(move || run(config, receiver, worker_shared))?;
        Ok(Self {
            sender,
            shared,
            pending: 0,
            next_id: 1,
            timeout,
            max_items,
        })
    }
    pub fn startup(&self) -> Startup {
        lock(&self.shared.startup).clone()
    }
    pub fn ownership(&self) -> Ownership {
        lock(&self.shared.ownership).clone()
    }
    fn submit(&mut self, batch: Option<FileBatch>) -> Result<Request> {
        let accepting = lock(&self.shared.accepting);
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        match self.startup() {
            Startup::Ready { .. } => {}
            Startup::Connecting => return Err(Error::Unavailable),
            Startup::Failed(e) => return Err(e),
            Startup::Closed => return Err(Error::Closed),
        }
        if !*accepting {
            return Err(Error::Closed);
        }
        if self.pending >= REQUEST_CAP {
            return Err(Error::Busy);
        }
        if batch
            .as_ref()
            .is_some_and(|b| b.paths.len() > self.max_items)
        {
            return Err(Error::TooLarge);
        }
        let id = self.next_id;
        self.next_id = id.checked_add(1).ok_or(Error::Closed)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or(Error::InvalidConfig)?;
        let ticket = Ticket {
            id,
            cancelled: cancelled.clone(),
            deadline,
        };
        let command = match batch {
            Some(batch) => Command::Set(ticket, batch),
            None => Command::Read(ticket),
        };
        self.sender.try_send(command).map_err(|e| match e {
            mpsc::TrySendError::Full(_) => Error::Busy,
            mpsc::TrySendError::Disconnected(_) => Error::Closed,
        })?;
        self.pending += 1;
        Ok(Request { id, cancelled })
    }
    pub fn set_files(&mut self, batch: FileBatch) -> Result<Request> {
        self.submit(Some(batch))
    }
    pub fn read_files(&mut self) -> Result<Request> {
        self.submit(None)
    }
    /// At most eight results; each request has exactly one terminal result.
    pub fn poll(&mut self) -> Vec<Completion> {
        let results: Vec<_> = lock(&self.shared.completions).drain(..).collect();
        self.pending -= results.len();
        results
    }
    pub fn close(&self) {
        *lock(&self.shared.accepting) = false;
        self.shared.shutdown.store(true, Ordering::Release);
    }
}
impl Drop for ClipboardService {
    fn drop(&mut self) {
        self.close();
    }
}
fn check(ticket: &Ticket, shared: &Shared) -> Result<()> {
    if shared.shutdown.load(Ordering::Acquire) || ticket.cancelled.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else if Instant::now() >= ticket.deadline {
        Err(Error::Timeout)
    } else {
        Ok(())
    }
}
fn complete(shared: &Shared, id: u64, result: Result<Outcome>) {
    // A caller reserves one of eight slots before enqueueing. No native event adds a completion.
    lock(&shared.completions).push_back(Completion {
        request: id,
        result,
    });
}
fn run(config: Config, receiver: Receiver<Command>, shared: Arc<Shared>) {
    let mut worker = match Worker::connect(config, shared.clone()) {
        Ok(worker) => worker,
        Err(error) => {
            *lock(&shared.accepting) = false;
            *lock(&shared.startup) = Startup::Failed(error);
            return;
        }
    };
    *lock(&shared.startup) = Startup::Ready {
        window: worker.window,
    };
    let failure = loop {
        if shared.shutdown.load(Ordering::Acquire) {
            break None;
        }
        if let Err(error) = worker.pump() {
            break Some(error);
        }
        worker.expire_transfers();
        match receiver.try_recv() {
            Ok(command) => {
                let id = command.ticket().id;
                let result = match command {
                    Command::Set(ticket, batch) => worker.set(&ticket, batch),
                    Command::Read(ticket) => worker.read(&ticket),
                };
                complete(&shared, id, result);
            }
            Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(2)),
            Err(TryRecvError::Disconnected) => break None,
        }
    };
    // A submit that already passed the Ready check must finish enqueueing before draining.
    // Future submissions observe accepting=false, so no accepted ticket can miss its result.
    *lock(&shared.accepting) = false;
    if let Some(error) = failure.as_ref() {
        *lock(&shared.startup) = Startup::Failed(error.clone());
    } else {
        *lock(&shared.startup) = Startup::Closed;
    }
    shared.shutdown.store(true, Ordering::Release);
    while let Ok(command) = receiver.try_recv() {
        complete(
            &shared,
            command.ticket().id,
            Err(failure.clone().unwrap_or(Error::Cancelled)),
        );
    }
    let mut ownership = lock(&shared.ownership);
    ownership.is_ours = false;
    // Connection drop releases the native owner/window; no source filesystem mutation.
}

#[derive(Clone, Copy)]
struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    uri: Atom,
    gnome: Atom,
    kde: Atom,
    property: Atom,
    clock: Atom,
}
struct Transfer {
    bytes: Arc<Vec<u8>>,
    target: Atom,
    offset: usize,
    eof_sent: bool,
    deadline: Instant,
}
struct Worker {
    conn: RustConnection,
    window: Window,
    root: Window,
    atoms: Atoms,
    config: Config,
    shared: Arc<Shared>,
    owned: Option<HashMap<Atom, Arc<Vec<u8>>>>,
    acquired: u32,
    // Full client request sequence of the confirmed publication, not an X11 millisecond time.
    claim_sequence: SequenceNumber,
    transfers: HashMap<(Window, Atom), Transfer>,
    read_window: Option<Window>,
    inbox: VecDeque<Event>,
    inbox_overflow: bool,
}
impl Worker {
    fn connect(config: Config, shared: Arc<Shared>) -> Result<Self> {
        let defaults = codec::Limits::default();
        if config.timeout.is_zero()
            || config.timeout > Duration::from_secs(60)
            || !(4..=64 << 10).contains(&config.chunk_bytes)
            || !(1..=8).contains(&config.max_outbound_transfers)
            || config.limits.max_payload_bytes > 16 << 20
            || config.limits.max_items > defaults.max_items
            || config.limits.max_lines > defaults.max_lines
            || config.limits.max_line_bytes > defaults.max_line_bytes
            || config.limits.max_path_bytes > defaults.max_path_bytes
            || config.limits.max_total_path_bytes > defaults.max_total_path_bytes
        {
            return Err(Error::InvalidConfig);
        }
        let (conn, screen) = native(x11rb::connect(config.display.as_deref()))?;
        native(native(conn.xfixes_query_version(2, 0))?.reply())?;
        let root = conn.setup().roots[screen].root;
        let window = native(conn.generate_id())?;
        native(
            native(
                conn.create_window(
                    0,
                    window,
                    root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &CreateWindowAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                ),
            )?
            .check(),
        )?;
        let intern = |name: &[u8]| -> Result<Atom> {
            Ok(native(native(conn.intern_atom(false, name))?.reply())?.atom)
        };
        let atoms = Atoms {
            clipboard: intern(b"CLIPBOARD")?,
            targets: intern(b"TARGETS")?,
            timestamp: intern(b"TIMESTAMP")?,
            incr: intern(b"INCR")?,
            uri: intern(codec::URI_LIST_MIME.as_bytes())?,
            gnome: intern(codec::GNOME_COPIED_FILES_MIME.as_bytes())?,
            kde: intern(codec::KDE_CUT_SELECTION_MIME.as_bytes())?,
            property: intern(b"_MEGAMAN_CLIPBOARD_READ")?,
            clock: intern(b"_MEGAMAN_CLIPBOARD_CLOCK")?,
        };
        native(
            native(conn.xfixes_select_selection_input(
                window,
                atoms.clipboard,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER
                    | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                    | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
            ))?
            .check(),
        )?;
        let owner = native(native(conn.get_selection_owner(atoms.clipboard))?.reply())?.owner;
        *lock(&shared.ownership) = Ownership {
            owner,
            generation: 0,
            selection_timestamp: 0,
            is_ours: false,
        };
        native(conn.flush())?;
        Ok(Self {
            conn,
            window,
            root,
            atoms,
            config,
            shared,
            owned: None,
            acquired: 0,
            claim_sequence: 0,
            transfers: HashMap::new(),
            read_window: None,
            inbox: VecDeque::new(),
            inbox_overflow: false,
        })
    }
    fn pump(&mut self) -> Result<()> {
        for _ in 0..256 {
            let Some((event, sequence)) = native(self.conn.poll_for_event_with_sequence())? else {
                break;
            };
            self.handle(event, sequence)?;
        }
        self.expire_transfers();
        Ok(())
    }
    fn handle(&mut self, event: Event, sequence: SequenceNumber) -> Result<()> {
        match event {
            Event::XfixesSelectionNotify(event) if event.selection == self.atoms.clipboard => {
                // An ownership notification carries the last request processed on OUR connection.
                // A pre-claim event cannot invalidate newer offers, even at the same server time.
                if self.owned.is_some() && sequence < self.claim_sequence {
                    return Ok(());
                }
                let mut state = lock(&self.shared.ownership);
                if state.owner != event.owner
                    || state.selection_timestamp != event.selection_timestamp
                {
                    state.generation = state.generation.saturating_add(1);
                }
                state.owner = event.owner;
                state.selection_timestamp = event.selection_timestamp;
                state.is_ours = event.owner == self.window && self.owned.is_some();
                if !state.is_ours {
                    self.owned = None;
                }
            }
            Event::SelectionClear(event) if event.selection == self.atoms.clipboard => {
                if self.owned.is_some() && sequence < self.claim_sequence {
                    return Ok(());
                }
                self.owned = None;
                lock(&self.shared.ownership).is_ours = false;
            }
            Event::SelectionRequest(event) => self.serve(event),
            Event::PropertyNotify(event)
                if event.state == Property::DELETE
                    && self.transfers.contains_key(&(event.window, event.atom)) =>
            {
                self.advance(event.window, event.atom)
            }
            Event::DestroyNotify(event) => {
                self.transfers
                    .retain(|(window, _), _| *window != event.window);
                if Some(event.window) == self.read_window {
                    self.push_inbox(Event::DestroyNotify(event));
                }
            }
            Event::SelectionNotify(event) if Some(event.requestor) == self.read_window => {
                self.push_inbox(Event::SelectionNotify(event))
            }
            Event::PropertyNotify(event) if Some(event.window) == self.read_window => {
                self.push_inbox(Event::PropertyNotify(event))
            }
            Event::Error(error) => {
                return Err(Error::Native(format!(
                    "X11 protocol error {}",
                    error.error_code
                )));
            }
            _ => {}
        }
        Ok(())
    }
    fn push_inbox(&mut self, event: Event) {
        if self.inbox.len() >= INBOX_CAP {
            self.inbox_overflow = true;
        } else {
            self.inbox.push_back(event);
        }
    }
    fn server_time(&mut self, ticket: &Ticket) -> Result<u32> {
        native(
            native(self.conn.change_property8(
                PropMode::REPLACE,
                self.window,
                self.atoms.clock,
                AtomEnum::INTEGER,
                &[1],
            ))?
            .check(),
        )?;
        native(self.conn.flush())?;
        loop {
            check(ticket, &self.shared)?;
            if let Some((event, sequence)) = native(self.conn.poll_for_event_with_sequence())? {
                if let Event::PropertyNotify(e) = &event
                    && e.window == self.window
                    && e.atom == self.atoms.clock
                    && e.state == Property::NEW_VALUE
                {
                    return Ok(e.time);
                }
                self.handle(event, sequence)?;
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    fn set(&mut self, ticket: &Ticket, batch: FileBatch) -> Result<Outcome> {
        check(ticket, &self.shared)?;
        let uri = codec::encode_uri_list(&batch.paths, &self.config.limits)
            .map_err(Error::InvalidPayload)?;
        let gnome = codec::encode_gnome_copied_files(&batch, &self.config.limits)
            .map_err(Error::InvalidPayload)?;
        let kde = codec::encode_kde_cut_selection(batch.intent).to_vec();
        let offers = HashMap::from([
            (self.atoms.uri, Arc::new(uri)),
            (self.atoms.gnome, Arc::new(gnome)),
            (self.atoms.kde, Arc::new(kde)),
        ]);
        let time = self.server_time(ticket)?;
        check(ticket, &self.shared)?;
        // Once sent, a native transport failure cannot prove the old selection survived.
        let publication = (|| -> Result<(Window, SequenceNumber)> {
            let claim = native(self.conn.set_selection_owner(
                self.window,
                self.atoms.clipboard,
                time,
            ))?;
            let sequence = claim.sequence_number();
            native(claim.check())?;
            let owner =
                native(native(self.conn.get_selection_owner(self.atoms.clipboard))?.reply())?.owner;
            Ok((owner, sequence))
        })();
        let (owner, sequence) = publication.map_err(|_| Error::PublicationUncertain)?;
        if owner != self.window {
            return Err(Error::OwnerChanged);
        }
        // This is the publication boundary. Do not report cancellation as rollback after it.
        self.owned = Some(offers);
        self.acquired = time;
        self.claim_sequence = sequence;
        {
            let mut state = lock(&self.shared.ownership);
            state.owner = self.window;
            state.selection_timestamp = time;
            state.is_ours = true;
            state.generation = state.generation.saturating_add(1);
        }
        self.pump().map_err(|_| Error::PublicationUncertain)?;
        let state = lock(&self.shared.ownership);
        if !state.is_ours {
            return Err(Error::OwnerChanged);
        }
        Ok(Outcome::Set {
            generation: state.generation,
            cancelled_after_commit: ticket.cancelled.load(Ordering::Acquire)
                || self.shared.shutdown.load(Ordering::Acquire),
        })
    }
    fn notify(&self, request: &SelectionRequestEvent, property: Atom) {
        let event = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: request.time,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property,
        };
        if let Ok(cookie) =
            self.conn
                .send_event(false, request.requestor, EventMask::NO_EVENT, event)
        {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
    }
    fn serve(&mut self, request: SelectionRequestEvent) {
        let property = if request.property == NONE {
            request.target
        } else {
            request.property
        };
        let valid_time =
            request.time == CURRENT_TIME || (request.time.wrapping_sub(self.acquired) as i32) >= 0;
        let owner = self
            .conn
            .get_selection_owner(self.atoms.clipboard)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.owner);
        if request.selection != self.atoms.clipboard
            || self.owned.is_none()
            || owner != Some(self.window)
            || !valid_time
            || self.transfers.contains_key(&(request.requestor, property))
        {
            self.notify(&request, NONE);
            return;
        }
        let result = if request.target == self.atoms.targets {
            let targets = [
                self.atoms.targets,
                self.atoms.timestamp,
                self.atoms.uri,
                self.atoms.gnome,
                self.atoms.kde,
            ];
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &targets,
                )
                .ok()
                .and_then(|c| c.check().ok())
                .is_some()
        } else if request.target == self.atoms.timestamp {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::INTEGER,
                    &[self.acquired],
                )
                .ok()
                .and_then(|c| c.check().ok())
                .is_some()
        } else if let Some(bytes) = self
            .owned
            .as_ref()
            .and_then(|o| o.get(&request.target))
            .cloned()
        {
            if bytes.len() <= self.chunk_size() {
                self.conn
                    .change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        request.target,
                        &bytes,
                    )
                    .ok()
                    .and_then(|c| c.check().ok())
                    .is_some()
            } else {
                self.begin_transfer(request.requestor, property, request.target, bytes)
            }
        } else {
            false
        };
        self.notify(&request, if result { property } else { NONE });
    }
    fn chunk_size(&self) -> usize {
        self.config
            .chunk_bytes
            .min(self.conn.maximum_request_bytes().saturating_sub(128))
            .max(1)
    }
    fn begin_transfer(
        &mut self,
        window: Window,
        property: Atom,
        target: Atom,
        bytes: Arc<Vec<u8>>,
    ) -> bool {
        if self.transfers.len() >= self.config.max_outbound_transfers {
            return false;
        }
        if self
            .conn
            .change_window_attributes(
                window,
                &xproto::ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
            )
            .ok()
            .and_then(|c| c.check().ok())
            .is_none()
        {
            return false;
        }
        let okay = self
            .conn
            .change_property32(
                PropMode::REPLACE,
                window,
                property,
                self.atoms.incr,
                &[bytes.len() as u32],
            )
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
        if !okay {
            self.unwatch(window);
            return false;
        }
        self.transfers.insert(
            (window, property),
            Transfer {
                bytes,
                target,
                offset: 0,
                eof_sent: false,
                deadline: Instant::now() + self.config.timeout,
            },
        );
        true
    }
    fn advance(&mut self, window: Window, property: Atom) {
        let chunk = self.chunk_size();
        let Some(transfer) = self.transfers.get_mut(&(window, property)) else {
            return;
        };
        if transfer.eof_sent {
            self.transfers.remove(&(window, property));
            self.unwatch(window);
            return;
        }
        let end = transfer
            .offset
            .saturating_add(chunk)
            .min(transfer.bytes.len());
        let bytes = &transfer.bytes[transfer.offset..end];
        let okay = self
            .conn
            .change_property8(PropMode::REPLACE, window, property, transfer.target, bytes)
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
        if !okay {
            self.transfers.remove(&(window, property));
            self.unwatch(window);
            return;
        }
        transfer.eof_sent = bytes.is_empty();
        transfer.offset = end;
        let _ = self.conn.flush();
    }
    fn unwatch(&self, window: Window) {
        if self.transfers.keys().any(|(w, _)| *w == window) {
            return;
        }
        let mask = if window == self.window || Some(window) == self.read_window {
            EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY
        } else {
            EventMask::NO_EVENT
        };
        if let Ok(cookie) = self.conn.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new().event_mask(mask),
        ) {
            let _ = cookie.check();
        }
    }
    fn expire_transfers(&mut self) {
        let expired: Vec<_> = self
            .transfers
            .iter()
            .filter(|(_, t)| Instant::now() >= t.deadline)
            .map(|(key, _)| *key)
            .collect();
        for (window, property) in expired {
            self.transfers.remove(&(window, property));
            self.unwatch(window);
        }
    }
    fn owner(&mut self) -> Result<Window> {
        let owner =
            native(native(self.conn.get_selection_owner(self.atoms.clipboard))?.reply())?.owner;
        self.pump()?;
        Ok(owner)
    }
    fn ensure_owner(&mut self, owner: Window, generation: u64) -> Result<()> {
        if self.owner()? != owner || lock(&self.shared.ownership).generation != generation {
            Err(Error::OwnerChanged)
        } else {
            Ok(())
        }
    }
    fn read(&mut self, ticket: &Ticket) -> Result<Outcome> {
        check(ticket, &self.shared)?;
        let window = native(self.conn.generate_id())?;
        native(
            native(
                self.conn.create_window(
                    0,
                    window,
                    self.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &CreateWindowAux::new()
                        .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
                ),
            )?
            .check(),
        )?;
        self.read_window = Some(window);
        self.inbox.clear();
        self.inbox_overflow = false;
        let result = self.read_inner(ticket, window);
        self.read_window = None;
        self.inbox.clear();
        self.transfers
            .retain(|(requestor, _), _| *requestor != window);
        let cleanup = native(native(self.conn.destroy_window(window))?.check());
        match result {
            Err(error) => Err(error),
            Ok(batch) => {
                cleanup?;
                Ok(Outcome::Read(batch))
            }
        }
    }
    fn read_inner(&mut self, ticket: &Ticket, window: Window) -> Result<FileBatch> {
        let owner = self.owner()?;
        if owner == NONE {
            return Err(Error::NoOwner);
        }
        let generation = lock(&self.shared.ownership).generation;
        let targets = self.receive(
            ticket,
            window,
            owner,
            generation,
            self.atoms.targets,
            AtomEnum::ATOM.into(),
            32,
            TARGET_CAP * 4,
        )?;
        if targets.len() % 4 != 0 {
            return Err(Error::Protocol("misaligned TARGETS"));
        }
        let targets: Vec<u32> = targets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| u32::from_ne_bytes(*b))
            .collect();
        let before = if targets.contains(&self.atoms.timestamp) {
            Some(self.selection_timestamp(ticket, window, owner, generation)?)
        } else {
            None
        };
        let batch = if targets.contains(&self.atoms.gnome) {
            let payload = self.receive(
                ticket,
                window,
                owner,
                generation,
                self.atoms.gnome,
                self.atoms.gnome,
                8,
                self.config.limits.max_payload_bytes,
            )?;
            codec::decode_gnome_copied_files(&payload, &self.config.limits)
                .map_err(Error::InvalidPayload)?
        } else if targets.contains(&self.atoms.uri) {
            let payload = self.receive(
                ticket,
                window,
                owner,
                generation,
                self.atoms.uri,
                self.atoms.uri,
                8,
                self.config.limits.max_payload_bytes,
            )?;
            let paths = codec::decode_uri_list(&payload, &self.config.limits)
                .map_err(Error::InvalidPayload)?;
            let intent = if targets.contains(&self.atoms.kde) {
                let bit = self.receive(
                    ticket,
                    window,
                    owner,
                    generation,
                    self.atoms.kde,
                    self.atoms.kde,
                    8,
                    1,
                )?;
                codec::decode_kde_cut_selection(&bit).map_err(Error::InvalidPayload)?
            } else {
                CopyIntent::Copy
            };
            FileBatch { intent, paths }
        } else {
            return Err(Error::UnsupportedTargets);
        };
        if let Some(before) = before
            && self.selection_timestamp(ticket, window, owner, generation)? != before
        {
            return Err(Error::OwnerChanged);
        }
        self.ensure_owner(owner, generation)?;
        check(ticket, &self.shared)?;
        Ok(batch)
    }
    fn selection_timestamp(
        &mut self,
        ticket: &Ticket,
        window: Window,
        owner: Window,
        generation: u64,
    ) -> Result<u32> {
        let bytes = self.receive(
            ticket,
            window,
            owner,
            generation,
            self.atoms.timestamp,
            AtomEnum::INTEGER.into(),
            32,
            4,
        )?;
        if bytes.len() != 4 {
            return Err(Error::Protocol("invalid TIMESTAMP"));
        }
        Ok(u32::from_ne_bytes(bytes.try_into().unwrap()))
    }
    #[allow(clippy::too_many_arguments)]
    fn receive(
        &mut self,
        ticket: &Ticket,
        window: Window,
        owner: Window,
        generation: u64,
        target: Atom,
        property_type: Atom,
        format: u8,
        limit: usize,
    ) -> Result<Vec<u8>> {
        self.ensure_owner(owner, generation)?;
        self.inbox.clear();
        native(native(self.conn.delete_property(window, self.atoms.property))?.check())?;
        native(
            native(self.conn.convert_selection(
                window,
                self.atoms.clipboard,
                target,
                self.atoms.property,
                CURRENT_TIME,
            ))?
            .check(),
        )?;
        native(self.conn.flush())?;
        loop {
            match self.next_read_event(ticket, owner, generation)? {
                Event::SelectionNotify(event)
                    if event.selection == self.atoms.clipboard && event.target == target =>
                {
                    if event.property == NONE {
                        return Err(Error::UnsupportedTargets);
                    }
                    if event.property != self.atoms.property {
                        return Err(Error::Protocol("unexpected result property"));
                    }
                    break;
                }
                _ => {}
            }
        }
        let initial = self.property(window, limit.max(4))?;
        if initial.type_ != self.atoms.incr {
            let bytes = self.checked_property(initial, property_type, format, limit)?;
            native(native(self.conn.delete_property(window, self.atoms.property))?.check())?;
            self.ensure_owner(owner, generation)?;
            return Ok(bytes);
        }
        if initial.format != 32 || initial.value.len() != 4 || initial.bytes_after != 0 {
            return Err(Error::Protocol("malformed INCR header"));
        }
        let advertised = u32::from_ne_bytes(initial.value[..4].try_into().unwrap()) as usize;
        if advertised > limit {
            return Err(Error::TooLarge);
        }
        // Discard the header's NewValue event before deleting it. Future NewValue means a chunk.
        self.inbox
            .retain(|e| !matches!(e, Event::PropertyNotify(_)));
        native(native(self.conn.delete_property(window, self.atoms.property))?.check())?;
        native(self.conn.flush())?;
        let mut output = Vec::new();
        loop {
            match self.next_read_event(ticket, owner, generation)? {
                Event::PropertyNotify(event)
                    if event.atom == self.atoms.property && event.state == Property::NEW_VALUE =>
                {
                    let remaining = limit.checked_sub(output.len()).ok_or(Error::TooLarge)?;
                    let reply = self.property(window, remaining.min(self.config.chunk_bytes))?;
                    let bytes = self.checked_property(
                        reply,
                        property_type,
                        format,
                        remaining.min(self.config.chunk_bytes),
                    )?;
                    native(
                        native(self.conn.delete_property(window, self.atoms.property))?.check(),
                    )?;
                    native(self.conn.flush())?;
                    if bytes.is_empty() {
                        if output.len() < advertised {
                            return Err(Error::Protocol("INCR lower bound not met"));
                        }
                        self.ensure_owner(owner, generation)?;
                        return Ok(output);
                    }
                    output
                        .try_reserve_exact(bytes.len())
                        .map_err(|_| Error::TooLarge)?;
                    output.extend_from_slice(&bytes);
                }
                _ => {}
            }
        }
    }
    fn property(&self, window: Window, allowance: usize) -> Result<xproto::GetPropertyReply> {
        // X11 length is in 4-byte units; a reply can exceed the allowance by at most three bytes.
        let units = allowance.checked_add(3).ok_or(Error::TooLarge)? / 4;
        let units = u32::try_from(units).map_err(|_| Error::TooLarge)?;
        native(
            native(self.conn.get_property(
                false,
                window,
                self.atoms.property,
                AtomEnum::ANY,
                0,
                units,
            ))?
            .reply(),
        )
    }
    fn checked_property(
        &self,
        property: xproto::GetPropertyReply,
        type_: Atom,
        format: u8,
        allowance: usize,
    ) -> Result<Vec<u8>> {
        if property.bytes_after != 0 || property.value.len() > allowance {
            return Err(Error::TooLarge);
        }
        if property.type_ != type_ || property.format != format {
            return Err(Error::Protocol("unexpected property type/format"));
        }
        Ok(property.value)
    }
    fn next_read_event(
        &mut self,
        ticket: &Ticket,
        owner: Window,
        generation: u64,
    ) -> Result<Event> {
        loop {
            check(ticket, &self.shared)?;
            self.pump()?;
            if self.inbox_overflow {
                return Err(Error::Protocol("native read event queue exceeded"));
            }
            let state = lock(&self.shared.ownership);
            if state.owner != owner || state.generation != generation {
                return Err(Error::OwnerChanged);
            }
            drop(state);
            if let Some(event) = self.inbox.pop_front() {
                return Ok(event);
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}
