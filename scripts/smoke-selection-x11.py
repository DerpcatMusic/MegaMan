#!/usr/bin/env python3
"""Owned-fixture native X11 multi-selection regression, forced CPU at scale 1.

Requires Xvfb, xdotool, ImageMagick, python-xlib and Pillow. Fixed smoke-window
points are harness inputs; application layout remains intrinsic. No latency,
Wayland, hardware GPU or accessibility qualification.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import time

from PIL import Image
from Xlib import X, display, protocol


SIZE = (1024, 720)
MARKER = "selection-edit-ok"


def digest(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def runs(indices):
    """Small independent set oracle; the million-row expectation stays lazy."""
    result = []
    for index in sorted(indices):
        if result and result[-1][1] == index:
            result[-1][1] += 1
        else:
            result.append([index, index + 1])
    return result


class Oracle:
    def __init__(self):
        self.selected = set()
        self.cursor = None
        self.anchor = None
        self.extension = None

    def click(self, index, control=False, shift=False):
        if shift:
            anchor = self.anchor if self.anchor is not None else self.cursor
            anchor = index if anchor is None else anchor
            self.anchor = anchor
            if self.extension is None or self.extension[0] != control:
                self.extension = (control, self.selected.copy())
            interval = set(range(min(anchor, index), max(anchor, index) + 1))
            self.selected = interval | (self.extension[1] if control else set())
        else:
            self.extension = None
            self.anchor = index
            if control:
                self.selected.symmetric_difference_update({index})
            else:
                self.selected = {index}
        self.cursor = index

    def cursor_down(self):
        self.cursor += 1
        self.anchor = self.cursor
        self.extension = None

    def toggle_cursor(self):
        self.selected.symmetric_difference_update({self.cursor})
        self.anchor = self.cursor
        self.extension = None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--arrival-files", type=int, default=0,
                        help="optional churn fixture (0 to 100000); ordering is reported, not assumed")
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an executable")
    if (not args.out.is_absolute() or out == Path("/")
            or out == Path.home()):
        parser.error("output must be an explicit absolute new or empty directory")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error("output must be new or empty")
    if not 0 <= args.arrival_files <= 100000:
        parser.error("arrival-files must be between 0 and 100000")
    for name in ("Xvfb", "xdotool", "import", "identify"):
        if not shutil.which(name):
            parser.error(f"required executable unavailable: {name}")
    out.mkdir(parents=True, exist_ok=True)
    files, runtime = out / "files", out / "runtime"
    files.mkdir()
    runtime.mkdir(mode=0o700)
    for index in range(20):
        (files / f"item-{index:02}.txt").write_bytes(b"owned selection fixture\n")
    arrival = out / "arrival-files"
    if args.arrival_files:
        arrival.mkdir()
        for index in range(args.arrival_files):
            (arrival / f"arrival-{index:06}.txt").write_bytes(b"owned arrival fixture\n")
    artifact_sha = digest(binary)
    report = {"schema_version": 1, "passed": False, "artifact_sha256": artifact_sha,
              "renderer": "forced CPU", "display": "owned Xvfb X11, scale1, 1024x720",
              "fixture": {"ordinary_files": 20, "synthetic_rows": 1000000,
                          "optional_arrival_files": args.arrival_files},
              "runs": [], "limits": [
                  "Native X11 functional regression; no hardware GPU, Wayland, other-OS or latency proof",
                  "Modifier/anchor behavior is tested with the explicit selection oracle",
                  "Selected identities are checked against a small set oracle; no operations or DnD implied",
                  "Optional background-arrival ordering needs intermediate capture review; exit focus alone is insufficient"]}
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1", MUI_RENDERER="cpu",
               XDG_RUNTIME_DIR=str(runtime))
    env.pop("WAYLAND_DISPLAY", None)
    read_fd, write_fd = os.pipe()
    with (out / "xvfb.log").open("w") as server_log:
        server = subprocess.Popen(["Xvfb", "-displayfd", str(write_fd), "-screen", "0",
                                   "1600x1200x24", "-nolisten", "tcp"], pass_fds=(write_fd,),
                                  stdout=server_log, stderr=server_log)
        os.close(write_fd)
        try:
            if not select.select([read_fd], [], [], 10)[0]:
                raise RuntimeError("Xvfb did not allocate a display")
            env["DISPLAY"] = ":" + os.read(read_fd, 64).decode().strip()
            os.close(read_fd)
            read_fd = None

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True,
                                      check=True, timeout=10)

            cases = ["point-toggle", "additive-shrink", "shift-shrink", "cursor-only",
                     "cursor-toggle-on", "cursor-toggle-off", "escape-route", "text-control-a",
                     "million-scroll-baseline", "million-select-all"]
            if args.arrival_files:
                cases.append("background-arrival")
            by_case = {}
            for case in cases:
                run = {"case": case, "passed": False, "screenshots": []}
                report["runs"].append(run)
                oracle = Oracle()
                synthetic = case.startswith("million-")
                log_path, csv_path = out / f"{case}.log", out / f"{case}.csv"
                with log_path.open("w") as log:
                    argv = [str(binary), "--size", "1024x720", "--profile", str(csv_path)]
                    argv += ["--fixture", "1000000"] if synthetic else [str(files)]
                    app = subprocess.Popen(argv, env=env, stdout=log, stderr=log, cwd=files)
                    connection = None
                    try:
                        deadline, window_id = time.monotonic() + 20, None
                        while time.monotonic() < deadline:
                            if app.poll() is not None:
                                raise RuntimeError(f"{case}: app exited before usable presentation")
                            found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "MegaMan"],
                                                   env=env, capture_output=True, text=True, timeout=10)
                            if found.returncode == 0:
                                window_id = found.stdout.splitlines()[0]
                                ready = out / f"{case}-ready.png"
                                command(["import", "-window", window_id, str(ready)])
                                if int(command(["identify", "-format", "%k", str(ready)]).stdout) > 20:
                                    break
                            time.sleep(.1)
                        else:
                            raise RuntimeError(f"{case}: no usable presentation")
                        connection = display.Display(env["DISPLAY"])
                        window = connection.create_resource_object("window", int(window_id))
                        command(["xdotool", "windowfocus", window_id])
                        time.sleep(.25)
                        step = 0

                        def snapshot(label):
                            path = out / f"{case}-{step:02}-{label}.png"
                            command(["import", "-window", window_id, str(path)])
                            with Image.open(path) as image:
                                if image.size != SIZE:
                                    raise RuntimeError(f"{case}: smoke points require exact {SIZE}, got {image.size}")
                            run["screenshots"].append(path.name)
                            return path

                        def action(label, argv):
                            nonlocal step
                            command(["xdotool", *argv])
                            time.sleep(.16)
                            step += 1
                            return snapshot(label)

                        def click(index, control=False, shift=False):
                            keys = (["ctrl"] if control else []) + (["shift"] if shift else [])
                            command(["xdotool", "mousemove", "--window", window_id, "300", str(115 + index * 25)])
                            for key in keys:
                                command(["xdotool", "keydown", key])
                            try:
                                action(f"row-{index}", ["click", "1"])
                            finally:
                                for key in reversed(keys):
                                    command(["xdotool", "keyup", key])
                            oracle.click(index, control, shift)

                        if case in ("point-toggle", "additive-shrink"):
                            click(1)
                            click(5, control=True)
                            if case == "additive-shrink":
                                click(9, control=True, shift=True)
                                click(3, control=True, shift=True)
                        elif case == "shift-shrink":
                            click(5)
                            click(9, shift=True)
                            click(7, shift=True)
                        elif case.startswith("cursor-"):
                            click(1)
                            before_toggle = action("cursor-down", ["key", "ctrl+Down"])
                            oracle.cursor_down()
                            if case != "cursor-only":
                                action("toggle-on", ["key", "ctrl+space"])
                                oracle.toggle_cursor()
                                if case == "cursor-toggle-off":
                                    action("toggle-off", ["key", "ctrl+space"])
                                    oracle.toggle_cursor()
                            # Same cursor/metadata before and after toggle: only row mask should change.
                            if case != "cursor-only":
                                final_toggle = snapshot("inspector-compare")
                                with Image.open(before_toggle) as before, Image.open(final_toggle) as after:
                                    box = (775, 110, 1024, 684)
                                    if before.crop(box).tobytes() != after.crop(box).tobytes():
                                        raise AssertionError("Ctrl+Space changed the Inspector at the same cursor")
                                run["inspector_pixels_unchanged"] = True
                        elif case == "escape-route":
                            click(1)
                            click(5, control=True)
                            action("clear", ["key", "Escape"])
                            action("next-after-clear", ["key", "Down"])
                            oracle.selected = {6}
                            oracle.cursor = 6
                        elif case == "text-control-a":
                            click(1)
                            action("path-pointer", ["mousemove", "--window", window_id, "500", "51", "click", "1"])
                            action("text-all", ["key", "ctrl+a"])
                            action("replace-text", ["type", "--clearmodifiers", "--delay", "2", MARKER])
                        elif synthetic:
                            click(1)
                            action("last-row", ["key", "End"])
                            oracle.selected = {999999}
                            oracle.cursor = 999999
                            action("manual-wheel", ["click", "--repeat", "12", "--delay", "30", "4"])
                            time.sleep(.8)
                            snapshot("wheel-settled")
                            if case == "million-select-all":
                                action("all-without-reveal", ["key", "ctrl+a"])
                        elif case == "background-arrival":
                            action("path", ["key", "ctrl+l"])
                            action("path-select-all", ["key", "ctrl+a"])
                            action("arrival-target", ["type", "--clearmodifiers", "--delay", "0", str(arrival)])
                            command(["xdotool", "key", "--delay", "0", "Return"])
                            time.sleep(.02)
                            command(["xdotool", "key", "--delay", "0", "ctrl+l", "ctrl+a"])
                            command(["xdotool", "type", "--clearmodifiers", "--delay", "0", MARKER])
                            time.sleep(.05)
                            snapshot("arrival-focus-before-wait")
                            time.sleep(1.5)
                            run["arrival_precedes_completion_proven"] = False
                            run["arrival_note"] = "Inspect intermediate PNG for visible loading while marker is focused; exit-only proof does not establish ordering."
                        time.sleep(.3)
                        final = snapshot("final")
                        footer = out / f"{case}-footer.png"
                        with Image.open(final) as image:
                            image.crop((850, 684, 1024, 720)).save(footer)
                        run["footer_sha256"] = digest(footer)
                        event = protocol.event.ClientMessage(window=int(window_id),
                            client_type=connection.intern_atom("WM_PROTOCOLS"),
                            data=(32, [connection.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0]))
                        window.send_event(event, event_mask=0)
                        connection.flush()
                        app.wait(timeout=10)
                        if app.returncode != 0:
                            raise RuntimeError(f"{case}: app exit {app.returncode}")
                    finally:
                        if app.poll() is None:
                            app.terminate()
                            try:
                                app.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                app.kill()
                                app.wait(timeout=5)
                        if connection:
                            connection.close()
                payloads = [json.loads(line) for line in log_path.read_text().splitlines() if line.startswith("{")]
                if not payloads:
                    raise RuntimeError(f"{case}: missing structured exit evidence")
                evidence = payloads[-1]
                run.update(exit_code=app.returncode, evidence=evidence)
                expected_ranges = [[0, 1000000]] if case == "million-select-all" else runs(oracle.selected)
                expected_count = 1000000 if case == "million-select-all" else len(oracle.selected)
                expected_model = args.arrival_files if case == "background-arrival" else 1000000 if synthetic else 20
                expected_mode = "synthetic" if synthetic else "filesystem"
                assert evidence["mode"] == expected_mode and evidence["model_count"] == expected_model, evidence
                assert evidence["selection_ranges_complete"] and evidence["selection_ranges"] == expected_ranges, evidence
                assert evidence["selected_count"] == expected_count, evidence
                assert evidence["selection_range_count"] == len(expected_ranges), evidence
                assert evidence["selected_index"] == oracle.cursor, evidence
                cursor_selected = case == "million-select-all" or oracle.cursor in oracle.selected
                assert evidence["cursor_selected"] == cursor_selected, evidence
                assert evidence["inspector_open"], evidence
                assert evidence["rows_built"] < 40, evidence
                if case in ("text-control-a", "background-arrival"):
                    assert evidence["focus_role"] == "location", evidence
                    assert evidence["location_text_bytes"] == len(MARKER.encode()), evidence
                if synthetic:
                    assert evidence["visible_end"] < 1000000, "manual wheel did not separate cursor and scroll"
                assert csv_path.stat().st_size > 0
                run["oracle"] = {"selected_ranges": expected_ranges, "selected_count": expected_count,
                                 "cursor": oracle.cursor, "cursor_selected": cursor_selected}
                run["passed"] = True
                by_case[case] = run
            baseline, selected_all = by_case["million-scroll-baseline"]["evidence"], by_case["million-select-all"]["evidence"]
            assert (baseline["visible_start"], baseline["visible_end"]) == (selected_all["visible_start"], selected_all["visible_end"]), "Ctrl+A changed the independently scrolled viewport"
            assert by_case["cursor-only"]["footer_sha256"] != by_case["cursor-toggle-on"]["footer_sha256"], "visible footer did not change from one to two selected"
            assert by_case["cursor-only"]["footer_sha256"] == by_case["cursor-toggle-off"]["footer_sha256"], "footer did not return to one selected after toggle off"
            assert by_case["cursor-only"]["footer_sha256"] == by_case["text-control-a"]["footer_sha256"], "text Ctrl+A changed selected footer"
            assert digest(binary) == artifact_sha, "binary changed during the native run"
            report["passed"] = True
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}"
            raise
        finally:
            (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            if read_fd is not None:
                os.close(read_fd)
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
    print(json.dumps(report))


if __name__ == "__main__":
    main()
