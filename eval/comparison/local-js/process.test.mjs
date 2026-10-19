import assert from 'node:assert/strict';
import test from 'node:test';
import { OUTPUT_LIMIT, run } from './process.mjs';

const env = { PATH: process.env.PATH };
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
