# Roadmap

MegaMan is being built in verified increments. Existing functionality is experimental; this list is a work breakdown, not a promise of completed platform support or release dates.

- `megaman PATH...` opens each path in a tab. Tabs exist (Ctrl+T new, Ctrl+W close, Ctrl+Shift+T reopen, Ctrl+Tab cycle, click to switch); drag reordering, pinning and closed-tab history beyond location remain.
- Bookmarks (Ctrl+D), recent folders and mounted volumes appear in the places rail. Split view (Ctrl+\\) shows a second folder pane; F5/F6 copy or move the selection to the other pane. Resizable splits, nested layouts and saved workspaces remain.
- A right-click context menu exists (inline, not yet floating). Files dropped on the window from other applications copy into the current folder (inbound; verified by unit test, not yet against a live XDND source). Complete outbound drag, Wayland clipboard/data-device support and platform context actions.
- Batch rename (select several, F2: {name}, {ext}, {n}, {n:3}) is all-or-nothing and undoes together. Extend safe operations: cut (Ctrl+X) + paste moves, across filesystems by verified copy then Trash (not undoable); rename (F2), create (Ctrl+N / Ctrl+Shift+N), trash (Delete) and undo (Ctrl+Z) exist; name clashes keep both as "name (2)"; replace/skip prompts remain. Compress to ZIP (palette / context menu) writes a new archive beside the selection and undoes by removal. Typing letters in the list jumps to the first matching name (1 s buffer). A breadcrumb row under the toolbar jumps to any of the last six ancestor folders. Extraction takes a masked password for encrypted ZIPs.
- Text and image previews (bounded, worker-decoded) exist in the inspector. Add thumbnails, richer inspectors and file-opening integrations.
- Extend archive workflows with password entry, creation, nested browsing, preview/drag staging, more formats and independent encryption interoperability tests.
- The listed folder refreshes after external changes (inotify, 300 ms settle; the focused item stays selected). Add watcher convergence for archives, large-folder sorting/filtering, system-wide indexed finding and explicit incomplete-coverage feedback.
- Sort columns (Name, Type, Modified, Size) and real sizes/dates exist. Add list/grid/tree views, grouping, disk usage and future column-based navigation. Folder sizes are measured in the background (… while pending, "≥" when partial).
- Copy path (Ctrl+Shift+C), open terminal here and GVfs network volumes exist. Command palette (Ctrl+P) and hidden-file toggle (Ctrl+H) exist. Expand settings, shortcuts, command search, persistence, accessibility, IME and multilingual testing.
- Add network/device/cloud providers with explicit cancellation, capability and offline behavior.
- Qualify Linux X11/Wayland, then Windows/macOS and ARM64 through native tests.
- Measure startup, navigation, input-to-present latency, memory, idle wakeups, throughput and distribution footprint with repeatable fixtures.
