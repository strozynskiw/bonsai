import { spawn } from 'node:child_process';
import { pathToFileURL } from 'node:url';

export const OUTPUT_LIMIT = 4 * 1024 * 1024;
function kill(pid, signal) {
  if (!pid) return;
  try { process.kill(-pid, signal); } catch (error) { if (error.code !== 'ESRCH') throw error; }
}

// Bound both retained bytes and runtime; always await close/reaping before return.
export async function run(cmd, args, cwd, env, input = '', ms = 10000) {
  const start = performance.now();
  const child = spawn(cmd, args, { cwd, env, detached: true, stdio: ['pipe', 'pipe', 'pipe'] });
  let timedOut = false, outputLimited = false, bytes = 0, escalation;
  const stdout = [], stderr = [];
  const collect = target => chunk => {
    const remaining = OUTPUT_LIMIT - bytes;
    if (remaining > 0) target.push(chunk.subarray(0, remaining));
    bytes += Math.min(remaining, chunk.length);
    if (chunk.length > remaining && !outputLimited) {
      outputLimited = true;
      kill(child.pid, 'SIGKILL');
    }
  };
  child.stdout.on('data', collect(stdout));
  child.stderr.on('data', collect(stderr));
  child.stdin.on('error', () => {});
  const done = new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', (code, signal) => resolve({ code, signal }));
  });
  const timer = setTimeout(() => {
    timedOut = true;
    kill(child.pid, 'SIGTERM');
    escalation = setTimeout(() => kill(child.pid, 'SIGKILL'), 1000);
  }, ms);
  child.stdin.end(input);
  let exit;
  try { exit = await done; } finally {
    clearTimeout(timer);
    clearTimeout(escalation);
    kill(child.pid, 'SIGKILL');
  }
  return { ...exit, timedOut, outputLimited, elapsedMs: Math.round(performance.now() - start),
    stdout: Buffer.concat(stdout).toString(), stderr: Buffer.concat(stderr).toString() };
}

// Offline Python verifier entry point. The parent receives bounded JSON only.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  let input = '';
  for await (const chunk of process.stdin) {
    input += chunk;
    if (Buffer.byteLength(input) > OUTPUT_LIMIT) throw new Error('checker input too large');
  }
  const result = await run(process.execPath, ['--input-type=module', '-'], process.argv[2],
    { PATH: process.env.PATH, LANG: 'en_US.UTF-8' }, input);
  console.log(JSON.stringify(result));
}
