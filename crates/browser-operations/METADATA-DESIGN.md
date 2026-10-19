# Basic metadata publication contract

Basic metadata preserves ordinary rwx bits and planned atime/mtime, stripping special permission bits. Source atime is captured separately from mutation fingerprints because reads may alter it. Metadata readback must match exactly; unsupported precision/operations fail explicitly.

Retained descriptor-relative identities protect mutation and cleanup. Restrictive child directory handles are charged before staging, with a default 64/hard 128 pin cap. Cleanup repairs only owned unpublished directories, verifies their identities and never follows a raced replacement path. Source access restrictions and unusual umasks that prevent secure reopening remain explicit failures.

The compatibility `RootPublication::MetadataBeforePublish` policy rejects directory roots without owner-write before staging. Linux cross-parent directory rename needs write permission on the moved directory. Explicit `FinalizeBasicAfterPublish` keeps the root private 0700 through content publication, then applies/readbacks desired root modes/times on a retained descriptor and syncs root/parent. Final root metadata is not atomically visible.

After content publication the finalizer does not honor late cancellation as permission to abandon metadata. Binding/mode/time failure retains committed output with MetadataCompletion::Incomplete; successful metadata followed by sync/journal failure can retain Achieved(Basic) with uncertain durability. Session results fail-stop and preserve published counts. Per-root and terminal UI reporting keep these facts distinct.

Version 2 journal records retain desired root metadata plus ContentPublished/MetadataFinalized phases; version 1 remains decodable. Torn or failed journal writes retain the valid breadcrumb prefix and recovery evidence. This is not automatic recovery or a power-loss durability guarantee.
