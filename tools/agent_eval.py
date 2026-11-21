#!/usr/bin/env python3
"""Run billable real-model evaluations through isolated nextty TUI/owner/daemon processes.

The default task runs file repair with compaction and interactive long-output cases.
`--task soak` runs the 24-stage interactive task with one truncated HTTP stream.
Only synthetic workspace evidence is sent to the configured Messages API.
"""
import argparse
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import select
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
import tomllib
import urllib.error
import urllib.request


def model_headers():
    headers = {'Content-Type': 'application/json', 'Accept': 'text/event-stream',
               'anthropic-version': '2023-06-01'}
    if os.environ.get('ANTHROPIC_AUTH_TOKEN'):
        headers['Authorization'] = 'Bearer ' + os.environ['ANTHROPIC_AUTH_TOKEN']
    elif os.environ.get('ANTHROPIC_API_KEY'):
        headers['x-api-key'] = os.environ['ANTHROPIC_API_KEY']
    return headers


def prepare_model_request(payload, model, results):
    body = json.loads(payload)
    body['model'] = model
    for message in body['messages']:
        if isinstance(message['content'], list):
            for block in message['content']:
                if block['type'] == 'tool_result':
                    results[block['tool_use_id']] = block
    last = body['messages'][-1]['content']
    kind = ('review' if isinstance(last, str) and last.startswith('Harness goal review:')
            else 'summary' if body.get('tool_choice', {}).get('type') == 'none'
            else 'execute')
    return json.dumps(body).encode(), kind


def sse_lines(response):
    """Yield each raw SSE line with its parsed event once the event completes, else None."""
    data = []
    for line in response:
        event = None
        if line.startswith(b'data:'):
            data.append(line[5:].strip())
        elif not line.strip() and data:
            event = json.loads(b'\n'.join(data))
            data.clear()
        yield line, event


def absorb(record, calls, event):
    kind = event['type']
    if kind == 'message_start':
        record['usage'].update(event['message'].get('usage', {}))
    elif kind == 'message_delta':
        record['usage'].update(event.get('usage', {}))
    elif kind == 'content_block_start':
        block = event['content_block']
        if block['type'] == 'tool_use':
            calls[event['index']] = [block, []]
            record['tools'].append(block)
        elif block['type'] == 'text':
            record['text'] += block['text']
    elif kind == 'content_block_delta':
        delta = event['delta']
        if delta['type'] == 'text_delta':
            record['text'] += delta['text']
        elif delta['type'] == 'input_json_delta':
            calls[event['index']][1].append(delta['partial_json'])
    elif kind == 'content_block_stop' and event['index'] in calls:
        block, partial = calls.pop(event['index'])
        partial = ''.join(partial)
        if partial:
            block['input'] = json.loads(partial)


def exit_zero_observed(call, result, pane, command):
    if (call['input'].get('pane') == pane and result.get('command') == command
            and result.get('running') is False and result.get('exit_code') == 0):
        return True
    return any(
        observation.get('pane') == pane and any(
            block['id'] == command and block['exit_code'] == 0
            for block in observation.get('command_blocks', []))
        for observation in [result, *result.get('panes', [])])


def agent_fixture(directory):
    import tty
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


