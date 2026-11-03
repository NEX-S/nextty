#!/usr/bin/env python3
"""Run a real-model long task with one truncated HTTP stream in a private workspace.

The 24-stage program takes at least 192 seconds and requires live terminal input.
The proxy never retries requests. JSON, raw SSE streams and terminal output are saved
under /tmp; only synthetic files are submitted to the configured billable API.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import socket
import sys
import tempfile
import threading
import time
import tomllib
import urllib.error
import urllib.request

from agent_bench import Lab, exit_zero_observed


FIXTURE = '''from pathlib import Path
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


def run(binary, config):
    evidence = Path(tempfile.mkdtemp(prefix='nextty-agent-soak-'))
    lab = Lab(binary)
    records = []
    results = {}
    finished = threading.Event()
    fault = None
    latencies = []
    headers = {'Content-Type': 'application/json', 'Accept': 'text/event-stream',
               'anthropic-version': '2023-06-01'}
    if os.environ.get('ANTHROPIC_AUTH_TOKEN'):
        headers['Authorization'] = 'Bearer ' + os.environ['ANTHROPIC_AUTH_TOKEN']
    elif os.environ.get('ANTHROPIC_API_KEY'):
        headers['x-api-key'] = os.environ['ANTHROPIC_API_KEY']

    class Proxy(http.server.BaseHTTPRequestHandler):
        protocol_version = 'HTTP/1.1'

        def log_message(self, *_):
            pass

        def do_POST(self):
            nonlocal fault
            body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            body['model'] = config['model']
            for message in body['messages']:
                if isinstance(message['content'], list):
                    for block in message['content']:
                        if block['type'] == 'tool_result':
                            results[block['tool_use_id']] = block
            last = body['messages'][-1]['content']
            kind = ('review' if isinstance(last, str) and last.startswith('Harness goal review:')
                    else 'summary' if body.get('tool_choice', {}).get('type') == 'none'
                    else 'execute')
            index = len(records)
            record = {'index': index, 'kind': kind, 'usage': {}, 'tools': [], 'text': '',
                      'response_bytes': 0, 'event_counts': {},
                      'request_sha256': hashlib.sha256(json.dumps(body).encode()).hexdigest(),
                      'at': time.monotonic()}
            records.append(record)
            (evidence / f'request-{index:03}.json').write_text(json.dumps(body))
            ledger = lab.root / 'ledger.jsonl'
            inject = fault is None and kind == 'execute' and ledger.exists() and len(ledger.read_text().splitlines()) >= 4
            request = urllib.request.Request(
                (config.get('base_url') or 'https://api.anthropic.com').rstrip('/') + '/v1/messages',
                data=json.dumps(body).encode(), headers=headers)
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
                    data = []
                    calls = {}
                    for line in response:
                        raw.write(line)
                        raw.flush()
                        if not record['response_bytes']:
                            record['first_line_seconds'] = time.monotonic() - record['at']
                        record['response_bytes'] += len(line)
                        event = None
                        if line.startswith(b'data:'):
                            data.append(line[5:].strip())
                        elif not line.strip() and data:
                            event = json.loads(b'\n'.join(data))
                            data.clear()
                            event_type = event['type']
                            counts = record['event_counts']
                            counts[event_type] = counts.get(event_type, 0) + 1
                            if event_type == 'message_start':
                                record['usage'].update(event['message'].get('usage', {}))
                            elif event_type == 'message_delta':
                                record['usage'].update(event.get('usage', {}))
                            elif event_type == 'content_block_start':
                                block = event['content_block'].copy()
                                if block['type'] == 'tool_use':
                                    calls[event['index']] = [block, '']
                                    record['tools'].append(block)
                                elif block['type'] == 'text':
                                    record['text'] += block['text']
                            elif event_type == 'content_block_delta':
                                delta = event['delta']
                                if delta['type'] == 'text_delta':
                                    record['text'] += delta['text']
                                elif delta['type'] == 'input_json_delta':
                                    calls[event['index']][1] += delta['partial_json']
                            elif event_type == 'content_block_stop' and event['index'] in calls:
                                block, partial = calls[event['index']]
                                if partial:
                                    block['input'] = json.loads(partial)
                            elif event_type == 'message_stop':
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
        binary_sha256 = hashlib.sha256((evidence / 'nextty-tested').read_bytes()).hexdigest()
        (lab.root / 'stages.py').write_text(FIXTURE)
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
                   'fixture_unchanged': (lab.root / 'stages.py').read_text() == FIXTURE,
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
    args = parser.parse_args()
    report = run(args.binary.resolve(), tomllib.loads(args.config.read_text()))
    print(json.dumps(report, indent=2))
    if not report['passed']:
        sys.exit(1)


if __name__ == '__main__':
    main()
