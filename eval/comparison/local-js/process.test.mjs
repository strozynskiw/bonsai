import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { OUTPUT_LIMIT, run, succeeded } from './process.mjs';

const env = { PATH: process.env.PATH };
test('execute refuses Node drift before credentials or agents', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'suite-node-drift-'));
  try {
    await writeFile(join(directory, 'tasks.json'), '[]');
    await writeFile(join(directory, 'manifest.json'), JSON.stringify({ node: 'v0.0.0' }));
    const result = await run(process.execPath,
      [fileURLToPath(new URL('./run.mjs', import.meta.url)), 'execute', directory], process.cwd(), env);
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /Node version differs from preflight/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
test('success requires a complete execution', () => {
  assert.equal(succeeded({ code: 0, timedOut: false, outputLimited: false }), true);
  assert.equal(succeeded({ code: 0, timedOut: true, outputLimited: false }), false);
  assert.equal(succeeded({ code: 0, timedOut: false, outputLimited: true }), false);
  assert.equal(succeeded({ code: 1 }), false);
  assert.equal(succeeded(null), false);
});
test('timeout remains a failure when SIGTERM exits zero', async () => {
  const result = await run(process.execPath, ['-e', "process.on('SIGTERM', () => process.exit(0)); setInterval(() => {}, 1000)"], process.cwd(), env, '', 1000);
  assert.equal(result.code, 0);
  assert.equal(result.timedOut, true);
  assert.equal(succeeded(result), false);
});
test('captures a normal result', async () => {
  const result = await run(process.execPath, ['-e', "console.log('ok')"], process.cwd(), env);
  assert.equal(result.code, 0);
  assert.equal(result.stdout, 'ok\n');
  assert.equal(result.outputLimited, false);
});
test('terminates noisy output and retains a bounded prefix', async () => {
  const result = await run(process.execPath, ['-e', "const {writeSync}=require('node:fs'); for (;;) writeSync(1,'x'.repeat(65536))"], process.cwd(), env);
  assert.equal(result.outputLimited, true);
  assert.notEqual(result.code, 0);
  assert.ok(Buffer.byteLength(result.stdout) + Buffer.byteLength(result.stderr) <= OUTPUT_LIMIT);
});
test('terminates and reaps a timeout', async () => {
  const result = await run(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], process.cwd(), env, '', 100);
  assert.equal(result.timedOut, true);
  assert.notEqual(result.code, 0);
});
