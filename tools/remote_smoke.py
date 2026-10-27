#!/usr/bin/env python3
"""Exercise nextty's real remote CLI through a private loopback OpenSSH server.

Uses existing ssh/sshd/ssh-keygen, throwaway keys and isolated HOME/cache/runtime
paths. No user SSH configuration, daemon, session or shell configuration is used.
Only the TUI is driven here; this does not measure latency or validate GUI/IBus.
"""
import argparse
import errno
import fcntl
import getpass
import hashlib
import json
import os
from pathlib import Path
import pty
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
import time


ANSI = re.compile(rb'\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]')


def wait_for(predicate, description, timeout=8):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        result = predicate()
        if result:
            return result
        time.sleep(.02)
    raise TimeoutError(description)


def process_start(pid):
    try:
        return Path(f'/proc/{pid}/stat').read_text().rsplit(') ', 1)[1].split()[19]
    except FileNotFoundError:
        return None


def fixture(root):
    """The private sshd ForceCommand: preserve the real bridge and wire codec."""
    command = shlex.split(os.environ.get('SSH_ORIGINAL_COMMAND', ''))
    if command != ['nextty', '--bridge', '3']:
        raise RuntimeError('Unexpected bridge invocation: ' + repr(command))
    mode = (root / 'mode').read_text().strip()
    env = dict(os.environ, HOME=str(root / 'remote-home'),
               XDG_RUNTIME_DIR=str(root / 'r'),
               XDG_CACHE_HOME=str(root / 'remote-cache'),
               XDG_CONFIG_HOME=str(root / 'remote-config'),
               SHELL='/bin/sh', PS1='smoke> ', ENV='', LANG='C.UTF-8',
               PATH='/usr/bin:/bin')
    with (root / 'fixture.jsonl').open('a') as log:
        log.write(json.dumps({'pid': os.getpid(), 'command': command, 'mode': mode,
                              'ssh_pty_allocated': os.isatty(0),
                              'start_ticks': process_start(os.getpid())}) + '\n')
    if mode == 'missing':
        env['PATH'] = str(root / 'empty-bin')
        os.execve('/bin/sh', ['sh', '-c', 'exec ' + shlex.join(command)], env)
    if mode == 'invalid-protocol':
        os.write(1, b'not a nextty protocol\n')
        return
    if mode == 'unsupported-version':
        command[-1] = '255'
    elif mode != 'normal':
        raise RuntimeError('Unknown fixture mode: ' + mode)
    binary = (root / 'binary').read_text().strip()
    os.execve(binary, [binary, *command[1:]], env)


class Terminal:
    def __init__(self, lab, args, name):
        self.master, slave = pty.openpty()
        self.output = bytearray()
        self.log = (lab.output / (name + '.ansi')).open('wb')
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 30, 120, 0, 0))
        try:
            self.process = subprocess.Popen([str(lab.binary), *args], env=lab.env,
                                            cwd=lab.root / 'local-home', stdin=slave,
                                            stdout=slave, stderr=slave, start_new_session=True)
        finally:
            os.close(slave)
        lab.terminals.append(self)

    def read(self, seconds=.1):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if select.select([self.master], [], [], min(.02, max(0., until-time.monotonic())))[0]:
                try:
                    data = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                self.output.extend(data)
                self.log.write(data)
        return bytes(self.output)

    def expect(self, text, timeout=8):
        expected = text.encode() if isinstance(text, str) else text
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            self.read(.05)
            if expected in ANSI.sub(b'', self.output):
                return
            if self.process.poll() is not None:
                break
        tail = ANSI.sub(b'', self.output)[-1500:].decode(errors='replace')
        raise AssertionError(f'TUI did not display {text!r}; exit={self.process.poll()}; tail={tail!r}')

    def send(self, data):
        os.write(self.master, data.encode() if isinstance(data, str) else data)

    def command(self, text):
        self.send(text + '\r')

    def resize(self, rows, columns):
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))
        self.process.send_signal(signal.SIGWINCH)

    def detach(self):
        self.send(b'\x1bq')
        self.process.wait(timeout=5)
        self.read(.05)
        if self.process.returncode:
            raise AssertionError('TUI detach failed: ' + repr(self.process.returncode))

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=3)
        self.read(.02)
        os.close(self.master)
        self.log.close()


