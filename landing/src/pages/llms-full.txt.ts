import type { APIRoute } from 'astro'
import { ENGINES, FEATURES, PRICE, SITE } from '@/lib/site'

/// The long form: every claim the page makes, with the detail behind it.
export const GET: APIRoute = () => {
  const engines = ENGINES.map(engine => `- ${engine.name}: ${engine.note}`).join('\n')
  const features = FEATURES.map(feature => `- ${feature}`).join('\n')

  const body = `# ${SITE.name}

> ${SITE.description}

Pre-release, under active development. The source is available under FSL-1.1-MIT: read it, build it for yourself. No signed builds are published yet.

## Shape

A native desktop app: a gpui frontend over a Rust core. The core owns everything heavy and sensitive: database drivers, SSH tunnels, connection pools, result streaming and credentials. The frontend is a thin in-process client over the core's operations.

## Engines

${engines}

## What the client does

${features}

## Agent access over MCP

Soquel can run a local MCP server so coding agents query databases through the app instead of getting a copy of the credentials. It is turned on from the connections screen. The server speaks MCP over streamable HTTP, so any compliant client works with the same URL and bearer token, whether that is Claude Code, Cursor, VS Code, Cline or something written in-house; the app prints the Claude Code one-liner because it is the shortest to paste.

The guardrails are the point:

- Off, and empty, by default. The server starts stopped, and every connection is invisible to agents until opted in. Read-only or writes-need-approval, per connection.
- Read-only is the engine's job, not a SQL parser's. Agent reads run inside a READ ONLY transaction on Postgres and MySQL, or on a handle opened read-only at the filesystem level on SQLite. A statement that slips past classification still cannot write.
- Writes stop for a human. A write opens a dialog showing the exact statement. Denying it, closing it, or ignoring it for a minute all refuse.
- Every call is logged: tool, connection, statement, outcome and duration, readable from the app.
- Bounded. Results are capped and paginated, and agent queries carry a 30 second engine-enforced timeout so a runaway query cannot camp on a connection.
- Local only. The server binds loopback and requires a bearer token that never leaves the machine.

Agents get read tools for every supported engine: schema and DDL, SQL queries, table samples, Redis keys, Mongo documents and indexes. No tool mutates anything except the one that asks permission first.

## Credentials

A connection's password comes from the OS keychain, from a prompt at connect time (kept in memory only), or from a command whose stdout is the password, which covers RDS IAM tokens, Vault and 1Password. The same three modes apply to SSH passwords and key passphrases.

An imported connections file cannot run code on its own: a credential command that arrived through an import stays inert until its exact arguments have been read and approved. Exports leave passwords out unless they are explicitly included, and then the file is encrypted.

An OS keyring is required. Without one, the keychain mode is unavailable and the app says so; the prompt and command modes still work.

## Pricing and licensing

One edition, no tiers. The free tier is the whole application with a single limit: ${PRICE.freeTabs} workspace tabs open per connection. Every engine, the SQL editor, tunnels, export and agent access over MCP work without a licence, with no time limit, because a differentiator nobody can try is not one.

A licence is ${PRICE.licence} ${PRICE.currency}, bought once, and it lifts that limit permanently. It is per person rather than per machine, so it covers every machine that person works on. It includes ${PRICE.months} months of updates, and a renewal at ${PRICE.renewal} moves that window forward. There is no subscription and nothing renews automatically.

The update window is a date, not a version range. Every build released inside it activates with the licence whatever its version number, including a major one, and stays activated for good. When the window closes nothing is taken away: the last covered build remains fully licensed indefinitely. Installing a build released after the window is the only thing the licence will not cover, and the app says so and falls back to the free tier's limit; going back to a covered build restores it.

The check runs offline. Activating a key once returns a signed licence file, and from then on the application verifies that file against a public key compiled into the binary, comparing the window to the date the build was made rather than to the system clock. No account, no sign-in, and nothing to reach for the licence to hold.

Building from source stays free under FSL-1.1-MIT, with no feature held back and no limit to lift. What is sold is the signed, notarised, self-updating builds. Nothing is on sale yet, because no builds are published yet.

## Design commitments

- Secrets never leave the Rust core and the OS keychain; the UI never holds one beyond a typed prompt.
- Every operation is a pure function on the core with a normalised error shape. The MCP tools are that same surface with a second client.
- Offline by design: no remote assets and a strict CSP. The application makes exactly two network requests, both to the same endpoint: the update check, at startup or when asked, and licence activation, once, if a key is pasted. Nothing else leaves the machine.

## Links

- Source: ${SITE.repo}
- Pricing: ${SITE.url}/pricing
- Terms of sale: ${SITE.url}/terms
- Privacy: ${SITE.url}/privacy
- Licence: FSL-1.1-MIT
`

  return new Response(body, { headers: { 'Content-Type': 'text/plain; charset=utf-8' } })
}
