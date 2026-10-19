#!/usr/bin/env python3
"""Owned-fixture native X11 find, paging and containing-folder regression; CPU only."""
import argparse
import json
import os
from pathlib import Path
import select
import subprocess
import time

from Xlib import X, display, protocol


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an existing executable")
    if not args.out.is_absolute() or out == Path("/") or out == Path.home():
        parser.error("output must be an explicit absolute new or empty directory")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error("output must be new or empty")
    out.mkdir(parents=True, exist_ok=True)
    runtime, files = out / "runtime", out / "files"
    runtime.mkdir(mode=0o700)
    files.mkdir()
    nested = files / "nested"
    nested.mkdir()
    for i in range(6123):
        (files / f"item-{i:05}.txt").write_bytes(b"owned fixture\n")
    (nested / "zz-late-needle.txt").write_bytes(b"expected match\n")
    (nested / "neighbor.txt").write_bytes(b"not a match\n")
    report = {"passed": False, "renderer": "forced CPU", "runs": [],
              "fixture": {"indexed_entries": 6126, "item_matches": 6123, "late_matches": 1},
              "limits": ["Native X11 under Xvfb only; no GPU, Wayland or other-OS qualification",
                         "Functional regression; no startup/input/display latency measurement",
                         "Paged search ergonomics are provisional, not verified complete feature coverage"]}
    read_fd, write_fd = os.pipe()
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1",
               MUI_RENDERER="cpu", XDG_RUNTIME_DIR=str(runtime))
    env.pop("WAYLAND_DISPLAY", None)
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

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True,
                                      check=True, timeout=10)

            for case in ("late", "paging", "reveal", "folder", "folder-main"):
                log_path = out / f"{case}.log"
                with log_path.open("w") as log:
                    app = subprocess.Popen([str(binary), str(files), "--size", "1024x720",
                                            "--profile", str(out / f"{case}.csv")],
                                           env=env, stdout=log, stderr=log, cwd=files)
                    connection = None
                    try:
                        deadline, window_id, colors = time.monotonic() + 20, None, 0
                        while time.monotonic() < deadline:
                            if app.poll() is not None:
                                raise RuntimeError(f"{case}: app exited before usable presentation")
                            found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "MegaMan"],
                                                   env=env, capture_output=True, text=True)
                            if found.returncode == 0:
                                window_id = found.stdout.splitlines()[0]
                                command(["import", "-window", window_id, str(out / f"{case}-ready.png")])
                                colors = int(command(["identify", "-format", "%k", str(out / f"{case}-ready.png")]).stdout)
                                if colors > 20:
                                    break
                            time.sleep(.1)
                        if colors <= 20:
                            raise RuntimeError(f"{case}: blank presentation")
                        connection = display.Display(env["DISPLAY"])
                        window = connection.create_resource_object("window", int(window_id))
                        command(["xdotool", "windowfocus", window_id])
                        if case == "folder-main":
                            command(["xdotool", "mousemove", "--window", window_id, "250", "115", "click", "1"])
                        else:
                            command(["xdotool", "key", "ctrl+shift+f"])
                            time.sleep(.25)
                            query = "item-" if case == "paging" else "nested" if case == "folder" else "LATE-NEEDLE"
                            command(["xdotool", "type", "--clearmodifiers", "--delay", "2", query])
                        time.sleep(1.2)
                        command(["import", "-window", window_id, str(out / f"{case}-search.png")])
                        if case == "paging":
                            # Fixed smoke-window footer point; module tests derive control geometry.
                            command(["xdotool", "mousemove", "--window", window_id, "1004", "704", "click", "1"])
                            time.sleep(.7)
                        elif case in ("reveal", "folder"):
                            command(["xdotool", "key", "Return"])
                            time.sleep(.15)
                            command(["xdotool", "key", "Down"])
                            time.sleep(.15)
                            if case == "reveal":
                                command(["xdotool", "key", "Return"])
                                time.sleep(.7)
                        command(["xdotool", "windowsize", "--sync", window_id, "640", "480"])
                        time.sleep(.3)
                        command(["import", "-window", window_id, str(out / f"{case}-final.png")])
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
                            app.wait(timeout=5)
                        if connection:
                            connection.close()
                evidence = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith("{")))
                run = {"case": case, "exit_code": app.returncode, "evidence": evidence}
                report["runs"].append(run)
                if case in ("reveal", "folder-main"):
                    assert evidence["mode"] == "filesystem", evidence
                    expected_count, expected_selection = (2, 1) if case == "reveal" else (6124, 0)
                    assert evidence["model_count"] == expected_count and evidence["selected_index"] == expected_selection, evidence
                else:
                    assert evidence["mode"] == "find-anywhere" and evidence["coverage"] == "Complete", evidence
                    assert evidence["indexed_entries"] == 6126, evidence
                    if case in ("late", "folder"):
                        assert evidence["total_matches"] == 1 and evidence["page_rows"] == 1, evidence
                    else:
                        assert evidence["total_matches"] == 6123, evidence
                        assert evidence["page_offset"] == 512 and evidence["page_rows"] == 512, evidence
                    assert evidence["rows_built"] < 40, evidence
                assert (out / f"{case}.csv").stat().st_size > 0
                run["passed"] = True
            report["passed"] = True
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}"
            raise
        finally:
            (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            server.terminate()
            server.wait(timeout=5)
    print(json.dumps(report))


if __name__ == "__main__":
    main()
