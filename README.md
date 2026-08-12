# soquel

A desktop database client that lends coding agents the query, not the credentials.

A native desktop app built on [gpui](https://www.gpui.rs) over a Rust core that owns everything heavy and sensitive: database drivers, SSH tunnels, connection pools, result streaming, credentials. Pre-release, under active development: no builds to download yet.

## Databases

| Engine | Notes |
| --- | --- |
| PostgreSQL | 14+ |
| MySQL / MariaDB | MySQL 8.0+, MariaDB LTS |
| SQLite | file-based, no server needed |
| Redis / Valkey | key browser + console |
| MongoDB | document browser + console |

## Features

- Table browser with inline editing, filters and export
- SQL editor with query plans (EXPLAIN tree)
- SSH tunnels: key, agent and password auth, host key verification
- TLS connections, including custom root certificates
- Passwords from the OS keychain, asked at each connection, or read from a command (RDS IAM tokens, Vault, 1Password)
- Export and import connections as a file, passwords left out by default and encrypted when included
- Redis key browser, Mongo document browser, dedicated consoles
- Command palette
- Agent access over MCP, off by default (see below)

## Agent access (MCP)

Soquel can run a local MCP server so coding agents query your databases through the app instead of getting a copy of your credentials. Turn it on from the connections screen and point your agent at it. Any client that speaks MCP over streamable HTTP works with the same URL and token; Claude Code takes the one-liner the app prints:

```bash
claude mcp add --transport http soquel http://127.0.0.1:52700/mcp --header "Authorization: Bearer <token>"
```

The guardrails are the point:

- **Off, and empty, by default.** The server starts stopped, and every connection is invisible to agents until you opt it in. Read-only or writes-need-approval, per connection.
- **Read-only is the engine's job, not a SQL parser's.** Agent reads run inside a `READ ONLY` transaction (Postgres, MySQL/MariaDB) or on a handle opened read-only at the filesystem level (SQLite). A statement that slips past classification still cannot write.
- **Writes stop for a human.** A write opens a dialog showing the exact statement. Denying, closing it, or ignoring it for a minute all refuse.
- **Every call is logged.** Tool, connection, statement, outcome and duration, readable from the app.
- **Bounded.** Results are capped and paginated; agent queries carry a 30s engine-enforced timeout so a runaway query cannot camp on a connection.
- **Local only.** The server binds loopback and requires a bearer token that never leaves your machine.

Agents get read tools for every supported engine (schema and DDL, SQL queries, table samples, Redis keys, Mongo documents and indexes). There is no tool that mutates anything except the one that asks permission first.

## Design

- **Secrets stay in the core and the OS keychain.** The UI only ever holds a password you typed into a prompt, and only long enough to hand it back.
- **An imported command runs nothing on its own.** A connection can get its password from a program, which makes a shared connections file a way to run code. One that arrived through an import stays inert until you have read the exact arguments and approved them.
- **Operations are pure functions on the core.** Each takes the app state and returns a normalized error shape; the app calls them in-process, and the MCP tools above are the same operations with a second client.
- **Offline by design.** No remote assets; the app bundles its fonts and icons and never phones home.

## Development

Prerequisites: Rust (stable), [`just`](https://github.com/casey/just), and gpui's system libraries on Linux (xkbcommon, wayland, xcb, fontconfig; see the CI workflow for the exact apt set).

```bash
just dev                   # cargo run -p soquel-app

just db-dev                # local dev databases (docker compose)
just db-dev-seed           # seed all dev databases, or pass pg/mysql/redis/mongo
just seed-dev-connections  # add Docker dev connections and store passwords in the dev keychain
just reset-dev-connections # remove those generated connections and their passwords
just db-test               # throwaway seeded databases for the test suites
just test-integration
```

Rust checks are plain cargo (`cargo clippy --workspace`, `cargo test --workspace`). See `AGENTS.md` for the full recipe list and architecture rules.

## License

[FSL-1.1-MIT](LICENSE). Source available: use it, read it, build it for yourself.
