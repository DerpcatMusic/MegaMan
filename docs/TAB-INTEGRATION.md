# Native tab integration design

Status: architecture proposal; native tab controls are not implemented or qualified.
This extends the merged memory-only workspace model. Keep the existing public
`BrowserView::new`, `fixture`, native clipboard enablement, evidence and host `View`
entry points while making BrowserView the app shell.

## Ownership

`BrowserView` will own `Workspace<TabView>`, one `AppJobs`, a checked presentation
sequence, presented-frame routing state, and a bounded tab-retirement service.
`TabView` retains the current directory Browser/history, filter/path drafts,
revision-scoped selection/cursor, ListState/scroll, FindPanel, ArchivePanel,
per-tab pane preferences and a focus-role bookmark. Switching retains this payload;
no worker, catalog or selection ticket is cloned or destroyed to activate it.

`AppJobs` exclusively owns ClipboardUi, TransfersUi, ExtractionUi, pending native
Read/offers, mutation reservation, job/result acknowledgement and refresh intents.
It is outside every tab, including recently closed records. Clipboard selection
export retains its listing-worker ticket independently of the originating tab.
Paste retains its already captured native destination. Prepared extraction already
retains its worker-owned selection. Closing a tab cannot cancel or acknowledge
these controllers, clear a mutation reservation, or release their leases.

ExtractionUi is lifted as an opaque controller through its existing
prepare/poll/build/after_frame/busy/show/refresh interface. Its process-owner/epoch
IDs remain global. Password state and credentials never enter tab resume data,
bookmarks or evidence. Any required visibility-only API change is coordinated with
the extraction owner before implementation; credential/render internals stay out
of this lane.

The shell selects a directory/Find/archive body or a global Copy/extraction body.
The tab strip and compact global job access remain reachable around each body,
including an empty workspace. Clicking a tab returns to that tab's body; job panels
can be hidden without cancelling, and reopened through global controls. Closing
an originating tab while a global job is shown does not remove that global screen.

## IDs and exact dispatch

The pinned MUI revision has public `Id::of/field/entity`, `Ui::focus`, `blur`,
`focus_key`, `focus_is_text`, `keys`, `shortcuts`, and `cancel`. It has no automatic
Ui subtree namespace. Assign every browser/Find/archive control, editor, divider,
collection, row and geometry lookup a composed ID under the complete opaque TabId.
Listing and subview epochs remain below this prefix; no visual index or filename
becomes an identity. New Find/archive instances receive checked, non-reused scope
epochs. Job controls remain under their existing app-owned owner/job epochs.

A typed `FrameOwner` records the actual presented surface, TabId when applicable,
presentation sequence, location/listing revision and subview generation. Tab
commands carry that owner. Resolve them only against that exact live TabId and
revision; reject missing, foreign, closed or replaced owners. Never fall back to
whichever tab is active when a delayed response is consumed. Reordering preserves
TabIds and does not remap gestures by row index. Reopened tabs have new IDs.

Collect at most one workspace-structural intent per delivered frame. A switch,
close or reopen takes priority over a new conflicting tab-local path/navigation/
selection/Copy/Paste/extraction action from that same batch. Already accepted
global requests continue; this rule does not discard jobs or prepared offers.

## Focus and input boundary

MUI responses are read on the build after resolution. Its cancel method clears
pointer capture/drag/preedit but does not clear already delivered key/text/paste
streams. Programmatic focus can therefore expose an old stream to a newly focused
widget. Explicit namespaces alone do not fix that keyboard case, and wrapping the
returned element does not namespace its internal IDs.

Proposed prerequisite: a narrow reviewed MUI input-discard operation for explicit
view transitions. It must preserve an owed gesture End, cancel capture/drag/IME,
and retire current transient keyboard/text/paste/native-text delivery without
acknowledging app commands or changing persistent editor values. This prerequisite
needs architecture approval before implementation; it is not an invented existing
API or a plan to manipulate runtime borrow guards.

On a committed surface transition, save the outgoing focus role, invalidate its
presentation route, discard that frame's transient delivery through the supported
boundary, blur, and build the incoming namespace without handling old tab input.
Restore focus only for a successfully presented incoming scope, to a remembered
role that exists there or its file list. New/closed/reopened tabs never inherit
another tab's text, IME composition, row press, path Enter or mutation shortcut.
If resolution fails, do not advance the presented owner or consume its barrier.

