#!/usr/bin/env python3
"""Actual native Paste with independent clipboard offers and filesystem oracles.

Owned Xvfb/CPU functional fixtures only; no timing qualification.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import stat
import subprocess
import time
from Xlib import X, display, protocol


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--out', required=True, type=Path)
    args = parser.parse_args()
    binary, out = args.binary.resolve(), args.out.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error('binary must be executable')
    if not args.out.is_absolute() or out == Path('/') or out == Path.home():
        parser.error('output must be an explicit absolute new or empty directory')
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error('output must be new or empty')
    out.mkdir(parents=True, exist_ok=True)
    runtime = out / 'runtime'
    runtime.mkdir(mode=0o700)
    peer = Path(__file__).resolve().parents[1] / 'crates/browser-native/tests/x11_peer.py'
    report = {'schema_version': 1, 'passed': False, 'artifact_sha256': digest(binary), 'runs': [],
              'limits': ['Linux X11 owned Xvfb, forced CPU scale1, functional proof only',
                         'No Cut/Move/overwrite support, native DnD, Wayland/other OS or latency qualification',
                         'Basic ordinary rwx and atime/mtime only; readonly roots use explicit post-publication bound-FD finalization; no ACL/xattr/ownership preservation']}
    env = os.environ.copy()
    env.update(WINIT_UNIX_BACKEND='x11', WINIT_X11_SCALE_FACTOR='1', MUI_RENDERER='cpu', XDG_RUNTIME_DIR=str(runtime))
    env.pop('WAYLAND_DISPLAY', None)
    read_fd, write_fd = os.pipe()
    with (out / 'xvfb.log').open('w') as server_log:
        server = subprocess.Popen(['Xvfb', '-displayfd', str(write_fd), '-screen', '0', '1600x1200x24', '-noreset', '-nolisten', 'tcp'],
                                  pass_fds=(write_fd,), stdout=server_log, stderr=server_log)
        os.close(write_fd)
        try:
            assert select.select([read_fd], [], [], 10)[0], 'Xvfb startup timeout'
            env['DISPLAY'] = ':' + os.read(read_fd, 64).decode().strip()
            os.close(read_fd)
            read_fd = None

            def command(argv):
                return subprocess.run(argv, env=env, capture_output=True, text=True, check=True, timeout=10)

            def peer_line(owner, prefix, timeout=8):
                deadline = time.monotonic() + timeout
                while time.monotonic() < deadline:
                    assert owner.poll() is None, 'clipboard peer exited early'
                    if select.select([owner.stdout], [], [], min(.05, deadline - time.monotonic()))[0]:
                        line = owner.stdout.readline().strip()
                        if line.startswith(prefix):
                            return line
                raise AssertionError('clipboard peer did not produce ' + prefix)

            for case in ('copy-basic', 'copy-partial', 'readonly-root', 'cut-refused', 'cancel-read', 'paste-then-navigate', 'text-paste', 'back-and-reopen'):
                root = out / case
                root.mkdir()
                sources, dest, other = (root / n for n in ('sources', 'destination', 'other'))
                for folder in (sources, dest, other):
                    folder.mkdir(mode=0o755)
                executable = Path(os.fsdecode(os.fsencode(sources) + b'/run-\xff%#'))
                executable.write_bytes(b'#!/bin/sh\nexit 0\n')
                executable.chmod(0o751)
                stamp = (1_600_000_000_123456789, 1_600_000_001_987654321)
                expected_hash = digest(executable)
                os.utime(executable, ns=stamp)
                directory = sources / 'folder'
                directory.mkdir(mode=0o755)
                child = directory / 'read-only.txt'
                child.write_bytes(b'read-only child payload\n')
                child.chmod(0o400)
                child_hash = digest(child)
                os.utime(child, ns=stamp)
                outside = root / 'outside'
                outside.write_bytes(b'outside unchanged\n')
                link = sources / 'link'
                link.symlink_to(outside)
                readonly = sources / 'readonly-root'
                readonly.mkdir(mode=0o755)
                readonly_child = readonly / 'nested-readonly'
                readonly_child.mkdir(mode=0o755)
                readonly_tool = readonly_child / 'tool'
                readonly_tool.write_bytes(b'#!/bin/sh\n# readonly-root fixture\nexit 0\n')
                readonly_tool.chmod(0o4751)
                readonly_hash = digest(readonly_tool)
                os.utime(readonly_tool, ns=stamp)
                readonly_child.chmod(0o2555)
                os.utime(readonly_child, ns=stamp)
                readonly.chmod(0o4555)
                os.utime(readonly, ns=stamp)
                empty_readonly = sources / 'empty-root0400'
                empty_readonly.mkdir(mode=0o755)
                empty_readonly.chmod(0o400)
                os.utime(empty_readonly, ns=stamp)
                paths = [executable, directory, link]
                if case == 'copy-partial':
                    paths = [executable, directory, link]
                    (dest / 'folder').mkdir()
                    (dest / 'folder' / 'sentinel').write_bytes(b'existing stays\n')
                elif case == 'readonly-root':
                    paths = [readonly, empty_readonly]
                elif case in ('cut-refused', 'cancel-read', 'paste-then-navigate', 'text-paste', 'back-and-reopen'):
                    paths = [executable]
                scenario = 'late-copy' if case in ('cancel-read', 'paste-then-navigate') else 'normal' if case == 'cut-refused' else 'gnome-copy'
                owner = subprocess.Popen(['python3', str(peer), 'own', env['DISPLAY'], scenario, *[os.fsencode(p).hex() for p in paths]],
                                         env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, bufsize=1)
                app, connection = None, None
                try:
                    peer_line(owner, 'READY ')
                    log_path = root / 'app.log'
                    with log_path.open('w') as log:
                        app = subprocess.Popen([str(binary), str(dest), '--size', '1024x720', '--profile', str(root / 'profile.csv')],
                                               env=env, cwd=root, stdout=log, stderr=log)
                        deadline, window_id = time.monotonic() + 20, None
                        while time.monotonic() < deadline:
                            assert app.poll() is None, 'early application exit'
                            found = subprocess.run(['xdotool', 'search', '--onlyvisible', '--name', 'MegaMan'], env=env, capture_output=True, text=True)
                            if found.returncode == 0:
                                window_id = found.stdout.splitlines()[0]
                                command(['import', '-window', window_id, str(root / 'ready.png')])
                                if int(command(['identify', '-format', '%k', str(root / 'ready.png')]).stdout) > 20:
                                    break
                            time.sleep(.05)
                        assert window_id, 'window presentation timeout'
                        command(['xdotool', 'windowfocus', window_id])
                        time.sleep(.5)
                        if case == 'text-paste':
                            command(['xdotool', 'key', 'ctrl+l', 'ctrl+v'])
                            time.sleep(.7)
                        else:
                            command(['xdotool', 'key', 'ctrl+v'])
                            if case in ('cancel-read', 'paste-then-navigate'):
                                peer_line(owner, 'REQUEST ')
                                if case == 'paste-then-navigate':
                                    command(['xdotool', 'key', 'ctrl+l', 'ctrl+a'])
                                    command(['xdotool', 'type', '--clearmodifiers', '--delay', '0', str(other)])
                                    command(['xdotool', 'key', 'Return'])
                                    time.sleep(.3)
                                    owner.stdin.write('normal\n')
                                    owner.stdin.flush()
                                else:
                                    # Clipboard status is the penultimate intrinsic row, above Paste.
                                    command(['import', '-window', window_id, str(root / 'reading.png')])
                                    command(['xdotool', 'mousemove', '--window', window_id, '988', '673', 'click', '1'])
                                    time.sleep(.25)
                                    owner.stdin.write('normal\n')
                                    owner.stdin.flush()
                            if case in ('copy-basic', 'copy-partial', 'paste-then-navigate', 'back-and-reopen', 'readonly-root'):
                                expected = dest / (empty_readonly.name if case == 'readonly-root' else executable.name)
                                deadline = time.monotonic() + 12
                                while not expected.exists() and time.monotonic() < deadline:
                                    assert app.poll() is None
                                    time.sleep(.02)
                                assert expected.exists(), 'native Paste did not publish expected content'
                            time.sleep(.7)
                        if case == 'back-and-reopen':
                            command(['xdotool', 'key', 'Escape'])
                            time.sleep(.3)
                            command(['import', '-window', window_id, str(root / 'hidden.png')])
                            # Persistent Show action is in the last intrinsic status row.
                            command(['xdotool', 'mousemove', '--window', window_id, '974', '704', 'click', '1'])
                            time.sleep(.3)
                        command(['import', '-window', window_id, str(root / 'final.png')])
                        if case == 'copy-basic':
                            for width, height in ((640, 480), (320, 240)):
                                command(['xdotool', 'windowsize', window_id, str(width), str(height)])
                                time.sleep(.25)
                                screenshot = root / f'final-{width}x{height}.png'
                                command(['import', '-window', window_id, str(screenshot)])
                                assert command(['identify', '-format', '%wx%h', str(screenshot)]).stdout == f'{width}x{height}'
                            command(['xdotool', 'windowsize', window_id, '1024', '720'])
                            time.sleep(.2)
                        connection = display.Display(env['DISPLAY'])
                        window = connection.create_resource_object('window', int(window_id))
                        window.send_event(protocol.event.ClientMessage(window=int(window_id), client_type=connection.intern_atom('WM_PROTOCOLS'),
                            data=(32, [connection.intern_atom('WM_DELETE_WINDOW'), X.CurrentTime, 0, 0, 0])), event_mask=0)
                        connection.flush()
                        app.wait(timeout=10)
                        assert app.returncode == 0
                    evidence = json.loads(next(line for line in reversed(log_path.read_text().splitlines()) if line.startswith('{')))
                    readonly_oracles = []
                    if case in ('copy-basic', 'paste-then-navigate', 'back-and-reopen'):
                        assert evidence['mode'] == 'copy-transfer' and evidence['state'] == 'complete', evidence
                        assert evidence['committed_roots'] == len(paths), evidence
                        assert evidence['basic_metadata_verified_roots'] == len(paths), evidence
                        assert evidence['metadata_incomplete_roots'] == 0 and evidence['durability_uncertain_roots'] == 0, evidence
                        copied = dest / executable.name
                        copied_stat = copied.stat()
                        assert stat.S_IMODE(copied_stat.st_mode) == 0o751
                        assert (copied_stat.st_atime_ns, copied_stat.st_mtime_ns) == stamp
                        assert digest(copied) == expected_hash
                        if case == 'copy-basic':
                            copied_child = dest / 'folder/read-only.txt'
                            assert stat.S_IMODE(copied_child.stat().st_mode) == 0o400
                            assert digest(copied_child) == child_hash
                            assert os.readlink(dest / 'link') == str(outside)
                        elif case == 'paste-then-navigate':
                            assert not any(other.iterdir()), 'navigation changed accepted destination'
                    elif case == 'copy-partial':
                        assert evidence['state'] == 'failed' and evidence['committed_roots'] == 1 and evidence['roots'] == 3, evidence
                        assert digest(dest / executable.name) == expected_hash
                        assert (dest / 'folder/sentinel').read_bytes() == b'existing stays\n'
                        assert not (dest / 'link').exists()
                    elif case == 'readonly-root':
                        assert evidence['state'] == 'complete' and evidence['committed_roots'] == 2, evidence
                        assert evidence['basic_metadata_verified_roots'] == 2, evidence
                        assert evidence['metadata_incomplete_roots'] == 0 and evidence['durability_uncertain_roots'] == 0, evidence
                        for relative, expected_mode in (('readonly-root', 0o555), ('readonly-root/nested-readonly', 0o555), ('readonly-root/nested-readonly/tool', 0o751), ('empty-root0400', 0o400)):
                            metadata = (dest / relative).stat()
                            assert stat.S_IMODE(metadata.st_mode) == expected_mode, (relative, oct(stat.S_IMODE(metadata.st_mode)))
                            assert (metadata.st_atime_ns, metadata.st_mtime_ns) == stamp, relative
                            readonly_oracles.append({'relative': relative, 'mode': oct(stat.S_IMODE(metadata.st_mode)),
                                                     'expected_mode': oct(expected_mode), 'atime_ns': metadata.st_atime_ns,
                                                     'mtime_ns': metadata.st_mtime_ns, 'expected_times_ns': stamp})
                        assert digest(dest / 'readonly-root/nested-readonly/tool') == readonly_hash
                        assert not list((dest / 'empty-root0400').iterdir())
                        assert len(list(dest.iterdir())) == 2, 'finalization left staging output'
                    else:
                        assert evidence['mode'] == 'filesystem', evidence
                        assert not any(dest.iterdir()), 'refused/cancelled/text paste wrote output'
                        if case == 'text-paste':
                            assert evidence['focus_role'] == 'location', evidence
                        if case == 'cancel-read':
                            assert evidence['file_clipboard']['state'] != 'reading', evidence
                    assert digest(executable) == expected_hash
                    assert stat.S_IMODE(executable.stat().st_mode) == 0o751 and executable.stat().st_mtime_ns == stamp[1]
                    assert outside.read_bytes() == b'outside unchanged\n'
                    for source_path, original_mode in ((readonly, 0o4555), (readonly_child, 0o2555), (readonly_tool, 0o4751), (empty_readonly, 0o400)):
                        source_metadata = source_path.stat()
                        assert stat.S_IMODE(source_metadata.st_mode) == original_mode
                        assert source_metadata.st_mtime_ns == stamp[1]
                    assert digest(readonly_tool) == readonly_hash
                    assert (root / 'profile.csv').stat().st_size > 0
                    report['runs'].append({'case': case, 'passed': True, 'exit_code': app.returncode, 'evidence': evidence,
                                           'offered_sources_native_hex': [os.fsencode(path).hex() for path in paths],
                                           'readonly_metadata_oracles': readonly_oracles,
                                           'readonly_tool_expected_sha256': readonly_hash if readonly_oracles else None,
                                           'native_source_hex': os.fsencode(executable).hex(), 'expected_sha256': expected_hash})
                finally:
                    if app is not None and app.poll() is None:
                        app.terminate()
                        app.wait(timeout=5)
                    if connection is not None:
                        connection.close()
                    if owner.poll() is None:
                        owner.communicate('quit\n', timeout=5)
                    assert owner.returncode == 0
            assert digest(binary) == report['artifact_sha256'], 'executable changed during run'
            report['passed'] = True
        except Exception as error:
            report['error'] = f'{type(error).__name__}: {error}'
            raise
        finally:
            (out / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
            if read_fd is not None:
                os.close(read_fd)
            server.terminate()
            server.wait(timeout=5)
    print(json.dumps({'passed': report['passed'], 'cases': len(report['runs']), 'artifact_sha256': report['artifact_sha256']}))


if __name__ == '__main__':
    main()
