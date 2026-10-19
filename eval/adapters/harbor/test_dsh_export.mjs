// Explicit integration lane using already-installed pinned DSH dependencies.
import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { loadRuntime, exportSessions } from './dsh_export.mjs'

const runtime = process.argv[2]
if (!runtime) throw new Error('explicit prebuilt pinned DSH workspace required')
const { Context, Jsonl } = await loadRuntime(runtime)
const root = await mkdtemp(join(tmpdir(), 'tb4-dsh-export-'))
const ctx = new Context()
const fiber = await ctx.plugin(Jsonl, { root, compression: 'zstd' })
try {
  for (const [id, parentSession] of [['root', undefined], ['child', 'root'], ['other', undefined]]) {
    const handle = await ctx.sessionPersistence.create({ version: 4, id, createdAt: 1000, isSeeded: false,
      cwd: '/fixture', ...(parentSession ? { parentSession } : {}) })
    await handle.append([
      { type: 'turn/start', seq: 0, time: 1, data: { turn: 1 } },
      { type: 'turn/end', seq: 1, time: 2, data: { turn: 1, reason: { kind: 'completed' } } },
    ])
    await handle.close()
  }
  const output = resolve(root, 'export.json')
  await exportSessions(runtime, root, 'root', output)
  const sessions = JSON.parse(await readFile(output, 'utf8'))
  assert.deepEqual(Object.keys(sessions).sort(), ['child', 'root'])
  assert.equal(JSON.parse(sessions.root.split('\n')[1]).data.reason.kind, 'completed')
  console.log('DSH compressed durable export: parent/child normalized, unrelated excluded')
} finally {
  await fiber.dispose()
  await rm(root, { recursive: true, force: true })
}