class Lab:
    def __init__(self, binary, tool_mode=False):
        self.binary = binary
        self.temp = tempfile.TemporaryDirectory(prefix='nextty-agent-eval-')
        self.root = Path(self.temp.name)
        runtime = self.root / 'runtime'
        runtime.mkdir(mode=0o700)
        self.env = dict(os.environ, HOME=str(self.root), XDG_RUNTIME_DIR=str(runtime),
                        XDG_CACHE_HOME=str(self.root / 'cache'), XDG_CONFIG_HOME=str(self.root),
                        ANTHROPIC_AUTH_TOKEN='benchmark', TERM='xterm-256color',
                        SHELL='/bin/bash', PS1='bench$ ')
        self.output = bytearray()
        self.process = self.master = None
        self.pids = {}
        self.tool_mode = tool_mode
        self.errors = []
        self.model = http.server.ThreadingHTTPServer(('127.0.0.1', 0), http.server.BaseHTTPRequestHandler)

    def start(self):
        threading.Thread(target=self.model.serve_forever, daemon=True).start()
        config = self.root / 'config.toml'
        config.write_text(f'base_url = "http://127.0.0.1:{self.model.server_port}"\nmodel = "benchmark"\n')
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 140, 0, 0))
        command = [str(self.binary), 'new', 'bench', '--tui', '--config', str(config)]
        if not self.tool_mode:
            command += ['-e', sys.executable, str(Path(__file__).resolve()), '--fixture', str(self.root)]
        self.process = subprocess.Popen(command, env=self.env, stdin=slave, stdout=slave,
                                        stderr=slave, start_new_session=True, cwd=self.root)
        os.close(slave)
        marker = b'bench$' if self.tool_mode else b'fixture-ready'
        self.wait(lambda: marker in self.output, 'fixture ready')

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

    def discover(self):
        children = Path(f'/proc/{self.process.pid}/task/{self.process.pid}/children').read_text().split()
        for child in children:
            command = Path(f'/proc/{child}/cmdline').read_bytes()
            if b'--server\0' in command:
                self.pids['daemon'] = int(child)
            elif b'--agent-owner\0' in command:
                self.pids['owner'] = int(child)

    def close(self):
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
            if self.master is not None:
                os.close(self.master)
            self.temp.cleanup()


