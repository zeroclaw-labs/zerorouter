import { readFileSync, rmSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const here = path.dirname(fileURLToPath(import.meta.url))

export default async function globalTeardown() {
  try {
    const pids = JSON.parse(readFileSync(path.join(here, '.state', 'pids.json'), 'utf8'))
    for (const pid of Object.values(pids)) {
      try { process.kill(pid) } catch {}
    }
    rmSync(path.join(here, '.state'), { recursive: true, force: true })
  } catch {}
}
