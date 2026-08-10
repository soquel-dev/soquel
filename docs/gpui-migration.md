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
  core/             # as today: connectors, ssh, credentials, secrets, licence
  agent/            # the MCP server, out of src-tauri, behind the Approver trait
  app/              # the gpui binary: workspace shell, panels, actions, theme
```

`ui` as a fourth crate (reusable widgets: grid glue, console, inspector) only
when `app` grows enough to hurt; not before.

**Sequencing rule: the root workspace lands the day src-tauri dies, not
before.** A workspace member shares the target dir and lockfile; folding
src-tauri in now would invalidate its build cache for zero benefit, and the
tauri + zed git dependency trees have no reason to meet in one lockfile.
Until then the three units stay detached (`[workspace]` stub in each).

## Test strategy

Four layers, from cheapest to heaviest. The first two exist today.

1. **Core unit + integration** (exists, the backbone): 240 tests in
   `crates/core`, integration suites `integration_<engine>_*` against the
   compose databases, engine-gated by env vars. Untouched by the migration.
2. **Ported logic tests**: every `packages/app/src/lib/*.ts` module with a
   `.test.ts` is pure logic that moves to Rust with its tests (see checklist).
   The tab-limit rules (`tabs.ts`) are licence enforcement: their tests port
   first.
3. **Panel tests** (`#[gpui::test]` + `TestAppContext`): state logic per panel
   through its `test_new` constructor: delegate paging, staged edits, dialog
   state machines. No window needed, milliseconds each.
4. **Flow tests** (`#[gpui::test]` + `VisualTestContext` + real test DBs):
   the wdio e2e suite's replacement, in-process this time: connect -> browse
   -> stage a cell edit -> apply; run a query -> read the grid; import a
   `.soquel` file; licence dialog states. Gated by the same `SOQUEL_TEST_*`
   env vars as the core suites, so they skip silently without docker.

To validate early: that gpui-component's widgets respond to
`simulate_keystrokes` through their own key contexts. Proven inside the first
flow test; if a widget resists, its panel test drives the entity directly
instead.

## Parity checklist

The migration is done when every line here is checked and the webview is
deleted. Each panel lands with its layer-3 tests; each flow with layer 4.

### Shell

- [ ] Workspace shell: sidebar + tabs + panel host + status bar
- [ ] Actions + keymap registry
- [ ] Command palette (from the action registry)
- [ ] Theme: port tokens from `style.css`, light/dark
- [ ] Toasts (gpui-component notifications)
- [x] Tab limit (`tabs.rs` ports `lib/tabs.ts` with its tests; free-tier toast,
      SOQUEL_TAB_LIMIT dev override honored; tab persistence across restarts
      still to do)

### Connections

- [ ] Connections page: list, groups, env badges
- [ ] Connection form (create/update, test connection, credential modes)
- [ ] Tunnel form + tunnel list
- [ ] Secret prompt dialog (`prompt` credential mode)
- [ ] Credential command approval dialog
- [ ] Host key trust dialog + panel
- [ ] Import dialog (preview, duplicates, passphrase, `.soquel` open/drop)
- [ ] Export dialog (secrets opt-in, encryption)

### SQL workspace

- [x] Table grid: virtual scroll, cell selection, column resize (spike)
- [x] Grid: server-side sort + filters (`filters.rs` ports `lib/filters.ts` with its tests)
- [x] Cell editing: staged changes, apply, ctid/xmin guards (`staged.rs` and
      `cell_editing.rs` port `staged.ts` / `cell-editing.ts` with their tests;
      bool/date cells edit as text for now, preview dialog wrapping to polish)
- [ ] Grid inspector (cell detail)
- [ ] Export menu (rows/statement to file)
- [x] SQL editor: highlighting, completion, run (spike)
- [x] SQL editor: sessions, cancel, history (`history.rs` ports
      `query-history.ts` with its tests; one pinned session per sql tab,
      selection-or-all runs, cancel while running; history is in-memory until
      the persistence layer lands - same gap as tab restore)
- [ ] Explain tree (`explain.ts` port)
- [x] Schema tree sidebar + DDL view (gpui-component Tree, kind icons via the
      custom `SoquelIcon` set in `assets/icons/`, filter input, compact row
      estimates; Data/DDL toggle with highlighted sql and copy)
- [x] Results vs browse tabs model (table tabs and sql tabs each own their
      grid/editor state; ctrl-tab cycles, close picks the right neighbor)

### Other engines

- [ ] Redis: key list, key detail, db select, console
- [ ] Mongo: collection list, doc list/detail, indexes, db select, console

### App surfaces

- [ ] MCP panel: status, port, token, audit log, trust windows
- [ ] MCP approval dialog (blocks the agent call until answered)
- [ ] Licence dialog: status, paste key (activation), paste file
- [ ] Diagnostics dialog: block preview, copy, open log folder
- [ ] Update panel: check, progress, install/restart
- [ ] Single instance + `.soquel` file association

### Platform work (no webview equivalent, must exist before shipping)

- [ ] Packaging: installers + updater feed replacing the Tauri bundler
      (candidate: cargo-packager; **verify the existing updater signing key
      and manifest format carry over**, the key is not rotatable and a format
      break orphans every installed client)
- [ ] File dialogs (rfd), opener, logging (fern/tracing to the same log files)
- [ ] Data dir + keychain service names unchanged (installed apps must not
      lose their connections or secrets on the switch)
- [ ] MCP autostart parity

## Feature-parity discipline

- A webview surface is deleted only in the commit where its gpui replacement
  and tests land. The webview keeps running (`pnpm dev`) until the last box is
  checked: it is the reference implementation, not dead code.
- The `bindings.ts` surface (77 commands) is the functional contract: any
  behavior reachable from a command must be reachable from the gpui app.
- CI: `src-gpui` gets a job (fmt, clippy, test) on all three platforms; the
  flow tests join `test-integration.sh`.
