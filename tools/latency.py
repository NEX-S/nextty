#!/usr/bin/env python3
"""Measure TUI echo, GUI presentation, and local agent-streaming input latency."""
import argparse
from contextlib import nullcontext
import errno
import fcntl
import getpass
import http.server
import json
import math
import os
from pathlib import Path
import pty
import random
import re
import select
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import tty


ANSI = re.compile(rb'\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]')


def wait_for(predicate, description, process=None, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        if process is not None and process.poll() is not None:
            raise RuntimeError(f'nextty exited {process.returncode} while waiting for {description}')
        time.sleep(.002)
    raise TimeoutError(description)


def report(label, samples, unit):
    ordered = sorted(samples)
    values = [ordered[math.ceil(len(ordered) * p / 100) - 1] for p in (50, 95, 99)]
    print(label)
    print(f'  {unit}: p50 {values[0]:.2f}, p95 {values[1]:.2f}, p99 {values[2]:.2f}; '
          f'{len(samples)} samples')


def environment(root):
    home = root / 'home'
    for directory in (home, root / 'cache', root / 'config'):
        directory.mkdir()
    return dict(os.environ, HOME=str(home), XDG_CACHE_HOME=str(root / 'cache'),
                XDG_CONFIG_HOME=str(root / 'config'), LANG='C.UTF-8')


def stop_process(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def stop_app(app, binary, workspace, env, destination=None):
    target = [destination] if destination else []
    listing = subprocess.run([str(binary), 'list', *target], env=env, check=True,
                             capture_output=True, text=True, timeout=5)
    if any(line.split('\t', 1)[0] == workspace for line in listing.stdout.splitlines()):
        subprocess.run([str(binary), 'terminate', *target, workspace], env=env, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
    if app.poll() is None:
        os.killpg(app.pid, signal.SIGTERM)
        app.wait(timeout=5)


def drain(terminal, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        terminal.read(min(.005, deadline - time.monotonic()))


def tui_fixture():
    tty.setraw(0)
    os.write(1, b'\x1b[?25l\x1b[2J\x1b[HLATENCY_READY')
    index = 0
    while True:
        data = os.read(0, 1)
        if not data:
            return
        index += 1
        os.write(1, f'\x1b[Hprobe-{index:04}'.encode())


class OuterTerminal:
    def __init__(self, binary, args, env, cwd):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        self.output = bytearray()
        try:
            self.process = subprocess.Popen(
                [str(binary), *args], env=env, cwd=cwd, stdin=slave, stdout=slave,
                stderr=slave, start_new_session=True)
        finally:
            os.close(slave)

    def read(self, timeout):
        if not select.select([self.master], [], [], max(0, timeout))[0]:
            return
        try:
            data = os.read(self.master, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                return
            raise
        self.output.extend(data)

    def expect(self, expected, timeout=8):
        deadline = time.monotonic() + timeout
        while expected not in ANSI.sub(b'', self.output):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                tail = ANSI.sub(b'', self.output)[-1500:].decode(errors='replace')
                raise TimeoutError(f'TUI did not display {expected!r}; tail={tail!r}')
            if self.process.poll() is not None:
                tail = ANSI.sub(b'', self.output)[-1500:].decode(errors='replace')
                raise RuntimeError(f'TUI exited {self.process.returncode}; tail={tail!r}')
            self.read(min(.02, remaining))

    def send(self, data):
        os.write(self.master, data)

    def close(self):
        os.close(self.master)


class PrivateSshd:
    def __init__(self, binary, root, env):
        self.binary = binary
        self.root = root / 'ssh'
        self.env = env
        self.server = self.log = None

    def __enter__(self):
        root = self.root
        root.mkdir()
        for name in ('remote-home', 'remote-runtime', 'remote-cache', 'remote-config', 'bin'):
            (root / name).mkdir(mode=0o700)
        (root / 'binary').write_text(str(self.binary) + '\n')
        for name in ('host', 'client'):
            subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(root / name)],
                           check=True)
        with socket.socket() as reserved:
            reserved.bind(('127.0.0.1', 0))
            port = reserved.getsockname()[1]
        user = getpass.getuser()
        self.destination = f'{user}@127.0.0.1:{port}'
        script = Path(__file__).resolve()
        (root / 'sshd.conf').write_text('\n'.join([
            'ListenAddress 127.0.0.1', f'Port {port}', f'HostKey {root / "host"}',
            f'AuthorizedKeysFile {root / "client.pub"}', f'PidFile {root / "sshd.pid"}',
            'StrictModes no', f'AllowUsers {user}', 'PasswordAuthentication no',
            'KbdInteractiveAuthentication no', 'UsePAM no', 'UseDNS no', 'PermitTTY no',
            'PrintMotd no', 'AllowTcpForwarding no', 'AllowAgentForwarding no', 'X11Forwarding no',
            f'SetEnv HOME={root / "remote-home"} ENV=/dev/null BASH_ENV=/dev/null',
            'ForceCommand ' + shlex.join([sys.executable, str(script), '--ssh-fixture', str(root)]),
            'LogLevel ERROR', '',
        ]))
        key = (root / 'host.pub').read_text().split()
        (root / 'known_hosts').write_text(f'[127.0.0.1]:{port} {key[0]} {key[1]}\n')
        (root / 'ssh_config').write_text('\n'.join([
            'Host *', f'  IdentityFile {root / "client"}', '  IdentitiesOnly yes',
            '  IdentityAgent none', '  BatchMode yes', '  StrictHostKeyChecking yes',
            f'  UserKnownHostsFile {root / "known_hosts"}', '  GlobalKnownHostsFile /dev/null',
            '  ConnectTimeout 2', '  ConnectionAttempts 1', '  LogLevel ERROR', '',
        ]))
        system_ssh, sshd = shutil.which('ssh'), shutil.which('sshd')
        if not system_ssh or not sshd:
            raise RuntimeError('private SSH measurement requires existing ssh and sshd')
        wrapper = root / 'bin' / 'ssh'
        wrapper.write_text('#!/bin/sh\nexec ' + shlex.join([system_ssh, '-F', str(root / 'ssh_config')]) + ' "$@"\n')
        wrapper.chmod(0o700)
        self.env['PATH'] = str(root / 'bin') + ':/usr/bin:/bin'
        subprocess.run([sshd, '-t', '-f', str(root / 'sshd.conf')], check=True)
        self.log = (root / 'sshd.log').open('w')
        self.server = subprocess.Popen([sshd, '-D', '-e', '-f', str(root / 'sshd.conf')],
                                       stdout=self.log, stderr=subprocess.STDOUT)

        def listening():
            if self.server.poll() is not None:
                raise RuntimeError(f'private sshd exited; see {root / "sshd.log"}')
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.1):
                    return True
            except OSError:
                return False

        wait_for(listening, 'private loopback sshd listener', timeout=5)
        return self.destination

    def __exit__(self, *_):
        if self.server is not None:
            stop_process(self.server)
        if self.log is not None:
            self.log.close()


def ssh_fixture(root):
    command = shlex.split(os.environ.get('SSH_ORIGINAL_COMMAND', ''))
    if command[:2] != ['nextty', '--bridge'] or command[2:] not in (['new'], ['attach'], ['list'], ['terminate']):
        raise RuntimeError(f'unexpected bridge invocation: {command!r}')
    env = dict(os.environ, HOME=str(root / 'remote-home'),
               XDG_RUNTIME_DIR=str(root / 'remote-runtime'),
               XDG_CACHE_HOME=str(root / 'remote-cache'),
               XDG_CONFIG_HOME=str(root / 'remote-config'), SHELL='/bin/bash',
               ENV='', LANG='C.UTF-8', PATH='/usr/bin:/bin')
    binary = (root / 'binary').read_text().strip()
    os.execve(binary, [binary, *command[1:]], env)


def run_tui(binary, samples, use_ssh):
    with tempfile.TemporaryDirectory(prefix='nextty-latency-tui-') as temporary:
        root = Path(temporary)
        runtime = root / 'runtime'
        runtime.mkdir(mode=0o700)
        env = environment(root)
        env.update(XDG_RUNTIME_DIR=str(runtime), TERM='xterm-256color')
        workspace = f'latency-{os.getpid()}'
        ssh = PrivateSshd(binary, root, env) if use_ssh else nullcontext()
        with ssh as destination:
            target = [destination] if destination else []
            args = ['new', *target, workspace, '--tui', '-e', sys.executable,
                    str(Path(__file__).resolve()), '--tui-fixture']
            terminal = OuterTerminal(binary, args, env, root / 'home')
            try:
                terminal.expect(b'LATENCY_READY')
                measurements = []
                for index in range(1, samples + 101):
                    terminal.output.clear()
                    expected = f'probe-{index:04}'.encode()
                    start = time.monotonic()
                    terminal.send(b'x')
                    terminal.expect(expected, timeout=5)
                    if index > 100:
                        measurements.append((time.monotonic() - start) * 1e6)
                terminal.send(b'\x1bq')
                terminal.process.wait(timeout=5)
                if terminal.process.returncode:
                    raise RuntimeError(f'TUI detach exited {terminal.process.returncode}')
            finally:
                try:
                    stop_app(terminal.process, binary, workspace, env, destination)
                finally:
                    terminal.close()
        label = 'private-sshd' if use_ssh else 'local daemon'
        report(f'TUI key to displayed raw-mode echo through {label}', measurements, 'microseconds')
        print('  path: outer PTY key decode → socket → daemon → PTY → fixture → grid → outer PTY output; 100 warmups')


def gui_fixture(directory, count):
    tty.setraw(0)

    def output(data):
        while data:
            data = data[os.write(1, data):]

    with (directory / 'events.jsonl').open('w', buffering=1) as events:
        def emit(value):
            events.write(json.dumps(value) + '\n')

        output(b'\x1b[?25l\x1b[2J\x1b[H')
        emit({'ready': True})
        for index in range(1, count + 1):
            if not os.read(0, 1):
                return
            marker = 0x20000 + index
            output(f'\x1b7\x1b[H\x1b[48;2;{marker >> 16};{marker >> 8 & 255};{marker & 255}m \x1b[0m\x1b8'.encode())
            emit({'index': index, 'marker': marker})
        # Do not overwrite the final key marker before the compositor presents it.
        time.sleep(.1)
        marker = 0x10000
        for phase in ('sparse', 'scrolling'):
            height = struct.unpack('HHHH', fcntl.ioctl(0, termios.TIOCGWINSZ, b'\0' * 8))[0]
            output(f'\x1b[2;{height}r\x1b[{height};1H'.encode())
            deadline = time.monotonic()
            for index in range(6 * 240):
                data = f'\x1b7\x1b[H\x1b[48;2;{marker >> 16};{marker >> 8 & 255};{marker & 255}m \x1b[0m\x1b8'.encode()
                if phase == 'scrolling':
                    data += (f'\x1b[32mframe {index:04}\x1b[0m 中文 UTF-8 λ → ' + '0123456789 ' * 4 + '\r\n').encode()
                emit({'output': marker, 'phase': phase, 'record': index >= 2 * 240,
                      'write_begin': time.monotonic()})
                output(data)
                marker += 1
                deadline += 1 / 240
                time.sleep(max(0, deadline - time.monotonic()))
    time.sleep(.3)


def events(path):
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def traces(path):
    frames, presented = {}, {}
    focused = False
    complete = path.read_text().rpartition('\n')[0] if path.exists() else ''
    for line in complete.splitlines():
        fields = line.split()
        if fields[0] == 'frame':
            frames[int(fields[1])] = {'marker': int(fields[2]), 'focused': fields[3] == '1'}
        elif fields[0] == 'presented':
            presented[int(fields[1])] = float(fields[2])
        elif fields[0] == 'focus':
            focused = fields[1] == '1'
    return frames, presented, focused


def run_gui(binary, session_record, samples):
    with tempfile.TemporaryDirectory(prefix='nextty-latency-gui-') as temporary:
        root = Path(temporary)
        env = environment(root)
        env.update(json.loads(session_record.read_text())['env'])
        for name in ('DISPLAY', 'IBUS_ADDRESS', 'LD_PRELOAD', 'LIBGL_ALWAYS_SOFTWARE', 'GALLIUM_DRIVER'):
            env.pop(name, None)
        env['NEXTTY_TRACE'] = str(root / 'trace.log')
        config = root / 'config.toml'
        config.write_text('font_family = "monospace"\nfont_size_px = 16.0\n')
        from gi.repository import Gio, GLib
        connection = Gio.DBusConnection.new_for_address_sync(
            env['DBUS_SESSION_BUS_ADDRESS'],
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None, None)
        dest = 'org.gnome.Mutter.RemoteDesktop'

        def call(path, interface, method, signature=None, values=()):
            variant = GLib.Variant(signature, values) if signature else None
            return connection.call_sync(dest, path, interface, method, variant, None,
                                        Gio.DBusCallFlags.NONE, 5000, None).unpack()

        def key(symbol, pressed):
            call(session, dest + '.Session', 'NotifyKeyboardKeysym', '(ub)', (symbol, pressed))

        def tap(symbol):
            key(symbol, True)
            key(symbol, False)

        session = None
        app = None
        workspace = f'latency-{os.getpid()}'
        try:
            session, = call('/org/gnome/Mutter/RemoteDesktop', dest, 'CreateSession')
            call(session, dest + '.Session', 'Start')
            tap(0xffe1)
            with (root / 'app.log').open('w') as log:
                app = subprocess.Popen(
                    [str(binary), 'new', workspace, '--gui', '--config', str(config),
                     '-e', sys.executable, str(Path(__file__).resolve()), '--gui-fixture',
                     str(root), str(samples + 1)],
                    env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                trace = root / 'trace.log'
                wait_for(lambda: any(e.get('ready') for e in events(root / 'events.jsonl')),
                         'PTY fixture ready', app)
                wait_for(lambda: traces(trace)[2], 'terminal focused', app)
                time.sleep(1)
                tap(ord('x'))
                wait_for(lambda: any(e.get('index') == 1 for e in events(root / 'events.jsonl')),
                         'warmup key', app)
                sent = []
                rng = random.Random(7491)
                deadline = time.monotonic()
                for index in range(2, samples + 2):
                    time.sleep(max(0, deadline - time.monotonic()))
                    start = time.monotonic()
                    key(ord('x'), True)
                    key(ord('x'), False)
                    sent.append((index, start))
                    deadline += .05 + rng.uniform(-.015, .015)
                if app.wait(timeout=30):
                    raise RuntimeError(f'nextty exited {app.returncode}; see {root / "app.log"}')
            frames, presented, _ = traces(trace)
            received = {e['index']: e for e in events(root / 'events.jsonl') if 'index' in e}
            measurements = []
            for index, start in sent:
                marker = received[index]['marker']
                visible = [(presented[frame], f) for frame, f in frames.items()
                           if frame in presented and f['marker'] == marker]
                if not visible:
                    raise RuntimeError(f'key {index - 1} was not presented')
                at, frame = min(visible, key=lambda pair: pair[0])
                if not frame['focused']:
                    raise RuntimeError('presentation samples include an unfocused terminal')
                measurements.append((at - start) * 1000)
            report('Synthetic Wayland key to presentation in isolated Mutter', measurements, 'milliseconds')
            print('  path: Mutter RemoteDesktop keysym → GUI → local daemon → PTY marker → wp_presentation; 1 warmup')
            writes = {e['output']: e for e in events(root / 'events.jsonl') if 'output' in e}
            for phase in ('sparse', 'scrolling'):
                first = {}
                for number, frame in frames.items():
                    write = writes.get(frame['marker'])
                    if (number in presented and write and write['record'] and write['phase'] == phase):
                        if not frame['focused']:
                            raise RuntimeError('output presentation includes an unfocused terminal')
                        first[frame['marker']] = min(presented[number], first.get(frame['marker'], math.inf))
                if len(first) < 2:
                    raise RuntimeError(f'insufficient presentation feedback for {phase} output')
                report(f'PTY {phase} output write to presentation',
                       [(at - writes[marker]['write_begin']) * 1000 for marker, at in first.items()],
                       'milliseconds')
        finally:
            try:
                if app is not None:
                    stop_app(app, binary, workspace, env)
            finally:
                if session is not None:
                    call(session, dest + '.Session', 'Stop')
                connection.close_sync(None)


def agent_fixture(directory):
    tty.setraw(0)
    with (directory / 'input.jsonl').open('w', buffering=1) as events:
        events.write(json.dumps({'ready': True}) + '\n')
        os.write(1, b'fixture-ready\r\n')
        index = 0
        while True:
            data = os.read(0, 4096)
            if not data:
                return
            at = time.monotonic_ns()
            for _ in data:
                index += 1
                events.write(json.dumps({'index': index, 'at': at}) + '\n')
                os.write(1, f'\x1b[Hprobe-{index:04}\x1b[K'.encode())


def run_agent(binary):
    with tempfile.TemporaryDirectory(prefix='nextty-latency-agent-') as temporary:
        root = Path(temporary)
        runtime = root / 'runtime'
        runtime.mkdir(mode=0o700)
        env = environment(root)
        env.update(XDG_RUNTIME_DIR=str(runtime), TERM='xterm-256color',
                   ANTHROPIC_AUTH_TOKEN='benchmark', SHELL='/bin/bash')
        started, go, done = threading.Event(), threading.Event(), threading.Event()
        errors = []

        class Model(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                    review = body.get('tool_choice', {}).get('type') == 'none'
                    if not review:
                        started.set()
                        if not go.wait(10):
                            raise TimeoutError('driver did not start agent stream')
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Connection', 'close')
                    self.end_headers()

                    def emit(event):
                        self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                        self.wfile.flush()

                    emit({'type': 'message_start', 'message': {'role': 'assistant', 'content': []}})
                    if review:
                        text = json.dumps({'status': 'complete', 'reason': 'Latency measurement finished.'})
                        emit({'type': 'content_block_start', 'index': 0,
                              'content_block': {'type': 'text', 'text': text}})
                    else:
                        emit({'type': 'content_block_start', 'index': 0,
                              'content_block': {'type': 'text', 'text': ''}})
                        start = time.monotonic()
                        for index in range(200):
                            emit({'type': 'content_block_delta', 'index': 0,
                                  'delta': {'type': 'text_delta', 'text': 'incremental agent text.\n'}})
                            time.sleep(max(0, start + (index + 1) / 50 - time.monotonic()))
                    emit({'type': 'content_block_stop', 'index': 0})
                    emit({'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}})
                    emit({'type': 'message_stop'})
                    done.set()
                except Exception as error:
                    errors.append(repr(error))
                    done.set()

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        config = root / 'agent.toml'
        config.write_text(f'base_url = "http://127.0.0.1:{server.server_port}"\nmodel = "benchmark"\n')
        workspace = f'agent-latency-{os.getpid()}'
        args = ['new', workspace, '--tui', '--config', str(config), '-e', sys.executable,
                str(Path(__file__).resolve()), '--agent-fixture', str(root)]
        terminal = OuterTerminal(binary, args, env, root)
        inputs = None
        try:
            terminal.expect(b'fixture-ready')
            inputs = (root / 'input.jsonl').open()
            assert json.loads(inputs.readline())['ready']
            terminal.send(b'\x1ba')
            drain(terminal, .1)
            terminal.send(b'benchmark streaming\r')
            wait_for(started.is_set, 'local model request', terminal.process)
            terminal.send(b'\x1bh')
            drain(terminal, .05)
            go.set()
            latency = []
            randomizer = random.Random(0)
            start = time.monotonic()
            for index in range(80):
                jitter = randomizer.uniform(-.01, .01) if 0 < index < 79 else 0
                drain(terminal, max(0, start + index / 20 + jitter - time.monotonic()))
                sent = time.monotonic_ns()
                terminal.send(b'x')
                row = []

                def received():
                    line = inputs.readline()
                    if line:
                        row.append(json.loads(line))
                    terminal.read(.001)
                    return row

                wait_for(received, 'raw input delivered during agent stream', terminal.process)
                if row[0]['at'] < sent:
                    raise RuntimeError('a control key reached the PTY fixture')
                latency.append((row[0]['at'] - sent) / 1e6)
                if index == 40 and b'incremental agent text.' not in terminal.output:
                    raise RuntimeError('agent stream did not render while active')
            wait_for(done.is_set, 'local model stream completion', terminal.process)
            if errors:
                raise RuntimeError(errors)
            report('Local TUI input delivery during agent streaming', latency, 'milliseconds')
            print('  path: outer PTY → frontend → daemon → child raw read; real owner/daemon and local SSE model; 80 probes')
        finally:
            go.set()
            try:
                stop_app(terminal.process, binary, workspace, env)
            finally:
                terminal.close()
                if inputs is not None:
                    inputs.close()
                server.shutdown()
                server.server_close()
                thread.join()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/nextty'))
    commands = parser.add_subparsers(dest='command', required=True)
    tui = commands.add_parser('tui', help='local or private-loopback-sshd raw-mode key echo')
    tui.add_argument('--ssh', action='store_true', help='measure through a private loopback OpenSSH server')
    gui = commands.add_parser('gui', help='Wayland key and PTY output to presentation in isolated Mutter')
    gui.add_argument('--session-record', type=Path, required=True,
                     help='JSON env record for the isolated Mutter session')
    commands.add_parser('agent', help='local input latency while a local SSE agent response streams')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if args.command == 'tui':
        run_tui(binary, 2000, args.ssh)
    elif args.command == 'gui':
        run_gui(binary, args.session_record, 100)
    else:
        run_agent(binary)


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--tui-fixture':
        tui_fixture()
    elif len(sys.argv) > 1 and sys.argv[1] == '--gui-fixture':
        gui_fixture(Path(sys.argv[2]), int(sys.argv[3]))
    elif len(sys.argv) > 1 and sys.argv[1] == '--agent-fixture':
        agent_fixture(Path(sys.argv[2]))
    elif len(sys.argv) > 1 and sys.argv[1] == '--ssh-fixture':
        ssh_fixture(Path(sys.argv[2]))
    else:
        main()
