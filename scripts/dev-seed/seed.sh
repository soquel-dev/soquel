#!/usr/bin/env bash
# Seeds the dev databases (docker-compose.dev.yml). Usage:
#   scripts/dev-seed/seed.sh [pg|mysql|redis|mongo]...   (no argument = all)
#
# SOQUEL_SEED_BASE_DATE pins the "now" every seed is built around, so a re-seed
# reproduces the same rows and a website screenshot can be retaken later.
# Unset means the wall clock.
# SOQUEL_SEED_DATABASE names the target database (default soquel_dev); it is
# created if missing. Redis has none.
set -euo pipefail

cd "$(dirname "$0")/../.."
compose=(docker compose -f docker-compose.dev.yml)
db="${SOQUEL_SEED_DATABASE:-soquel_dev}"

# Resolved once on the host, and by node rather than date(1): the containers
# have busybox date or none, and BSD date has no -d.
resolved=$(node -e '
  let raw = process.argv[1]
  if (raw === "now") {
    raw = new Date().toISOString()
  }
  else {
    raw = raw.replace(" ", "T")
    if (!raw.includes("T"))
      raw += "T00:00:00"
    if (!/[zZ]|[+-]\d\d:?\d\d$/.test(raw))
      raw += "Z"
  }
  const at = new Date(raw)
  if (Number.isNaN(+at))
    throw new Error(`SOQUEL_SEED_BASE_DATE is not a date: ${process.argv[1]}`)
  const iso = at.toISOString()
  process.stdout.write(`${iso.slice(0, 10)} ${iso.slice(11, 19)}|${Math.floor(+at / 1000)}|${iso.slice(0, 7)}`)
' "${SOQUEL_SEED_BASE_DATE:-now}")
IFS='|' read -r base_sql base_epoch base_month <<< "$resolved"
echo "seeding $db with base date $base_sql UTC"

seed_pg() {
  # Nonzero when it already exists, which is the normal case.
  "${compose[@]}" exec -T postgres createdb -U soquel "$db" 2> /dev/null || true
  "${compose[@]}" exec -T postgres \
    psql -U soquel -d "$db" -v base_date="$base_sql" -f /seeds/postgres.sql
}

seed_mysql() {
  # The prelude is built here rather than in the container: quoting an
  # identifier and a password through docker exec sh -c is not worth it. As
  # root, since creating a database and a user is beyond the soquel grants.
  {
    cat << SQL
CREATE DATABASE IF NOT EXISTS \`$db\`;
USE \`$db\`;
CREATE USER IF NOT EXISTS 'api'@'%' IDENTIFIED BY 'api';
GRANT ALL PRIVILEGES ON \`$db\`.* TO 'api'@'%';
SET @base_date = '$base_sql';
SQL
    cat scripts/dev-seed/mysql.sql
  } | "${compose[@]}" exec -T mysql mysql -uroot -proot
}

seed_redis() {
  "${compose[@]}" exec -T -e SEED_BASE_EPOCH="$base_epoch" -e SEED_BASE_MONTH="$base_month" \
    redis sh /seeds/redis.sh
}

seed_mongo() {
  "${compose[@]}" exec -T -e SEED_BASE_EPOCH="$base_epoch" -e SEED_DATABASE="$db" mongo \
    mongosh --quiet -u soquel -p soquel --authenticationDatabase admin /seeds/mongo.js
}

engines=("$@")
if [ ${#engines[@]} -eq 0 ]; then
  engines=(pg mysql redis mongo)
fi

for engine in "${engines[@]}"; do
  case "$engine" in
    pg | mysql | redis | mongo) "seed_$engine" ;;
    *)
      echo "unknown engine: $engine (pg, mysql, redis, mongo)" >&2
      exit 1
      ;;
  esac
done
