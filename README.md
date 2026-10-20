# MegaMan

A native file manager for Linux, built in Rust with [MUI](https://github.com/Matari-Audio/MUI).

MegaMan is under active development. It currently includes a virtualized file table, keyboard and range selection, recursive filename search, ZIP browsing and verified extraction, and native X11 file clipboard copy/paste with background jobs and per-item results.

Copy operations preserve ordinary permissions and timestamps, verify copied content, refuse overwrites, and retain published results when a later item fails or cancellation arrives. Read-only folder roots use explicit descriptor-bound metadata finalization. ZIP extraction publishes a new folder after all selected entries verify.

## Build and run

Use Rust 1.98.1 or newer and a Linux desktop with the system libraries required by MUI/winit. Clipboard integration currently uses X11; Wayland-native file transfers are still being built.

```sh
cargo run --release --locked -- /path/to/folder
```

The executable is `target/release/megaman`, or your configured Cargo target directory. Use `megaman --help` for fixture, snapshot and profiling options.

## Local checks

```sh
cargo fmt --all --check
cargo test --workspace --no-default-features --locked
cargo clippy --workspace --all-targets --no-default-features --locked -- -D warnings
```

Native fixture scripts in `scripts/` use owned Xvfb displays, actual keyboard/mouse input and filesystem oracles. They require Xvfb, xdotool, ImageMagick and Python-Xlib. Pass an explicit executable and a new or empty output directory. Functional fixtures do not establish latency or hardware GPU performance.

See [development](docs/DEVELOPMENT.md), [architecture](docs/ARCHITECTURE.md), [roadmap](docs/ROADMAP.md), [local validation](docs/VALIDATION.md) and [project board](https://github.com/users/DerpcatMusic/projects/4).

## Status and license

Available now: tabs, a command palette (Ctrl+P), list and grid views with image thumbnails, sortable size/date columns, text and image previews, folder totals, bookmarks/recents/volumes, rename (including template batch rename), create, cut/move on one filesystem, Trash with undo, copy path, open with the default app, and live refresh after external changes.

Still in development: nested/resizable split layouts, outbound drag/drop, Wayland clipboard, replace/skip conflict prompts, password-protected archive creation, accessibility/IME and localization. Windows/macOS and native Wayland are not yet qualified. Keep the current experimental scope in mind when testing.

MegaMan is [MIT licensed](LICENSE). Dependencies retain their own licenses; the Ubuntu font license is preserved in [licenses/](licenses/Ubuntu-font-LICENSE.txt).
