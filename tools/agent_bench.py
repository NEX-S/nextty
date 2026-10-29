#!/usr/bin/env python3
"""Benchmark real TUI/owner/daemon agent streaming and key delivery with a local SSE model.

Input latency is PTY injection to the child application's raw read, not physical
keyboard or GUI presentation latency. Product CPU/PSS excludes this driver,
the fixture and the simulated model. All workspaces and configuration are private.
"""
import argparse
import fcntl
import hashlib
import http.server
import json
import os
import pty
import random
from pathlib import Path
import select
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import tty

from native_perf import cost, percentile


def fixture(directory):
    tty.setraw(0)
    with (directory / 'input.jsonl').open('w', buffering=1) as events:
        events.write(json.dumps({'ready': True}) + '\n')
        os.write(1, b'fixture-ready\r\n')
        index = 0
        activity = ''
        control = directory / 'activity'
        while True:
            if control.exists():
                marker = control.read_text()
                if marker and marker != activity:
                    activity = marker
                    os.write(1, f'\r\n{marker}\r\n'.encode())
                    events.write(json.dumps({'activity': marker, 'at': time.monotonic_ns()}) + '\n')
            if not select.select([0], [], [], .01)[0]:
                continue
            data = os.read(0, 4096)
            if not data:
                break
            at = time.monotonic_ns()
            for _ in data:
                index += 1
                events.write(json.dumps({'index': index, 'at': at}) + '\n')
                os.write(1, f'\x1b[Hprobe-{index:04}\x1b[K'.encode())


