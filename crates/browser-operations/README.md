# Copy operations

Linux descriptor-relative copy-to-new-destination planning and execution. Sources and parent identities are bound; symlinks are not followed into unrelated targets. Private staging is independently read back before no-replace publication. A batch publishes roots separately and stops at the first failed/cancelled/uncertain root; later roots remain unstarted.

Private metadata is the compatibility default. Basic preserves ordinary rwx and planned atime/mtime with exact readback, strips special bits, and does not promise ownership, ACLs, xattrs, sparse layout or hardlink topology. Read-only roots require explicit post-publication finalization. Paste opts in; DnD currently retains the default refusal. See [metadata policy](METADATA-DESIGN.md).

After publication, failures and late cancellation cannot become no-output rollback. Receipts distinguish achieved/incomplete metadata, synced/uncertain durability, warnings and last-known owned residue identity/path. Journals are bounded breadcrumbs, not automatic crash recovery. Resource/FD limits and unsupported filesystem/kernel operations fail explicitly.
