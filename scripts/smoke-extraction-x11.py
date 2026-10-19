#!/usr/bin/env python3
"""Owned-fixture ZIP extraction through actual X11 input; CPU presentation only."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import select
import stat
import struct
import subprocess
import time
import zipfile

from Xlib import X, display, protocol


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


PAYLOADS = {
    "folder/child.txt": b"owned child payload\n",
    "folder/deeper/nested.bin": bytes(range(256)) * 3,
    "folder/root.txt": b"different record with the same basename\n",
    "root.txt": b"owned root record\n",
}
CASES = ("whole", "selected-record", "current-folder", "retained-lease",
         "destination-conflict", "unsafe-name", "cancel-prepared", "source-changed",
         "native-parent")


def zip_bytes(payloads):
    stream = io.BytesIO()
    with zipfile.ZipFile(stream, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in payloads.items():
            info = zipfile.ZipInfo(name, (2026, 10, 19, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = (stat.S_IFREG | 0o644) << 16
            archive.writestr(info, data)
    return stream.getvalue()


def tree(root):
    """Exact lexical tree, including staging/symlinks; never follow links."""
    result = {}
    if not root.exists():
        return result
    for directory, directories, files in os.walk(root, followlinks=False):
        for name in directories + files:
            path = Path(directory) / name
            relative = path.relative_to(root).as_posix()
            mode = path.lstat().st_mode
            if stat.S_ISLNK(mode):
                result[relative] = {"kind": "symlink", "target": os.readlink(path)}
            elif stat.S_ISDIR(mode):
                result[relative] = {"kind": "directory"}
            elif stat.S_ISREG(mode):
                result[relative] = {"kind": "file", "sha256": digest(path)}
            else:
                result[relative] = {"kind": "unexpected", "mode": mode}
    return result


def expected_tree(payloads):
    result = {}
    for name, data in payloads.items():
        for parent in Path(name).parents:
            if parent != Path("."):
                result[parent.as_posix()] = {"kind": "directory"}
        result[name] = {"kind": "file", "sha256": hashlib.sha256(data).hexdigest()}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--case", choices=CASES, action="append", dest="cases")
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be an existing executable")
    if not args.out.is_absolute() or out == Path("/") or out == Path.home():
        parser.error("output must be an explicit absolute new or empty directory")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error("output must be new or empty")
    out.mkdir(parents=True, exist_ok=True)
    runtime = out / "runtime"
    runtime.mkdir(mode=0o700)
    report = {"schema_version": 1, "passed": False, "artifact_sha256": digest(binary),
              "runs": [], "renderer": "forced CPU", "display": "owned Xvfb -noreset X11, scale1",
              "limits": ["Native owned-fixture functional proof only; no latency, GPU, Wayland or other OS qualification",
                         "ZIP extraction to a new folder; no overwrite, password or general DnD qualification",
                         "Final rendered outcomes require capture review in addition to counters and filesystem oracles"]}
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1", MUI_RENDERER="cpu", XDG_RUNTIME_DIR=str(runtime))
    env.pop("WAYLAND_DISPLAY", None)
    read_fd, write_fd = os.pipe()
    with (out / "xvfb.log").open("w") as server_log:
        server = subprocess.Popen(["Xvfb", "-noreset", "-displayfd", str(write_fd), "-screen", "0",
                                   "1600x1200x24", "-nolisten", "tcp"], pass_fds=(write_fd,),
                                  stdout=server_log, stderr=server_log)
        os.close(write_fd)
        try:
            assert select.select([read_fd], [], [], 10)[0], "Xvfb did not allocate a display"
            env["DISPLAY"] = ":" + os.read(read_fd, 64).decode().strip()
            os.close(read_fd)
            read_fd = None

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True, check=True, timeout=10)

            for case in args.cases or CASES:
                root = out / case
                root.mkdir()
                host = root / (os.fsdecode(b"host-\xff") if case == "native-parent" else "host")
                host.mkdir()
                archive = host / "sample.zip"
                archive.write_bytes(zip_bytes({"../escape.txt": b"must not escape\n", **PAYLOADS}
                                              if case == "unsafe-name" else PAYLOADS))
                source_before = digest(archive)
                with zipfile.ZipFile(archive) as reference:
                    assert reference.testzip() is None
                    assert {info.filename: reference.read(info) for info in reference.infolist()} == (
                        {"../escape.txt": b"must not escape\n", **PAYLOADS} if case == "unsafe-name" else PAYLOADS)
                destination = host / "Extracted ZIP"
                if case == "destination-conflict":
                    destination.mkdir()
                    (destination / "keep.txt").write_bytes(b"existing destination must survive\n")
                original_destination = tree(destination)
                sentinel = root / "outside-sentinel"
                sentinel.write_bytes(b"outside owned sentinel unchanged\n")
                sentinel_hash = digest(sentinel)
                run = {"case": case, "passed": False, "source_before_sha256": source_before,
                       "native_host_hex": os.fsencode(host).hex(), "screenshots": []}
                report["runs"].append(run)
                app, connection, window_id = None, None, None
                log_path = root / "app.log"
                with log_path.open("w") as log:
                    app = subprocess.Popen([str(binary), str(host), "--size", "1024x720", "--profile", str(root / "profile.csv")],
                                           env=env, cwd=host, stdout=log, stderr=log)
                    try:
                        deadline = time.monotonic() + 20
                        while time.monotonic() < deadline:
                            assert app.poll() is None, f"{case}: app exited before presentation"
                            found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "MegaMan"],
                                                   env=env, capture_output=True, text=True)
                            if found.returncode == 0:
                                window_id = found.stdout.splitlines()[0]
                                command(["import", "-window", window_id, str(root / "ready.png")])
                                if int(command(["identify", "-format", "%k", str(root / "ready.png")]).stdout) > 20:
                                    break
                            time.sleep(.1)
                        assert window_id, f"{case}: no native window"
                        connection = display.Display(env["DISPLAY"])
                        window = connection.create_resource_object("window", int(window_id))

                        def capture(name):
                            path = root / name
                            command(["import", "-window", window_id, str(path)])
                            if name not in run["screenshots"]:
                                run["screenshots"].append(name)
                            return digest(path)

                        def settle(name, minimum=.45):
                            # Require two identical rendered frames after a minimum input interval.
                            deadline, start, previous = time.monotonic() + 5, time.monotonic(), None
                            while time.monotonic() < deadline:
                                current = capture(name)
                                if current == previous and time.monotonic() - start >= minimum:
                                    return
                                previous = current
                                time.sleep(.12)
                            raise RuntimeError(f"{case}: presentation did not settle for {name}")

                        def clear_hover(width=1024, height=720):
                            command(["xdotool", "mousemove", "--window", window_id,
                                     str(width // 2), str(height // 2)])

                        def click(x, y):
                            command(["xdotool", "mousemove", "--window", window_id, str(x), str(y), "click", "1"])

                        command(["xdotool", "windowfocus", window_id])
                        time.sleep(.4)
                        command(["xdotool", "key", "ctrl+f"])
                        command(["xdotool", "type", "--clearmodifiers", "--delay", "2", "sample.zip"])
                        settle("host-filtered.png")
                        click(250, 112)
                        command(["xdotool", "key", "Home", "Return"])
                        settle("archive.png", .7)
                        if case == "selected-record":
                            click(250, 229)
                            settle("selected.png")
                            click(60, 113)
                        elif case == "current-folder":
                            click(250, 204)
                            command(["xdotool", "key", "Return"])
                            settle("archive-folder.png")
                            click(160, 113)
                        else:
                            click(260, 113)
                        clear_hover()
                        settle("prepared.png", .8)
                        if case == "whole":
                            for width, height in ((640, 480), (320, 240), (1024, 720)):
                                command(["xdotool", "windowsize", "--sync", window_id, str(width), str(height)])
                                clear_hover(width, height)
                                settle(f"prepared-{width}x{height}.png")
                        if case == "retained-lease":
                            command(["xdotool", "key", "Escape"])
                            settle("hidden-archive.png")
                            command(["xdotool", "key", "Escape"])
                            settle("hidden-host.png")
                            click(950, 15)
                            settle("reopened.png")
                        if case == "source-changed":
                            archive.write_bytes(zip_bytes({"replacement.txt": b"intentional source mutation\n"}))
                        if case == "cancel-prepared":
                            click(980, 704)
                        elif case != "unsafe-name":
                            click(80, 704)
                        clear_hover()
                        if case not in ("destination-conflict", "unsafe-name", "cancel-prepared", "source-changed"):
                            expected = ({"root.txt": PAYLOADS["root.txt"]} if case == "selected-record" else
                                        {n: v for n, v in PAYLOADS.items() if n.startswith("folder/")} if case == "current-folder" else PAYLOADS)
                            expected = expected_tree(expected)
                            deadline = time.monotonic() + 12
                            while tree(destination) != expected and time.monotonic() < deadline:
                                assert app.poll() is None, f"{case}: exited while extracting"
                                time.sleep(.05)
                            assert tree(destination) == expected, (case, "output mismatch", tree(destination), expected)
                        settle("final.png", .9)
                        if case == "whole":
                            for width, height in ((640, 480), (320, 240)):
                                command(["xdotool", "windowsize", "--sync", window_id, str(width), str(height)])
                                clear_hover(width, height)
                                settle(f"final-{width}x{height}.png")
                        window.send_event(protocol.event.ClientMessage(window=int(window_id),
                            client_type=connection.intern_atom("WM_PROTOCOLS"),
                            data=(32, [connection.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0])), event_mask=0)
                        connection.flush()
                        app.wait(timeout=10)
                        assert app.returncode == 0, f"{case}: app exit {app.returncode}"
                    finally:
                        if app.poll() is None:
                            app.terminate()
                            app.wait(timeout=5)
                        if connection:
                            connection.close()
                run["exit_code"] = app.returncode
                evidence = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith("{")))
                run["evidence"] = evidence
                assert evidence["mode"] == "extraction" and evidence["state"] == "Some(Terminal)", evidence
                assert evidence["visible"] and evidence["busy"] and not evidence["destination_edited"], evidence
                if case == "cancel-prepared":
                    assert evidence["cancel_requested"], evidence
                if case in ("destination-conflict", "unsafe-name", "cancel-prepared", "source-changed"):
                    assert tree(destination) == original_destination, (case, "failed request changed destination", tree(destination))
                    if case != "destination-conflict":
                        assert not destination.exists(), (case, "failure left a destination")
                assert set(host.iterdir()) == ({archive, destination} if destination.exists() else {archive}), (case, "host staging or escape residue", list(host.iterdir()))
                assert not list(root.rglob("escape.txt")), (case, "unsafe record escaped")
                assert digest(sentinel) == sentinel_hash
                if case == "source-changed":
                    assert digest(archive) != source_before, "intentional source mutation did not apply"
                else:
                    assert digest(archive) == source_before, f"{case}: source ZIP changed"
                assert (root / "profile.csv").stat().st_size > 0, f"{case}: no profile evidence"
                run["source_after_sha256"] = digest(archive)
                run["destination_tree"] = tree(destination)
                run["screenshots"] = [{"path": name, "sha256": digest(root / name),
                                        "dimensions": struct.unpack(">II", (root / name).read_bytes()[16:24])}
                                       for name in run["screenshots"]]
                run["passed"] = True
                (out / "result.json").write_text(json.dumps(report, indent=2) + "\n")
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
    print(json.dumps({"passed": report["passed"], "cases": len(report["runs"]), "out": str(out)}))


if __name__ == "__main__":
    main()
