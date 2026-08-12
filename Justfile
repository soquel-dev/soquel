# Task shortcuts over cargo + docker. `just` (cargo install just / brew install just).
# Rust lint/format/test are plain cargo; landing/ has its own pnpm scripts.

# List the recipes.
default:
    @just --list

# Run the app.
dev:
    cargo run -p soquel-app

# Run the app with file-backed plaintext secrets, for a host with no OS keychain; dev only.
dev-insecure:
    SOQUEL_INSECURE_FILE_SECRETS=1 cargo run -p soquel-app

# Build the app.
build:
    cargo build -p soquel-app

# Unit tests (integration_* skip without their SOQUEL_TEST_* env vars).
test:
    cargo test --workspace

# Integration tests against the compose databases (needs `just db-test`).
test-integration:
    bash scripts/test-integration.sh

# The throwaway test databases (docker-compose.test.yml).
db-test:
    docker compose -f docker-compose.test.yml up -d --wait
db-test-down:
    docker compose -f docker-compose.test.yml down

# The persistent dev databases: postgres 5470, mysql 5471, redis 5472, mongo 5473.
db-dev:
    docker compose -f docker-compose.dev.yml up -d --wait
db-dev-down:
    docker compose -f docker-compose.dev.yml down

# (Re)seed the dev databases; no engine = all. e.g. `just db-dev-seed pg`.
db-dev-seed engine="":
    bash scripts/dev-seed/seed.sh {{ engine }}

# Remove generated Docker dev connections and their stored passwords.
reset-dev-connections:
    cargo run -p soquel-core --bin soquel-dev-connections -- reset

# Recreate Docker dev connections and store their passwords in the keychain.
seed-dev-connections:
    cargo run -p soquel-core --bin soquel-dev-connections -- seed
