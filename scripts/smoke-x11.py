#!/usr/bin/env python3
"""Native CPU-presented pointer/keyboard/filter/resize/close regression on Xvfb.

Requires Xvfb, xdotool, ImageMagick and python-xlib. Never measures GPU/display latency.
"""
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
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if out == Path("/") or out == Path.home():
        parser.error("output must be under the dev drive")
    if out.exists() and any(out.iterdir()):
        parser.error("output must be new or empty")
    out.mkdir(parents=True, exist_ok=True)
    runtime = out / "runtime"
    runtime.mkdir(mode=0o700)
    files = out / "files"
    files.mkdir()
    (files / "Alpha").mkdir()
    (files / "needle-only.txt").write_text("native test fixture\n")
    (files / "other.txt").write_text("not a match\n")
    read_fd, write_fd = os.pipe()
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1",
               XDG_RUNTIME_DIR=str(runtime), MUI_RENDERER="cpu")
    env.pop("WAYLAND_DISPLAY", None)
    results = []
    with (out / "xvfb.log").open("w") as server_log:
        server = subprocess.Popen(["Xvfb", "-displayfd", str(write_fd), "-screen", "0",
                                   "1600x1200x24", "-nolisten", "tcp"],
                                  pass_fds=(write_fd,), stdout=server_log, stderr=server_log)
        os.close(write_fd)
        try:
            if not select.select([read_fd], [], [], 10)[0]:
                raise RuntimeError("Xvfb did not allocate a display")
            env["DISPLAY"] = ":" + os.read(read_fd, 64).decode().strip()
            os.close(read_fd)

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True,
                                      check=True, timeout=10)

            for case, fixture in (("fixture", True), ("live", False)):
                log_path = out / f"{case}.log"
                with log_path.open("w") as log:
                    argv = [str(binary), "--size", "1024x720", "--profile", str(out / f"{case}.csv")]
                    argv += ["--fixture", "1000000"] if fixture else [str(files)]
                    app = subprocess.Popen(argv, env=env, stdout=log, stderr=log)
                    connection = None
                    try:
                        deadline, window_id, colors = time.monotonic() + 20, None, 0
                        while time.monotonic() < deadline:
                            if app.poll() is not None:
                                raise RuntimeError(f"{case} exited before presentation")
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
                        if fixture:
                            # Known first-row point in the fixed 1024x720 smoke fixture.
                            # Unit tests independently derive the point from resolved geometry.
                            command(["xdotool", "mousemove", "--window", window_id, "250", "115", "click", "1"])
                            time.sleep(.2)
                            command(["xdotool", "key", "End"])
                        else:
                            command(["xdotool", "key", "ctrl+f"])
                            command(["xdotool", "type", "--clearmodifiers", "--delay", "2", "needle-only"])
                        time.sleep(.5)
                        command(["import", "-window", window_id, str(out / f"{case}-action.png")])
                        command(["xdotool", "windowsize", "--sync", window_id, "640", "480"])
                        time.sleep(.3)
                        command(["import", "-window", window_id, str(out / f"{case}-resized.png")])
                        event = protocol.event.ClientMessage(window=int(window_id),
                            client_type=connection.intern_atom("WM_PROTOCOLS"),
                            data=(32, [connection.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0]))
                        window.send_event(event, event_mask=0)
                        connection.flush()
                        app.wait(timeout=10)
                        if app.returncode != 0:
                            raise RuntimeError(f"{case}: exit {app.returncode}")
                    finally:
                        if app.poll() is None:
                            app.terminate()
                            app.wait(timeout=5)
                        if connection:
                            connection.close()
                payload = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith("{")))
                if fixture:
                    assert payload["selected_index"] == 999999, payload
                    assert payload["visible_end"] == 1000000 and payload["rows_built"] < 40, payload
                else:
                    assert payload["mode"] == "filesystem" and payload["model_count"] == 1, payload
                assert (out / f"{case}.csv").stat().st_size > 0
                results.append({"case": case, "exit_code": app.returncode, "evidence": payload})
            report = {"passed": True, "renderer": "forced CPU", "runs": results,
                      "limits": ["Xvfb native X11 only; no hardware GPU, Wayland or other-OS qualification",
                                 "No native IME/screenreader or display-latency measurement"]}
            (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            print(json.dumps(report))
        finally:
            server.terminate()
            server.wait(timeout=5)


if __name__ == "__main__":
    main()
