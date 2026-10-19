"""Independent Python-Xlib selection peer; no browser-transfer code is imported."""
import select
import sys
import time
import urllib.parse
from Xlib import X, Xatom, display, error, protocol

mode, address = sys.argv[1:3]
d = display.Display(address)
d.set_error_handler(lambda *_: None)
root = d.screen().root
w = root.create_window(0, 0, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent,
                       event_mask=X.PropertyChangeMask | X.StructureNotifyMask)
a = {name: d.intern_atom(name) for name in (
    'CLIPBOARD', 'TARGETS', 'TIMESTAMP', 'INCR', 'text/uri-list',
    'x-special/gnome-copied-files', 'application/x-kde-cutselection', '_PEER_DATA')}

def wait_event(predicate, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        while d.pending_events():
            e = d.next_event()
            if predicate(e):
                return e
        select.select([d.fileno()], [], [], 0.01)
    raise RuntimeError('peer event timeout')


def receive(name, pause=False):
    target, prop = a[name], a['_PEER_DATA']
    w.delete_property(prop)
    w.convert_selection(a['CLIPBOARD'], target, prop, X.CurrentTime)
    d.flush()
    e = wait_event(lambda e: e.type == X.SelectionNotify and e.target == target)
    if not e.property:
        raise RuntimeError('conversion refused: ' + name)
    p = w.get_full_property(prop, X.AnyPropertyType)
    if p.property_type == a['INCR']:
        if pause:
            print('INCR_READY', flush=True)
            assert sys.stdin.readline().strip() == 'continue'
        # Header NewValue is already queued; consume it before deleting the header.
        while d.pending_events():
            d.next_event()
        w.delete_property(prop)
        d.flush()
        data = bytearray()
        while True:
            wait_event(lambda e: e.type == X.PropertyNotify and e.atom == prop and e.state == X.PropertyNewValue)
            p = w.get_full_property(prop, X.AnyPropertyType)
            chunk = bytes(p.value)
            assert p.property_type == target and p.format == 8
            w.delete_property(prop)
            d.flush()
            if not chunk:
                return bytes(data)
            data.extend(chunk)
    w.delete_property(prop)
    d.flush()
    return bytes(p.value) if p.format == 8 else p.value


if mode in ('read', 'read-paused'):
    targets = receive('TARGETS')
    assert all(a[n] in targets for n in ('TARGETS', 'TIMESTAMP', 'text/uri-list',
                                        'x-special/gnome-copied-files', 'application/x-kde-cutselection'))
    stamp = receive('TIMESTAMP')
    assert len(stamp) == 1 and stamp[0] != 0
    uri = receive('text/uri-list', mode == 'read-paused')
    print('URI ' + uri.hex(), flush=True)
    # For a paused transfer ownership may have changed; only its existing URI lease survives.
    if mode == 'read-paused':
        sys.exit(0)
    gnome = receive('x-special/gnome-copied-files')
    kde = receive('application/x-kde-cutselection')
    assert gnome.startswith((b'copy\n', b'cut\n')) and not gnome.endswith(b'\n')
    assert kde in (b'0', b'1')
    assert gnome.split(b'\n')[1:] == uri.split(b'\r\n')[:-1]
    assert gnome.startswith(b'cut\n') == (kde == b'1')
    print('GNOME ' + gnome.hex(), flush=True)
    print('KDE ' + kde.hex(), flush=True)
    for line in uri.split(b'\r\n'):
        if line:
            assert line.startswith(b'file:///')
            raw = urllib.parse.unquote_to_bytes(line[7:])
            print('PATH ' + raw.hex(), flush=True)
    print('OWNER ' + str(d.get_selection_owner(a['CLIPBOARD']).id), flush=True)
    sys.exit(0)

if mode == 'flood-targets':
    count = 22000
    for start in range(0, count, 64):
        batch = min(64, count - start)
        for _ in range(batch):
            w.convert_selection(a['CLIPBOARD'], a['TARGETS'], a['_PEER_DATA'], X.CurrentTime)
        d.flush()
        for _ in range(batch):
            e = wait_event(lambda e: e.type == X.SelectionNotify)
            assert e.property == a['_PEER_DATA']
    p = w.get_full_property(a['_PEER_DATA'], X.AnyPropertyType)
    assert p.property_type == Xatom.ATOM and a['text/uri-list'] in p.value
    print('FLOODED ' + str(count), flush=True)
    sys.exit(0)

if mode == 'lease-probe':
    for i in range(9):
        prop = d.intern_atom('_PEER_LEASE_' + str(i))
        w.convert_selection(a['CLIPBOARD'], a['text/uri-list'], prop, X.CurrentTime)
        d.flush()
        e = wait_event(lambda e: e.type == X.SelectionNotify)
        assert bool(e.property) == (i < 8)
        if e.property:
            p = w.get_full_property(prop, X.AnyPropertyType)
            assert p.property_type == a['INCR']
    print('FULL', flush=True)
    assert sys.stdin.readline().strip() == 'continue'
    prop = d.intern_atom('_PEER_LEASE_REUSE')
    w.convert_selection(a['CLIPBOARD'], a['text/uri-list'], prop, X.CurrentTime)
    d.flush()
    e = wait_event(lambda e: e.type == X.SelectionNotify)
    assert e.property == prop
    assert w.get_full_property(prop, X.AnyPropertyType).property_type == a['INCR']
    print('REUSED', flush=True)
    sys.exit(0)

scenario = sys.argv[3]
paths = [bytes.fromhex(p) for p in sys.argv[4:]]
uri_lines = [b'file://' + urllib.parse.quote_from_bytes(p, safe='/').encode('ascii') for p in paths]
uri = b'\r\n'.join(uri_lines) + b'\r\n'
gnome = b'cut\n' + b'\n'.join(uri_lines)
intent = b'1'
if scenario in ('gnome-copy', 'uri-copy', 'late-copy'):
    gnome = b'copy\n' + b'\n'.join(uri_lines)
    intent = b'0'
if scenario == 'malformed':
    gnome += b'\nfile://remote/bad'
if scenario in ('oversize-direct', 'oversize-chunk'):
    gnome = b'X' * 8192
if scenario == 'bad-escape':
    gnome = b'copy\nfile:///bad%GG'
if scenario == 'incr-overall':
    gnome = b'copy\nfile:///' + b'A' * 1500
if scenario == 'incr-empty':
    gnome = b''
# Use a real server timestamp, independently of the Rust service.
w.change_property(a['_PEER_DATA'], Xatom.INTEGER, 8, b'1')
d.flush()
timestamp = wait_event(lambda e: e.type == X.PropertyNotify).time
w.set_selection_owner(a['CLIPBOARD'], timestamp)
d.sync()
assert d.get_selection_owner(a['CLIPBOARD']).id == w.id
print('READY ' + str(w.id), flush=True)
transfers = {}
late = []


def notify(e, prop):
    n = protocol.event.SelectionNotify(time=e.time, requestor=e.requestor,
                                      selection=e.selection, target=e.target, property=prop)
    e.requestor.send_event(n, propagate=False)
    d.flush()


def respond(e):
    global timestamp
    target, prop = e.target, e.property or e.target
    names = {v: k for k, v in a.items()}
    name = names.get(target, 'unknown')
    if target == a['TARGETS']:
        names = ['TARGETS', 'TIMESTAMP']
        if scenario != 'unsupported':
            names += ['text/uri-list', 'application/x-kde-cutselection']
            if scenario not in ('uri-kde', 'uri-copy'):
                names += ['x-special/gnome-copied-files']
        e.requestor.change_property(prop, Xatom.ATOM, 32, [a[n] for n in names] if scenario != 'targets-huge' else [a['TARGETS']] * 128)
    elif target == a['TIMESTAMP']:
        e.requestor.change_property(prop, Xatom.INTEGER, 32, [timestamp])
    elif target in (a['text/uri-list'], a['x-special/gnome-copied-files'], a['application/x-kde-cutselection']):
        print('REQUEST ' + name, flush=True)
        if scenario == 'owner-exit':
            sys.exit(0)
        if scenario == 'timeout':
            return
        if scenario in ('late-reply', 'late-copy'):
            late.append(e)
            return
        if scenario == 'change-owner':
            newer = root.create_window(0, 0, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent)
            newer.set_selection_owner(a['CLIPBOARD'], X.CurrentTime)
            d.sync()
        data = {a['text/uri-list']: uri, a['x-special/gnome-copied-files']: gnome,
                a['application/x-kde-cutselection']: intent}[target]
        if scenario in ('incr', 'lying-incr', 'oversize-chunk', 'incr-empty', 'incr-overall', 'incr-large-hint', 'incr-timeout', 'incr-short'):
            e.requestor.change_attributes(event_mask=X.PropertyChangeMask | X.StructureNotifyMask)
            hint = 1 if scenario in ('lying-incr', 'oversize-chunk', 'incr-overall', 'incr-timeout') else (8192 if scenario == 'incr-large-hint' else len(data) + (1 if scenario == 'incr-short' else 0))
            e.requestor.change_property(prop, a['INCR'], 32, [hint])
            if scenario != 'incr-timeout':
                transfers[(e.requestor.id, prop)] = [e.requestor, target, data, 0, False]
        else:
            typ = Xatom.STRING if scenario == 'bad-type' else target
            e.requestor.change_property(prop, typ, 32 if scenario == 'bad-format' else 8, [1] if scenario == 'bad-format' else data)
        if scenario == 'timestamp-change':
            timestamp += 1
    else:
        notify(e, X.NONE)
        return
    notify(e, prop)

while True:
    while d.pending_events():
        e = d.next_event()
        if e.type == X.SelectionRequest:
            respond(e)
        elif e.type == X.PropertyNotify and e.state == X.PropertyDelete:
            key = (e.window.id, e.atom)
            t = transfers.get(key)
            if t:
                window, target, data, offset, eof = t
                if eof:
                    del transfers[key]
                    continue
                size = len(data) if scenario == 'oversize-chunk' else 31
                chunk = data[offset:offset + size]
                window.change_property(e.atom, target, 8, chunk)
                t[3] += len(chunk)
                t[4] = not chunk
                d.flush()
    readable, _, _ = select.select([d.fileno(), sys.stdin], [], [], 0.01)
    if sys.stdin in readable:
        command = sys.stdin.readline().strip()
        if command in ('', 'quit'):
            break
        if command == 'steal':
            newer = root.create_window(0, 0, 1, 1, 0, 0, X.InputOnly, X.CopyFromParent)
            newer.set_selection_owner(a['CLIPBOARD'], X.CurrentTime)
            d.sync()
            print('STOLEN ' + str(newer.id), flush=True)
        elif command == 'normal':
            scenario = 'normal'
            for request in late:
                respond(request)
            late.clear()
            print('NORMAL', flush=True)
