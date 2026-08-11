@~/.claude/stack/web-saas.md

# soquel

Guidance for agents in this repo. The shared stack profile is imported above, but soquel is a **Rust desktop app, not a web SaaS**: only its pnpm/eslint conventions and the `landing/` site follow the profile. Everything below overrides it.

## What this is

Soquel is a desktop database client in the TablePlus mould: a **gpui** app (gpui + gpui-component, the Zed UI stack) over a Rust core. The core owns everything heavy and sensitive (DB drivers, SSH tunnels, connection pools, result streaming, credentials, licence, the MCP server); the gpui frontend is a thin in-process client. It targets Postgres, MySQL/MariaDB, SQLite, Redis and MongoDB behind a capability-based connector trait.

It was a Tauri 2 + Vue webview app until the frontend was migrated to gpui; the tauri shell and the webview are gone (`docs/gpui-migration.md` is the record). No IPC boundary survives: the app calls the core directly.

Two architecture rules that must hold:

- **Operations are pure functions on the core, independently callable.** They live in `core::ops`, `core::mcp` and the connector traits, take `&AppState`, and return `Result<T, Error>`. The gpui app reaches them through the bridges in `crates/app/src/core.rs`; the MCP server exposes the same surface to agents. Keep new operations in the core, not in a view.
- **Secrets never leave the core and the OS keychain.** Credentials are resolved per-connection inside the core; the UI only ever handles a password the user typed into a prompt, and only long enough to hand it back with `unlock_secret`.

## Layout

Root **cargo workspace** (`Cargo.toml`, `resolver = "2"`), one committed `Cargo.lock`, one `target/`:

- `crates/core` - the Rust core, UI-agnostic: connectors, SSH tunnels, credentials, secrets, licence, activation, MCP server + tools, diagnostics block, import/export. `src/error.rs` holds the normalized `Error` enum, `src/lib.rs` the `AppState`, `src/ops.rs` the connection lifecycle. `build.rs` stamps `SOQUEL_BUILD_DATE`.
- `crates/app` - the gpui binary (`soquel-app`). `src/core.rs` bridges the core's private tokio runtime to gpui (futures oneshot/mpsc channels) and holds `init_state`/`init_logging`; `src/app.rs` is the window root (branches screens on connector capability); one view module per surface (`connections`, `workspace`, `tunnels`, `kv`, `doc`, `mcp`, `licence`, `diagnostics`, `grid`, ...). gpui + gpui-component are git-pinned via the committed root `Cargo.lock` (gpui follows gpui-component's pin; never rev-pinned in a manifest).
- `landing` - Astro + Tailwind v4 for soquel.dev. **Its own workspace**: own `pnpm-workspace.yaml`, own lockfile, own eslint, own CI job. Run its commands from `landing/`. Its palette is the app's tokens verbatim, and the only colour on the page is the one the app gives data (syntax tokens plus the destructive action): soquel has no brand hue.

The root has no node stack: a `Justfile` holds the cargo + docker shortcuts, and `landing/` is the only node project. Clippy owns the Rust, `landing/` lints itself.

### The core surface and the gpui bridges

There is no command layer, no specta, no generated bindings. `crates/app/src/core.rs` wraps each core operation in a small bridge: sync ones call `soquel_core::*` directly, async ones spawn on a private multi-thread tokio runtime and return a `futures::oneshot::Receiver` the view awaits with `cx.spawn`. New error variants go on the `Error` enum in `core::error`, tagged by `kind`. Connectors are capability-gated (`Capability::{SqlQuery, Introspection, KvBrowse, DocBrowse}`; `conn.sql()/kv()/doc()` accessors), and `App::open_workspace` picks the workspace view from the connection's capabilities.

### Credentials

