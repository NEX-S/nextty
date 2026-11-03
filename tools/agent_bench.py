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
        response_delay = directory / 'response-delay'
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
                if response_delay.exists():
                    time.sleep(float(response_delay.read_text()))
                os.write(1, f'\x1b[Hprobe-{index:04}\x1b[K'.encode())


def exit_zero_observed(call, result, pane, command):
    if (call['input'].get('pane') == pane and result.get('command') == command
            and result.get('running') is False and result.get('exit_code') == 0):
        return True
    return any(
        observation.get('pane') == pane and any(
            block['id'] == command and block['exit_code'] == 0
            for block in observation.get('command_blocks', []))
        for observation in [result, *result.get('panes', [])])


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
        self.execution_requests = 0
        self.goal_reviews = 0
        self.verified = threading.Event()
        self.expected = ""
        self.tool_mode = tool_mode
        self.tool_index = 0
        self.tool_sent = None
        self.tool_samples = {}
        self.submission = None
        self.command = None
        self.page = None
        self.output_prefix = ''
        self.errors = []
        self.ready = threading.Event()
        self.go = threading.Event()
        self.done = threading.Event()
        self.prefix = ''
        self.hz = 50
        lab = self

        class Model(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                try:
                    body = self.rfile.read(int(self.headers['Content-Length']))
                    lab.requests.append(len(body))
                    request = json.loads(body)
                    if request.get('tool_choice', {}).get('type') == 'none' and request['messages'][-1]['content'].startswith('Harness goal review:'):
                        self.goal_reply(request)
                        return
                    if request.get('tool_choice', {}).get('type') == 'none':
                        self.text_reply(lab.summary_text(request))
                        return
                    lab.execution_requests += 1
                    if lab.tool_mode:
                        self.tool_reply(request)
                        return
                    if lab.execution_requests == 2:
                        messages = request['messages']
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
                    prefix = lab.prefix
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

            def goal_reply(self, body):
                verdict = lab.goal_verdict(body)
                self.text_reply(json.dumps(verdict))
                lab.goal_reviews += 1

            def text_reply(self, text):
                self.send_response(200)
                self.send_header('Content-Type', 'text/event-stream')
                self.send_header('Connection', 'close')
                self.end_headers()
                events = [
                    {'type':'message_start','message':{'role':'assistant','content':[]}},
                    {'type':'content_block_start','index':0,'content_block':{'type':'text','text':text}},
                    {'type':'content_block_stop','index':0},
                    {'type':'message_delta','delta':{'stop_reason':'end_turn'}},
                    {'type':'message_stop'},
                ]
                for event in events:
                    self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                self.wfile.flush()

            def tool_reply(self, body):
                lab.ready.set()
                if not lab.go.wait(10):
                    raise TimeoutError('driver did not start tools')
                if lab.submission is None:
                    content = next(message['content'] for message in reversed(body['messages'])
                                   if isinstance(message['content'], str)
                                   and message['content'].startswith('Target terminal at submission'))
                    lab.submission, _ = json.JSONDecoder().raw_decode(content.split('\n', 1)[1])
                action = lab.next_tool(body)
                if action and 'pane' not in action[2] and not (action[1] == 'read' and action[2].get('source') == 'workspace'):
                    action[2]['pane'] = lab.submission['pane']
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

    def goal_verdict(self, body):
        return {'status':'complete', 'reason':'Fixture evidence verified.'}

    def summary_text(self, body):
        raise AssertionError('unexpected context summary request')

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
            if name == 'shell-ready' and result['shell_prompt'] is None:
                return ('shell-ready', 'read', {'source':'screen','wait':1})
            if name in ('shell-output', 'command-wait'):
                if name == 'shell-output':
                    assert result['input_queued'], result
                    self.command = result['command']
                if result['running']:
                    assert result['outcome'] == 'timeout', result
                    return ('command-wait', 'read', {'source':'command','command':self.command,'wait':5})
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
                assert result['input_queued'], result
                assert not result['running'] and result['exit_code'] == 0, result
                assert result['text'].strip() == 'tool-proof', result
            if name == 'input-verify':
                rows = [text for _, text in result['screen']['rows']]
                if 'input-proof' not in rows or result['shell_prompt'] is None:
                    return ('input-verify', 'read', {'source':'screen','wait':5})
                completed = next(block for block in result['command_blocks'] if block['has_output'])
                assert completed['exit_code'] == 0, result
        index = self.tool_index
        if (index == 0 or 5 <= index < 13) and (not self.tool_sent or self.tool_sent[0] != 'shell-ready'):
            return ('shell-ready', 'read', {'source':'screen'})
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
        self.wait(lambda: self.goal_reviews == 1, 'tools goal review')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'tools final answer rendered')
        end = self.sample()
        return {'case':'tools', 'seconds':time.monotonic() - start,
                'roundtrip_ms':{name:{'samples':len(values), 'median':statistics.median(values), 'max':max(values)}
                                for name,values in self.tool_samples.items()},
                'product_cpu_seconds':{role:end[role]['cpu_seconds']-value['cpu_seconds'] for role,value in begin.items()},
                'request_bytes':self.requests}

    def prepare_workspace(self, root, connection='local'):
        broken = 'def add(a, b):\n    return a - b\n'
        fixed = broken.replace('a - b', 'a + b')
        (root / 'tiny.py').write_text(broken)
        check = ('from pathlib import Path\nfrom tiny import add\n'
                 'with Path("verification-count").open("a") as log:\n    log.write("run\\n")\n'
                 'assert add(2, 3) == 5\nprint("workspace-check-proof")\n')
        (root / 'check.py').write_text(check)
        failure = subprocess.run([sys.executable, '-B', 'check.py'], cwd=root, capture_output=True)
        assert failure.returncode != 0 and b'AssertionError' in failure.stderr, failure
        (root / 'verification-count').unlink()
        corpus = root / 'search-space'
        corpus.mkdir()
        for index in range(8):
            (corpus / f'{index:02d}.txt').write_text('payload without the requested literal\n' * 100000)
        (corpus / '99.txt').write_text('bounded-match payload\n' * 1000)
        state = {'search_started':threading.Event(), 'search_result_bytes':0,
                 'roundtrip_ms':{}, 'shell_submissions':0, 'original_pane':None,
                 'new_pane':None, 'active_tab':None, 'command':None}

        def next_tool(body):
            original = self.submission['pane']
            if self.tool_sent:
                name, sent = self.tool_sent
                state['roundtrip_ms'][name] = (time.monotonic_ns() - sent) / 1e6
                block = body['messages'][-1]['content'][0]
                assert not block.get('is_error'), block
                result = json.loads(block['content'])
                if name in ('workspace-before', 'workspace-opened', 'workspace-final'):
                    assert result['connection'] == connection, result
                    panes = {pane['pane']:pane for pane in result['panes']}
                    assert panes[original]['cwd'] == str(root), result
                    assert panes[original]['shell_prompt'] is None, result
                    if name == 'workspace-before':
                        state['original_pane'] = original
                        state['active_tab'] = result['active_tab']
                        assert len(panes) == 1, result
                    else:
                        assert result['active_tab'] == state['active_tab'], result
                        assert len(panes) == 2 and panes[state['new_pane']]['cwd'] == str(root), result
                    if name == 'workspace-final':
                        assert any(command['exit_code'] == 0 for command in panes[state['new_pane']]['command_blocks']), result
                if name == 'find-bug':
                    assert len(result['matches']) == 1 and not result['truncated'], result
                    match = result['matches'][0]
                    assert match['path'] == str(root / 'tiny.py') and match['line'] == 2, result
                if name in ('read-bug', 'read-check', 'read-fixed'):
                    expected = check if name == 'read-check' else fixed if name == 'read-fixed' else broken
                    assert result['text'] == expected and result['offset'] == 0, result
                    assert result['next_offset'] == len(expected.encode()) and not result['has_more'], result
                if name == 'bounded-search':
                    assert result['truncated'] and result['matches'], result
                    assert all(match['path'] == str(corpus / '99.txt') for match in result['matches']), result
                    state['search_result_bytes'] = len(block['content'].encode())
                    assert state['search_result_bytes'] < 64 * 1024, result
                if name == 'fix-bug':
                    assert result['written'] and result['path'] == str(root / 'tiny.py'), result
                if name == 'open-terminal':
                    assert result['created'] and result['pane'] != original, result
                    state['new_pane'] = result['pane']
                    assert 'screen' in result and 'input_generation' in result, result
                    if result['shell_prompt'] is None:
                        return ('new-prompt', 'read', {'source':'screen','pane':state['new_pane'],'wait':5})
                if name == 'new-prompt' and result['shell_prompt'] is None:
                    return ('new-prompt', 'read', {'source':'screen','pane':state['new_pane'],'wait':5})
                if name in ('verify-fix', 'verify-wait'):
                    if name == 'verify-fix':
                        assert result['input_queued'], result
                        state['command'] = result['command']
                    if result['running']:
                        return ('verify-wait', 'read', {'source':'command','pane':state['new_pane'],
                                                       'command':state['command'],'wait':5})
                    assert result['exit_code'] == 0 and 'workspace-check-proof' in result['text'], result
                if name == 'original-screen':
                    assert result['pane'] == original and result['shell_prompt'] is None, result
            index = self.tool_index
            self.tool_index += 1
            if index == 0:
                inventory = self.submission['workspace']
                assert inventory['connection'] == connection, inventory
                assert any(pane['pane'] == original for pane in inventory['panes']), inventory
                return ('workspace-before', 'read', {'source':'workspace'})
            if index == 1:
                return ('find-bug', 'search_files', {'pane':original,'path':'tiny.py','query':'return a - b'})
            if index == 2:
                return ('read-bug', 'read_file', {'pane':original,'path':'tiny.py'})
            if index == 3:
                return ('read-check', 'read_file', {'pane':original,'path':'check.py'})
            if index == 4:
                state['search_started'].set()
                return ('bounded-search', 'search_files', {'pane':original,'path':'search-space','query':'bounded-match'})
            if index == 5:
                return ('fix-bug', 'edit_file', {'pane':original,'path':'tiny.py','old_text':'return a - b','new_text':'return a + b'})
            if index == 6:
                return ('read-fixed', 'read_file', {'pane':original,'path':'tiny.py'})
            if index == 7:
                return ('open-terminal', 'open_terminal', {'pane':original})
            if index == 8:
                return ('workspace-opened', 'read', {'source':'workspace'})
            if index == 9:
                state['shell_submissions'] += 1
                return ('verify-fix', 'shell', {'pane':state['new_pane'],'command':'python3 check.py'})
            if index == 10:
                return ('workspace-final', 'read', {'source':'workspace'})
            if index == 11:
                return ('original-screen', 'read', {'source':'screen','pane':original})
            return None

        self.tool_mode = True
        self.next_tool = next_tool
        return state

    def run_workspace(self):
        state = self.prepare_workspace(self.root)
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'Fix tiny.py and verify it while preserving my foreground application.\r')
        self.wait(self.ready.is_set, 'workspace model request')
        self.discover()
        os.write(self.master, b'\x1bh')
        self.drain(.05)
        begin = self.sample()
        start = time.monotonic()
        self.go.set()
        self.wait(state['search_started'].is_set, 'native file search started')
        latency = self.probes(False)
        self.wait(lambda: self.goal_reviews == 1, 'workspace verification reviewed')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'workspace final answer rendered')
        assert (self.root / 'verification-count').read_text() == 'run\n'
        assert state['shell_submissions'] == 1 and self.tool_index == 13, state
        end = self.sample()
        return {'case':'workspace-tools', 'seconds':time.monotonic()-start,
                'input_ms':latency, 'original_pane':state['original_pane'],
                'new_pane':state['new_pane'], 'verification_exit_code':0,
                'shell_submissions':state['shell_submissions'], 'foreground_preserved':True,
                'search_result_bytes':state['search_result_bytes'], 'roundtrip_ms':state['roundtrip_ms'],
                'product_cpu_seconds':{role:end[role]['cpu_seconds']-value['cpu_seconds'] for role,value in begin.items()},
                'request_bytes':self.requests}

    def run_goal(self):
        def next_tool(body):
            index = self.tool_index
            self.tool_index += 1
            if index == 0:
                return None
            if index == 1:
                assert 'goal-next-proof' in body['messages'][-1]['content']
                return ('goal-command', 'shell', {'command':"printf 'goal-continuation-proof\\n'"})
            result = json.loads(body['messages'][-1]['content'][0]['content'])
            assert result['exit_code'] == 0 and result['text'].strip() == 'goal-continuation-proof', result
            return None

        def verdict(body):
            if self.goal_reviews == 0:
                return {'status':'continue', 'reason':'goal-next-proof: verify with a terminal command.'}
            assert 'goal-continuation-proof' in json.dumps(body['messages'])
            return {'status':'complete', 'reason':'The terminal command verified the goal.'}

        self.next_tool = next_tool
        self.goal_verdict = verdict
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'verify goal continuation\r')
        self.wait(self.ready.is_set, 'goal model request')
        self.discover()
        start = time.monotonic()
        self.go.set()
        self.wait(lambda: self.goal_reviews == 2, 'goal continuation and verification')
        self.wait(lambda: b'goal-next-proof' in self.output, 'goal continuation displayed')
        self.drain(.1)
        assert self.tool_index == 3 and len(self.requests) == 5, self.requests
        return {'case':'default-goal', 'seconds':time.monotonic()-start,
                'goal_reviews':self.goal_reviews, 'terminal_actions':1,
                'request_bytes':self.requests}

    def run_compaction(self):
        content = ('{"payload":"context for bounded native reads"}\n' * 14000)[:512 * 1024]
        (self.root / 'context.txt').write_text(content)
        phase = 0
        summaries = []

        def summarize(body):
            text = json.dumps(body['messages'])
            summaries.append('active' if 'compaction-phase-1' in text else 'history')
            pending = set()
            for message in body['messages']:
                if isinstance(message['content'], list):
                    for block in message['content']:
                        if block['type'] == 'tool_use':
                            pending.add(block['id'])
                        elif block['type'] == 'tool_result':
                            pending.remove(block['tool_use_id'])
            assert not pending, 'summary split a tool exchange'
            assert body.get('cache_control') is None
            return ('Earlier context.txt read_file pages were checked byte-for-byte. '
                    'The file is unchanged; continue the requested range from the newest original result.')

        def next_tool(body):
            if phase == -1:
                assert 'compaction-phase-0' in json.dumps(body['messages']), 'settled history lost'
                return None
            assert f'compaction-phase-{phase}' in json.dumps(body['messages']), 'active request lost'
            if self.tool_index:
                block = body['messages'][-1]['content'][0]
                assert not block.get('is_error'), block
                result = json.loads(block['content'])
                offset = (self.tool_index - 1) * 16384
                assert result['offset'] == offset and result['next_offset'] == offset + 16384, result
                assert result['text'] == content[offset:offset + 16384], 'latest original result changed'
            if self.tool_index == (10 if phase == 0 else 32):
                return None
            offset = self.tool_index * 16384
            self.tool_index += 1
            return ('context-page', 'read_file', {'path':'context.txt','offset':offset})

        self.tool_mode = True
        self.next_tool = next_tool
        self.summary_text = summarize
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'compaction-phase-0: read the first ten pages\r')
        self.wait(self.ready.is_set, 'context model request')
        self.discover()
        begin = self.sample()
        self.go.set()
        self.wait(lambda: self.goal_reviews == 1, 'history turn completed')
        self.drain(.1)
        phase = -1
        for index in range(8):
            os.write(self.master, f'history-reuse-{index}: answer from earlier context without tools\r'.encode())
            self.wait(lambda: self.goal_reviews == index + 2, 'settled history reused')
            self.drain(.1)
        phase = 1
        self.tool_index = 0
        os.write(self.master, b'compaction-phase-1: read all thirty-two pages\r\x1bh')
        latency = self.probes(False)
        self.wait(lambda: self.goal_reviews == 10, 'compaction goal verified')
        assert self.tool_index == 32 and 'history' in summaries and 'active' in summaries, summaries
        assert (self.root / 'context.txt').read_text() == content
        end = self.sample()
        return {'case':'context-compaction', 'summary_kinds':summaries, 'file_pages_verified':42,
                'history_followups_verified':8,
                'input_ms':latency, 'request_bytes':self.requests,
                'product_cpu_seconds':{role:end[role]['cpu_seconds']-value['cpu_seconds']
                                       for role,value in begin.items()}}

    def run_transport_completion(self):
        lab = self
        samples = []

        class Model(self.model.RequestHandlerClass):
            def do_POST(self):
                first = not lab.requests
                super().do_POST()
                if first:
                    lab.done.wait(2)

        def next_tool(body):
            if self.tool_index == 0:
                self.tool_index += 1
                return ('completion-proof', 'shell', {'command':"printf 'transport-proof\\n'"})
            samples.append((time.monotonic_ns() - self.tool_sent[1]) / 1e6)
            block = body['messages'][-1]['content'][0]
            assert not block.get('is_error'), block
            result = json.loads(block['content'])
            assert result['input_queued'] and not result['running'] and result['exit_code'] == 0, result
            assert result['text'].strip() == 'transport-proof', result
            return None

        self.model.RequestHandlerClass = Model
        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'verify completed messages without waiting for HTTP EOF\r')
        self.wait(self.ready.is_set, 'completion model request')
        self.go.set()
        self.wait(lambda: self.goal_reviews == 1, 'completion goal verified')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'completion final answer rendered')
        assert len(self.requests) == 3 and self.tool_index == 1, self.requests
        return {'case':'transport-completion', 'http_tail_limit_seconds':2,
                'tool_result_ms':samples[0], 'terminal_submissions':1,
                'request_bytes':self.requests}

    def run_input_observation(self, delay):
        (self.root / 'response-delay').write_text(str(delay))
        sent = False
        reads = 0
        outcomes = []

        def next_tool(body):
            nonlocal sent, reads
            if not sent:
                sent = True
                return ('input', 'input', {'text':'x'})
            block = body['messages'][-1]['content'][0]
            assert not block.get('is_error'), block
            result = json.loads(block['content'])
            if self.tool_sent[0] == 'input':
                assert result['input_queued'], result
                outcomes.append(result.get('outcome'))
            if any('probe-0001' in text for _, text in result['screen']['rows']):
                return None
            reads += 1
            return ('observe', 'read', {'source':'screen','wait':5})

        self.tool_mode = True
        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'operate the foreground application\r')
        self.wait(self.ready.is_set, 'interactive input request')
        self.discover()
        start = time.monotonic()
        self.go.set()
        self.wait(lambda: self.goal_reviews == 1, 'interactive input result verified')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'input final answer rendered')
        return {'case':'input-observation', 'response_delay_seconds':delay,
                'seconds':time.monotonic()-start, 'tool_calls':1+reads,
                'read_calls':reads, 'input_outcomes':outcomes, 'request_bytes':self.requests}

    def run_input_cancel(self):
        (self.root / 'response-delay').write_text('0.5')
        sent = False

        def next_tool(body):
            nonlocal sent
            if not sent:
                sent = True
                return ('input', 'input', {'text':'x'})
            results = [block for message in body['messages'] if isinstance(message['content'], list)
                       for block in message['content'] if block['type'] == 'tool_result']
            assert len(results) == 1 and not results[0].get('is_error'), results
            result = json.loads(results[0]['content'])
            assert result['input_queued'] and result['observation_error'], result
            return None

        def delivered():
            line = self.input.readline()
            return bool(line and json.loads(line).get('index') == 1)

        self.tool_mode = True
        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'operate the foreground application\r')
        self.wait(self.ready.is_set, 'cancelable input request')
        self.go.set()
        self.wait(delivered, 'input delivered before stop')
        os.write(self.master, b'\x03')
        self.drain(.1)
        os.write(self.master, b'confirm the recorded input\r')
        self.wait(lambda: self.goal_reviews == 1, 'canceled input fact preserved')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'cancellation recovery rendered')
        assert self.execution_requests == 2, self.requests
        return {'case':'input-observation-cancel', 'tool_calls':1,
                'input_queued_preserved':True, 'request_bytes':self.requests}

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
                        assert 5000 <= elapsed < 15000, f'output woke command wait: {elapsed}ms'
                        assert result['text'] == ''.join(f'tick-{i:02d}\n' for i in range(1, 29)), result
            index = self.tool_index
            self.tool_index += 1
            if index == 0:
                return ('interactive-shell', 'shell', {'command': 'sleep 2; printf \'续读🙂 Continue?\'; read -r -s answer; printf \'追加λ=%s\\n软折行🙂\' "$answer"; printf \'w%.0s\' {1..300}; printf \'\\n\''})
            if index == 1:
                wait_begin.update(self.sample())
                return ('interactive-wait', 'read', {'source':'command','command':self.command,'wait':5})
            if index == 2:
                return ('interactive-eof', 'read', {'source':'command','command':self.command,'cursor':self.page})
            if index == 3:
                return ('interactive-answer', 'input', {'text':'yes\n'})
            if index == 4:
                return ('interactive-complete', 'read', {'source':'command','command':self.command,'cursor':self.page,'wait':5})
            if index == 5:
                return ('interactive-final-eof', 'read', {'source':'command','command':self.command,'cursor':self.page})
            if index == 6:
                return ('continuous-shell', 'shell', {'command': 'for i in $(seq 1 28); do printf \'tick-%02d\\n\' "$i"; sleep .25; done'})
            if index == 7:
                return ('continuous-wait', 'read', {'source':'command','command':self.command,'wait':15})
            return None

        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'benchmark interactive command waits\r')
        self.wait(self.ready.is_set, 'interactive model request')
        self.discover()
        start = time.monotonic()
        self.go.set()
        self.wait(self.done.is_set, 'interactive command observations', 20)
        self.wait(lambda: self.goal_reviews == 1, 'interactive goal review')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'interactive final answer rendered')
        assert len(self.requests) == 10, self.requests
        return {'case':'interactive-correctness', 'seconds':time.monotonic() - start,
                'roundtrip_ms':samples, 'interactive_exit_code':0, 'continuous_wait_requests':1,
                'wait_costs':wait_costs,
                'request_bytes':self.requests}

    def run_shell_observation_error(self):
        command = None

        def next_tool(body):
            nonlocal command
            index = self.tool_index
            self.tool_index += 1
            if index == 0:
                return ('clear-history', 'shell', {'command': "printf 'submitted\\n' >> submission-count; printf '\\033[3J'"})
            block = body['messages'][-1]['content'][0]
            if index == 1:
                assert not block.get('is_error'), block
                result = json.loads(block['content'])
                assert result['input_queued'], result
                command = result['command']
                assert isinstance(command, int), result
                assert result['observation_error'] == 'command block no longer retained', result
                return ('missing-command', 'read', {'source':'command', 'command':command})
            assert block.get('is_error') and block['content'] == 'command block no longer retained', block
            return None

        self.next_tool = next_tool
        os.write(self.master, b'\x1ba')
        self.drain(.05)
        os.write(self.master, b'benchmark accepted shell with cleared output\r')
        self.wait(self.ready.is_set, 'clear-history model request')
        self.go.set()
        self.wait(self.done.is_set, 'clear-history observation')
        self.wait(lambda: self.goal_reviews == 1, 'clear-history goal review')
        assert (self.root / 'submission-count').read_text() == 'submitted\n'
        assert self.execution_requests == 3, self.execution_requests
        return {'case':'shell-observation-error', 'command':command,
                'input_queued_preserved':True, 'terminal_submissions':1,
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
                return ('screen-before', 'read', {'source':'screen','wait':5})
            if index == 1:
                activity('screen-after', .15)
                return ('screen-after', 'read', {'source':'screen','wait':5})
            if index == 2:
                quiet_begin.update(self.sample())
                return ('screen-timeout', 'read', {'source':'screen','wait':5})
            if index == 3:
                return ('screen-query-hit', 'read', {'source':'screen','query':'screen-before'})
            if index == 4:
                return ('screen-query-miss', 'read', {'source':'screen','query':'absent-agent-benchmark-query'})
            if index == 5:
                activity('history-before')
                return ('history-before', 'read', {'source':'history','wait':5,'query':'history-before'})
            if index == 6:
                activity('history-after', .15)
                return ('history-after', 'read', {'source':'history','wait':5,'query':'history-after'})
            if index == 7:
                quiet_begin.update(self.sample())
                return ('history-timeout', 'read', {'source':'history','wait':5})
            if index == 8:
                os.write(self.master, b'\x1bhx')
                time.sleep(.15)
                return ('history-input-state', 'read', {'source':'history','wait':5,'query':'probe-0001'})
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
        self.wait(lambda: self.goal_reviews == 1, 'terminal wait goal review')
        self.wait(lambda: b'tools-finished-proof' in self.output, 'wait final answer rendered')
        return {'case':'wait-observations', 'seconds':time.monotonic() - start,
                'roundtrip_ms':samples, 'history_input_rejected':True,
                'quiet_waits':quiet_waits,
                'request_bytes':self.requests}

    def run_conversations(self):
        lab = self
        ready = set()
        verified = set()
        prefix = '**concurrent agent** `output`\n\n' * 4096
        expected = {}

        class Model(self.model.RequestHandlerClass):
            def do_POST(self):
                try:
                    raw = self.rfile.read(int(self.headers['Content-Length']))
                    lab.requests.append(len(raw))
                    body = json.loads(raw)
                    submission = next(message['content'] for message in reversed(body['messages'])
                                      if isinstance(message['content'], str)
                                      and message['content'].startswith('Target terminal at submission'))
                    name = submission.rsplit('\n', 1)[-1]
                    if body.get('tool_choice', {}).get('type') == 'none':
                        text = ''.join(block.get('text', '') for message in body['messages']
                                       if message['role'] == 'assistant' for block in message['content'])
                        assert text == expected[name], 'conversations mixed or lost streamed text'
                        self.goal_reply(body)
                        verified.add(name)
                        return
                    assert name not in ready, 'conversation generated an unexpected extra request'
                    expected[name] = prefix + (name + '\n') * 800 + name + '-finished'
                    ready.add(name)
                    if not lab.go.wait(10):
                        raise TimeoutError('driver did not start concurrent streams')
                    self.send_response(200)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Connection', 'close')
                    self.end_headers()

                    def emit(event):
                        self.wfile.write(('data: ' + json.dumps(event) + '\n\n').encode())
                        self.wfile.flush()

                    emit({'type':'message_start','message':{'role':'assistant','content':[]}})
                    emit({'type':'content_block_start','index':0,'content_block':{'type':'text','text':prefix}})
                    start = time.monotonic()
                    for index in range(800):
                        emit({'type':'content_block_delta','index':0,
                              'delta':{'type':'text_delta','text':name + '\n'}})
                        time.sleep(max(0, start + (index + 1) / 200 - time.monotonic()))
                    emit({'type':'content_block_delta','index':0,
                          'delta':{'type':'text_delta','text':name + '-finished'}})
                    emit({'type':'content_block_stop','index':0})
                    emit({'type':'message_delta','delta':{'stop_reason':'end_turn'}})
                    emit({'type':'message_stop'})
                except Exception as error:
                    lab.errors.append(repr(error))

        self.model.RequestHandlerClass = Model
        os.write(self.master, b'\x1ba')
        self.drain(.1)
        for index in range(4):
            if index:
                # From the editor, reverse through close-panel, close-tab and the conversation tabs.
                os.write(self.master, b'\x1b[Z' * (index + 3) + b'\r')
                self.drain(.05)
            os.write(self.master, f'conversation-{index}\r'.encode())
            self.wait(lambda: len(ready) == index + 1, 'concurrent conversation request')
        self.discover()
        os.write(self.master, b'\x1bh')
        self.drain(.05)
        begin = self.sample()
        self.go.set()
        latency = self.probes(False)
        self.wait(lambda: len(verified) == 4, 'all conversation histories verified')
        self.wait(lambda: b'conversation-3-finished' in self.output, 'active conversation rendered')
        end = self.sample()
        os.write(self.master, b'\x1ba\x1ba')
        self.drain(.05)
        for index in range(4):
            output_start = len(self.output)
            os.write(self.master, b'\x1b[Z' * (6 - index) + b'\r')
            marker = f'conversation-{index}-finished'.encode()
            self.wait(lambda: marker in self.output[output_start:], 'completed conversation rendered after switching')
        return {'case':'concurrent-conversations', 'conversations':4, 'input_ms':latency,
                'histories_verified':len(verified), 'rendered_after_switch':4, 'request_bytes':self.requests,
                'product_cpu_seconds':{role:end[role]['cpu_seconds']-value['cpu_seconds']
                                       for role,value in begin.items()},
                'product_pss_kib':{role:value['pss_kib'] for role,value in end.items()},
                'threads':{role:value['threads'] for role,value in end.items()}}

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
        self.hz = hz
        self.prefix = ('**agent** `shell` and history.\n\n' * 8192)[:prefix]
        if name in ('code-stream', 'open-code-stream'):
            self.prefix = '```text\n' + ('0123456789abcdef output line\n' * 8192)[:prefix] + '\n'
            if name == 'code-stream':
                self.prefix += '```\n'
        elif name == 'unicode-stream':
            paragraph = ('终端响应 延迟 agent 输出 文本🙂 e\u0301 ' * 20) + '\n\n'
            size = len(paragraph.encode())
            self.prefix = paragraph * (prefix // size) + ' ' * (prefix % size)
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
            self.wait(lambda: self.goal_reviews == 1, 'stream goal review')
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
            self.wait(lambda: self.goal_reviews == 2, 'history goal review')
            self.wait(lambda: b'state-verified-proof' in self.output, 'verification rendered')
        return {'case': name, 'seconds': elapsed, 'input_ms': latency,
                'product_cpu_seconds': {role: end[role]['cpu_seconds'] - before['cpu_seconds']
                                        for role, before in begin.items()},
                'product_pss_kib': {role: value['pss_kib'] for role, value in end.items()},
                'threads': {role: value['threads'] for role, value in end.items()},
                'frontend_output_bytes': output_bytes, 'request_bytes': self.requests,
                'history_verified': name != 'idle', 'stream_prefix_bytes': len(self.prefix.encode()), 'stream_deltas': hz * 4 if name != 'idle' else 0}

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
             ('long-stream', 128 * 1024, 200, True), ('code-stream', 128 * 1024, 200, True),
             ('open-code-stream', 128 * 1024, 200, True), ('unicode-stream', 128 * 1024, 200, True),
             ('long-hidden', 128 * 1024, 200, False)]
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
    lab = Lab(binary)
    try:
        lab.start()
        result = lab.run_conversations()
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
        result = lab.run_goal()
        report['cases'].append(result)
        print(json.dumps(result), file=sys.stderr, flush=True)
    finally:
        lab.close()
    lab = Lab(binary, tool_mode=True)
    try:
        lab.start()
        result = lab.run_transport_completion()
        report['cases'].append(result)
        print(json.dumps(result), file=sys.stderr, flush=True)
    finally:
        lab.close()
    lab = Lab(binary)
    try:
        lab.start()
        result = lab.run_compaction()
        report['cases'].append(result)
        print(json.dumps(result), file=sys.stderr, flush=True)
    finally:
        lab.close()
    for delay in (.02, .3):
        lab = Lab(binary)
        try:
            lab.start()
            result = lab.run_input_observation(delay)
            report['cases'].append(result)
            print(json.dumps(result), file=sys.stderr, flush=True)
        finally:
            lab.close()
    lab = Lab(binary)
    try:
        lab.start()
        result = lab.run_input_cancel()
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
    lab = Lab(binary, tool_mode=True)
    try:
        lab.start()
        result = lab.run_shell_observation_error()
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
    lab = Lab(binary)
    try:
        lab.start()
        result = lab.run_workspace()
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
