# soquel

Guidance for agents in this repo. Shared stack conventions are imported above; everything below is soquel-specific and overrides the profile where they differ.

## What this is

Soquel is a desktop database client in the TablePlus mould: a Tauri 2 desktop app. The Rust core owns everything heavy and sensitive (DB drivers, SSH tunnels, connection pools, result streaming, credentials); the Vue webview is a thin client. v1 targets Postgres + SSH tunnels + table browser + SQL editor; MySQL and Redis come later behind a capability-based connector trait.

Two architecture rules that must hold:

- **Typed command layer is the only IPC boundary.** Every operation is a Tauri command with a normalized error shape. Commands are pure and independently callable: this surface later becomes the agent/MCP tool surface, the UI is just its first client.
- **Secrets never cross into the webview.** Credentials live in the Rust core and the OS keychain.

## Layout

- `crates/core` - the Rust core, UI-agnostic: connectors, SSH tunnels, credentials, secrets, licence, import/export. `src/error.rs` holds the normalized error enum, `src/lib.rs` the `AppState`. Its own detached cargo workspace (own target dir, own Cargo.lock, both committed).
- `packages/app` - Vue 3 + vue-router + shadcn-vue (Reka UI) + Tailwind v4, Vite on port 5173.
- `src-tauri` - the Tauri shell over the core: `src/commands.rs` holds the command layer, plus the MCP agent surface, diagnostics and updater glue.
- `src-gpui` - gpui frontend (gpui + gpui-component, git-pinned via committed Cargo.lock), migration in progress on this branch. Detached workspace like the core.
- `landing` - Astro + Tailwind v4 for soquel.dev. **Its own workspace**: own `pnpm-workspace.yaml`, own lockfile, own eslint, own CI job, and the root lint ignores it. Run its commands from `landing/`. Its palette is the app's tokens verbatim, and the only colour on the page is the one the app gives data (syntax tokens plus the destructive action): soquel has no brand hue.

### Command layer

Commands are annotated `#[tauri::command] #[specta::specta]`, return `Result<T, Error>`, and are registered in `specta_builder()` (`src-tauri/src/lib.rs`). TypeScript bindings are generated to `packages/app/src/lib/bindings.ts` (committed, eslint-ignored): automatically on `tauri dev`, or headless via `cargo test --manifest-path src-tauri/Cargo.toml export_typescript_bindings`. The frontend imports `commands` from the bindings and never calls `invoke` directly. Error handling is `ErrorHandlingMode::Result`: bindings return `{ status: 'ok', data } | { status: 'error', error: { kind, message } }`. New error variants go on the `Error` enum in `error.rs`, tagged by `kind`.

No server/backend package: this is a desktop app. Data-layer conventions from the profile (Hono/tRPC/Drizzle/knex) don't apply here.

### Credentials

`CredentialSource` says where a password comes from: `keychain`, `prompt` (asked at connect, memory only, `unlock_secret` hands it over), or `command` (argv run without a shell, stdout is the password, cached with a TTL). It sits on both `ConnectionProfile` and `TunnelProfile` (ssh password, key passphrase). `credentials.rs` owns resolution: `CredentialTarget::{connection,tunnel}` names what is asking, and connectors get an `Arc<Credentials>` they resolve per connection so a pool picks up a refreshed token. Only `keychain` writes to the OS keychain; switching away from it deletes what was stored.

Secrets are keyed by `SecretKey::{Connection,Tunnel,McpToken}` (`secrets.rs`), not by a raw string: `storage_id()` produces the strings already on disk, so the keychain entries stay as they are.

An OS keyring is required, not emulated: `SecretStore::probe()` runs once at startup and its failure lands in `AppState::secrets_problem`, which `secrets_status` hands to the webview. Both forms then disable the `keychain` mode and show why, and a new profile defaults to `prompt`. The app stays usable that way, since `prompt` and `command` never touch the keyring. No encrypted-file fallback: with no keyring there is nowhere safe to keep its key, so it would take a master password, a whole feature. `SOQUEL_INSECURE_FILE_SECRETS` stays what its name says, dev only.