`CredentialSource` says where a password comes from: `keychain`, `prompt` (asked at connect, memory only, `unlock_secret` hands it over), or `command` (argv run without a shell, stdout is the password, cached with a TTL). It sits on both `ConnectionProfile` and `TunnelProfile` (ssh password, key passphrase). `credentials.rs` owns resolution: `CredentialTarget::{connection,tunnel}` names what is asking, and connectors get an `Arc<Credentials>` they resolve per connection so a pool picks up a refreshed token. Only `keychain` writes to the OS keychain; switching away from it deletes what was stored.

Secrets are keyed by `SecretKey::{Connection,Tunnel,McpToken}` (`secrets.rs`), not by a raw string: `storage_id()` produces the strings already on disk, so the keychain entries stay as they are.

An OS keyring is required, not emulated: `SecretStore::probe()` runs once at startup and its failure lands in `AppState::secrets_problem`. The gpui forms read that field directly (the app holds the `Arc<AppState>`): both the connection and tunnel forms then drop `keychain` from the credential picker (`available_credential_modes`), default a new profile to `prompt` (`default_credential_mode`), and show why in an amber line. The app stays usable that way, since `prompt` and `command` never touch the keyring. No encrypted-file fallback: with no keyring there is nowhere safe to keep its key, so it would take a master password, a whole feature. `SOQUEL_INSECURE_FILE_SECRETS` stays what its name says, dev only.

Importing reads soquel files and nothing else: parsing another client's private format is a one-shot path with permanent upkeep, and a pasted connection URL already prefills the form (`parse_connection_url` in `connections.rs`). Two rules hold whatever the file says: a password lands only when import runs `with_secrets`, and a credential command arriving in a file stays inert until approved.

Exports carry `.soquel`, so the OS can associate the format later. Not `.soquel.json`: Windows binds the last extension and a macOS UTI extension cannot contain a dot, so only Linux would ever match it. The import picker still lists `.json` too, for files written before the extension existed.

## Commands

Run from the repo root. Shortcuts are a `Justfile` (`just`), not npm; Rust is plain cargo.

```bash
just dev               # cargo run -p soquel-app
just dev-wsl           # same with file-backed plaintext secrets (WSL has no OS keychain); dev only
just build             # cargo build -p soquel-app
just test              # cargo test --workspace (unit; integration_* skip without their env vars)
just --list            # every recipe

cargo check --workspace                          # fast validation
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo test -p soquel-app        # gpui view + logic tests
cargo test -p soquel-core       # connector + mcp + core suites

just db-test           # start the test databases (docker-compose.test.yml), seeded + throwaway
just test-integration  # cargo integration_* tests against them
just db-test-down

just db-dev            # dev postgres 5470 + mysql 5471 + redis 5472 + mongo 5473 (docker-compose.dev.yml), persistent volumes
                       # dev stays out of 5455-5464: that whole range belongs to docker-compose.test.yml
just db-dev-seed       # (re)seed every dev database; `just db-dev-seed pg` for one
                       #   pg ~1.5M rows / mysql soquel_dev / redis ~16k keys / mongo ~85k docs, SaaS-shaped
just db-dev-down
```

Don't start `just dev` yourself: it opens a window Joris watches. Verify with `cargo check`/`clippy`/tests, or ask him to run it. Under WSL the app runs via WSLg (software rendering, `libEGL` warnings are noise); Joris validates the look on his Mac (Metal).

## Testing

Weight: Rust integration against real databases is the core; unit tests for pure logic; `#[gpui::test]` view tests for the frontend.

