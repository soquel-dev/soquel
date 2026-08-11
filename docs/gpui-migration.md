# gpui migration

Target: the webview (Vue + Tauri) is replaced by a gpui app with 100% feature
parity. The spike (branch `gpui`, 2026-08-10) validated the bet: DataTable over
1M rows, tree-sitter SQL editor, schema completion, all against the live core.
This document is the map for the rest.

## Where the patterns come from

Zed is the reference gpui app (242 crates, ~4800 `#[gpui::test]`). We take what
fits a one-person product and skip the rest:

**Take**

- A small cargo workspace of focused crates, one shared target dir.
- The workspace-shell pattern: one `Workspace` root view owning panels and
  modals; panels are entities implementing a common trait, not ad-hoc views.
- Actions + keymap from the start: every user operation is a gpui `action!`
  bound in a keymap context, so the palette, menus and shortcuts share one
  registry (the webview's CommandPalette falls out of this for free).
- A theme module owning design tokens, mapped once onto gpui-component's
  `Theme` (soquel's tokens live in `packages/app/src/style.css` today).
- `test-support` constructors on every panel (`Panel::test_new`) so tests
  build UI without the app shell.
- In-process UI testing: `TestAppContext` (headless) and `VisualTestContext`
  (windowed, `simulate_keystrokes` / `simulate_input` / clicks) on gpui's
  deterministic executor. Zed ships no WebDriver anywhere; neither will we.

**Skip**

- The per-feature crate explosion: zed's granularity serves a large team.
- FakeFs and the settings-sync machinery: soquel's fixture story is real
  databases in docker, which we already have and trust.

## Target layout (after src-tauri is deleted)

```
Cargo.toml          # single workspace, shared target + lockfile
crates/
  core/             # as today: connectors, ssh, credentials, secrets, licence,
                    #   plus the MCP server + tools + approval (core::mcp),
                    #   behind the Approver trait; split to its own agent/ crate
                    #   only if it grows enough to hurt
  app/              # the gpui binary: workspace shell, panels, actions, theme
```

`ui` as a fourth crate (reusable widgets: grid glue, console, inspector) only
when `app` grows enough to hurt; not before.

**Sequencing rule: the root workspace lands the day src-tauri dies, not
before.** A workspace member shares the target dir and lockfile; folding
src-tauri in early would invalidate its build cache for zero benefit, and the
tauri + zed git dependency trees have no reason to meet in one lockfile. Until
then the three units stay detached (`[workspace]` stub in each). That day is the
completion PR below: src-tauri and the webview are deleted, then the three
detached units collapse to one root workspace (core + app), one lockfile, one
target dir.

## Test strategy

Four layers, from cheapest to heaviest. The first two exist today.

1. **Core unit + integration** (exists, the backbone): 240 tests in
   `crates/core`, integration suites `integration_<engine>_*` against the
   compose databases, engine-gated by env vars. Untouched by the migration.
2. **Ported logic tests**: every `packages/app/src/lib/*.ts` module with a
   `.test.ts` is pure logic that moves to Rust with its tests (see checklist).
   The tab-limit rules (`tabs.ts`) are licence enforcement: their tests port
   first.
3. **Panel tests** (`#[gpui::test]` + `TestAppContext`): state logic per panel.
   Started: `grid.rs` covers the insert-on-top slot math, commit un-dirtying,
   readonly/deleted guards, hidden-lead + xmin keys and fk lookup;
   `workspace.rs` covers the ghost-activate guard, close picking the neighbor
   and sql tab numbering. Gotcha, learned the hard way: a file whose parent
   has `use gpui::*` must `use ::core::prelude::v1::test;` inside its tests
   module, or `#[gpui::test]`'s generated `#[test]` resolves to gpui's own
   macro and recurses (the crate also carries `#![recursion_limit = "256"]`).
4. **Flow tests** (real test DBs): started with
   `integration_flow_browse_stage_apply` in the gpui crate (connect ->
   browse sorted -> stage -> apply -> reread), joined by
   `integration_flow_tunnel_trust_and_connect` (TOFU refusal -> trust ->
   query through the forward, against the compose sshd), wired into
   `test-integration.sh` and gated by `SOQUEL_TEST_PG` / `SOQUEL_TEST_SSH`
   like the core suites.
   Input-simulation started with the approval round: `test_support.rs` roots
   the test window on `Root` + a `Shell` that renders the dialog layer,
   buttons carry `debug_selector` tags for `debug_bounds` + `simulate_click`,
   and `wait_until` polls across the core's private tokio runtime. The
   transfer round added the platform seams: native pickers are simulated via
   `simulate_new_path_selection` / `simulate_path_prompt_response`, file
   drops via `FileDropEvent` Entered/Submit/Exited. Still to grow: the
   licence dialog.

## Parity checklist

The migration is done when every line here is checked and the webview is
deleted. Each panel lands with its layer-3 tests; each flow with layer 4.

### Shell

- [x] Workspace shell: sidebar + tabs + panel host + status bar
- [x] Actions + keymap registry
- [x] Command palette (secondary-k, `command_palette.rs` on gpui-component's
      `ListState`/`ListDelegate`; a curated context-aware entry list, not the
      raw action registry - quick-connect + actions per screen, substring
      filter; App holds the cross-screen focus home so the global key works on
      both screens; the List's query input is focused after the dialog opens so
      Enter reaches its confirm, not the dialog's)
- [x] Theme: port tokens from `style.css`, light/dark
- [x] Toasts (gpui-component `push_notification`, used by revoke and transfer)
- [x] Tab limit (`tabs.rs` ports `lib/tabs.ts` with its tests; free-tier toast,
      SOQUEL_TAB_LIMIT dev override honored; `effective_tab_limit` now consults
      the installed licence so a valid one lifts the cap - `Workspace` reads the
      licence file per tab-open, so an activation lifts it live; tab persistence
      across restarts still to do)

### Connections

- [x] Connections page: list, groups, env badges, connect/edit/delete (the
      startup screen; connects through core::ops against the same data dir as
      the tauri dev app)
- [x] Connection form, all kinds (fields, test connection, keychain/prompt/
      command credential modes with the argv preview; agent-access select maps
      into `ConnectionInput` and an "agent" badge marks opted-in rows).
      Multi-kind landed as phase 1 of the tauri-removal PR: an engine select
      (`ENGINE_CHOICES`, MariaDB a display entry riding the mysql kind),
      kind-conditional fields (sqlite=path, redis=host/port/db-index/tls/user,
      mongo=host/port/database/auth-source/tls/user, pg+mysql=host/port/database/
      user/ssl + CA cert on verify-full), `form_input`/`prefill_form` per kind,
      `port_for_kind_change` on engine switch, a "From URL" field that prefills
      via `parse_connection_url` (libpq + mysql ssl-mode maps, percent-decoded
      userinfo), `server_badge` (mariadb/valkey by version) on the workspace,
      per-kind `dsn`, redis/mongo credential-command caveats. `lib/connections.ts`
      is fully ported. Small-screen dialog height still to do
- [x] Tunnel form + tunnel list (`tunnels.rs` ports `lib/tunnels.ts` with its
      tests; the connection form's picker maps by index since names collide)
- [x] Secret prompt dialog (SecretRequired -> unlock -> retry, with the
      keep-for-session checkbox; title is subject-aware for tunnels; Enter
      submits)
- [x] Credential command approval dialog (resolved argv chips, approve ->
      retry; "Revoke command" on command-mode rows, both kinds; Enter is a
      deliberate no-op on approval and host-key dialogs, Escape cancels)
- [x] Host key trust dialog + panel (dialog only: gpui dialogs stack in
      `Root.active_dialogs`, so the form-inline panel the Vue focus trap
      forced has no reason to exist)
- [x] Import dialog (`transfer.rs` ports `lib/transfer.ts` with its tests;
      native picker + a `.soquel` drop on the connections screen replace the
      tauri `importFileRequested` event, which had no production caller;
      sticky passphrase step; RadioGroup's first use for the duplicates)
- [x] Export dialog (passphrase validated before the picker opens; gpui
      pickers have no extension filters, so a bare name gets `.soquel`
      appended after the pick)

### SQL workspace

- [x] Table grid: virtual scroll, cell selection, column resize (spike)
- [x] Grid: server-side sort + filters (`filters.rs` ports `lib/filters.ts` with its tests)
- [x] Cell editing: staged changes, apply, ctid/xmin guards (`staged.rs` and
      `cell_editing.rs` port `staged.ts` / `cell-editing.ts` with their tests;
      bool/date cells edit as text for now, preview dialog wrapping to polish)
- [x] Grid inspector (cell detail panel: json pretty + highlight, copy,
      FK hop into a filtered tab; the inline cell hop arrow is still to add)
- [x] Export menu (copy-as to clipboard, save-as via the native dialog; table
      tabs stream the full filtered/sorted table with live progress, sql tabs
      write the held result; format/export helpers moved into core, shared
      with the webview)
- [x] SQL editor: highlighting, completion, run (spike)
- [x] SQL editor: sessions, cancel, history (`history.rs` ports
      `query-history.ts` with its tests; one pinned session per sql tab,
      selection-or-all runs, cancel while running; history is in-memory until
      the persistence layer lands - same gap as tab restore)
- [x] Explain tree (`explain.rs` ports `explain.ts` with its full suite and
      the real captured fixtures: pg json, mysql wrappers/prefix_cost,
      mariadb flavor, sqlite EQP, tree text; heat bars, collapse, estimate-off.
      mysql/sqlite parsers wait on their connectors; raw json view to add)
- [x] Schema tree sidebar + DDL view (gpui-component Tree, kind icons via the
      custom `SoquelIcon` set in `assets/icons/`, filter input, compact row
      estimates; Data/DDL toggle with highlighted sql and copy)
- [x] Results vs browse tabs model (table tabs and sql tabs each own their
      grid/editor state; ctrl-tab cycles, close picks the right neighbor)

### Other engines

- [x] Redis: key list, key detail, db select, console (`kv.rs` is a whole
      `KvWorkspace` mounted by App when the connection has the `KvBrowse`
      capability - the first branch on kind, leaving the SQL `Workspace`
      untouched; ports `lib/kv.ts` (contains-search glob, ttl format, type
      badges) with its tests; scan/scan-more, per-type value render, string
      edit/ttl/delete, db reconnect-swap, redis console. Detail render is
      covered by `integration_flow_redis_browse` + the visual pass, not a
      gpui render test - faking a `Connection` for a headless view isn't worth
      it)
- [x] Mongo: collection list, doc list/detail, indexes, db select, console
      (`doc.rs` is a `DocWorkspace` mounted by App on the `DocBrowse`
      capability - the third kind branch after redis; reuses the KvWorkspace
      skeleton with the doc specifics: two-level tree (databases -> collections,
      no db reconnect - mongo addresses any db per call), a nested list+detail
      split, the 3-view toggle documents/indexes/console, JSON render via
      `TextView::markdown` (the grid inspector's pattern), edit = full-doc
      replace from the canonical extjson, read-only for docs without `_id`, and
      a JSON find filter (not a glob). `doc.rs` ports `lib/docs.ts` with its
      tests; behaviour is covered by `integration_flow_mongo_browse` +
      `integration_doc_workspace_*` against the seeded `soquel_e2e`, render by
      the visual pass)

### App surfaces

- [x] MCP panel: status dot + endpoint, port (commits on blur/Enter, restarts
      a running server), on/off toggle, masked setup command with copy, the
      exposed-summary line, the trust-windows amber card (ticks down every 1s,
      Revoke), "Regenerate token" while stopped; opened from the palette
      ("MCP server") and an "Agents…" header button (`mcp.rs`, `McpPanel`)
- [x] MCP audit log: `McpAuditView` dialog, newest-first rows (ok/err dot,
      tool, target, time, detail, error), empty state; palette "Agent activity"
- [x] MCP approval dialog (`mcp_approval.rs`, global like host-key/command):
      operation + payload blocks, "{n} more waiting", Deny / Allow for 15 min /
      Run this write; Enter is a no-op, dismissing denies. The core lift moved
      the server, tools and approval into `core::mcp` (rmcp/axum now core deps),
      the 7 `integration_mcp_*` + unit tests moved with it (the last tauri-only
      tests; the `app` test leg is retired), and the tauri event became the
      `GpuiApprover` mpsc seam: a blocked write rides the channel to the App,
      the human's answer fires the oneshot the server thread waits on, the 60s
      timeout and silence-denies staying in core
- [x] Licence dialog (`licence.rs`, `LicenceView`, from the palette): status
      blurb per state (Free / Licensed / Expired), key field + Activate (the
      activation HTTP is in the core, no network permission), a folded "I have a
      licence file" textarea + Add, outcome line (green/destructive) via
      `installed_outcome` + the exhaustive `activation_message` map. A successful
      install writes the file, so the open workspace's cap lifts on next open.
      `soquel_core::licence::path` is now shared by both frontends
- [x] Diagnostics dialog (`diagnostics.rs`, `DiagnosticsView`, from the palette):
      the pasteable block (built in `core::diagnostics`, no names or hosts - the
      `render`/`block` lifted out of `src-tauri`, tests moved too), Copy, Copy
      path (pulled from the block's `log:` line), and a best-effort Open log
      folder via the `open` crate (detached, creates the dir first). Logs land
      under `<data dir>/logs` and the path is real now that logging is wired
      (below)
- [ ] Update panel: check, progress, install/restart. **Deferred past the
      tauri-removal PR** (greenfield, no `updater.rs` lift): dev builds skip the
      update check and nothing has shipped, so auto-update is not a deletion
      blocker. When built, decide whether the gpui updater reuses the existing
      minisign keypair (`tauri.conf.json` pubkey, **not rotatable**, private key
      in 1Password) or mints a fresh one - one-time window while no client exists.
- [ ] Single instance + `.soquel` file association. **Never existed in either
      frontend** (no `bundle.fileAssociations`, no single-instance plugin, no
      argv reading; the `open_connections_file` command only emitted an event to
      the webview as an e2e route). Nothing to port - build fresh in gpui later
      if wanted (argv parse in `main.rs` + single-instance + OS association in
      the bundler).

### Platform work (no webview equivalent, must exist before shipping)

- [ ] Packaging: installers + updater feed replacing the Tauri bundler.
      **Deferred past the tauri-removal PR** (greenfield, the tauri bundler
      config does not carry over; candidate: cargo-packager). Ships with the
      updater decision above (signing key reuse-or-mint).
- [x] Logging: `core::init_logging` (fern) to `<data dir>/logs/soquel[-dev].log`,
      called before `init_state` so the keyring probe is captured; Warn floor,
      Info for `soquel_core`/`soquel_gpui` (our lines stay above russh/hyper/
      rustls), Stdout in debug, a startup size cap in place of the plugin's
      rotation. The opener rides the `open` crate (diagnostics, above)
- [ ] File dialogs (gpui's own `prompt_for_paths`/`prompt_for_new_path` cover
      import/export - no filters, extension appended by hand; rfd likely
      unneeded)
- [ ] Data dir + keychain service names unchanged (installed apps must not
      lose their connections or secrets on the switch)
- [x] MCP autostart parity (App reads the persisted toggle at launch and
      brings the server back on `runtime()` with the `GpuiApprover` factory)

## Feature-parity discipline

Held through the chapter-by-chapter migration; the completion PR below closes it
out. While both frontends existed:

- A webview surface was deleted only in the commit where its gpui replacement
  and tests landed. The webview kept running (`pnpm dev`) as the reference
  implementation, not dead code.
- The `bindings.ts` surface was the functional contract: any behavior reachable
  from a command had to be reachable from the gpui app. Every core lift kept the
  bindings byte-identical so the webview never regressed.

## Removing tauri (the completion PR)

Decided once it was clear nothing had shipped and there are no users but the dev:
the only thing keeping tauri was "it is what ships", which is false, so the
migration completes now rather than dragging two frontends. An audit of
`src-tauri` and `packages/app` (2026-08-11) drew the line between what must be
ported first and what is safe to delete.

**The audit result.** `src-tauri/src/commands.rs` (74 commands) is a thin IPC
skin over `soquel_core::*`; `src-gpui/src/core.rs` already mirrors that surface,
so the command layer, `lib.rs` (specta builder + bindings export + plugin chain
+ setup), `mcp.rs` (DialogApprover/event) and `diagnostics.rs` are safe to
delete. The only genuinely tauri-only Rust logic is `updater.rs`. `.soquel` file
association and single-instance never existed. `SOQUEL_BUILD_DATE` is stamped by
`crates/core/build.rs`, so gpui inherits it. On the webview side, every
`lib/*.ts` pure-logic module already has a tested Rust twin - except the
multi-kind surface of `lib/connections.ts`, which is the one deletion blocker
(the gpui form is postgres-only).

**Order (one PR, staged commits):**

1. **Port the blockers.** DONE. The multi-kind connection form landed (see the
   Connections checklist). `secrets_status` is wired: both the connection and
   tunnel forms read `AppState.secrets_problem` directly (gpui holds the
   `Arc<AppState>`, no bridge), drop keychain from the credential picker when the
   keyring probe failed (`available_credential_modes`), default a new profile to
   `prompt` (`default_credential_mode`), and show the amber problem line.
2. **Delete** `src-tauri` and `packages/app` (with the wdio e2e, `bindings.ts`,
   `tauri.conf.json`, `capabilities/`, tauri icons). Rewrite the root
   `package.json` (`dev` = `cargo run -p soquel-app`, drop the `pnpm -r` webview
   scripts and `test:e2e`, keep the `db:*`/docker scripts and eslint), drop the
   wdio/tauri-driver devDeps.
3. **Collapse to a root cargo workspace**: `crates/core` + `crates/app`
   (`src-gpui` renamed), remove the two `[workspace]` stubs, one committed
   `Cargo.lock` (the git pins for zed/gpui-component must survive the merge), one
   target dir.
4. **CI**: the `rust` job builds/clippies/tests core + app with the gpui system
   deps (`libxkbcommon-x11-dev`, `libx11-xcb-dev`, x11/wayland) instead of the
   tauri ones, no bindings check; drop the `check` (webview) and `e2e_tests`
   (wdio) jobs; keep `landing` and `integration_tests`. Verify whether gpui
   `cargo test` links on Windows (the tauri `STATUS_ENTRYPOINT_NOT_FOUND` skip
   may not apply) - if not, tests Linux-first, clippy on all three.
5. **Restore the lost coverage**: `integration_flow_mysql` + `integration_flow_
   sqlite` (the only real e2e gap, unblocked once the form can create them).

**Deferred past this PR** (greenfield, not deletion blockers, nothing ships
yet): the updater (+ the non-rotatable signing-key reuse-or-mint decision) and
the packaging pipeline; optionally `.soquel` association + single-instance.

**Design differences kept, not ported:** `stream_table_rows`/`RowsChunk` (the
gpui grid pages via `fetch_rows`, no streaming); the server-side sql-session
registry (`state.sessions` keyed by uuid) is a UI-held `Session` handle in gpui.