Importing reads soquel files and nothing else: parsing another client's private format is a one-shot path with permanent upkeep, and a pasted connection URL already prefills the form. Two rules hold whatever the file says: a password lands only when `import_connections` is called with `with_secrets`, and a credential command arriving in a file stays inert until approved.

Exports carry `.soquel` (`transfer.ts`), so the OS can associate the format later. Not `.soquel.json`: Windows binds the last extension and a macOS UTI extension cannot contain a dot, so only Linux would ever match it. The import picker still lists `.json` too, for files written before the extension existed.

## Commands

Run from the repo root.

```bash
pnpm install
pnpm dev:app       # vite only (webview in a browser, no Rust)
pnpm dev           # tauri dev: builds the Rust core, launches the app, serves vite
pnpm dev:wsl       # same with file-backed plaintext secrets (WSL has no OS keychain); dev only
pnpm build         # pnpm -r build (frontend)
pnpm build:desktop # tauri build (bundles the app)
pnpm typecheck     # vue-tsc across the workspace
pnpm lint          # eslint . (lint:fix to autofix)
pnpm test          # vitest across the workspace
pnpm updates:stub <dir>  # fake update endpoint over a built AppImage (see Updater below)
pnpm test:e2e      # wdio drives the built debug binary via tauri-driver (Linux/Windows only)
                   # isolated app data (SOQUEL_DATA_DIR) + in-memory secrets (SOQUEL_EPHEMERAL_SECRETS)
                   # screenshots land in packages/app/e2e/screenshots/ (gitignored)

cargo check --manifest-path crates/core/Cargo.toml   # fast Rust validation (same for src-tauri, src-gpui)
cargo clippy --manifest-path crates/core/Cargo.toml -- -D warnings
cargo test --manifest-path crates/core/Cargo.toml    # connector suites; src-tauri holds the mcp ones

pnpm db:test           # start the test databases (docker-compose.test.yml), seeded + throwaway
pnpm test:integration  # cargo integration_* tests against them
pnpm db:test:down

pnpm db:dev            # dev postgres 5470 + mysql 5471 + redis 5472 + mongo 5473 (docker-compose.dev.yml), persistent volumes
                       # dev stays out of 5455-5464: that whole range belongs to docker-compose.test.yml
pnpm db:dev:seed:pg    # (re)seed the dev postgres: ~1.5M rows across a SaaS-shaped app schema
pnpm db:dev:seed:mysql # same shape for the dev mysql (soquel_dev)
pnpm db:dev:seed:redis # SaaS-shaped keys for the key browser (~16k: sessions, cache, queues, stream)
pnpm db:dev:seed:mongo # SaaS-shaped documents for the doc browser (~85k: users, orders, events, sessions)
pnpm db:dev:down
```

## Testing

Weight: Rust integration against real databases is the core; unit tests for pure logic; e2e stays a thin smoke layer.

