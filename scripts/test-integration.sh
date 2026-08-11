#!/usr/bin/env bash
# Runs the Rust integration suites against the compose databases (pnpm db:test).
# Port plan and version floors: docker-compose.test.yml.
set -euo pipefail
cd "$(dirname "$0")/.."

# Whoever runs this script means it: a missing database must fail the gated
# tests loudly instead of letting them no-op green.
export SOQUEL_REQUIRE_INTEGRATION=1

# Connector suites and the mcp agent surface live in the core crate; the gpui
# frontend runs its browse/stage/apply flow against the same postgres.
core=crates/core/Cargo.toml
gpui=crates/app/Cargo.toml

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
# TLS handshakes are covered by the full suite; the oldest service has no cert.
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5460/soquel_test \
  cargo test --manifest-path "$core" integration_postgres_ -- --skip integration_postgres_tls

echo "==> mysql oldest supported"
SOQUEL_TEST_MYSQL=localhost:5462 \
  cargo test --manifest-path "$core" integration_mysql_

echo "==> mcp agent surface (needs pg + redis + mongo)"
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5455/soquel_test \
SOQUEL_TEST_REDIS=localhost:5457 \
SOQUEL_TEST_MONGO=localhost:5464 \
  cargo test --manifest-path "$core" integration_mcp_

echo "==> gpui flow (browse, stage, apply, tunnel, pg/mysql/sqlite/redis/mongo)"
SOQUEL_TEST_PG=postgres://soquel:soquel@localhost:5455/soquel_test \
SOQUEL_TEST_MYSQL=localhost:5456 \
SOQUEL_TEST_SQLITE=1 \
SOQUEL_TEST_SSH=localhost:5458 \
SOQUEL_TEST_REDIS=localhost:5457 \
SOQUEL_TEST_MONGO=localhost:5464 \
  cargo test --manifest-path "$gpui" integration_

echo "==> mariadb (mysql kind)"
SOQUEL_TEST_MYSQL=localhost:5463 \
  cargo test --manifest-path "$core" integration_mysql_
