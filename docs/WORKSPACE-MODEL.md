# MegaMan workspace model

`browser_core::workspace` provides ordered in-memory tabs and bounded recently
closed directory state. It does not create browser workers, read files, render a
tab strip, register shortcuts, or persist a session. This document specifies the
model's behavior and the ownership contract for the application integration.
Native tabs and global job routing still need implementation and native tests.

## Identities and ordering

A `Workspace<T>` owns live payloads in visual order. `TabId` and `ClosedTabId` are
opaque, distinct process-local types. Each identity includes a unique workspace
owner and a monotonically allocated sequence. They cannot be constructed from a
number or display string, and are not serialization keys. A foreign workspace's
ID is rejected separately from an ID that has become stale. Identity allocation
uses checked arithmetic; exhaustion never wraps or reuses an identity.

A nonempty workspace always has an active tab. The first background insertion
becomes active. Foreground creation or reopening activates the new tab;
background creation or reopening preserves the current active tab. Reordering
uses the final visual index and preserves active identity. Closing an inactive
tab preserves the active tab. Closing the active tab chooses its former right
neighbor, then its left neighbor when no right neighbor remains. Closing the
last tab leaves an empty workspace with no active ID. The application decides
whether to show an empty workspace or admit a replacement tab.

`create` appends; `insert` accepts positions from zero through the current length.
`reorder` accepts a final index below the current length. `tabs()` yields visual
order. `recently_closed()` yields newest first; explicitly returned eviction and
clear results are oldest first.

## Resume state and limits

A `TabSnapshot` contains only native directory location, back and forward paths,
and filter text. History order is supplied by the caller and preserved verbatim.
It contains no live browser, catalog, selection lease, clipboard offer, mutation
controller, credentials, or worker handle. Paths are stored losslessly as native
paths, without display-string reconstruction, canonicalization, existence checks,
or absolute-path requirements. Relative locations keep their native relative
meaning; the application should use absolute locations when stable resolution
across working-directory changes is required.

Snapshot storage uses boxed paths, boxed history arrays, and boxed text so excess
caller-provided collection capacity is not retained in closed records. The
constructor itself accepts caller-owned input of any size; close admission
checks history lengths and accounted bytes before retaining it. An oversized
snapshot is returned unchanged with the error.

| Limit | Default | Accepted maximum |
| --- | ---: | ---: |
| Open tabs | 32 | 256 |
| Recently closed records | 16 | 64 |
| History paths per direction | 32 | 64 |
| Accounted bytes per snapshot | 256 KiB | 1 MiB |
| Total accounted closed bytes | 2 MiB | 16 MiB |

Open-tab count and per-snapshot bytes must be positive. Zero recently closed
records disables remembered closes. Total closed bytes must fit one permitted
snapshot when history is enabled. History depth can be zero. Invalid limits are
rejected when constructing the workspace; limits do not mutate later.

Accounted bytes include the closed-record header, history path headers, native
encoded path bytes, and UTF-8 filter bytes. Allocator overhead and bounded
collection spare capacity are separate. The model does not bound memory inside
live `T`; the owning application must bound live catalogs, caches, and workers.
Lookup and ordering operations scan bounded collections. No operation performs
filesystem I/O, invokes an application factory, or spawns a worker.

## Admission and retirement

`close(id, snapshot)` validates the ID, history limits, snapshot bytes, and a new
identity before changing state. It returns the removed `OpenTab<T>`, the new
`ClosedTabId`, and any oldest records evicted to satisfy count or byte limits.
The live payload is moved out; it is neither cloned nor retired by the model.
`close_without_history` returns the same live ownership without retaining resume
state or allocating an identity. It remains usable when history is disabled or
identities are exhausted.

To reopen, the owner first reads `closed(id)?.snapshot()` and prepares a fresh
live payload outside the model. `reopen(id, live, activation)` inserts it at the
former position clamped to the current length and returns the original snapshot.
It consumes the close record only on success and always mints a new live tab ID.
A delayed event for the old tab cannot target the reopened tab.

Creation and reopening failures return their supplied live payload through
`AdmissionError<T>`. Remembered-close failures return their supplied snapshot.
Returned errors preserve order, active identity, retained close records, byte
accounting, and the identity counter. Applications can retire rejected payloads
through their usual nonjoining path. Allocation failure follows Rust's normal
allocation behavior rather than this recoverable admission guarantee.

The owner must consume `Closed::removed.into_parts()` and schedule any heavy
catalog reclamation or worker retirement appropriately. Switching tabs retains
live state and does not retire it. At workspace shutdown, `into_parts()` returns
all live tabs for the same explicit retirement path. Ordinary Rust destruction
of a workspace drops its live payloads; the model does not intercept arbitrary
`T` destructors or guarantee that caller-written destructors are nonblocking.

## Application-owned Copy, extraction, and clipboard

The app shell must own Copy and extraction controllers, their mutation admission
gate, native clipboard requests and offers, progress/results, and acknowledgement
state outside every tab payload. These are application jobs, not directory-tab
resume state. Closing or switching a tab must not cancel a job, acknowledge a
result, release a mutation reservation, or silently hide the only reachable
Cancel/Show/Acknowledge controls.

At job admission, the shell captures native source and destination state and
retains any required worker/catalog leases independently of the originating
tab. A tab ID may be kept as optional provenance; it must not be required to
continue, cancel, or acknowledge the job. Closing the source tab transfers its
live payload to retirement while accepted job leases remain owned by the global
controller. Prepared extraction state must likewise remain owned by the shell
until explicit cancellation, submission, or acknowledgement.

Async tab-specific delivery must route by `TabId` plus the relevant listing or
location revision. A visual index or whichever tab happens to be active is not
a valid delivery address. Stale and foreign identities must discard the delivery
rather than fall back to the active tab. Reopening starts fresh tab identity,
workers, listing state, and selection state from the plain snapshot.

Publication refresh should examine all matching live tabs, not only the active
tab or the former originating tab. The shell should use native locations and
revision checks; path alias/canonicalization policy is an integration decision,
not a filesystem query performed by this model. A refresh must not navigate an
unrelated tab. Hidden tabs still need bounded worker/result polling and explicit
invalidation when their state changes.

These job-lifetime and refresh rules are integration requirements. The pure model
tests establish identity and ownership behavior, not completion of app-shell job
migration or native desktop routing.

## Local validation

The workspace lifecycle tests cover interleaved reorder/close/reopen operations,
deterministic neighbors, stale and foreign delivery rejection, byte and count
eviction, native undecodable paths, returned-payload retirement ownership,
transactional capacity and identity exhaustion, disabled history, and concurrent
workspace identity allocation. The complete browser-core suite and strict
browser-core Clippy pass locally.

No native tab interaction, shortcut compatibility, session persistence, split
panes, archive-prefix restoration, cross-platform desktop behavior, or tab UI
performance is qualified by these tests.