- `docker-compose.test.yml`: one service per connector kind, seeded from `scripts/test-seeds/<engine>.sql`. Port plan: postgres 5455, mysql 5456, redis 5457, sshd tunnel target 5458, postgres-tls 5459 (self-signed cert from `scripts/test-tls/`, unseeded, TLS handshake tests only), postgres-oldest 5460, sshd-reconnect 5461, mysql-oldest 5462, mongo 5464 (seeds soquel_e2e for the e2e spec; integration tests own their soquel_test_* databases).
- Minimum supported postgres = oldest non-EOL major (currently 14). Minimum supported mysql = 8.0 (EOL upstream but dominant in the wild via RDS/Aurora extended support). The `*-oldest` services run them with the same seeds, and `pnpm test:integration` runs the `integration_postgres_*` / `integration_mysql_*` suites against both versions; the seeds must stay valid on the oldest.
- MariaDB is supported through the mysql kind (suite runs against `mariadb` LTS on 5463). Known quirks, asserted flavor-aware in tests: JSON columns read as text (LONGTEXT alias), COLUMN_TYPE keeps display widths (`int(11)`), KILL QUERY raises ER_QUERY_INTERRUPTED instead of returning; the workspace badge shows "MariaDB x.y.z" from the version string.
- Rust integration tests are named `integration_<engine>_*`, each gated by its env var (`SOQUEL_TEST_PG`, `SOQUEL_TEST_SSH`, later `SOQUEL_TEST_MYSQL`, ...) and skipped silently when unset. They live in `crates/core` next to their connectors (the `integration_mcp_*` suite stays in `src-tauri`); `pnpm test:integration` wires the env vars to the compose databases and runs both manifests. SSH tunnel tests use the sshd service (key auth via the committed throwaway keypair in `scripts/test-ssh/`).
- e2e specs take DB coordinates from `packages/app/e2e/fixtures.ts` (never hardcode), and need `pnpm db:test` up.
- The CI Rust job runs on all three release platforms, but `cargo test` is skipped on Windows: the test binary links and then the loader refuses it with `STATUS_ENTRYPOINT_NOT_FOUND`. Not a shadowed C runtime, measured rather than assumed (pruning `PATH` to `System32` changes nothing), and both upstream reports are open with no workaround (tauri-apps/tauri#13419, #13948). Clippy still runs there, which is what the leg is for: the cfg-gated code has to compile and lint on every platform we ship, or the release workflow cannot be green. No test asserts a Windows-only path today, the lone `#[cfg(not(unix))]` function being the ssh-agent stub.
- Native file pickers are invisible to WebDriver, and the webview's IPC cannot be stubbed around them: `window.__TAURI_INTERNALS__.invoke` is non-writable and non-configurable (assignment is silently ignored, `defineProperty` throws). A synthetic `popstate` does not move vue-router either, and the `tauri://` scheme drops the query string on navigation. Calling a command from a spec does work: `invokeCommand` in `e2e/helpers.ts`, and `open_connections_file` is how a spec reaches the import dialog.
- WebKitGTK returns empty rendered text for elements carrying Tailwind's `truncate`, so `getText()` / `toHaveText` see `""` on them however the page is scrolled. Assert `getProperty('textContent')` instead (see `shownEndpoint` in `e2e/mcp.spec.ts`).

## Tauri specifics

- Linux dev needs the Tauri v2 system prerequisites (webkit2gtk-4.1 etc.); `tauri dev` under WSL runs the Linux build via WSLg, not representative of Windows/macOS.
- Debug builds are isolated from releases: data dir gets a `/dev` subtree and the keychain service a `.dev` suffix, so `tauri dev` never touches an installed app's connections or secrets (`SOQUEL_DATA_DIR` still overrides everything for e2e).
- `tauri.conf.json`: dev server is vite on 5173 (strictPort); `beforeDevCommand`/`beforeBuildCommand` drive the app package through pnpm filters.
- Add Tauri plugins with `pnpm tauri add <name>`; their permissions go in `src-tauri/capabilities/default.json`.
- No remote assets in the webview (fonts, scripts): the app must work offline and keep a strict CSP. Bundle everything.

### Licence and the free tier

`licence.rs` validates offline against the public key compiled into the binary; the format is public in `docs/licence-format.md` so a buyer can check their own file. The signature is verified over the **stored bytes**, never a re-serialised payload, and the algorithm is hardcoded rather than read from the token.

The window is compared against `SOQUEL_BUILD_DATE`, stamped by `build.rs`, not against the wall clock: moving the system clock must not switch a licence off, and a build has to know when it was made to tell whether a licence covers it.

**Three states, not two.** `Free`, `Licensed`, and `Expired` for a signature that verifies against a window that closed before this build. Collapsing `Expired` into `Free` is what turns a lapsed renewal into a bug report.

The public key is a parameter with the constant as its default: inline, no test could sign with a key it holds.

The limit is two tabs **per connection**, enforced in the pure functions of `lib/tabs.ts`. Only opening counts: `openTableTab` re-activating a tab that is already there must always pass, or the limit blocks navigation instead of a purchase.

`activation.rs` is the normal way in: `activate_licence` posts the key a buyer pasted to the licence service and installs the file it returns through `licence::install`, so a bad answer is refused by the same validation as a bad paste. The HTTP call lives in the core like the updater's, so the capabilities need no `http:` permission. `SOQUEL_ACTIVATION_ENDPOINT` overrides the endpoint in debug builds only. The refusals are told apart by the body, not the status: the service answers 403 for both a revoked key and one from another product, and `ActivationReason` is what the dialog keys its wording on in `lib/licence.ts`. reqwest is pinned to `rustls-no-provider` and handed a ring config per call, since it panics if a client is built with no provider and the alternative pulls aws-lc-rs into the graph.

Pasting a licence **file** stays as the second path, folded away in `LicenceDialog.vue`. Not to save activations: it works with no network, it outlives the service, and it is how a licence gets issued with no Polar order behind it.

### Logs and diagnostics

`diagnostics.rs` owns both. The log plugin is registered on the builder chain, not in `setup`, so anything logged while starting up (the keyring probe first of all) is captured. One file target always, Stdout only in debug (a bundle has no console). The log dir derives from the identifier and is therefore shared, so the file name is what separates them: `soquel-dev.log` in debug, `soquel.log` in release. With `SOQUEL_DATA_DIR` set, logs go to `<data dir>/logs` so an e2e run is as isolated for its logs as for its data.

Levels: `Warn` globally, `Info` for `soquel_lib`. An `Info` floor everywhere buries our lines under russh, hyper and rustls.

`diagnostics` returns one preformatted block, built in the core where the facts are. It carries **no connection names, no hosts, no database paths** and never the log's contents: it is meant to be pasted into a public issue, and driver errors in the log can hold a table name or a query fragment. Counts per kind are enough to triage. The webview reaches both this and `open_log_folder` through the command layer, so `tauri-plugin-opener` is a Rust-only dependency with no `opener:` permission in the capabilities.

`DiagnosticsDialog.vue` is the surface, reached from the palette: it shows the block before anyone copies it (proving the no-names claim rather than promising it in a toast) and offers to open the log folder. Opening cannot be verified, since the opener spawns detached and a session with no file manager reports success while doing nothing, so the log path stays visible in the block and a "Copy path" button sits next to the open button.

Known dead end under WSL: the `open` crate puts PowerShell first there and passes the target in an env var without listing it in `WSLENV`, which WSL interop requires, so the Windows process receives an empty path and `Start-Process` refuses. Detached, that surfaces as nothing at all. Shipped platforms are unaffected; use "Copy path" while developing.

### Updater

`src-tauri/src/updater.rs` wraps the Tauri updater behind `check_update` / `install_update`, so the webview never touches the plugin's own JS API and `capabilities/default.json` needs no `updater:` permission. Download progress rides the `UpdateProgress` event; `install_update` only ever returns on failure, since a successful install restarts the app.

The signing keypair is **not rotatable**: `plugins.updater.pubkey` is compiled into every binary, so a new key orphans every installed client (no auto-update, manual reinstall). Private key + passphrase live in 1Password, and become `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` for release builds.

`endpoints` points at a dynamic service, never a static `latest.json`: the license window is enforced server-side (see the Pricing issue), and the license key will ride an `UpdaterBuilder::header` set in `pending()`.

`pnpm updates:stub <dir>` serves a fake endpoint over a built AppImage (`scripts/update-server.mjs`): 204 when the caller is current, otherwise a manifest pointing at the bundle. Hardcoded release, no licence logic, and it stays that way. It paces the download so the progress bar is watchable (`SLOW=0` to send at full speed).

`SOQUEL_UPDATE_ENDPOINT` overrides the endpoint in debug builds only. A debug build with no override skips the check entirely (nothing to replace), and a release build ignores the variable so a shipped app cannot be redirected. Consequence for end-to-end testing: use `tauri build --debug`, since a release bundle both ignores the override and refuses plain `http`. Only the AppImage is updatable on Linux; `.deb` is not.

## UI

- shadcn-vue components via CLI from `packages/app`: `pnpm exec shadcn-vue add <name>` (check the registry before hand-rolling).
- Use the `frontend-design` skill for UI work; the app identity (theme, typography) is defined in `packages/app/src/style.css`.
