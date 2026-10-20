# Architecture

MegaMan separates its application-rendered MUI interface from filesystem and desktop workers. UI construction performs no filesystem access and does not wait for an operation. Redraw follows invalidation; settled geometry must not continuously mark the model dirty.

- `browser-core`: native directory catalog workers, immutable revision leases and lazy range selection.
- `browser-search`: cancellable explicit-root filename indexing, complete match counts and bounded result pages.
- `browser-archive`: bounded ZIP catalogs, typed member identities, integrity-checked transactions and retained extraction jobs.
- `browser-operations`: Linux copy-to-new-destination plans, private staging, independent readback, metadata policy, no-replace publication and truthful receipts.
- `browser-operations::fsops`: single-step create, rename, move and home-Trash operations with no-replace semantics; each returns an inverse used by bounded undo. Trash falls back to `$topdir/.Trash-$uid` on other filesystems; cross-filesystem move is not implemented and reports an error.
- `browser-transfer`: lossless native file URI/desktop clipboard codecs.
- `browser-native`: worker-owned X11 clipboard and a bounded drag/drop lifecycle contract; actual native drag/drop transport is still in development.
- `browser-ui`: intrinsic controls, virtualized collections, persistent job screens and shared mutation admission.

Native paths never round-trip through display text. Listing, job, extraction and transfer handles include opaque ownership/generation identity; stale controls cannot redirect operations. Directory and search catalogs can grow with item count. Uniform list state and Select All are constant-space, while fragmented selection can grow with its number of ranges.

Copy publishes each root independently. A later failure or cancellation retains prior published roots and leaves later roots unstarted. ZIP extraction publishes its selected output into a new folder only after verification. Final Basic metadata for read-only copy roots is applied after content publication through retained descriptors; metadata achievement, durability and cleanup are separate receipt facts. Journals are bounded breadcrumbs, not automatic recovery.

Clipboard reads and admitted copy/extraction jobs hold one shared mutation reservation through terminal acknowledgement. Back hides a job without cancelling it. Known publication refreshes a matching current host folder without changing navigation elsewhere. These services will stay app-owned when tabs and panels are connected.
