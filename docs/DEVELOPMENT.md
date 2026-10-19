# Development

Build with Rust 1.98.1 or newer. MUI and its matching winit host are pinned to one immutable public revision in Cargo.toml/Cargo.lock. No web runtime is required.

Use feature branches and local checks from the README. Native functionality is tested separately from CPU-resolved scenes. The `smoke-*` scripts create their own fixtures and displays, preserve failures and close windows through the native protocol. Output directories must be explicit and new or empty. Each run records the executable hash and actual result evidence; keep a retained executable for reproducibility.

Do not override the configured Cargo target directory. Parallel worktrees sharing one target can reuse artifacts with misleading timestamps; verify the compiling source path and test names when changing APIs between branches. Serialize conflicting build windows when necessary.

Tests create generated fixtures beneath each crate's ignored test directories. Unprivileged source permission limits, Linux filesystem/kernel capabilities and cooperative cancellation under blocked OS calls remain explicit. New desktop backends need their own native tests.

Archive interoperability has a separate opt-in test target requiring `7z` on PATH: `cargo test --locked -p browser-archive --test zip_interop -- --ignored`. The ordinary test run marks these external-tool cases ignored; it never silently counts a missing tool as a passing compatibility check. See the [ZIP service notes](../crates/browser-archive/README.md) for fixture scope.

Pull requests should explain behavior, local checks and known limits. Performance reports need exact source/build/fixture identity, correctness oracles, sample counts and measurement boundaries. A valid output is required before a speed sample counts.