Global tab shortcuts read `Ui::shortcuts()` plus only the whitelisted control
chords from the focused field's `Ui::keys()`. Text Ctrl+C/Ctrl+V/Select All stay
with their editor; file Copy/Paste require the matching non-editor tab scope.
Ctrl+Tab is handled once by the shell, with focus explicitly restored after MUI's
ordinary Tab-cycle handling. Event/input tests must qualify this boundary before
native tab controls are enabled.

## Commands and refresh

Split construction into bounded typed intents and state commits. TabView builds
with read-only global availability and emits app actions; AppJobs never borrows
active state by index. A prepare-extraction action identifies its live TabId and
archive generation, then briefly borrows that exact panel alongside AppJobs to
capture the preparation before it can outlive the tab. Use ordinary disjoint
struct-field borrowing; no RefCell/unsafe alias or temporary controller removal.

After a resolved frame, global Cancel/Dismiss controls run before admission of a
completed native Read offer, preserving the existing Cancel/read-completion rule.
Admitted Read offers use their frozen native destination even after navigation or
tab closure. New preparation is refused while the global mutation gate is held;
accepted Copy/extraction stays busy until its matching terminal is acknowledged.
Clipboard file export remains a separate non-mutating operation.

AppJobs polling emits bounded native destination/parent refresh intents. Examine
all live TabIds; refresh every directory Browser whose current native path equals
that destination, retaining its filter/history and resetting only that tab's
listing-scoped selection as on today's refresh. Do not canonicalize on the UI
thread or navigate a nonmatching tab. Hidden tabs poll their directory/Find/archive
workers in a count-bounded pass; accepted listing delivery invalidates their own
revision. Operation provenance is optional, never a required live origin tab.

## Creation, closure and retirement

Provisional controls: Ctrl+T opens the active native directory (or the startup
home when empty), Ctrl+W closes the active tab, Ctrl+Shift+T reopens the newest
remembered close, Ctrl+Tab/Ctrl+Shift+Tab cycles, and stable-ID drag/drop reorders.
Use the model's 32-open/16-closed defaults and explicit limit feedback. Closing the
last tab shows an empty workspace with New tab and global job access; it does not
close the application or lose receipts. The intrinsic horizontal tab strip clips
long labels and overflows within its own viewport; no window-height subtraction
or absolute positioning allocates the content region.

Add a narrow Browser resume API for native directory/back/forward/filter export
and reconstruction. Bound history and snapshot bytes before cloning into a close
record. Back history is oldest-to-nearest; forward is next-to-farthest. Reopen
starts fresh worker/catalog/selection state from that snapshot. Archive prefixes,
search results, credentials and jobs are not persisted. If a snapshot cannot be
remembered, close only through an explicit no-history outcome with visible feedback.

Before removing a live tab, reserve a bounded retirement slot. If unavailable,
leave the tab intact and show retry feedback. Close returns the TabView and plain
evictions to that worker; heavy selection/history/panel destruction and worker
shutdown never run in rendering. The directory worker already owns final catalog
reclamation. Verify Send bounds rather than assuming every UI payload is movable.
A dedicated bounded final shutdown batch handles all remaining live tabs without
joining workers on the UI thread; ordinary Workspace<TabView> Drop is insufficient.

## Local and native gates

First qualify the MUI transition-input primitive and explicit ID routing, then
lift global jobs without changing their semantics, then expose tab chrome. Run
focused local tests and strict Clippy; do not wait on remote Actions.

Adversarial tests must cover same-count/local-serial tab pairs; press in A,
switch to B, release; close/reopen; old Enter/path/Copy/selection; foreign IDs;
Ctrl+Tab during editor/IME input; retained per-tab history/filter/selection/scroll;
reorder with a held target; failed resolution; retirement backpressure and final
shutdown. Cross-lifecycle fixtures retain clipboard ownership, deferred Paste,
prepared/running extraction and terminal receipts after origin closure, and refresh
all matching hidden tabs while leaving unrelated tabs unchanged. Global Cancel
and acknowledgement remain reachable at 320x240 and 640x480.

Owned-X11 native fixtures must verify real shortcuts/pointer switching, global
job access and close/reopen behavior against the exact app artifact. Pure model or
synthetic presentation tests do not establish native routing, IME support,
performance or other-platform qualification. Session persistence and split panes
remain separate work.
