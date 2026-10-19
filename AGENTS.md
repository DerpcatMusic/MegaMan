# Contributor instructions

- Work on a feature branch in an isolated worktree. Preserve uncommitted work, commit small verified changes, and finish through a pull request.
- Rust is the application language. Use the shared configured Cargo target directory; do not override it or duplicate checkouts for builds.
- Run appropriate local formatting, tests and strict Clippy. Do not wait on GitHub Actions unless explicitly requested. Native integrations require native fixtures; synthetic presentation tests are not native fault qualification.
- Keep filesystem I/O, blocking waits and large catalog destruction off UI layout/render paths. Preserve native path bytes and opaque operation/listing identities.
- Active operations and terminal receipts must survive navigation and tab closure until explicitly cancelled or acknowledged. Never report published output as rolled back.
- Use intrinsic layout and shared design tokens. Verify short/narrow panes, keyboard focus and error/progress states.
- Keep product documentation focused on MegaMan behavior. Do not publish private research, credentials, external proprietary material or unrelated local environment files.
- Report measured performance and tested platform scope precisely. Do not imply complete support from one fixture.
