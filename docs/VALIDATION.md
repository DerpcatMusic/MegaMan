# Local validation

Launch source `4b5a8b7aaa70f260347e9227b9b5582eec5fbb82` (the tree merged as `05ae974`) passed formatting, all 281 local Rust tests and strict all-target workspace Clippy. It built with Rust 1.98.1 using `cargo build --release --locked`.

The retained Linux executable is 16,985,176 bytes, SHA256 `becd7a044c88d69d9482ecd6ec3db20b2ed6b8f551c70ec9d824718a4fc1cda8`. This is the executable measurement only; external runtime/font/dependency costs are separate. No timing or comparative performance sample was collected.

All 22 native owned-fixture cases passed on that exact renamed executable:

- 8 Paste cases: ordinary file/folder/symlink, fail-stop conflict, read-only 0555 tree plus empty 0400 root, refused Cut, cancelled native read, navigation during read, text editing and Back/reopen.
- 9 ZIP cases: all/selected/current-folder extraction, retained lease after closing Archive, conflict, traversal rejection, cancelled preparation, changed source and native-byte parent.
- 5 cross-operation cases: shared admission through explicit acknowledgement, one-shot host refresh, retained idle-controller routing and reachable delayed-Read Cancel in Archive/Find.

Read-only Paste checks ordinary permissions, special-bit stripping and exact planned nanosecond atime/mtime before verifier reads, plus achieved metadata/uncertainty counts and unchanged source content/modes. Filesystem/hash/peer oracles and clean native window exits supply functional proof. These are not native UI fault-injection or late-publication cancellation tests.

The [compact result](../benchmarks/results/launch-20261019/summary.json) pins source, executable hash, cases and evidence fields. Full raw manifests/screenshots/binary are retained locally under `/mnt/Windows11/DEV_WORKSPACE/PluginArtifacts/MegaMan/launch-20261019`. The committed scripts reproduce the fixture flows against an explicit build; raw local artifacts are not a public download service.

A separate bounded workspace-model increment passed 10 focused tests, 32 total core tests, strict core Clippy and an independent 10,000-operation oracle before merge. That qualifies tab state semantics, not native tab UI or app-shell operation lifetimes.

The [encrypted ZIP interoperability increment](../benchmarks/results/zip-interop-20261019/summary.json) passed three explicit tests against installed 7-Zip 26.04 on Linux. MegaMan read/extracted eight AES-128/192/256 or legacy ZipCrypto combinations with Stored/Deflate; 7-Zip verified/extracted both MegaMan AES-256 creation combinations. Missing/wrong credentials and a corrupted AES authentication trailer failed without published output or staging residue. All 80 existing archive Rust tests, formatting and strict archive Clippy also passed. This qualifies those owned fixtures and service APIs; password UI and exhaustive ZIP coverage remain separate work.

The [directory-model baseline](PERFORMANCE.md) measured 1,000/10,000/100,000 owned empty files with 15 warm rescans per count and a full ordered native-identity oracle after every catalog delivery. The result includes source/build/executable identity, raw samples and host/filesystem scope. Native startup, rendering and input-to-present latency were not measured.

Native results cover owned Xvfb/X11, forced CPU and scale 1. They do not establish Wayland, Windows/macOS, ARM64, real hardware GPU, accessibility/IME, latency, idle CPU or all-filesystem support.

Independent finish review inspected the read-only Copy result, ordinary/partial Copy at 1024/640/320, short extraction screens and stacked archive status. No blocking alignment, critical-result or control-containment defect was found in those exact captures. Unknown-metadata and durability-failure presentation remain separately tested states rather than newly qualified native faults.