- `docker-compose.test.yml`: one service per connector kind, seeded from `scripts/test-seeds/<engine>.sql`. Port plan: postgres 5455, mysql 5456, redis 5457, sshd tunnel target 5458, postgres-tls 5459 (self-signed cert from `scripts/test-tls/`, unseeded, TLS handshake tests only), postgres-oldest 5460, sshd-reconnect 5461, mysql-oldest 5462, mariadb 5463, mongo 5464 (seeds soquel_e2e; integration tests own their soquel_test_* databases).
- Minimum supported postgres = oldest non-EOL major (currently 14). Minimum supported mysql = 8.0 (EOL upstream but dominant via RDS/Aurora extended support). The `*-oldest` services run them with the same seeds, and `pnpm test:integration` runs the `integration_postgres_*` / `integration_mysql_*` suites against both versions; the seeds must stay valid on the oldest.
- MariaDB is supported through the mysql kind (suite runs against `mariadb` LTS on 5463). Known quirks, asserted flavor-aware in tests: JSON columns read as text (LONGTEXT alias), COLUMN_TYPE keeps display widths (`int(11)`), KILL QUERY raises ER_QUERY_INTERRUPTED instead of returning; `server_badge` shows "MariaDB x.y.z" from the version string (Valkey likewise on the redis kind).
- Rust integration tests are named `integration_<engine>_*` and `integration_mcp_*` / `integration_flow_*`, each gated by its env var (`SOQUEL_TEST_PG`, `SOQUEL_TEST_MYSQL`, `SOQUEL_TEST_REDIS`, `SOQUEL_TEST_MONGO`, `SOQUEL_TEST_SSH`, ...) and skipped silently when unset. The connector + mcp suites live in `crates/core`; the browse/stage/apply flow tests (`integration_flow_*`) live in `crates/app`. `scripts/test-integration.sh` wires the env vars to the compose databases and runs both. SSH tunnel tests use the sshd service (key auth via the committed throwaway keypair in `scripts/test-ssh/`).
- gpui view tests use `#[gpui::test]` + `TestAppContext`; `crates/app/src/test_support.rs` roots a dialog-capable window (`shell_window`), `wait_until` polls across the core's private tokio runtime (`allow_parking()`), buttons carry `debug_selector` for `debug_bounds` + `simulate_click`, and native pickers are simulated via `simulate_path_prompt_response` / `simulate_new_path_selection`. A file whose parent has `use gpui::*` must `use ::core::prelude::v1::test;` in its tests module, or `#[gpui::test]`'s generated `#[test]` recurses.
- CI runs `cargo clippy --workspace` on all three platforms (the cfg-gated code, e.g. the `#[cfg(not(unix))]` ssh-agent stub, must compile everywhere), but `cargo test` skips Windows: gpui's test link there is unverified. Revisit if a Windows-only path ever needs asserting.

## Dev and platform

- Linux needs gpui's system dev libraries (the `$GPUI_DEPS` set in CI: xkbcommon-x11, wayland, xcb, x11-xcb, fontconfig, freetype, gbm; that apt list is a first-pass guess, tune it if a Linux build fails). macOS uses Metal, Windows DirectX.
- Debug builds are isolated from releases in the core, `cfg`-driven: the data dir gets a `/dev` subtree and the keychain service a `.dev` suffix, so a dev run never touches an installed app's connections or secrets. `SOQUEL_DATA_DIR` overrides the data dir (used by tests). Both frontends' debug builds share the same sandbox by construction, which is why a dev gpui build sees the connections a dev tauri build created while both existed.
- No remote assets: the app bundles its fonts and icons and must work offline.

### Licence and the free tier

`licence.rs` validates offline against the public key compiled into the binary; the format is public in `docs/licence-format.md` so a buyer can check their own file. The signature is verified over the **stored bytes**, never a re-serialised payload, and the algorithm is hardcoded rather than read from the token.

The window is compared against `SOQUEL_BUILD_DATE`, stamped by `crates/core/build.rs`, not against the wall clock: moving the system clock must not switch a licence off, and a build has to know when it was made to tell whether a licence covers it. The gpui app inherits the stamp through its `soquel-core` dependency (no build.rs of its own).

**Three states, not two.** `Free`, `Licensed`, and `Expired` for a signature that verifies against a window that closed before this build. Collapsing `Expired` into `Free` is what turns a lapsed renewal into a bug report.

The public key is a parameter with the constant as its default: inline, no test could sign with a key it holds.

