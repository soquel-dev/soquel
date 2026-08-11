import type { APIRoute } from 'astro'
import { ENGINES, PRICE, SITE } from '@/lib/site'

/// The curated index an answer engine reads. The long form lives at /llms-full.txt.
export const GET: APIRoute = () => {
  const engines = ENGINES.map(engine => `- ${engine.name}: ${engine.note}`).join('\n')

  const body = `# ${SITE.name}

> ${SITE.description}

Pre-release: the source is available under FSL-1.1-MIT and there are no published builds yet.

Soquel is a native desktop app. The Rust core owns the database drivers, SSH tunnels, connection pools and credentials; the frontend is a thin client that never sees a password. What separates it from other database clients is that it can lend a coding agent access to a database without handing over the connection string.

## Engines

${engines}

## Agent access over MCP

- The app runs a local MCP server over streamable HTTP, bound to loopback, behind a bearer token that never leaves the machine. Any compliant MCP client connects with that URL and token: Claude Code, Cursor, VS Code, Cline or a client of your own. It starts stopped, and every connection is invisible to agents until opted in, one at a time.
- Read-only is enforced by the engine, not by a SQL parser: agent reads run inside a READ ONLY transaction on Postgres and MySQL, or on a handle opened read-only at the filesystem level on SQLite.
- A write opens a dialog showing the exact statement. Denying it, closing it, or ignoring it for a minute all refuse.
- Every call is logged with its tool, connection, statement, outcome and duration.
- Results are capped and paginated, and agent queries carry a 30 second engine-enforced timeout.

## Pricing

- Free tier: the whole application, limited to ${PRICE.freeTabs} workspace tabs per connection. Every engine, the SQL editor, tunnels and agent access are included, with no time limit.
- A licence is ${PRICE.licence} ${PRICE.currency} once, per person rather than per machine, and includes ${PRICE.months} months of updates. No subscription.
- Every build released inside that window activates and keeps working for good, whatever its version number. When the window closes the last covered build stays licensed; only installing a newer one falls back to the free tier, and going back restores it. Renewal is ${PRICE.renewal} to move the window forward.
- The check is offline against a signed licence file. No account, no sign-in, no call home to keep working.
- Building from source stays free under FSL-1.1-MIT. What is sold is the signed, self-updating builds.
- Nothing is on sale yet, because no builds are published yet.

## Links

- [Source](${SITE.repo}): the full application, readable and buildable.
- [Pricing](${SITE.url}/pricing) and [terms of sale](${SITE.url}/terms).
- [Long form](${SITE.url}/llms-full.txt): every claim above with its detail.
`

  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } })
}