def run_case(binary, config, interactive):
    lab = Lab(binary, tool_mode=interactive)
    records = []
    results = {}
    finished = threading.Event()
    headers = model_headers()

    class Proxy(http.server.BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            payload, kind = prepare_model_request(
                self.rfile.read(int(self.headers['Content-Length'])), config['model'], results)
            record = {'kind': kind, 'request_bytes': len(payload), 'usage': {},
                      'tools': [], 'text': ''}
            records.append(record)
            request = urllib.request.Request(
                (config.get('base_url') or 'https://api.anthropic.com').rstrip('/') + '/v1/messages',
                data=payload, headers=headers)
            start = time.monotonic()
            try:
                with urllib.request.urlopen(request, timeout=150) as response:
                    self.send_response(response.status)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.end_headers()
                    calls = {}
                    for line, event in sse_lines(response):
                        if event:
                            absorb(record, calls, event)
                        self.wfile.write(line)
                        self.wfile.flush()
                if kind == 'review':
                    record['verdict'] = json.loads(record['text'])
            except urllib.error.HTTPError as error:
                record['error'] = repr(error)
                payload = error.read()
                self.send_response(error.code)
                self.send_header('Content-Length', str(len(payload)))
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(payload)
                self.close_connection = True
            except Exception as error:
                lab.errors.append(repr(error))
                finished.set()
                raise
            finally:
                record['seconds'] = time.monotonic() - start
                print(json.dumps({key: value for key, value in record.items() if key != 'text'}),
                      file=sys.stderr, flush=True)
                if record.get('verdict', {}).get('status') in ('complete', 'blocked'):
                    finished.set()

    lab.model.RequestHandlerClass = Proxy
    try:
        lab.start()
        if interactive:
            fixture = '''from pathlib import Path
import json
import time
with Path("launches").open("a") as log:
    log.write("run\\n")
for index in range(20000):
    if index == 14000:
        print("verification code: violet-orbit-731")
    print(f"log-{index:05d}: processing ordinary terminal output")
answer = input("Enter the verification code printed in the output: ")
time.sleep(2)
assert answer == "violet-orbit-731", answer
Path("interactive-result.json").write_text(json.dumps({"answer": answer, "completed": True}))
print("interactive-check-passed")
'''
            (lab.root / 'interactive.py').write_text(fixture)
            prompt = ('Run python3 interactive.py exactly once. Retrieve the verification code from '
                      'its terminal output, enter it at the interactive prompt, and wait for a '
                      'successful exit. Do not read or modify the program source or create its '
                      'result files yourself. Use a final read with source screen to verify the command exit code. '
                      'Finish with a brief report.')
        else:
            (lab.root / 'tiny.py').write_text('def add(a, b):\n    return a - b\n')
            marker = 'history-' + os.urandom(8).hex()
            fixture = f'''from pathlib import Path
from tiny import add
with Path("checks").open("a") as log:
    log.write("run\\n")
assert add(2, 3) == 5
Path("verified").write_text("passed")
print("check passed {marker}")
'''
            (lab.root / 'check.py').write_text(fixture)
            prompt = ('Fix the addition bug in tiny.py and run check.py to verify. Preserve the '
                      'current foreground application and terminal focus. Do not change check.py. '
                      'Remember the verification marker printed by check.py. Finish with a brief report.')
        os.write(lab.master, b'\x1ba')
        lab.drain(.1)
        start = time.monotonic()
        os.write(lab.master, prompt.encode() + b'\r')
        lab.wait(finished.is_set, 'real model task completion', 900)
        if lab.errors:
            raise RuntimeError(lab.errors)
        lab.drain(.2)
        if not interactive:
            context = ('context pressure payload, unrelated to the verification marker\n' * 10000)[:512 * 1024]
            (lab.root / 'context.txt').write_text(context)
            first = len(records)
            finished.clear()
            os.write(lab.master, b'Read every byte of context.txt. Use only read_file on context.txt, one call per response. '
                     b'Then answer which verification marker check.py printed in the previous task, '
                     b'using conversation history. Do not use tools to retrieve the marker.\r')
            lab.wait(finished.is_set, 'verification marker recalled through compaction', 900)
            if lab.errors:
                raise RuntimeError(lab.errors)
            lab.drain(.2)
            followup = records[first:]
            answer = next((record['text'] for record in reversed(followup)
                           if record['kind'] == 'execute' and not record['tools']), '')
            context_reads = [json.loads(results[call['id']]['content']) for record in followup
                             for call in record['tools'] if call['name'] == 'read_file'
                             and not results[call['id']].get('is_error')]
            context_reads.sort(key=lambda read: read['offset'])
            context_end = 0
            for read in context_reads:
                if read['offset'] > context_end or read['text'] != context[read['offset']:read['next_offset']]:
                    break
                context_end = max(context_end, read['next_offset'])
        calls = [call for record in records for call in record['tools']]
        if interactive:
            launch = next(call for call in calls if call['name'] == 'shell'
                          and call['input']['command'] == 'python3 interactive.py')
            command = json.loads(results[launch['id']]['content'])['command']
            command_results = [(call, json.loads(results[call['id']]['content'])) for call in calls
                               if call['id'] in results and not results[call['id']].get('is_error')]
            outcome = {
                'launched_once': (lab.root / 'launches').read_text() == 'run\n',
                'correct_result': json.loads((lab.root / 'interactive-result.json').read_text()) == {
                    'answer': 'violet-orbit-731', 'completed': True},
                'fixture_unchanged': (lab.root / 'interactive.py').read_text() == fixture,
                'exit_zero_observed': any(exit_zero_observed(call, result, launch['input']['pane'], command)
                                          for call, result in command_results),
                'screen_exit_zero_observed': any(
                    exit_zero_observed(call, result, launch['input']['pane'], command)
                    for call, result in command_results
                    if call['name'] == 'read' and call['input'].get('source') == 'screen'),
                'terminal_input_used': any(call['name'] == 'input' and
                                           'violet-orbit-731' in call['input']['text'] for call in calls),
                'terminal_code_retrieved': any(call['name'] == 'read' and
                                               call['input'].get('source') in ('command', 'history') and
                                               'violet-orbit-731' in results[call['id']]['content'] and
                                               not results[call['id']].get('is_error') for call in calls),
            }
        else:
            checked_by_agent = ((lab.root / 'verified').read_text() == 'passed' and
                                (lab.root / 'checks').read_text() == 'run\n')
            untouched = (lab.root / 'input.jsonl').read_text().splitlines() == ['{"ready": true}']
            os.write(lab.master, b'\x1bh')
            lab.drain(.05)
            os.write(lab.master, b'x')
            lab.wait(lambda: len((lab.root / 'input.jsonl').read_text().splitlines()) == 2,
                     'original foreground receives input in current tab')
            check = subprocess.run([sys.executable, '-B', 'check.py'], cwd=lab.root,
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            outcome = {
                'checked_by_agent': checked_by_agent,
                'independent_check_exit_zero': check.returncode == 0,
                'fixture_unchanged': (lab.root / 'check.py').read_text() == fixture,
                'foreground_untouched': untouched,
                'focus_and_foreground_preserved': True,
                'history_marker_recalled': marker in answer,
                'followup_only_read_context_file': all(
                    call['name'] == 'read_file' and (lab.root / call['input']['path']).resolve() == lab.root / 'context.txt'
                    for record in followup for call in record['tools']),
                'context_file_read_completely': context_end == len(context),
                'summary_exercised': any(record['kind'] == 'summary' for record in followup),
            }
        report = {'case': 'interactive-long-output' if interactive else 'occupied-pane-file-repair',
                  'seconds': time.monotonic() - start, 'outcome': outcome,
                  'passed': all(outcome.values()) and records[-1]['verdict']['status'] == 'complete',
                  'tool_errors': [result for result in results.values() if result.get('is_error')],
                  'tool_result_bytes': sum(len(result['content'].encode()) for result in results.values()),
                  'requests': records}
        return report
    finally:
        lab.close()


SOAK_FIXTURE = '''from pathlib import Path
import json
import secrets
import time
with Path("launches").open("a") as log:
    log.write("run\\n")
for stage in range(1, 25):
    print(f"Preparing stage {stage}/24...", flush=True)
    time.sleep(8)
    token = secrets.token_hex(5)
    answer = input(f"Stage {stage}/24 verification token: {token}\\nEnter that token: ")
    assert answer == token, (stage, answer)
    with Path("ledger.jsonl").open("a") as log:
        log.write(json.dumps({"stage": stage, "token": token, "answer": answer}) + "\\n")
    print(f"Stage {stage}/24 accepted", flush=True)
Path("finished").write_text("all 24 stages passed")
print("long-task-check-passed", flush=True)
'''


def run_soak(binary, config):
    evidence = Path(tempfile.mkdtemp(prefix='nextty-agent-soak-'))
    lab = Lab(binary)
    records = []
    results = {}
    finished = threading.Event()
    fault = None
    latencies = []
    headers = model_headers()

    class Proxy(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def log_message(self, *_):
            pass

        def do_POST(self):
            nonlocal fault
            payload, kind = prepare_model_request(
                self.rfile.read(int(self.headers['Content-Length'])), config['model'], results)
            index = len(records)
            record = {'index': index, 'kind': kind, 'usage': {}, 'tools': [], 'text': '',
                      'response_bytes': 0, 'event_counts': {},
                      'request_sha256': hashlib.sha256(payload).hexdigest(),
                      'at': time.monotonic()}
            records.append(record)
            (evidence / f'request-{index:03}.json').write_bytes(payload)
            ledger = lab.root / 'ledger.jsonl'
            inject = fault is None and kind == 'execute' and ledger.exists() and len(ledger.read_text().splitlines()) >= 4
            request = urllib.request.Request(
                (config.get('base_url') or 'https://api.anthropic.com').rstrip('/') + '/v1/messages',
                data=payload, headers=headers)
            try:
                with (
                    urllib.request.urlopen(request, timeout=150) as response,
                    (evidence / f'stream-{index:03}.sse').open('wb') as raw,
                ):
                    record['http_status'] = response.status
                    record['headers_seconds'] = time.monotonic() - record['at']
                    self.send_response(response.status)
                    self.send_header('Content-Type', 'text/event-stream')
                    self.send_header('Transfer-Encoding', 'chunked')
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    calls = {}
                    for line, event in sse_lines(response):
                        raw.write(line)
                        raw.flush()
                        if not record['response_bytes']:
                            record['first_line_seconds'] = time.monotonic() - record['at']
                        record['response_bytes'] += len(line)
                        if event:
                            counts = record['event_counts']
                            counts[event['type']] = counts.get(event['type'], 0) + 1
                            absorb(record, calls, event)
                            if event['type'] == 'message_stop':
                                record['message_stop'] = True
                                record['message_stop_seconds'] = time.monotonic() - record['at']
                        self.wfile.write(f'{len(line):x}\r\n'.encode() + line + b'\r\n')
                        self.wfile.flush()
                        if inject and event and event['type'] == 'content_block_delta':
                            record['injected_truncation'] = True
                            fault = {'request': index, 'at': time.monotonic(),
                                     'stages': len(ledger.read_text().splitlines())}
                            self.close_connection = True
                            self.connection.shutdown(socket.SHUT_RDWR)
                            break
                    else:
                        self.wfile.write(b'0\r\n\r\n')
                        self.wfile.flush()
                if kind == 'review' and record.get('message_stop'):
                    record['verdict'] = json.loads(record['text'])
            except urllib.error.HTTPError as error:
                record['error'] = repr(error)
                record['http_status'] = error.code
                record['headers_seconds'] = time.monotonic() - record['at']
                payload = error.read()
                record['response_bytes'] = len(payload)
                self.send_response(error.code)
                self.send_header('Content-Length', str(len(payload)))
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(payload)
                self.close_connection = True
            except Exception as error:
                record['error'] = repr(error)
                self.close_connection = True
                self.connection.close()
            finally:
                record['seconds'] = time.monotonic() - record['at']
                (evidence / f'response-{index:03}.json').write_text(json.dumps(record))
                print(json.dumps(record), file=sys.stderr, flush=True)
                if record.get('verdict', {}).get('status') in ('complete', 'blocked'):
                    finished.set()

    lab.model.RequestHandlerClass = Proxy
    try:
        lab.start()
        shutil.copyfile(f'/proc/{lab.process.pid}/exe', evidence / 'nextty-tested')
        with (evidence / 'nextty-tested').open('rb') as source:
            binary_sha256 = hashlib.file_digest(source, 'sha256').hexdigest()
        (lab.root / 'stages.py').write_text(SOAK_FIXTURE)
        os.write(lab.master, b'\x1ba')
        lab.drain(.1)
        prompt = ('Run python3 stages.py exactly once in a new terminal, preserving my current '
                  'foreground program and active tab. Complete all 24 stages by reading each '
                  'fresh verification token from its live terminal output and entering that token '
                  'with input. Each stage prepares for 8 seconds. Use bounded screen waits '
                  'while it awaits interaction, then verify final command exit status. Do not '
                  'read or change the program source, ledger or finished marker. Keep going '
                  'until all stages pass, then give a brief report.')
        os.write(lab.master, prompt.encode() + b'\r')
        os.write(lab.master, b'\x1bh')
        lab.drain(.05)
        start = time.monotonic()
        next_probe = start + 5
        while not finished.is_set() and time.monotonic() - start < 900:
            lab.drain(.05)
            now = time.monotonic()
            if now >= next_probe:
                before = len((lab.root / 'input.jsonl').read_text().splitlines())
                sent = time.monotonic_ns()
                os.write(lab.master, b'x')
                lab.wait(lambda: len((lab.root / 'input.jsonl').read_text().splitlines()) == before + 1,
                         'foreground input during long task', 3)
                event = json.loads((lab.root / 'input.jsonl').read_text().splitlines()[-1])
                latencies.append((event['at'] - sent) / 1e6)
                next_probe = now + 5
            if fault and len(records) == fault['request'] + 1 and now - fault['at'] >= 30:
                break
        lab.drain(.2)
        ledger_path = lab.root / 'ledger.jsonl'
        ledger = [json.loads(line) for line in ledger_path.read_text().splitlines()] if ledger_path.exists() else []
        launches = (lab.root / 'launches').read_text().splitlines() if (lab.root / 'launches').exists() else []
        calls = [call for record in records if record.get('message_stop') for call in record['tools']]
        launch = next((call for call in calls
                       if call['name'] == 'shell' and call['input']['command'] == 'python3 stages.py'
                       and call['id'] in results and not results[call['id']].get('is_error')), None)
        command = json.loads(results[launch['id']]['content'])['command'] if launch else None
        command_results = [(call, json.loads(results[call['id']]['content'])) for call in calls
                           if call['id'] in results and not results[call['id']].get('is_error')]
        outcome = {'all_stages': [entry['stage'] for entry in ledger] == list(range(1, 25)),
                   'tokens_correct': all(entry['token'] == entry['answer'] for entry in ledger),
                   'launched_once': launches == ['run'],
                   'fixture_unchanged': (lab.root / 'stages.py').read_text() == SOAK_FIXTURE,
                   'finished_marker': (lab.root / 'finished').exists(),
                   'exit_zero_observed': command is not None and any(
                       exit_zero_observed(call, result, launch['input']['pane'], command)
                       for call, result in command_results),
                   'network_fault_injected': fault is not None,
                   'model_resumed_after_fault': fault is not None and len(records) > fault['request'] + 1,
                   'same_request_retried': fault is not None and len(records) > fault['request'] + 1
                   and records[fault['request']]['request_sha256'] == records[fault['request'] + 1]['request_sha256'],
                   'foreground_responsive': bool(latencies),
                   'at_least_20_execution_rounds': sum(record['kind'] == 'execute' for record in records) >= 20,
                   'goal_review_complete': records[-1].get('verdict', {}).get('status') == 'complete'}
        report = {'binary_sha256': binary_sha256,
                  'model': config['model'], 'seconds': time.monotonic() - start,
                  'stages_completed': len(ledger), 'launches': len(launches), 'outcome': outcome,
                  'passed': all(outcome.values()) and finished.is_set(),
                  'curl18_visible': b'Transferred a partial file' in lab.output,
                  'foreground_input_ms': latencies, 'tool_calls': len(calls),
                  'requests': records, 'evidence': str(evidence)}
        (evidence / 'report.json').write_text(json.dumps(report, indent=2))
        (evidence / 'frontend.bin').write_bytes(lab.output)
        for name in ('ledger.jsonl', 'launches', 'finished'):
            if (lab.root / name).exists():
                shutil.copyfile(lab.root / name, evidence / name)
        return report
    finally:
        lab.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', nargs='?', type=Path, default=Path('target/release/nextty'))
    parser.add_argument('--config', required=True, type=Path)
    parser.add_argument('--task', choices=('eval', 'soak'), default='eval')
    args = parser.parse_args()
    binary = args.binary.resolve()
    config = tomllib.loads(args.config.read_text())
    if args.task == 'soak':
        report = run_soak(binary, config)
        print(json.dumps(report, indent=2))
        if not report['passed']:
            sys.exit(1)
        return
    cases = [run_case(binary, config, interactive) for interactive in (False, True)]
    with binary.open('rb') as source:
        binary_sha256 = hashlib.file_digest(source, 'sha256').hexdigest()
    print(json.dumps({'binary_sha256': binary_sha256,
                      'model': config['model'], 'cases': cases}, indent=2))
    if not all(case['passed'] for case in cases):
        sys.exit(1)


if __name__ == '__main__':
    if len(sys.argv) == 3 and sys.argv[1] == '--fixture':
        agent_fixture(Path(sys.argv[2]))
    else:
        main()
