import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdtemp, mkdir, readFile, writeFile, rm } from 'node:fs/promises';
import { createHash } from 'node:crypto';
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
test('Bonsai-only execution reaches credential gate without a DSH checkout', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'suite-no-dsh-'));
  try {
    const tasks = JSON.parse(await readFile(new URL('./tasks.json', import.meta.url), 'utf8'));
    const hash = value => createHash('sha256').update(value).digest('hex');
    const binary = 'unused fake binary';
    await mkdir(join(directory, 'target/release'), { recursive: true });
    await writeFile(join(directory, 'target/release/bonsai'), binary);
    await writeFile(join(directory, '.env'), '');
    await writeFile(join(directory, 'tasks.json'), JSON.stringify(tasks));
    const manifest = {
      node: process.version, harnesses: ['bonsai'],
      tasksDigest: hash(JSON.stringify(tasks)), bonsaiSha256: hash(binary),
    };
    await writeFile(join(directory, 'manifest.json'), JSON.stringify(manifest));
    const runner = fileURLToPath(new URL('./run.mjs', import.meta.url));
    const absentRuntime = join(directory, 'absent-dsh');
    const result = await run(process.execPath, [runner, 'execute', directory], directory,
      { ...env, SUITE_HARNESSES: 'bonsai', TB4_DSH_RUNTIME: absentRuntime });
    assert.notEqual(result.code, 0);
    assert.match(result.stderr, /missing project key/);

    // Selecting DSH must still validate its checkout before credentials.
    await writeFile(join(directory, 'manifest.json'), JSON.stringify({ ...manifest, harnesses: ['dsh'] }));
    const selectedDsh = await run(process.execPath, [runner, 'execute', directory], directory,
      { ...env, SUITE_HARNESSES: 'dsh', TB4_DSH_RUNTIME: absentRuntime });
    assert.notEqual(selectedDsh.code, 0);
    assert.match(selectedDsh.stderr, /AssertionError/);
    assert.doesNotMatch(selectedDsh.stderr, /missing project key/);
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
