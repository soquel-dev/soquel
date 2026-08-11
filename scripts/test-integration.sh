#!/usr/bin/env bash
# Runs the Rust integration suites against the compose databases (pnpm db:test).
# Port plan and version floors: docker-compose.test.yml.
set -euo pipefail
cd "$(dirname "$0")/.."

# Connector suites live in the core crate; the mcp agent surface stays app-side,
# and the gpui frontend runs its browse/stage/apply flow against the same postgres.
core=crates/core/Cargo.toml
app=src-tauri/Cargo.toml
gpui=src-gpui/Cargo.toml

echo "==> full suite (current versions)"
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5455/soquel_test \
SOQUEL_TEST_PG_TLS=postgres://soquel:soquel@localhost:5459/soquel_test \
SOQUEL_TEST_MYSQL=localhost:5456 \
SOQUEL_TEST_REDIS=localhost:5457 \
SOQUEL_TEST_MONGO=localhost:5464 \
SOQUEL_TEST_SSH=localhost:5458 \
SOQUEL_TEST_SSH_RECONNECT=localhost:5461 \
  cargo test --manifest-path "$core" integration_

echo "==> postgres oldest supported"
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5460/soquel_test \
  cargo test --manifest-path "$core" integration_postgres_

echo "==> mysql oldest supported"
SOQUEL_TEST_MYSQL=localhost:5462 \
  cargo test --manifest-path "$core" integration_mysql_

echo "==> mcp agent surface (needs pg + redis + mongo)"
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5455/soquel_test \
SOQUEL_TEST_REDIS=localhost:5457 \
SOQUEL_TEST_MONGO=localhost:5464 \
  cargo test --manifest-path "$app" integration_mcp_

echo "==> gpui flow (browse, stage, apply, tunnel)"
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5455/soquel_test \
SOQUEL_TEST_SSH=localhost:5458 \
  cargo test --manifest-path "$gpui" integration_

echo "==> mariadb (mysql kind)"
SOQUEL_TEST_MYSQL=localhost:5463 \
  cargo test --manifest-path "$core" integration_mysql_
