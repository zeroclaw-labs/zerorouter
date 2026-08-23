// The e2e suite's database lifecycle.
//
// WHY THIS EXISTS. The suite used to run against whatever `DATABASE_URL`
// pointed at, and never reset it. Every run therefore accumulated: users, API
// keys, ledger rows. That is not merely untidy — it made the suite
// self-limiting. A user may mint only 20 keys per 24 hours
// (`MAX_KEYS_CREATED_PER_WINDOW`, router/src/db.rs), the suite mints two or
// three per run against ONE shared account, and so after roughly six to ten
// local runs key creation began failing and the suite went red for a reason
// that had nothing to do with the code under test. `portal.spec.ts` carried a
// comment warning contributors to budget their key mints, which is a workaround
// for a harness that should not have had the problem.
//
// The fix is that the harness now OWNS a database rather than borrowing one.
// `DATABASE_URL` is read as a connection template — server, credentials, and
// the name to derive from — and the suite runs against a scratch database
// beside it, dropped and recreated at the start of every run. The router
// migrates it on boot (migrations run from `serve`), so an empty database is
// all that is needed.
//
// The supplied database is never touched. Dropping the database a developer
// pointed at, or that a sibling `cargo test` run is using, would be a much
// worse failure than the one being fixed.
//
// WHY DROP AT SETUP RATHER THAN TEARDOWN. A failed run leaves its database
// intact to be inspected, and the next run still starts clean. Teardown-based
// cleanup gets skipped exactly when it matters — a crash, a killed run, a
// Ctrl-C — which is how accumulation starts again.
import { Client } from 'pg'

/// Postgres truncates identifiers at 63 bytes; a name that gets silently
/// truncated is a name that could collide with the database it was derived
/// from, so it is shortened deliberately instead.
const MAX_IDENTIFIER_BYTES = 63
const SCRATCH_SUFFIX = '_e2e'

/// Quote an identifier for interpolation. Database names cannot be bound as
/// parameters in `CREATE`/`DROP DATABASE`, so this is the only safe path.
function quoteIdentifier(name) {
  return `"${name.replace(/"/g, '""')}"`
}

/// The scratch database's name, derived from the supplied one.
export function scratchDatabaseName(databaseUrl) {
  const url = new URL(databaseUrl)
  const supplied = decodeURIComponent(url.pathname.replace(/^\//, '')) || 'postgres'
  const room = MAX_IDENTIFIER_BYTES - SCRATCH_SUFFIX.length
  return `${supplied.slice(0, room)}${SCRATCH_SUFFIX}`
}

/// The URL the suite's router, CLI, and helpers should all use.
export function scratchDatabaseUrl(databaseUrl) {
  const url = new URL(databaseUrl)
  url.pathname = `/${encodeURIComponent(scratchDatabaseName(databaseUrl))}`
  return url.toString()
}

/// Connect to the server's maintenance database — `CREATE`/`DROP DATABASE`
/// cannot be issued from inside the database being dropped.
function maintenanceUrl(databaseUrl) {
  const url = new URL(databaseUrl)
  url.pathname = '/postgres'
  return url.toString()
}

/// Drop and recreate the scratch database, returning its URL.
export async function recreateScratchDatabase(databaseUrl) {
  const supplied = new URL(databaseUrl)
  const suppliedName = decodeURIComponent(supplied.pathname.replace(/^\//, ''))
  const scratch = scratchDatabaseName(databaseUrl)
  if (scratch === suppliedName) {
    // Unreachable while the suffix is non-empty, and asserted anyway: the one
    // catastrophic outcome here is dropping the database that was handed in.
    throw new Error(`refusing to drop the supplied database ${suppliedName}`)
  }

  const client = new Client({ connectionString: maintenanceUrl(databaseUrl) })
  await client.connect()
  try {
    // FORCE terminates sessions still attached — a router left running by a
    // previous crashed run would otherwise block the drop indefinitely.
    // Postgres 13+; the fallback covers an older server, where a lingering
    // connection makes the drop fail loudly instead of hanging.
    try {
      await client.query(`DROP DATABASE IF EXISTS ${quoteIdentifier(scratch)} WITH (FORCE)`)
    } catch (error) {
      if (!/syntax error/i.test(String(error?.message))) throw error
      await client.query(`DROP DATABASE IF EXISTS ${quoteIdentifier(scratch)}`)
    }
    await client.query(`CREATE DATABASE ${quoteIdentifier(scratch)}`)
  } finally {
    await client.end()
  }
  return scratchDatabaseUrl(databaseUrl)
}
