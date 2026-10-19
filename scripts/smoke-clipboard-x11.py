#!/usr/bin/env python3
"""Native Ctrl+C and toolbar file-copy proof with an independent X11 consumer."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import time

from Xlib import X, Xatom, display, protocol


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be executable")
    if not args.out.is_absolute() or out == Path("/") or out == Path.home():
        parser.error("output must be an explicit absolute new or empty directory")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error("output must be new or empty")
    out.mkdir(parents=True, exist_ok=True)
    files, other, big, runtime = (out / n for n in ("files", "other", "big", "runtime"))
    for path in (files, other, big, runtime):
        path.mkdir(mode=0o700)
    for i in range(12):
        (files / f"item-{i:02}.txt").write_bytes(b"owned file clipboard fixture\n")
    (files / "folder").mkdir()
    (files / "link").symlink_to("item-00.txt")
    for name in (b"native-\xff", b"line\nbreak", b"percent%# space", "Straße.txt".encode()):
        path = os.fsencode(files) + b"/" + name
        with open(path, "wb") as stream:
            stream.write(b"owned special filename\n")
    for i in range(4097):
        (big / f"item-{i:04}.txt").write_bytes(b"owned limit fixture\n")
    native_paths = {os.fsencode(entry.path) for entry in os.scandir(files)}
    source_hashes = {os.fsencode(p).hex(): digest(Path(p)) for p in files.iterdir() if p.is_file() and not p.is_symlink()}
    peer = Path(__file__).resolve().parents[1] / "crates/browser-native/tests/x11_peer.py"
    assert peer.is_file(), "independent Python-Xlib clipboard peer is missing"
    report = {"schema_version": 1, "passed": False, "artifact_sha256": digest(binary),
              "runs": [], "fixture": {"native_items": len(native_paths), "limit_items": 4097},
              "limits": ["X11/Xvfb forced CPU scale1 functional proof only; no latency, Wayland or other-OS qualification",
                         "Native clipboard ownership/bytes only; no filesystem paste, move or DnD qualification"]}
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1", MUI_RENDERER="cpu", XDG_RUNTIME_DIR=str(runtime))
    env.pop("WAYLAND_DISPLAY", None)
    read_fd, write_fd = os.pipe()
    with (out / "xvfb.log").open("w") as server_log:
        server = subprocess.Popen(["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1600x1200x24", "-noreset", "-nolisten", "tcp"],
                                  pass_fds=(write_fd,), stdout=server_log, stderr=server_log)
        os.close(write_fd)
        try:
            assert select.select([read_fd], [], [], 10)[0], "Xvfb startup timed out"
            env["DISPLAY"] = ":" + os.read(read_fd, 64).decode().strip()
            os.close(read_fd)
            read_fd = None

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True, check=True, timeout=10)

            for case in ("copy-all", "copy-selected", "copy-then-navigate", "text-copy", "over-limit"):
                # Each case has a fresh X11 service and an independently observed owner.
                connection = display.Display(env["DISPLAY"])
                clipboard = connection.intern_atom("CLIPBOARD")
                sentinel = None
                target = big if case == "over-limit" else files
                if case == "over-limit":
                    sentinel_path = out / "sentinel-does-not-exist"
                    sentinel = subprocess.Popen(["python3", str(peer), "own", env["DISPLAY"], "gnome-copy", os.fsencode(sentinel_path).hex()],
                                                env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                    assert select.select([sentinel.stdout], [], [], 5)[0]
                    assert sentinel.stdout.readline().startswith("READY ")
                initial_owner = connection.get_selection_owner(clipboard)
                initial_owner = initial_owner.id if initial_owner else 0
                log_path = out / f"{case}.log"
                app = None
                try:
                    with log_path.open("w") as log:
                        app = subprocess.Popen([str(binary), str(target), "--size", "1024x720", "--profile", str(out / f"{case}.csv")],
                                               env=env, cwd=files, stdout=log, stderr=log)
                        deadline = time.monotonic() + 20
                        window_id = None
                        while time.monotonic() < deadline:
                            assert app.poll() is None, f"{case}: early exit"
                            found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "MegaMan"], env=env, capture_output=True, text=True)
                            if found.returncode == 0:
                                window_id = found.stdout.splitlines()[0]
                                command(["import", "-window", window_id, str(out / f"{case}-ready.png")])
                                if int(command(["identify", "-format", "%k", str(out / f"{case}-ready.png")]).stdout) > 20:
                                    break
                            time.sleep(.1)
                        assert window_id, "window presentation timed out"
                        command(["xdotool", "windowfocus", window_id])
                        time.sleep(.6)
                        if case in ("copy-selected", "text-copy"):
                            command(["xdotool", "mousemove", "--window", window_id, "250", "136", "click", "1"])
                            if case == "copy-selected":
                                command(["xdotool", "keydown", "ctrl", "mousemove", "--window", window_id, "250", "186", "click", "1", "keyup", "ctrl"])
                        else:
                            command(["xdotool", "key", "ctrl+a"])
                        time.sleep(.15)
                        if case == "text-copy":
                            command(["xdotool", "key", "ctrl+l", "ctrl+a", "ctrl+c"])
                        elif case == "copy-selected":
                            # The intrinsically sized Copy button is immediately before Search/Inspector.
                            command(["xdotool", "mousemove", "--window", window_id, "925", "51", "click", "1"])
                        else:
                            command(["xdotool", "key", "ctrl+c"])
                        deadline = time.monotonic() + 6
                        if case != "over-limit":
                            while time.monotonic() < deadline:
                                owner = connection.get_selection_owner(clipboard)
                                if owner and owner.id != initial_owner:
                                    break
                                time.sleep(.03)
                            else:
                                raise RuntimeError(f"{case}: native clipboard ownership was not published")
                        else:
                            time.sleep(.8)
                            assert connection.get_selection_owner(clipboard).id == initial_owner, "over-limit selection replaced old clipboard"
                        if case == "copy-then-navigate":
                            command(["xdotool", "key", "ctrl+l", "ctrl+a"])
                            command(["xdotool", "type", "--clearmodifiers", "--delay", "1", str(other)])
                            command(["xdotool", "key", "Return"])
                            time.sleep(.5)
                        if case == "text-copy":
                            window = connection.screen().root.create_window(0, 0, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent)
                            prop, utf8 = connection.intern_atom("_UI_COPY_TEXT"), connection.intern_atom("UTF8_STRING")
                            window.convert_selection(clipboard, utf8, prop, X.CurrentTime)
                            connection.flush()
                            deadline = time.monotonic() + 5
                            while time.monotonic() < deadline:
                                if connection.pending_events():
                                    event = connection.next_event()
                                    if event.type == X.SelectionNotify:
                                        assert event.property == prop, "text clipboard conversion refused"
                                        text = window.get_full_property(prop, utf8)
                                        assert bytes(text.value) == os.fsencode(files), "Ctrl+C did not copy selected editor text"
                                        break
                                select.select([connection.fileno()], [], [], .02)
                            else:
                                raise RuntimeError("text clipboard response timed out")
                            window.destroy()
                            transferred = []
                        else:
                            received = command(["python3", str(peer), "read", env["DISPLAY"]]).stdout
                            (out / f"{case}-peer.txt").write_text(received)
                            transferred = [bytes.fromhex(line[5:]) for line in received.splitlines() if line.startswith("PATH ")]
                            expected = ({os.fsencode(files / "item-00.txt"), os.fsencode(files / "item-02.txt")} if case == "copy-selected"
                                        else {os.fsencode(sentinel_path)} if case == "over-limit" else native_paths)
                            assert len(transferred) == len(expected) and set(transferred) == expected, (case, transferred, expected)
                            assert "KDE 30" in received, "file clipboard intent must be Copy"
                        time.sleep(.3)
                        command(["import", "-window", window_id, str(out / f"{case}-final.png")])
                        window = connection.create_resource_object("window", int(window_id))
                        window.send_event(protocol.event.ClientMessage(window=int(window_id), client_type=connection.intern_atom("WM_PROTOCOLS"),
                            data=(32, [connection.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0])), event_mask=0)
                        connection.flush()
                        app.wait(timeout=10)
                        assert app.returncode == 0, f"{case}: exit {app.returncode}"
                    evidence = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith("{")))
                    native = evidence["file_clipboard"]
                    if case in ("over-limit", "text-copy"):
                        assert native["copied_items"] == 0, evidence
                        if case == "text-copy":
                            assert evidence["selected_count"] == 1 and evidence["focus_role"] == "location", evidence
                        else:
                            assert native["state"] == "failed" and evidence["selected_count"] == 4097, evidence
                    else:
                        assert native["copied_items"] == len(transferred) and native["owns_clipboard"], evidence
                        if case == "copy-then-navigate":
                            assert evidence["model_count"] == 0 and evidence["selected_count"] == 0, evidence
                    assert (out / f"{case}.csv").stat().st_size > 0
                    report["runs"].append({"case": case, "passed": True, "exit_code": app.returncode, "evidence": evidence,
                                           "transferred_native_path_hex": [p.hex() for p in transferred]})
                finally:
                    if app is not None and app.poll() is None:
                        app.terminate()
                        app.wait(timeout=5)
                    connection.close()
                    if sentinel is not None:
                        sentinel.communicate("quit\n", timeout=5)
                        assert sentinel.returncode == 0
            for path_hex, expected in source_hashes.items():
                assert digest(Path(os.fsdecode(bytes.fromhex(path_hex)))) == expected, "copying paths changed a source"
            assert digest(binary) == report["artifact_sha256"], "binary changed during native run"
            report["passed"] = True
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}"
            raise
        finally:
            (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
            if read_fd is not None:
                os.close(read_fd)
            server.terminate()
            server.wait(timeout=5)
    print(json.dumps(report))


if __name__ == "__main__":
    main()
