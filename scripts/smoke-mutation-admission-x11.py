#!/usr/bin/env python3
"""Cross-operation admission and reachable native Read cancellation on owned X11."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import struct
import subprocess
import time
import zipfile

from Xlib import X, display, protocol


CASES = ("extraction-blocks-paste", "extraction-refresh", "copy-blocks-extraction", "cancel-read-archive", "cancel-read-find")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def host_tree(root):
    result = {}
    for folder, directories, files in os.walk(root, followlinks=False):
        for name in directories + files:
            path = Path(folder) / name
            assert not path.is_symlink(), ("unexpected symlink", path)
            result[path.relative_to(root).as_posix()] = "directory" if path.is_dir() else digest(path)
    return result


class Peer:
    """Drain binary stdout explicitly: TextIO buffering must not defeat select()."""
    def __init__(self, peer, env, scenario, source, root):
        self.log = (root / "peer.log").open("wb")
        self.err = (root / "peer.stderr").open("wb")
        self.process = subprocess.Popen(["python3", str(peer), "own", env["DISPLAY"], scenario,
                                         os.fsencode(source).hex()], env=env, stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.err, bufsize=0)
        self.buffer = b""
        self.lines = []
        self.wait("READY ")

    def pump(self):
        while select.select([self.process.stdout], [], [], 0)[0]:
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                break
            self.log.write(chunk)
            self.log.flush()
            self.buffer += chunk
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                self.lines.append(line.decode("utf-8", "replace"))

    def wait(self, prefix, timeout=6):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            if any(line.startswith(prefix) for line in self.lines):
                return
            assert self.process.poll() is None, "clipboard peer exited unexpectedly"
            time.sleep(.01)
        raise AssertionError("clipboard peer did not produce " + prefix)

    def requests(self):
        self.pump()
        return sum(line.startswith("REQUEST ") for line in self.lines)

    def release(self):
        self.process.stdin.write(b"normal\n")
        self.process.stdin.flush()
        self.wait("NORMAL")

    def close(self):
        try:
            self.pump()
            if self.process.poll() is None:
                self.process.stdin.write(b"quit\n")
                self.process.stdin.flush()
                self.process.wait(timeout=5)
            self.pump()
        finally:
            if self.process.poll() is None:
                self.process.terminate()
                self.process.wait(timeout=5)
            self.log.close()
            self.err.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--case", choices=CASES, action="append", dest="cases")
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error("binary must be executable")
    if not args.out.is_absolute() or out == Path("/") or out == Path.home():
        parser.error("output must be an explicit absolute new or empty directory")
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error("output must be new or empty")
    out.mkdir(parents=True, exist_ok=True)
    runtime = out / "runtime"
    runtime.mkdir(mode=0o700)
    peer_path = Path(__file__).resolve().parents[1] / "crates/browser-native/tests/x11_peer.py"
    assert peer_path.is_file()
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND="x11", WINIT_X11_SCALE_FACTOR="1", MUI_RENDERER="cpu", XDG_RUNTIME_DIR=str(runtime))
    env.pop("WAYLAND_DISPLAY", None)
    report = {"schema_version": 1, "passed": False, "artifact_sha256": digest(binary), "runs": [],
              "limits": ["Owned Xvfb -noreset/X11 scale1 forced CPU functional fixtures only",
                         "No GPU/Wayland/other OS, latency or accessibility qualification",
                         "Admission through explicit acknowledgement and delayed native Read cancellation; no running-copy cancellation/fault claim"]}
    read_fd, write_fd = os.pipe()
    with (out / "xvfb.log").open("w") as server_log:
        server = subprocess.Popen(["Xvfb", "-noreset", "-displayfd", str(write_fd), "-screen", "0",
                                   "1600x1200x24", "-nolisten", "tcp"], pass_fds=(write_fd,), stdout=server_log, stderr=server_log)
        os.close(write_fd)
        try:
            assert select.select([read_fd], [], [], 10)[0], "Xvfb startup timeout"
            env["DISPLAY"] = ":" + os.read(read_fd, 64).decode().strip()
            os.close(read_fd)
            read_fd = None

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True, check=True, timeout=10)

            for case in args.cases or CASES:
                root = out / case
                root.mkdir()
                host = root / "host"
                host.mkdir()
                source = root / "clipboard-item.txt"
                source.write_bytes(b"independent native clipboard payload\n")
                source_hash = digest(source)
                archive = host / "sample.zip"
                zip_payload = b"independent verified ZIP record\n"
                with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as fixture:
                    fixture.writestr("verified.txt", zip_payload)
                with zipfile.ZipFile(archive) as fixture:
                    assert fixture.read("verified.txt") == zip_payload and fixture.testzip() is None
                archive_hash = digest(archive)
                original = {"sample.zip": archive_hash}
                extraction = host / "Extracted ZIP"
                copied = host / source.name
                expected = {**original, source.name: source_hash, "Extracted ZIP": "directory",
                            "Extracted ZIP/verified.txt": hashlib.sha256(zip_payload).hexdigest()}
                run = {"case": case, "passed": False, "screenshots": [], "boundaries": []}
                report["runs"].append(run)
                peer = Peer(peer_path, env, "late-copy" if case.startswith("cancel-read") else "gnome-copy", source, root)
                app, connection = None, None
                log_path = root / "app.log"
                try:
                    with log_path.open("w") as log:
                        app = subprocess.Popen([str(binary), str(host), "--size", "1024x720", "--profile", str(root / "profile.csv")],
                                               env=env, cwd=root, stdout=log, stderr=log)
                        deadline, window_id = time.monotonic() + 20, None
                        while time.monotonic() < deadline:
                            assert app.poll() is None, "app exited before presentation"
                            found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "MegaMan"], env=env, capture_output=True, text=True)
                            if found.returncode == 0:
                                window_id = found.stdout.splitlines()[0]
                                command(["import", "-window", window_id, str(root / "ready.png")])
                                if int(command(["identify", "-format", "%k", str(root / "ready.png")]).stdout) > 20:
                                    break
                            time.sleep(.05)
                        assert window_id
                        connection = display.Display(env["DISPLAY"])
                        window = connection.create_resource_object("window", int(window_id))

                        def keys(*values):
                            command(["xdotool", "key", *values])

                        def click(x, y):
                            command(["xdotool", "mousemove", "--window", window_id, str(x), str(y), "click", "1"])

                        def settle(name, minimum=.4):
                            command(["xdotool", "mousemove", "--window", window_id, "512", "400"])
                            deadline, start, previous = time.monotonic() + 5, time.monotonic(), None
                            while time.monotonic() < deadline:
                                path = root / name
                                command(["import", "-window", window_id, str(path)])
                                current = digest(path)
                                if name not in run["screenshots"]:
                                    run["screenshots"].append(name)
                                if current == previous and time.monotonic() - start >= minimum:
                                    return
                                previous = current
                                time.sleep(.1)
                            raise AssertionError("UI did not settle: " + name)

                        def open_archive(top_status=False):
                            # The owned host has only sample.zip and, in Copy cases,
                            # clipboard-item.txt. End selects the ZIP without a filter
                            # that could hide a missing published-folder refresh.
                            click(250, 144 if top_status else 112)
                            keys("End", "Return")
                            settle("archive.png", .45)

                        def wait_file(path, data):
                            deadline = time.monotonic() + 12
                            while time.monotonic() < deadline:
                                assert app.poll() is None
                                if path.exists() and path.read_bytes() == data:
                                    return
                                time.sleep(.03)
                            raise AssertionError("expected publication absent: " + str(path))

                        def boundary(name, expected_tree, no_requests=False):
                            actual = host_tree(host)
                            assert actual == expected_tree, (name, actual, expected_tree)
                            count = peer.requests()
                            if no_requests:
                                assert count == 0, (name, "blocked Paste started a native Read", peer.lines)
                            run["boundaries"].append({"name": name, "host_tree": actual, "native_payload_requests": count})

                        command(["xdotool", "windowfocus", window_id])
                        time.sleep(.5)
                        if case in ("extraction-blocks-paste", "extraction-refresh"):
                            open_archive()
                            click(260, 113)
                            settle("prepared.png", .6)
                            keys("Escape")
                            settle("hidden-archive.png")
                            keys("Escape")
                            settle("hidden-host.png")
                            click(30, 704)  # Disabled Paste, then the actual keyboard binding.
                            keys("ctrl+v")
                            settle("prepared-blocks-paste.png")
                            boundary("Prepared blocks Paste", original, no_requests=True)
                            click(950, 15)
                            settle("prepared-reopened.png")
                            click(80, 704)
                            wait_file(extraction / "verified.txt", zip_payload)
                            settle("extraction-terminal.png")
                            keys("Escape")
                            settle("terminal-hidden.png")
                            click(30, 704)
                            keys("ctrl+v")
                            settle("terminal-blocks-paste.png")
                            boundary("Unacknowledged extraction terminal blocks Paste", {k:v for k,v in expected.items() if k != source.name}, no_requests=True)
                            click(950, 15)
                            settle("terminal-reopened.png")
                            click(80, 704)  # Explicit Acknowledge.
                            settle("extraction-acknowledged.png")
                            if case == "extraction-refresh":
                                settle("final.png", .6)
                                boundary("Publication refreshes the unfiltered host listing", {k:v for k,v in expected.items() if k != source.name}, no_requests=True)
                            else:
                                keys("ctrl+v")
                                wait_file(copied, source.read_bytes())
                                settle("final.png", .6)
                                assert peer.requests() > 0
                                boundary("Paste succeeds after extraction acknowledgement", expected)
                        elif case == "copy-blocks-extraction":
                            keys("ctrl+v")
                            wait_file(copied, source.read_bytes())
                            settle("copy-terminal.png", .6)
                            keys("Escape")
                            settle("copy-hidden.png")
                            open_archive()
                            click(260, 113)  # Whole archive must remain disabled.
                            settle("copy-terminal-blocks-extraction.png")
                            boundary("Unacknowledged Copy result blocks archive extraction", {**original, source.name: source_hash})
                            for width, height in ((640, 480), (320, 240), (1024, 720)):
                                command(["xdotool", "windowsize", "--sync", window_id, str(width), str(height)])
                                settle(f"copy-terminal-blocks-extraction-{width}x{height}.png")
                            click(974, 704)  # Reopen retained Copy result.
                            settle("copy-reopened.png")
                            click(995, 17)  # Explicit Done acknowledgement.
                            settle("copy-acknowledged.png")
                            click(260, 113)
                            settle("prepared-after-copy-ack.png", .6)
                            keys("Escape")
                            settle("idle-copy-hidden-archive.png")
                            keys("Escape")
                            settle("idle-copy-hidden-host.png")
                            old_requests = peer.requests()
                            keys("ctrl+v")
                            settle("retained-idle-copy-does-not-steal-routing.png")
                            assert peer.requests() == old_requests
                            boundary("Prepared extraction still blocks Paste with retained idle Copy controller", {**original, source.name: source_hash})
                            click(950, 15)
                            settle("prepared-final-reopened.png")
                            click(80, 704)
                            wait_file(extraction / "verified.txt", zip_payload)
                            settle("final.png", .6)
                            boundary("Extraction succeeds after Copy acknowledgement", expected)
                        else:
                            keys("ctrl+v")
                            peer.wait("REQUEST ")
                            request_seen = time.monotonic()
                            settle("reading-host.png", .2)
                            if case.endswith("archive"):
                                open_archive()
                            else:
                                keys("ctrl+shift+f")
                                settle("find.png", .35)
                            settle("cancel-reachable.png", .2)
                            assert time.monotonic() - request_seen < 3.5, "witness reached Cancel too late for the five-second request deadline"
                            click(988, 704)
                            settle("cancel-requested.png", .2)
                            peer.release()  # A late native response must not admit Copy.
                            settle("cancelled-subview.png", .65)
                            boundary("Late clipboard reply after visible Cancel produces no copy", original)
                            keys("Escape")
                            settle("final.png")
                        window.send_event(protocol.event.ClientMessage(window=int(window_id), client_type=connection.intern_atom("WM_PROTOCOLS"),
                            data=(32, [connection.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0])), event_mask=0)
                        connection.flush()
                        app.wait(timeout=10)
                        assert app.returncode == 0
                finally:
                    if app and app.poll() is None:
                        app.terminate()
                        app.wait(timeout=5)
                    if connection:
                        connection.close()
                    peer.close()
                evidence = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith("{")))
                run["evidence"] = evidence
                if case == "extraction-blocks-paste":
                    assert evidence["mode"] == "copy-transfer" and evidence["state"] == "complete" and evidence["committed_roots"] == 1, evidence
                elif case == "extraction-refresh":
                    assert evidence["mode"] == "filesystem" and evidence["model_count"] == 2, evidence
                elif case == "copy-blocks-extraction":
                    assert evidence["mode"] == "extraction" and evidence["state"] == "Some(Terminal)", evidence
                else:
                    assert evidence["mode"] == "filesystem" and evidence["file_clipboard"]["state"] == "ready", evidence
                    assert host_tree(host) == original
                assert digest(source) == source_hash and digest(archive) == archive_hash
                assert (root / "profile.csv").stat().st_size > 0
                assert peer.process.returncode == 0, "clipboard peer did not close cleanly"
                run["exit_code"] = app.returncode
                run["peer_exit_code"] = peer.process.returncode
                run["peer_requests"] = peer.lines
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