The limit is two tabs **per connection**, enforced in the pure functions of `crates/app/src/tabs.rs`; `effective_tab_limit` consults the installed licence so a valid one lifts the cap, and `Workspace` reads the licence file per tab-open so an activation lifts it live. Only opening counts: re-activating a tab already open must always pass, or the limit blocks navigation instead of a purchase.

`activation.rs` is the normal way in: `activate_licence` posts the key a buyer pasted to the licence service and installs the file it returns through `licence::install`, so a bad answer is refused by the same validation as a bad paste. The HTTP lives in the core. `SOQUEL_ACTIVATION_ENDPOINT` overrides the endpoint in debug builds only. The refusals are told apart by the body, not the status: the service answers 403 for both a revoked key and one from another product, and `ActivationReason` keys the dialog's wording (`activation_message` in `crates/app/src/licence.rs`). reqwest is pinned to `rustls-no-provider` and handed a ring config per call, since it panics if built with no provider and the alternative pulls aws-lc-rs into the graph.

Pasting a licence **file** stays as the second path, folded away in `LicenceView`. Not to save activations: it works with no network, it outlives the service, and it is how a licence gets issued with no Polar order behind it.

### Logs and diagnostics

`core::diagnostics` builds the pasteable block; `crates/app/src/core.rs` owns logging (`init_logging`, fern) and the folder open (the `open` crate). Logging is set up before `init_state` so the keyring probe is captured. One file target always, Stdout only in debug (a bundle has no console). The log dir is `<data dir>/logs`; the file name separates dev from release: `soquel-dev.log` in debug, `soquel.log` in release. Levels: `Warn` globally, `Info` for `soquel_core`/`soquel_app` (an `Info` floor everywhere buries our lines under russh, hyper and rustls). No size rotation; a startup size cap keeps the file bounded.

The block is built in the core where the facts are. It carries **no connection names, no hosts, no database paths** and never the log's contents: it is meant to be pasted into a public issue, and driver errors in the log can hold a table name or a query fragment. Counts per kind are enough to triage.

`DiagnosticsView` (from the palette) shows the block before anyone copies it (proving the no-names claim rather than promising it in a toast) and offers to open the log folder. Opening cannot be verified, since the opener spawns detached and a session with no file manager reports success while doing nothing, so the log path stays visible in the block and a "Copy path" button sits next to the open button. Known dead end under WSL: the `open` crate puts PowerShell first and passes the target in an env var not listed in `WSLENV`, so the Windows process receives an empty path; use "Copy path" while developing.

### Updater and packaging (deferred)

The tauri updater was removed with the shell; a gpui updater and a packaging pipeline are **not built yet** (nothing has shipped, so there is no auto-update to preserve). When they land, one decision is one-time: the old minisign signing keypair is **not rotatable** (it would have been compiled into every binary, so a new key orphans every installed client) - the gpui updater either reuses that keypair (private key + passphrase in 1Password) or mints a fresh one before the first release. The licence key is meant to ride an updater request header so the licence window can be enforced server-side. Only an AppImage is updatable on Linux; `.deb` is not.

## UI

- Components come from **gpui-component** (the Zed component library); check it before hand-rolling a widget.
- Use the `frontend-design` skill for UI work. The app identity (theme tokens, typography) lives in `crates/app/src/theme.rs`; the palette is shared verbatim with `landing/`.
- gpui gotchas worth knowing: `DataTable` sizes itself, its parent needs `flex_1` + `min_h_0`; `.overflow_y_scroll()` is on `StatefulInteractiveElement` so the element needs an `.id()` first; `.when`/`.when_some` need `use gpui::prelude::FluentBuilder`; setting an `InputState`/`SelectState` value from an async task needs the `cx.update(|cx| cx.active_window())` + `update_window` dance (set_value needs a window); dialogs stack, and a global dialog (host-key, command-approval, mcp-approval) is a free `fn` that defers onto the active window.
