// Install beside the pinned DSH workspace's node_modules during explicit setup.
// Read-only persistence service: decodes current compressed/aliased generations.
import { writeFile } from 'node:fs/promises'
import { createRequire } from 'node:module'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

export async function loadRuntime(runtime) {
  const require = createRequire(resolve(runtime, 'packages/session/session-persistence-jsonl/package.json'))
  const [{ Context }, Jsonl] = await Promise.all([
    import(pathToFileURL(require.resolve('@deepseek-ai/cordis')).href),
    import(pathToFileURL(require.resolve('@deepseek-ai/dsh-session-persistence-jsonl')).href),
  ])
  return { Context, Jsonl }
}

export async function exportSessions(runtime, root, rootId, output) {
  const { Context, Jsonl } = await loadRuntime(runtime)
  const ctx = new Context()
  const fiber = await ctx.plugin(Jsonl, { root, compression: 'zstd' })
  try {
  const persistence = ctx.sessionPersistence
  const snapshots = await persistence.list()
  const selected = new Set([rootId])
  // Fixed point includes child/retry-agent sessions, not unrelated sessions.
  for (;;) {
    const before = selected.size
    for (const { header } of snapshots) {
      if (header.parentSession && selected.has(header.parentSession)) selected.add(header.id)
    }
    if (before === selected.size) break
  }
  const sessions = {}
  for (const { header } of snapshots) {
    if (!selected.has(header.id)) continue
    const handle = await persistence.open(header.id, 'read')
    try {
      const { events } = await handle.read(0)
      // A fork-inherited prefix is not new billed work. Keep its suffix only.
      sessions[header.id] = events.slice(handle.inheritedEventCount)
        .map(event => JSON.stringify(event)).join('\n')
    } finally {
      await handle.close()
    }
  }
  if (!Object.hasOwn(sessions, rootId)) throw new Error('root durable session missing')
  await writeFile(output, JSON.stringify(sessions), { mode: 0o600, flag: 'wx' })
  } finally {
    await fiber.dispose()
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const [runtime, root, rootId, output] = process.argv.slice(2)
  if (!runtime || !root || !rootId || !output) throw new Error('runtime, root, root session id and output path required')
  await exportSessions(runtime, root, rootId, output)
}
