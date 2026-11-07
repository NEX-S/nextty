#!/usr/bin/env python3
"""Run two real-model tasks through isolated nextty TUI, owner and daemon processes.

Forwards only synthetic workspace evidence to the configured Messages API. JSON
reports actual provider usage and independent outcomes; these tasks establish
feasibility, not a statistical change in task quality. API requests are billable.
Each task has a 900-second observation window, including transport retries.
The file-repair case recalls its verification marker after reading 512 KiB through compaction.
"""
import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
import tomllib
import urllib.error
import urllib.request

from agent_bench import Lab, exit_zero_observed, model_headers, prepare_model_request


def run(binary, config, interactive):
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
                    data = []
                    calls = {}
                    for line in response:
                        if line.startswith(b'data:'):
                            data.append(line[5:].strip())
                        elif not line.strip() and data:
                            event = json.loads(b'\n'.join(data))
                            data.clear()
                            event_type = event['type']
                            if event_type == 'message_start':
                                record['usage'].update(event['message'].get('usage', {}))
                            elif event_type == 'message_delta':
                                record['usage'].update(event.get('usage', {}))
                            elif event_type == 'content_block_start':
                                block = event['content_block']
                                if block['type'] == 'tool_use':
                                    calls[event['index']] = [block, []]
                                    record['tools'].append(block)
                                elif block['type'] == 'text':
                                    record['text'] += block['text']
                            elif event_type == 'content_block_delta':
                                delta = event['delta']
                                if delta['type'] == 'text_delta':
                                    record['text'] += delta['text']
                                elif delta['type'] == 'input_json_delta':
                                    calls[event['index']][1].append(delta['partial_json'])
                            elif event_type == 'content_block_stop' and event['index'] in calls:
                                block, partial = calls.pop(event['index'])
                                partial = ''.join(partial)
                                if partial:
                                    block['input'] = json.loads(partial)
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('binary', nargs='?', type=Path, default=Path('target/release/nextty'))
    parser.add_argument('--config', required=True, type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    config = tomllib.loads(args.config.read_text())
    cases = [run(binary, config, interactive) for interactive in (False, True)]
    with binary.open('rb') as source:
        binary_sha256 = hashlib.file_digest(source, 'sha256').hexdigest()
    print(json.dumps({'binary_sha256': binary_sha256,
                      'model': config['model'], 'cases': cases}, indent=2))
    if not all(case['passed'] for case in cases):
        sys.exit(1)


if __name__ == '__main__':
    main()
