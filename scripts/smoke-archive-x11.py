#!/usr/bin/env python3
"""Read-only ZIP browsing regression through real X11 input, CPU presentation only."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time
import zipfile

from Xlib import X, display, protocol


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an executable")
    if not args.out.is_absolute() or out == Path("/") or out == Path.home():
        parser.error("output must be an explicit absolute new or empty directory")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error("output must be new or empty")
    out.mkdir(parents=True, exist_ok=True)
    files, runtime = out / "files", out / "runtime"
    files.mkdir()
    runtime.mkdir(mode=0o700)
    archive = files / "basic.zip"
    with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED) as writer:
        for i in range(700):
            writer.writestr(f"file-{i:04}.txt", f"owned payload {i}\n")
        writer.writestr("empty/", b"")
        writer.writestr("nested/deeper/readme.txt", b"owned nested payload\n")
    (files / "invalid.zip").write_bytes(b"not a ZIP fixture\n")
    (files / "ordinary.txt").write_bytes(b"host folder remains unchanged\n")
    original_hash = digest(archive)
    report = {"passed": False, "runs": [], "artifact_sha256": digest(binary),
              "fixture": {"records": 702, "root_children": 702, "host_children": 3,
                          "original_archive_sha256": original_hash},
              "limits": ["Read-only archive metadata UI, not extraction/create/DnD qualification",
                         "Native X11 under Xvfb at1x forced CPU only; no latency or performance comparison"]}
    read_fd, write_fd = os.pipe()
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1", MUI_RENDERER="cpu",
               XDG_RUNTIME_DIR=str(runtime))
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

            for case in ("root", "paging", "nested", "close", "invalid", "changed"):
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
                                raise RuntimeError(f"{case}: exited before presentation")
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
                        # Root file list is fixed for this fixture; independent module tests use resolved geometry.
                        if case == "invalid":
                            command(["xdotool", "mousemove", "--window", window_id, "250", "115", "click", "1"])
                            command(["xdotool", "key", "Home", "Down", "Return"])
                        else:
                            command(["xdotool", "mousemove", "--window", window_id, "250", "115", "click", "--repeat", "2", "--delay", "100", "1"])
                        time.sleep(.9)
                        command(["import", "-window", window_id, str(out / f"{case}-opened.png")])
                        if case in ("paging", "changed"):
                            if case == "changed":
                                replacement = out / "replacement.zip"
                                with zipfile.ZipFile(replacement, "w") as writer:
                                    writer.writestr("replacement.txt", b"new source identity")
                                replacement.replace(archive)
                            command(["xdotool", "mousemove", "--window", window_id, "1004", "704", "click", "1"])
                            time.sleep(.7)
                        elif case == "nested":
                            command(["xdotool", "key", "Home", "Down", "Return"])
                            time.sleep(.4)
                            command(["xdotool", "key", "Home", "Return"])
                            time.sleep(.4)
                        elif case == "close":
                            command(["xdotool", "key", "Escape"])
                            time.sleep(.3)
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
                            raise RuntimeError(f"{case}: exit {app.returncode}")
                    finally:
                        if app.poll() is None:
                            app.terminate()
                            app.wait(timeout=5)
                        if connection:
                            connection.close()
                evidence = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith("{")))
                run = {"case": case, "exit_code": app.returncode, "evidence": evidence}
                report["runs"].append(run)
                if case == "close":
                    assert evidence["mode"] == "filesystem" and evidence["model_count"] == 3, evidence
                    assert evidence["selected_index"] == 0, evidence
                else:
                    assert evidence["mode"] == "archive" and evidence["read_only"], evidence
                    if case in ("invalid", "changed"):
                        assert evidence["catalog_state"] == "Failed" and not evidence["source_valid"], evidence
                        if case == "changed":
                            assert evidence["source_changed"], evidence
                    else:
                        assert evidence["catalog_state"] == "Complete" and evidence["source_valid"], evidence
                        assert evidence["records_seen"] == 702, evidence
                        if case == "root":
                            assert evidence["prefix_depth"] == 0 and evidence["total_children"] == 702, evidence
                        elif case == "paging":
                            assert evidence["page_offset"] == 512 and evidence["page_rows"] == 190, evidence
                        elif case == "nested":
                            assert evidence["prefix_depth"] == 2 and evidence["total_children"] == 1, evidence
                    assert evidence["rows_built"] < 40, evidence
                assert (out / f"{case}.csv").stat().st_size > 0
                if case != "changed":
                    assert digest(archive) == original_hash, "read-only browsing changed source ZIP"
                assert not (files / "nested").exists(), "archive member materialized during metadata browsing"
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