class Lab:
    def __init__(self, binary, tool_mode=False):
        self.binary = binary
        self.temp = tempfile.TemporaryDirectory(prefix='nextty-agent-bench-')
        self.root = Path(self.temp.name)
        runtime = self.root / 'runtime'
        runtime.mkdir(mode=0o700)
        self.env = dict(os.environ, HOME=str(self.root), XDG_RUNTIME_DIR=str(runtime),
                        XDG_CACHE_HOME=str(self.root / 'cache'), XDG_CONFIG_HOME=str(self.root),
                        ANTHROPIC_AUTH_TOKEN='benchmark', TERM='xterm-256color', SHELL='/bin/bash', PS1='bench$ ')
        self.output = bytearray()
        self.process = None
        self.master = None
        self.input = None
        self.pids = {}
        self.requests = []
        self.verified = threading.Event()
        self.expected = ""
        self.tool_mode = tool_mode
        self.tool_index = 0
        self.tool_sent = None
        self.tool_samples = {}
        self.command = None
        self.page = None
        self.output_prefix = ''
        self.errors = []
        self.ready = threading.Event()
        self.go = threading.Event()
        self.done = threading.Event()
        self.prefix = 0
        self.hz = 50
        lab = self

        class Model(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    body = self.rfile.read(int(self.headers['Content-Length']))
                    lab.requests.append(len(body))
                    if lab.tool_mode:
                        self.tool_reply(json.loads(body))
                        return
                    if len(lab.requests) == 2:
                        messages = json.loads(body)['messages']
                        text = ''.join(block.get('text', '') for message in messages
                                       if message['role'] == 'assistant'
                                       for block in message['content'])
                        assert text == lab.expected, 'streamed model history changed'
                        self.send_response(200)
                        self.send_header('Content-Type', 'text/event-stream')
                        self.send_header('Connection', 'close')
                        self.end_headers()
                        events = [
                            {'type':'message_start','message':{'role':'assistant','content':[]}},
                            {'type':'content_block_start','index':0,'content_block':{'type':'text','text':'state-verified-proof'}},
                            {'type':'content_block_stop','index':0},
                            {'type':'message_delta','delta':{'stop_reason':'end_turn'}},
                            {'type':'message_stop'},
                        ]
                        for event in events:
                            self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                        self.wfile.flush()
                        lab.verified.set()
                        return
                    lab.ready.set()
                    if not lab.go.wait(10):
                        raise TimeoutError('driver did not start stream')
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Connection', 'close')
                    self.end_headers()

                    def emit(event):
                        self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                        self.wfile.flush()

                    emit({'type': 'message_start', 'message': {'role': 'assistant', 'content': []}})
                    prefix = ('**agent** `shell` and history.\n\n' * 8192)[:lab.prefix]
                    lab.expected = prefix + 'incremental agent text.\n' * (lab.hz * 4) + '\nstream-finished-proof'
                    emit({'type': 'content_block_start', 'index': 0,
                          'content_block': {'type': 'text', 'text': prefix}})
                    start = time.monotonic()
                    for index in range(lab.hz * 4):
                        emit({'type': 'content_block_delta', 'index': 0,
                              'delta': {'type': 'text_delta', 'text': 'incremental agent text.\n'}})
                        time.sleep(max(0, start + (index + 1) / lab.hz - time.monotonic()))
                    emit({'type': 'content_block_delta', 'index': 0,
                          'delta': {'type': 'text_delta', 'text': '\nstream-finished-proof'}})
                    emit({'type': 'content_block_stop', 'index': 0})
                    emit({'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}})
                    emit({'type': 'message_stop'})
                    lab.done.set()
                except (BrokenPipeError, ConnectionResetError):
                    lab.done.set()
                except Exception as error:
                    lab.errors.append(repr(error))
                    lab.done.set()

            def tool_reply(self, body):
                lab.ready.set()
                if not lab.go.wait(10):
                    raise TimeoutError('driver did not start tools')
                action = lab.next_tool(body)
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Connection', 'close')
                self.end_headers()
                block = {'type':'tool_use','id':f'tool-{len(lab.requests)}',
                         'name':action[1], 'input':action[2]} if action else {'type':'text','text':'tools-finished-proof'}
                events = [
                    {'type':'message_start','message':{'role':'assistant','content':[]}},
                    {'type':'content_block_start','index':0,'content_block':block},
                    {'type':'content_block_stop','index':0},
                    {'type':'message_delta','delta':{'stop_reason':'tool_use' if action else 'end_turn'}},
                    {'type':'message_stop'},
                ]
                lab.tool_sent = (action[0], time.monotonic_ns()) if action else None
                for event in events:
                    self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                self.wfile.flush()
                if not action:
                    lab.done.set()

        self.model = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Model)

    def start(self):
        threading.Thread(target=self.model.serve_forever, daemon=True).start()
        config = self.root / 'config.toml'
        config.write_text(f'base_url = "http://127.0.0.1:{self.model.server_port}"\nmodel = "benchmark"\n')
        self.master, slave = tty_pair()
        command = [str(self.binary), 'new', 'bench', '--config', str(config)]
        if not self.tool_mode:
            command += ['-e', sys.executable, str(Path(__file__).resolve()), '--fixture', str(self.root)]
        self.process = subprocess.Popen(command, env=self.env, stdin=slave, stdout=slave,
                                        stderr=slave, start_new_session=True, cwd=self.root)
        os.close(slave)
        marker = b'bench$' if self.tool_mode else b'fixture-ready'
        self.wait(lambda: marker in self.output, 'fixture ready')
        self.pids['frontend'] = self.process.pid
        self.discover()
        self.input = None if self.tool_mode else (self.root / 'input.jsonl').open()
        if self.input:
            assert json.loads(self.input.readline())['ready']

    def next_tool(self, body):
        if self.tool_sent:
            name, sent = self.tool_sent
            self.tool_samples.setdefault(name, []).append((time.monotonic_ns() - sent) / 1e6)
            block = next(block for message in reversed(body['messages'])
                         if isinstance(message['content'], list) for block in reversed(message['content'])
                         if block['type'] == 'tool_result')
            assert not block.get('is_error'), block
            result = json.loads(block['content'])
            if name in ('shell-output', 'command-wait'):
                if name == 'shell-output':
                    self.command = result['command']
                if result['running']:
                    assert result['outcome'] == 'timeout', result
                    return ('command-wait', 'read', {'source':'command','command':self.command,'wait':True})
                assert not result['running'] and result['exit_code'] == 0, result
            if name == 'page':
                self.output_prefix = result['text']
                expected = ''.join(f'history-{i:06d} payload-for-agent-benchmark\n' for i in range(1, 20001))
                assert self.output_prefix and expected.startswith(self.output_prefix)
                self.page = result['cursor']
                assert result['has_more'] and not result['incomplete'], result
            if name == 'page-next':
                expected = ''.join(f'history-{i:06d} payload-for-agent-benchmark\n' for i in range(1, 20001))
                assert result['text'] and expected.startswith(self.output_prefix + result['text'])
            if name == 'query-hit':
                expected = ''.join(f'history-{i:06d} payload-for-agent-benchmark\n' for i in range(9999, 10002))
                assert result['text'] == expected and result['matched'], result
                assert result['has_more'] and not result['incomplete'], result
            if name == 'query-miss':
                assert result['text'] == '' and not result['matched'], result
                assert not result['has_more'] and result['cursor'] is not None, result
            if name == 'input-ack':
                assert result['input_queued']
            if name == 'shell-roundtrip':
                assert not result['running'] and result['exit_code'] == 0, result
                assert result['text'].strip() == 'tool-proof', result
            if name == 'input-verify':
                rows = [text for _, text in result['screen']['rows']]
                if 'input-proof' not in rows or result['shell_prompt'] is None:
                    return ('input-verify', 'read', {'source':'screen','wait':True})
                completed = next(block for block in result['command_blocks'] if block['has_output'])
                assert completed['exit_code'] == 0, result
        index = self.tool_index
        self.tool_index += 1
        if index == 0:
            return ('shell-output', 'shell', {'command':"for i in $(seq 1 20000); do printf 'history-%06d payload-for-agent-benchmark\\n' \"$i\"; done"})
        if index == 1:
            return ('page', 'read', {'source':'command','command':self.command})
        if index == 2:
            return ('page-next', 'read', {'source':'command','command':self.command,'cursor':self.page})
        if index == 3:
            return ('query-hit', 'read', {'source':'command','command':self.command,'query':'history-010000'})
        if index == 4:
            return ('query-miss', 'read', {'source':'history','query':'absent-agent-benchmark-query'})
        if index < 13:
            return ('shell-roundtrip', 'shell', {'command':"printf 'tool-proof\\n'"})
        if index == 13:
            return ('input-ack', 'input', {'text':"printf 'input-proof\\n'\n"})
        if index == 14:
            return ('input-verify', 'read', {'source':'screen'})
        return None

    def run_tools(self):
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'benchmark tools\r')
        self.wait(self.ready.is_set, 'tool model request')
        self.discover()
        begin = self.sample()
        start = time.monotonic()
        self.go.set()
        self.wait(self.done.is_set, 'tools completed', 30)
        self.wait(lambda: b'tools-finished-proof' in self.output, 'tools final answer rendered')
        end = self.sample()
        return {'case':'tools', 'seconds':time.monotonic() - start,
                'roundtrip_ms':{name:{'samples':len(values), 'median':statistics.median(values), 'max':max(values)}
                                for name,values in self.tool_samples.items()},
                'product_cpu_seconds':{role:end[role]['cpu_seconds']-value['cpu_seconds'] for role,value in begin.items()},
                'request_bytes':self.requests}

    def run_interactive(self):
        samples = {}
        wait_costs = {}
        wait_begin = {}
        prefix = '续读🙂 Continue?'
        appended = '追加λ=yes\n软折行🙂' + 'w' * 300 + '\n'

        def next_tool(body):
            if self.tool_sent:
                name, sent = self.tool_sent
                elapsed = (time.monotonic_ns() - sent) / 1e6
                samples[name] = elapsed
                block = next(block for message in reversed(body['messages'])
                             if isinstance(message['content'], list) for block in reversed(message['content'])
                             if block['type'] == 'tool_result')
                assert not block.get('is_error'), block
                result = json.loads(block['content'])
                if name in ('interactive-shell', 'continuous-shell'):
                    assert result['running'] and result['outcome'] == 'timeout', result
                    self.command = result['command']
                if name == 'interactive-wait':
                    end = self.sample()
                    wait_costs[name] = {
                        'seconds': elapsed / 1000,
                        'product_cpu_seconds': {role: end[role]['cpu_seconds'] - value['cpu_seconds']
                                                for role, value in wait_begin.items()},
                        'product_pss_kib': {role: value['pss_kib'] for role, value in end.items()},
                    }
                    assert result['running'] and result['outcome'] == 'timeout', result
                    assert 4500 <= elapsed < 6500, elapsed
                    assert result['text'] == prefix, result
                    assert not result['has_more'] and not result['incomplete'], result
                    self.page = result['cursor']
                    assert self.page['offset'] == len(prefix.encode()), result
                    assert any('Continue?' in text for _, text in result['screen']['rows']), result
                    assert result['shell_prompt'] is None, result
                    assert result['screen']['cursor']['visible'], result
                    assert isinstance(result['input_generation'], int), result
                if name in ('interactive-eof', 'interactive-final-eof'):
                    assert result['text'] == '' and result['cursor'] == self.page, result
                    assert not result['has_more'] and not result['incomplete'], result
                if name == 'interactive-answer':
                    assert result['input_queued'], result
                if name in ('interactive-complete', 'continuous-wait'):
                    assert not result['running'] and result['exit_code'] == 0, result
                    assert result['outcome'] == 'completed', result
                    if name == 'interactive-complete':
                        assert result['text'] == appended, result
                        assert not result['has_more'] and not result['incomplete'], result
                        self.page = result['cursor']
                    else:
                        assert elapsed >= 2500, f'output woke command wait: {elapsed}ms'
                        assert result['text'] == ''.join(f'tick-{i:02d}\n' for i in range(1, 17)), result
            index = self.tool_index
            self.tool_index += 1
            if index == 0:
                return ('interactive-shell', 'shell', {'command': 'sleep 2; printf \'续读🙂 Continue?\'; read -r -s answer; printf \'追加λ=%s\\n软折行🙂\' "$answer"; printf \'w%.0s\' {1..300}; printf \'\\n\''})
            if index == 1:
                wait_begin.update(self.sample())
                return ('interactive-wait', 'read', {'source':'command','command':self.command,'wait':True})
            if index == 2:
                return ('interactive-eof', 'read', {'source':'command','command':self.command,'cursor':self.page})
            if index == 3:
                return ('interactive-answer', 'input', {'text':'yes\n'})
            if index == 4:
                return ('interactive-complete', 'read', {'source':'command','command':self.command,'cursor':self.page,'wait':True})
            if index == 5:
                return ('interactive-final-eof', 'read', {'source':'command','command':self.command,'cursor':self.page})
            if index == 6:
                return ('continuous-shell', 'shell', {'command': 'for i in $(seq 1 16); do printf \'tick-%02d\\n\' "$i"; sleep .25; done'})
            if index == 7:
                return ('continuous-wait', 'read', {'source':'command','command':self.command,'wait':True})
            return None

        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'benchmark interactive command waits\r')
        self.wait(self.ready.is_set, 'interactive model request')
        self.discover()
        start = time.monotonic()
        self.go.set()
        self.wait(self.done.is_set, 'interactive command observations', 16)
        self.wait(lambda: b'tools-finished-proof' in self.output, 'interactive final answer rendered')
        assert len(self.requests) == 9, self.requests
        return {'case':'interactive-correctness', 'seconds':time.monotonic() - start,
                'roundtrip_ms':samples, 'interactive_exit_code':0, 'continuous_wait_requests':1,
                'wait_costs':wait_costs,
                'request_bytes':self.requests}

    def run_waits(self):
        samples = {}
        quiet_waits = {}
        quiet_begin = {}
        full_screen = None

        def activity(marker, delay=0):
            def emit():
                (self.root / 'activity').write_text(marker)
            if delay:
                threading.Timer(delay, emit).start()
            else:
                emit()
                time.sleep(.15)

        def next_tool(body):
            nonlocal full_screen
            if self.tool_sent:
                name, sent = self.tool_sent
                elapsed = (time.monotonic_ns() - sent) / 1e6
                samples[name] = elapsed
                block = next(block for message in reversed(body['messages'])
                             if isinstance(message['content'], list) for block in reversed(message['content'])
                             if block['type'] == 'tool_result')
                if name == 'history-input':
                    assert block.get('is_error') and 'Terminal input changed' in block['content'], block
                else:
                    assert not block.get('is_error'), block
                    result = json.loads(block['content'])
                    if name in ('screen-before', 'screen-after'):
                        assert name in str(result['screen']['rows']), result
                    if name in ('history-before', 'history-after', 'history-input-state'):
                        marker = 'probe-0001' if name == 'history-input-state' else name
                        assert marker in result['text'] and result['matched'], result
                    if name in ('screen-before', 'screen-after', 'history-before', 'history-after', 'history-input-state'):
                        assert result['outcome'] == 'changed', result
                    if name in ('screen-timeout', 'history-timeout'):
                        end = self.sample()
                        quiet_waits[name] = {
                            'seconds': elapsed / 1000,
                            'product_cpu_seconds': {role: end[role]['cpu_seconds'] - value['cpu_seconds']
                                                    for role, value in quiet_begin.items()},
                            'product_pss_kib': {role: value['pss_kib'] for role, value in end.items()},
                        }
                        assert result['outcome'] == 'timeout', result
                        assert 4500 <= elapsed < 6500, elapsed
                    if name == 'screen-timeout':
                        full_screen = result['screen']
                    if name in ('screen-query-hit', 'screen-query-miss'):
                        assert result['outcome'] == 'current', result
                        screen = result['screen']
                        matches = [row for row, text in full_screen['rows'] if 'screen-before' in text] if name == 'screen-query-hit' else []
                        assert screen['matches'] == matches, result
                        cursor = screen['cursor']
                        selected = {row for match in matches for row in range(match - 1, match + 2)}
                        if cursor['visible']:
                            selected.add(cursor['row'])
                        rows = dict(screen['rows'])
                        assert rows.keys() <= selected, result
                        for row, text in full_screen['rows']:
                            if row in selected:
                                assert rows[row].rstrip(' ') == text.rstrip(' '), result
                        assert any(row not in selected for row, _ in full_screen['rows']), result
                    if name.endswith('-after'):
                        assert elapsed >= 100, f'{name} returned without new activity: {elapsed}ms'
                    if name.endswith('-before'):
                        assert elapsed < 1000, f'{name} missed existing activity: {elapsed}ms'
                    if name.startswith('input-ack'):
                        assert result['input_queued'], result
            index = self.tool_index
            self.tool_index += 1
            if index == 0:
                activity('screen-before')
                return ('screen-before', 'read', {'source':'screen','wait':True})
            if index == 1:
                activity('screen-after', .15)
                return ('screen-after', 'read', {'source':'screen','wait':True})
            if index == 2:
                quiet_begin.update(self.sample())
                return ('screen-timeout', 'read', {'source':'screen','wait':True})
            if index == 3:
                return ('screen-query-hit', 'read', {'source':'screen','query':'screen-before'})
            if index == 4:
                return ('screen-query-miss', 'read', {'source':'screen','query':'absent-agent-benchmark-query'})
            if index == 5:
                activity('history-before')
                return ('history-before', 'read', {'source':'history','wait':True,'query':'history-before'})
            if index == 6:
                activity('history-after', .15)
                return ('history-after', 'read', {'source':'history','wait':True,'query':'history-after'})
            if index == 7:
                quiet_begin.update(self.sample())
                return ('history-timeout', 'read', {'source':'history','wait':True})
            if index == 8:
                os.write(self.master, b'\x1bhx')
                time.sleep(.15)
                return ('history-input-state', 'read', {'source':'history','wait':True,'query':'probe-0001'})
            if index == 9:
                return ('history-input', 'input', {'text':'x'})
            if index == 10:
                return ('input-state', 'read', {'source':'screen'})
            if index in (11, 12):
                return (f'input-ack-{index}', 'input', {'text':'x'})
            return None

        self.tool_mode = True
        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'benchmark terminal waits\r')
        self.wait(self.ready.is_set, 'wait model request')
        self.discover()
        start = time.monotonic()
        self.go.set()
        self.wait(self.done.is_set, 'terminal wait observations', 18)
        self.wait(lambda: b'tools-finished-proof' in self.output, 'wait final answer rendered')
        return {'case':'wait-observations', 'seconds':time.monotonic() - start,
                'roundtrip_ms':samples, 'history_input_rejected':True,
                'quiet_waits':quiet_waits,
                'request_bytes':self.requests}

    def discover(self):
        children = Path(f'/proc/{self.process.pid}/task/{self.process.pid}/children').read_text().split()
        for child in children:
            command = Path(f'/proc/{child}/cmdline').read_bytes()
            if b'--server\0' in command:
                self.pids['daemon'] = int(child)
            elif b'--agent-owner\0' in command:
                self.pids['owner'] = int(child)

    def wait(self, predicate, label, seconds=12):
        until = time.monotonic() + seconds
        while not predicate():
            if self.errors:
                raise RuntimeError(self.errors)
            if self.process.poll() is not None:
                raise RuntimeError(f'frontend exited: {label}: {self.output[-1000:]!r}')
            if time.monotonic() >= until:
                raise TimeoutError(f'{label}: {self.output[-1000:]!r}')
            ready, _, _ = select.select([self.master], [], [], .001)
            if ready:
                self.output.extend(os.read(self.master, 65536))

    def drain(self, seconds):
        until = time.monotonic() + seconds
        self.wait(lambda: time.monotonic() >= until, 'drain', seconds + 2)

    def probes(self, expect_stream):
        latency = []
        sent_times = []
        start = time.monotonic()
        randomizer = random.Random(0)
        for index in range(80):
            # Avoid locking probes to the stream and display intervals.
            jitter = randomizer.uniform(-.01, .01) if 0 < index < 79 else 0
            self.drain(max(0, start + index / 20 + jitter - time.monotonic()))
            sent = time.monotonic_ns()
            sent_times.append(sent)
            os.write(self.master, b'x')
            row = []

            def received():
                line = self.input.readline()
                if line:
                    row.append(json.loads(line))
                return row

            self.wait(received, 'raw input delivered')
            assert row[0]['at'] >= sent, 'a control key reached the PTY fixture'
            latency.append((row[0]['at'] - sent) / 1e6)
            if index == 40 and expect_stream:
                assert b'incremental agent text.' in self.output, 'stream did not render while active'
        return dict(percentile(latency), samples=len(latency),
                    probe_span_seconds=(sent_times[-1] - sent_times[0]) / 1e9)

    def sample(self):
        return {role: cost(pid) for role, pid in self.pids.items()}

    def run(self, name, prefix=0, hz=50, visible=True):
        self.prefix, self.hz = prefix, hz
        if name != 'idle':
            os.write(self.master, b'\x1ba')
            self.drain(.05)
            os.write(self.master, b'benchmark streaming\r')
            self.wait(self.ready.is_set, 'model request')
            self.discover()
            # Alt+h focuses the terminal even when there is only one pane.
            os.write(self.master, b'\x1bh' if visible else b'\x1ba')
            self.drain(.05)
        begin = self.sample()
        output_start = len(self.output)
        start = time.monotonic()
        self.go.set()
        latency = self.probes(visible and name != 'idle')
        if name != 'idle':
            self.wait(self.done.is_set, 'model stream completed')
            if visible:
                self.wait(lambda: b'stream-finished-proof' in self.output, 'final answer rendered')
        self.drain(.1)
        elapsed = time.monotonic() - start
        end = self.sample()
        output_bytes = len(self.output) - output_start
        if name != 'idle':
            os.write(self.master, b'\x1ba\x1ba' if visible else b'\x1ba')
            self.drain(.05)
            os.write(self.master, b'verify stream history\r')
            self.wait(self.verified.is_set, 'stream history verified')
            self.wait(lambda: b'state-verified-proof' in self.output, 'verification rendered')
        return {'case': name, 'seconds': elapsed, 'input_ms': latency,
                'product_cpu_seconds': {role: end[role]['cpu_seconds'] - before['cpu_seconds']
                                        for role, before in begin.items()},
                'product_pss_kib': {role: value['pss_kib'] for role, value in end.items()},
                'threads': {role: value['threads'] for role, value in end.items()},
                'frontend_output_bytes': output_bytes, 'request_bytes': self.requests,
                'history_verified': name != 'idle', 'stream_prefix_bytes': prefix, 'stream_deltas': hz * 4 if name != 'idle' else 0}

    def close(self):
        self.go.set()
        try:
            if self.process:
                if self.process.poll() is None:
                    self.discover()
                subprocess.run([str(self.binary), 'terminate', 'bench'], env=self.env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
        finally:
            if self.process:
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait()
            for role in ('owner', 'daemon'):
                if role in self.pids:
                    try:
                        os.kill(self.pids[role], signal.SIGTERM)
                    except ProcessLookupError:
                        pass
            self.model.shutdown()
            self.model.server_close()
            if self.input:
                self.input.close()
            if self.master is not None:
                os.close(self.master)
            self.temp.cleanup()


def tty_pair():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
    return master, slave


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', nargs='?', type=Path, default=Path('target/release/nextty'))
    args = parser.parse_args()
    binary = args.binary.resolve()
    cases = [('idle', 0, 50, False), ('short-stream', 0, 50, True),
             ('long-stream', 128 * 1024, 200, True), ('long-hidden', 128 * 1024, 200, False)]
    report = {'binary': str(binary), 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
              'scope': __doc__, 'platform': list(os.uname()), 'samples_per_case': 80, 'cases': []}
    for name, prefix, hz, visible in cases:
        lab = Lab(binary)
        try:
            lab.start()
            result = lab.run(name, prefix, hz, visible)
            report['cases'].append(result)
            print(json.dumps(result), file=sys.stderr, flush=True)
        finally:
            lab.close()
    lab = Lab(binary, tool_mode=True)
    try:
        lab.start()
        result = lab.run_tools()
        report['cases'].append(result)
        print(json.dumps(result), file=sys.stderr, flush=True)
    finally:
        lab.close()
    lab = Lab(binary, tool_mode=True)
    try:
        lab.start()
        result = lab.run_interactive()
        report['cases'].append(result)
        print(json.dumps(result), file=sys.stderr, flush=True)
    finally:
        lab.close()
    lab = Lab(binary)
    try:
        lab.start()
        result = lab.run_waits()
        report['cases'].append(result)
        print(json.dumps(result), file=sys.stderr, flush=True)
    finally:
        lab.close()
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--fixture':
        fixture(Path(sys.argv[2]))
    else:
        main()
