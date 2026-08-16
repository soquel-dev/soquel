#!/bin/sh
# Dev seed: SaaS-shaped keys mirroring the sql dev seeds (sessions, cache,
# queues, counters). Re-runnable: flushes the instance first.
#   session:<hex>        10 000 strings, TTL
#   cache:user:<id>       2 000 json strings, TTL
#   user:<id>             1 000 hashes
#   queue:emails/webhooks   500 lists
#   leaderboard:tasks     1 000 zset members
#   online:users            300 set members
#   events:stream         1 000 stream entries
#   counter:api:<day>        30 strings
set -e
auth="-a soquel --no-auth-warning"

# Everything dated hangs off this instead of the wall clock, so re-seeding with
# the same base date reproduces the same keys (website screenshots).
base_epoch="${SEED_BASE_EPOCH:-$(date -u +%s)}"
base_month="${SEED_BASE_MONTH:-$(date -u +%Y-%m)}"

redis-cli $auth FLUSHALL > /dev/null

# j(): quote an inline-protocol argument; | stands for a json quote.
# Names come from the same lists as the sql seeds, walked by a stride coprime
# with their 340 pairs so both names move on every key.
awk -v base_epoch="$base_epoch" -v base_month="$base_month" '
function j(s) { gsub(/\|/, "\\\\\"", s); return "\"" s "\"" }
function pair(i) { return (i * 21) % (nf * nl) }
function local_part(i) { return tolower(F[1 + pair(i) % nf]) "." tolower(L[1 + int(pair(i) / nf) % nl]) }
function full_name(i) { return F[1 + pair(i) % nf] " " L[1 + int(pair(i) / nf) % nl] }
function email(i) { return local_part(i) "@" B[1 + i % nb] "." T[1 + i % nt] }
BEGIN {
  nf = split("Alice Marcus Priya Tomas Chloe Daniel Sofia Omar Hannah Lucas Nadia Felix Clara Victor Amina Jonas Elena Mateo Iris Samuel", F, " ")
  nl = split("Bennett Novak Iyer Lindqvist Moreau Okafor Ferrari Haddad Weber Silva Kovacs Duarte Larsen Nakamura Fischer Almeida Whitfield", L, " ")
  nb = split("northwind lakeside meridian brightline harborview crestline evergreen stonebridge westport ironwood", B, " ")
  nt = split("io com dev", T, " ")

  # % 2^31: busybox awk %x clamps above INT32_MAX, which would collide keys.
  for (i = 1; i <= 10000; i++)
    printf "SET session:%08x %s EX %d\n", i * 2654435761 % 2147483648, \
      j(sprintf("{|uid|:%d,|email|:|%s|}", i % 1000, email(i))), 3600 + i % 86400
  for (i = 1; i <= 2000; i++)
    printf "SET cache:user:%d %s EX %d\n", i, \
      j(sprintf("{|id|:%d,|name|:|%s|,|plan|:|%s|,|seen|:%d}", i, full_name(i), (i % 5 == 0 ? "pro" : "free"), base_epoch - i)), 300 + i % 3600
  for (i = 1; i <= 1000; i++)
    printf "HSET user:%d name %s email %s plan %s logins %d\n", i, j(full_name(i)), email(i), (i % 5 == 0 ? "pro" : "free"), i % 400
  for (i = 1; i <= 500; i++) {
    printf "RPUSH queue:emails %s\n", j(sprintf("{|to|:|%s|,|template|:|digest|}", email(i)))
    printf "RPUSH queue:webhooks %s\n", j(sprintf("{|url|:|https://hooks.%s.%s/soquel|,|attempt|:%d}", B[1 + i % nb], T[1 + i % nt], i % 5))
  }
  for (i = 1; i <= 1000; i++)
    printf "ZADD leaderboard:tasks %d %s\n", i * 7 % 5000, local_part(i)
  for (i = 1; i <= 300; i++)
    printf "SADD online:users %s\n", local_part(i * 3 % 1000)
  for (i = 1; i <= 1000; i++)
    printf "XADD events:stream * type %s user %s\n", (i % 3 == 0 ? "login" : "task.done"), local_part(i)
  for (i = 1; i <= 30; i++)
    printf "SET counter:api:%s-%02d %d\n", base_month, i, i * 1337
}' | redis-cli $auth --pipe

# One binary payload so the hex rendering shows up in the browser.
redis-cli $auth SET bin:packed "$(printf '\377\376packed-bytes\375')" > /dev/null

redis-cli $auth DBSIZE
