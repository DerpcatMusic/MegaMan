# Roadmap

MegaMan is being built in verified increments. Existing functionality is experimental; this list is a work breakdown, not a promise of completed platform support or release dates.

- Tabs exist (Ctrl+T new, Ctrl+W close, Ctrl+Shift+T reopen, Ctrl+Tab cycle, click to switch); drag reordering, pinning and closed-tab history beyond location remain.
- Add resizable split panels, saved workspaces, bookmarks, recents and volume navigation.
- Complete native inbound/outbound drag/drop, Wayland clipboard/data-device support and platform context actions.
- Extend safe operations: rename (F2), create (Ctrl+N / Ctrl+Shift+N), trash (Delete) and undo (Ctrl+Z) exist; move, cross-filesystem move, per-volume trash, conflict decisions and recovery remain.
- Add previews, thumbnails, inspectors and file-opening integrations on bounded workers.
- Extend archive workflows with password entry, creation, nested browsing, preview/drag staging, more formats and independent encryption interoperability tests.
- Add watcher convergence, large-folder sorting/filtering, system-wide indexed finding and explicit incomplete-coverage feedback.
- Sort columns (Name, Type, Modified, Size) and real sizes/dates exist. Add list/grid/tree views, grouping, disk usage and future column-based navigation.
- Expand settings, shortcuts, command search, persistence, accessibility, IME and multilingual testing.
- Add network/device/cloud providers with explicit cancellation, capability and offline behavior.
- Qualify Linux X11/Wayland, then Windows/macOS and ARM64 through native tests.
- Measure startup, navigation, input-to-present latency, memory, idle wakeups, throughput and distribution footprint with repeatable fixtures.