class Lab:
    def __init__(self, binary, output):
        self.binary = binary.resolve(strict=True)
        self.output = output.resolve()
        self.output.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix='run-', dir=self.output))
        self.terminals = []
        self.server = self.server_log = None
        self.env = dict(os.environ)
        for key in ['WAYLAND_DISPLAY', 'DISPLAY', 'SSH_AUTH_SOCK', 'NEXTTY_SOCKET']:
            self.env.pop(key, None)
        self.checks = []

    def start(self):
        root = self.root
        for name in ['local-home', 'l', 'local-cache', 'local-config',
                     'remote-home', 'r', 'remote-cache', 'remote-config',
                     'empty-bin', 'bin']:
            (root / name).mkdir(mode=0o700)
        (root / 'remote-home' / 'work').mkdir()
        (root / 'binary').write_text(str(self.binary))
        (root / 'mode').write_text('normal')
        for name in ['host', 'client']:
            subprocess.run(['ssh-keygen', '-q', '-t', 'ed25519', '-N', '', '-f', str(root / name)], check=True)
        with socket.socket() as reserved:
            reserved.bind(('127.0.0.1', 0))
            port = reserved.getsockname()[1]
        self.destination = getpass.getuser() + '@127.0.0.1:' + str(port)
        (root / 'sshd.conf').write_text('\n'.join([
            'ListenAddress 127.0.0.1', f'Port {port}', f'HostKey {root / "host"}',
            f'AuthorizedKeysFile {root / "client.pub"}', f'PidFile {root / "sshd.pid"}',
            # The throwaway key can live beneath a shared temporary directory.
            'StrictModes no',
            f'AllowUsers {getpass.getuser()}', 'PasswordAuthentication no',
            'KbdInteractiveAuthentication no', 'UsePAM no', 'UseDNS no',
            'PermitRootLogin no', 'PermitTTY no', 'PrintMotd no',
            'AllowTcpForwarding no', 'AllowAgentForwarding no', 'X11Forwarding no',
            f'SetEnv HOME={root / "remote-home"} ZDOTDIR={root / "remote-config"} ENV=/dev/null BASH_ENV=/dev/null',
            'ForceCommand ' + shlex.join([sys.executable, str(Path(__file__).resolve()), '--fixture', str(root)]),
            'LogLevel VERBOSE', '',
        ]))
        key = (root / 'host.pub').read_text().split()
        (root / 'known_hosts').write_text(f'[127.0.0.1]:{port} {key[0]} {key[1]}\n')
        (root / 'ssh_config').write_text('\n'.join([
            'Host *', f'  IdentityFile {root / "client"}', '  IdentitiesOnly yes',
            '  IdentityAgent none', '  BatchMode yes', '  StrictHostKeyChecking yes',
            f'  UserKnownHostsFile {root / "known_hosts"}', '  GlobalKnownHostsFile /dev/null',
            '  ConnectTimeout 2', '  ConnectionAttempts 1', '  LogLevel ERROR', '',
        ]))
        system_ssh = shutil.which('ssh')
        sshd = shutil.which('sshd')
        if not system_ssh or not sshd:
            raise RuntimeError('Existing OpenSSH ssh and sshd are required; nothing is installed by this test.')
        wrapper = root / 'bin' / 'ssh'
        wrapper.write_text('#!/bin/sh\nexec ' + shlex.join([system_ssh, '-F', str(root / 'ssh_config')]) + ' "$@"\n')
        wrapper.chmod(0o700)
        self.env.update(HOME=str(root / 'local-home'), XDG_RUNTIME_DIR=str(root / 'l'),
                        XDG_CACHE_HOME=str(root / 'local-cache'), XDG_CONFIG_HOME=str(root / 'local-config'),
                        PATH=str(root / 'bin') + ':/usr/bin:/bin', TERM='xterm-256color', LANG='C.UTF-8')
        subprocess.run([sshd, '-t', '-f', str(root / 'sshd.conf')], check=True)
        self.server_log = (self.output / 'sshd.log').open('w')
        self.server = subprocess.Popen([sshd, '-D', '-e', '-f', str(root / 'sshd.conf')],
                                       stdout=self.server_log, stderr=subprocess.STDOUT)
        def listening():
            if self.server.poll() is not None:
                raise RuntimeError('Private sshd exited; see sshd.log')
            try:
                with socket.create_connection(('127.0.0.1', port), timeout=.1):
                    return True
            except OSError:
                return False
        wait_for(listening, 'Private loopback sshd listener', 5)

    def cli(self, *args, success=True):
        result = subprocess.run([str(self.binary), *args], env=self.env,
                                cwd=self.root / 'local-home', capture_output=True, timeout=12)
        text = (result.stdout + result.stderr).decode(errors='replace')
        if success != (result.returncode == 0):
            raise AssertionError(f'{args!r}: exit={result.returncode}, output={text!r}')
        return text

    def file(self, name, expected=None):
        path = self.root / 'remote-home' / name
        wait_for(lambda: path.exists() and (expected is None or path.read_bytes() == expected),
                 'Remote fixture file ' + name)
        return path.read_bytes()

    def record(self, case, **details):
        self.checks.append(dict(case=case, passed=True, **details))
        print('PASS ' + case, flush=True)

    def exercise(self):
        target = self.destination
        empty = self.cli('list', target)
        if 'No workspaces.' not in empty:
            raise AssertionError('Remote list should initially be empty: ' + empty)
        self.record('remote list on an isolated empty target')
        first = Terminal(self, ['new', target, 'smoke', '--tui', '-e', '/bin/sh', '-i'], 'new')
        first.expect('smoke>')
        first.expect('Local')
        first.send(b'\x1bf')
        first.expect('Files are unavailable in remote sessions')
        self.record('remote TUI labels statistics as local and disables remote file navigation')
        first.command('pwd > "$HOME/cwd"; KEEP=retained; export KEEP; printf "%s%s\\n" SCREEN _ALPHA')
        first.expect('SCREEN_ALPHA')
        if self.file('cwd') != (str(self.root / 'remote-home') + '\n').encode():
            raise AssertionError('Remote new inherited the local cwd instead of remote HOME')
        self.record('remote new uses the remote HOME and renders shell output')
        listing = self.cli('list', target)
        if not re.search(r'^smoke\t1 tabs\t1 panes\t1 attached$', listing, re.M):
            raise AssertionError('Remote list did not show the attached session: ' + listing)
        self.record('remote list reports the real session and attached frontend')
        first.detach()
        attached = Terminal(self, ['attach', target, 'smoke', '--tui'], 'reattach')
        attached.expect('SCREEN_ALPHA')
        attached.command('printf "%s" "$KEEP" > "$HOME/keep-after"')
        self.file('keep-after', b'retained')
        self.record('detach and reattach preserve shell variables and visible history')
        explicit = Terminal(self, ['new', target, 'explicit', '--tui', '--working-directory', 'work',
                                   '-e', '/bin/sh', '-i'], 'explicit-cwd')
        explicit.expect('smoke>')
        explicit.command('pwd > "$HOME/explicit-cwd"')
        self.file('explicit-cwd', (str(self.root / 'remote-home' / 'work') + '\n').encode())
        explicit.detach()
        self.record('relative remote cwd resolves from remote HOME')
        controller = Terminal(self, ['attach', target, 'smoke', '--tui'], 'controller')
        controller.expect('SCREEN_ALPHA')
        attached.expect('Read-only')
        attached.command('printf forbidden > "$HOME/forbidden"')
        controller.command('printf controller > "$HOME/controller"')
        self.file('controller', b'controller')
        time.sleep(.1)
        if (self.root / 'remote-home' / 'forbidden').exists():
            raise AssertionError('A superseded read-only frontend still sent input')
        self.record('new attach takes control and the previous frontend is read-only')
        controller.resize(33, 113)
        time.sleep(.1)
        controller.command('stty size > "$HOME/size"')
        actual_size = self.file('size').decode().strip()
        if actual_size != '31 113':
            raise AssertionError('Remote PTY resize: expected 31 rows and 113 columns, got ' + actual_size)
        self.record('local TUI resize reaches the remote PTY', outer_rows=33, outer_columns=113,
                    pty_rows=31, pty_columns=113)
        text = '你好e\u0301👩\u200d💻'
        controller.send(b'\x1b[200~' + ('printf "%s" ' + shlex.quote(text) + ' > "$HOME/utf8"').encode() + b'\x1b[201~\r')
        self.file('utf8', text.encode())
        self.record('bracketed paste preserves UTF-8 wide, combining and ZWJ text')
        controller.process.kill()
        controller.process.wait(timeout=3)
        recovered = Terminal(self, ['attach', target, 'smoke', '--tui'], 'after-crash')
        recovered.expect('SCREEN_ALPHA')
        recovered.command('printf "%s" "$KEEP" > "$HOME/keep-crash"')
        self.file('keep-crash', b'retained')
        recovered.detach()
        attached.detach()
        self.record('abrupt local frontend exit leaves the remote shell attachable')
        for mode in ['missing', 'invalid-protocol', 'unsupported-version']:
            (self.root / 'mode').write_text(mode)
            error = self.cli('list', target, success=False)
            if not error.strip():
                raise AssertionError(mode + ' produced no error diagnostic')
            self.record(mode + ' produces a nonzero exit with a diagnostic', diagnostic=error.strip())
        (self.root / 'mode').write_text('normal')
        with socket.socket() as reserved:
            reserved.bind(('127.0.0.1', 0))
            reserved.listen()
            unreachable = getpass.getuser() + '@127.0.0.1:' + str(reserved.getsockname()[1])
            # Bound but not accepted: SSH must fail within the configured connect timeout.
            error = self.cli('list', unreachable, success=False)
        if not error.strip():
            raise AssertionError('SSH failure produced no diagnostic')
        self.record('SSH connection failure preserves an error diagnostic', diagnostic=error.strip())
        events = [json.loads(line) for line in (self.root / 'fixture.jsonl').read_text().splitlines()]
        if not events or any(event['ssh_pty_allocated'] for event in events):
            raise AssertionError('Bridge unexpectedly allocated a remote SSH PTY')
        self.record('real OpenSSH transports the bridge without allocating a remote PTY', connections=len(events))
        if (self.root / 'l' / 'nextty').exists():
            raise AssertionError('A remote operation unexpectedly created a local daemon directory')
        self.record('remote commands do not start a local workspace daemon')

    def close(self):
        errors = []
        for terminal in self.terminals:
            try:
                terminal.close()
            except Exception as error:
                errors.append(repr(error))
        # Control the private remote daemon directly, even if SSH was the failed case.
        env = dict(self.env, HOME=str(self.root / 'remote-home'),
                   XDG_RUNTIME_DIR=str(self.root / 'r'),
                   XDG_CACHE_HOME=str(self.root / 'remote-cache'))
        for name in ['smoke', 'explicit']:
            try:
                subprocess.run([str(self.binary), 'terminate', name], env=env,
                               capture_output=True, timeout=5)
            except subprocess.TimeoutExpired as error:
                errors.append(repr(error))
        # No session may have been created. Locate only a daemon using our exact private socket.
        for entry in Path('/proc').iterdir():
            if not entry.name.isdecimal():
                continue
            try:
                args = (entry / 'cmdline').read_bytes().split(b'\0')
                if b'--server' in args and any(arg.startswith(str(self.root / 'r').encode() + b'/') for arg in args):
                    os.kill(int(entry.name), signal.SIGTERM)
            except (FileNotFoundError, ProcessLookupError, PermissionError):
                pass
        if self.server is not None:
            self.server.terminate()
            try:
                self.server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.server.kill()
                self.server.wait(timeout=3)
        if self.server_log:
            self.server_log.close()
        fixture_log = self.root / 'fixture.jsonl'
        if fixture_log.exists():
            events = [json.loads(line) for line in fixture_log.read_text().splitlines()]
            for event in events:
                pid, start = event['pid'], event['start_ticks']
                if process_start(pid) == start:
                    try:
                        wait_for(lambda: process_start(pid) != start, 'SSH bridge exit', 2)
                    except TimeoutError:
                        os.kill(pid, signal.SIGTERM)
                        errors.append(f'SSH bridge {pid} did not exit when its client closed')
            shutil.copyfile(fixture_log, self.output / 'fixture.jsonl')
        for log in (self.root / 'r' / 'nextty').glob('*.log'):
            shutil.copyfile(log, self.output / log.name)
        shutil.rmtree(self.root)
        if errors:
            raise RuntimeError('Private fixture cleanup failed: ' + '; '.join(errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=Path('target/release/nextty'))
    parser.add_argument('--output-dir', type=Path, default=Path('target/remote-ssh-20261024/validation'))
    parser.add_argument('--fixture', type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.fixture:
        fixture(args.fixture)
        return
    lab = Lab(args.binary, args.output_dir)
    report = {'scope': __doc__, 'binary': str(lab.binary),
              'binary_sha256': hashlib.sha256(lab.binary.read_bytes()).hexdigest(), 'checks': lab.checks,
              'not_covered': ['GUI rendering and IBus', 'large command-block clipboard export',
                              'prediction/local echo (not implemented in this stage)', 'WAN latency and performance']}
    try:
        lab.start()
        lab.exercise()
        report['passed'] = True
    except BaseException as error:
        report.update(passed=False, failure=repr(error))
        raise
    finally:
        try:
            lab.close()
            report['cleanup'] = 'private keys, sockets, processes and runtime directory removed'
        except BaseException as error:
            report.update(passed=False, cleanup_failure=repr(error))
            raise
        finally:
            (lab.output / 'result.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')


if __name__ == '__main__':
    main()
