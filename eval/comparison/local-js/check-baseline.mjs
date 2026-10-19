// Offline consistency check of sanitized metrics, not a substitute for raw-run verification.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';

const load = async path => JSON.parse(await readFile(new URL(path, import.meta.url), 'utf8'));
const tasks = await load('./tasks.json');
const baseline = await load('./baselines/deepseek-flash.json');
assert.equal(createHash('sha256').update(JSON.stringify(tasks)).digest('hex'), baseline.manifest.tasksDigest);
assert.deepEqual(tasks.map(task => task.id), baseline.manifest.taskIds);
assert.equal(tasks.length, 10);
assert.equal(baseline.manifest.attempts, 3);
const expected = new Set(tasks.flatMap(task => [1, 2, 3].flatMap(attempt =>
  ['bonsai', 'dsh'].map(harness => `${task.id}/${attempt}/${harness}`))));
for (const record of baseline.records) {
  assert.ok(expected.delete(`${record.task}/${record.attempt}/${record.harness}`), 'unexpected or duplicate identity');
  assert.equal(record.code_correct, record.checksPass && record.scopePass);
  assert.equal(record.strict_success, record.code_correct && record.terminal === 'completed' && record.code === 0 && !record.timedOut && !record.outputLimited);
  assert.equal(record.normalized_usage.input + record.normalized_usage.output, record.normalized_usage.total);
}
assert.equal(expected.size, 0);
assert.equal(baseline.records.length, 60);
for (const harness of ['bonsai', 'dsh']) {
  const records = baseline.records.filter(record => record.harness === harness);
  const summary = baseline.aggregates[harness];
  const sum = fn => records.reduce((total, record) => total + fn(record), 0);
  assert.equal(summary.runs, records.length);
  assert.equal(summary.strict_successes, sum(record => Number(record.strict_success)));
  assert.equal(summary.code_correct, sum(record => Number(record.code_correct)));
  assert.equal(summary.total_ms, sum(record => record.elapsedMs));
  assert.equal(summary.mean_ms, summary.total_ms / records.length);
  const times = records.map(record => record.elapsedMs).sort((a, b) => a - b);
  assert.equal(summary.median_ms, (times[14] + times[15]) / 2);
  assert.equal(summary.p95_ms_nearest_rank, times[28]);
  assert.equal(summary.model_requests, sum(record => record.requests));
  for (const key of ['input', 'output', 'cache_read', 'total']) {
    assert.equal(summary.usage[key], sum(record => record.normalized_usage[key]));
  }
  for (const task of tasks) {
    const row = baseline.tasks.find(row => row.task === task.id)?.[harness];
    const attempts = records.filter(record => record.task === task.id);
    assert.equal(row.successes, attempts.filter(record => record.strict_success).length);
    assert.equal(row.correct, attempts.filter(record => record.code_correct).length);
  }
}
assert.deepEqual(baseline.failures, baseline.records.filter(record => !record.strict_success).map(record =>
  Object.fromEntries(['task', 'attempt', 'harness', 'code', 'terminal', 'checksPass', 'scopePass'].map(key => [key, record[key]]))));
console.log('Baseline consistency passed: 60 identities; Bonsai 26/30 completed, DSH 30/30 completed.');
