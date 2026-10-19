"""Offline verification of all frozen paid-suite outcomes; no model calls."""
import argparse
import dataclasses
import hashlib
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[3]))
from eval.adapters.harbor.dsh_outcome import parse_sessions

os.umask(0o077)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('run_directory', type=Path)
parser.add_argument('--output', type=Path, help='New report path; defaults to run-directory/verified-suite.json')
args = parser.parse_args()
root = args.run_directory.resolve()
node = shutil.which('node')
assert node
load = lambda path: json.loads(path.read_text(encoding='utf-8'))
tasks = load(root / 'tasks.json')
manifest = load(root / 'manifest.json')
# JS JSON.stringify determines the original digest; preserve its serialization.
digest_script = "import{readFileSync}from'node:fs';import{createHash}from'node:crypto';const p=JSON.parse(readFileSync(process.argv[1]));console.log(createHash('sha256').update(JSON.stringify(p)).digest('hex'));"
digest = subprocess.run([node, '--input-type=module', '-e', digest_script, str(root / 'tasks.json')], capture_output=True, text=True, check=True)
assert digest.stdout.strip() == manifest['tasksDigest']
expected = {(task['id'], attempt, harness) for task in tasks for attempt in (1, 2, 3) for harness in ('bonsai', 'dsh')}
records = []
for task in tasks:
    for attempt in (1, 2, 3):
        for harness in ('bonsai', 'dsh'):
            base = root / 'runs' / f"{task['id']}-{attempt}-{harness}"
            record = load(base / 'result.json')
            assert (record['task'], record['attempt'], record['harness']) in expected
            workspace = base / 'workspace'
            actual = []
            for path in workspace.rglob('*'):
                assert not path.is_symlink(), path
                if path.is_file():
                    actual.append([path.relative_to(workspace).as_posix(), hashlib.sha256(path.read_bytes()).hexdigest()])
            actual.sort()
            assert dict(actual) == dict(record['after']), (base, actual, record['after'])
            original = dict(record['before'])
            scope_pass = len(actual) == len(original) and all(name in original and (name == 'solution.mjs' or digest == original[name]) for name, digest in actual)
            assert scope_pass == record['scopePass']
            checker = "import assert from 'node:assert/strict';import * as mod from " + json.dumps((workspace / 'solution.mjs').as_uri()) + ";let count=0;function check(fn){fn();count++;}" + task['checks'] + "\nconsole.log(JSON.stringify({passed:count}));"
            process_helper = Path(__file__).with_name('process.mjs')
            completed = subprocess.run([node, str(process_helper), str(workspace)], input=checker,
                                       env={'PATH': os.defpath, 'LANG': 'en_US.UTF-8'},
                                       capture_output=True, text=True, timeout=15, check=True)
            verification = json.loads(completed.stdout)
            checks_pass = verification['code'] == 0 and not verification['timedOut'] and not verification['outputLimited']
            assert checks_pass == record['checksPass'], base
            record['independent_check_count'] = json.loads(verification['stdout'])['passed'] if checks_pass else None
            record['fresh_check_stderr'] = verification['stderr']
            record['normalized_usage'] = None
            if harness == 'dsh':
                assert record['exportPass']
                sessions = load(base / 'sessions.json')
                outcome = parse_sessions(sessions, record['session'], record['code'])
                record['durable_outcome'] = dataclasses.asdict(outcome)
                record['terminal'] = outcome.state
                assert outcome.telemetry_complete and outcome.usage
                usage = outcome.usage
                assert all(key in usage for key in ('inputTokens', 'cacheReadTokens', 'cacheWriteTokens', 'outputTokens', 'totalTokens'))
                record['normalized_usage'] = {'input': usage['inputTokens'] + usage['cacheReadTokens'] + usage['cacheWriteTokens'],
                                              'output': usage['outputTokens'], 'cache_read': usage['cacheReadTokens'], 'total': usage['totalTokens']}
                events = [json.loads(line) for text in sessions.values() for line in text.splitlines() if line.strip()]
                record['requests'] = sum(event['type'] in ('assistant/message', 'assistant/attempt') for event in events)
                record['local_estimated_usd'] = None
            else:
                payload = json.loads(load(base / 'execution.json')['stdout'])
                assert payload['status'] == record['terminal'] and payload['usage'] == record['usage']
                usage = record['usage']
                record['normalized_usage'] = {'input': usage['prompt_tokens'], 'output': usage['completion_tokens'],
                                              'cache_read': usage['input_cache']['read_tokens'], 'total': usage['total_tokens']}
                record['local_estimated_usd'] = usage['cost_micros'] / 1000000
            assert record['normalized_usage']['input'] + record['normalized_usage']['output'] == record['normalized_usage']['total']
            record['code_correct'] = checks_pass and scope_pass
            record['strict_success'] = record['code_correct'] and record['terminal'] == 'completed' and record['code'] == 0 and not record['timedOut']
            records.append(record)
assert len(records) == 60 and len({(r['task'], r['attempt'], r['harness']) for r in records}) == 60
assert len(list((root / 'runs').glob('*/reservation.json'))) == 60
aggregates = {}
for harness in ('bonsai', 'dsh'):
    selected = [r for r in records if r['harness'] == harness]
    timings = sorted(r['elapsedMs'] for r in selected)
    aggregates[harness] = {'runs': len(selected), 'strict_successes': sum(r['strict_success'] for r in selected),
        'code_correct': sum(r['code_correct'] for r in selected), 'median_ms': statistics.median(timings),
        'mean_ms': statistics.mean(timings), 'p95_ms_nearest_rank': timings[28], 'total_ms': sum(timings),
        'model_requests': sum(r['requests'] for r in selected),
        'usage': {key: sum(r['normalized_usage'][key] for r in selected) for key in ('input', 'output', 'cache_read', 'total')},
        'local_estimated_usd': sum(r['local_estimated_usd'] for r in selected) if harness == 'bonsai' else None,
        'actual_billed_usd': None}
task_summary = []
for task in tasks:
    row = {'task': task['id']}
    for harness in ('bonsai', 'dsh'):
        selected = [r for r in records if r['task'] == task['id'] and r['harness'] == harness]
        row[harness] = {'successes': sum(r['strict_success'] for r in selected), 'correct': sum(r['code_correct'] for r in selected),
                        'median_ms': statistics.median(r['elapsedMs'] for r in selected)}
    task_summary.append(row)
summary = {'manifest': manifest, 'aggregates': aggregates, 'tasks': task_summary,
           'failures': [{k: r[k] for k in ('task', 'attempt', 'harness', 'code', 'terminal', 'checksPass', 'scopePass')} for r in records if not r['strict_success']],
           'records': records}
with (args.output or root / 'verified-suite.json').open('x', encoding='utf-8') as stream:
    json.dump(summary, stream, indent=2)
print(json.dumps({k: v for k, v in summary.items() if k not in ('records', 'manifest')}, indent=2))
