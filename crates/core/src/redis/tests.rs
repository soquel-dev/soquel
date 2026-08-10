use super::*;
use crate::connectors::Connector;
use crate::profiles::{Env, RedisParams};

// -------- pure logic, always on --------

#[test]
fn split_command_line_handles_quotes_and_escapes() {
  assert_eq!(
    split_command_line("SET mykey \"hello world\"").unwrap(),
    ["SET", "mykey", "hello world"]
  );
  assert_eq!(
    split_command_line("  GET   'a b'  ").unwrap(),
    ["GET", "a b"]
  );
  assert_eq!(
    split_command_line(r#"SET k "line\nnext""#).unwrap(),
    ["SET", "k", "line\nnext"]
  );
  // Empty quoted strings are real arguments.
  assert_eq!(split_command_line("SET k \"\"").unwrap(), ["SET", "k", ""]);
  assert!(split_command_line("GET \"unbalanced").is_err());
  assert_eq!(split_command_line("   ").unwrap(), Vec::<String>::new());
}

#[test]
fn render_value_speaks_redis_cli() {
  let mut lines = Vec::new();
  render_value(&Value::Okay, 0, None, &mut lines);
  render_value(&Value::Nil, 0, None, &mut lines);
  render_value(&Value::Int(42), 0, None, &mut lines);
  render_value(&Value::BulkString(b"hi".to_vec()), 0, None, &mut lines);
  assert_eq!(lines, ["OK", "(nil)", "(integer) 42", "\"hi\""]);

  let mut nested = Vec::new();
  render_value(
    &Value::Array(vec![
      Value::BulkString(b"a".to_vec()),
      Value::Array(vec![Value::Int(1), Value::Int(2)]),
    ]),
    0,
    None,
    &mut nested,
  );
  // Nested replies indent one level and renumber.
  assert_eq!(nested, ["1) \"a\"", "  1) (integer) 1", "  2) (integer) 2"]);

  let mut empty = Vec::new();
  render_value(&Value::Array(vec![]), 0, None, &mut empty);
  assert_eq!(empty, ["(empty array)"]);
}

#[test]
fn parse_info_version_prefers_valkey() {
  let redis = "# Server\r\nredis_version:7.4.1\r\nredis_mode:standalone\r\n";
  assert_eq!(parse_info_version(redis).as_deref(), Some("7.4.1"));
  let valkey = "# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.1\r\n";
  assert_eq!(parse_info_version(valkey).as_deref(), Some("8.0.1-valkey"));
  assert_eq!(parse_info_version("# Server\r\n"), None);
}

#[test]
fn ttl_sentinels_map_to_none() {
  assert_eq!(ttl_ms(-2), None);
  assert_eq!(ttl_ms(-1), None);
  assert_eq!(ttl_ms(0), Some(0.0));
  assert_eq!(ttl_ms(1500), Some(1500.0));
}

#[test]
fn display_bytes_hexes_non_utf8() {
  assert_eq!(display_bytes(b"plain".to_vec()), "plain");
  assert_eq!(display_bytes("héllo".as_bytes().to_vec()), "héllo");
  assert_eq!(display_bytes(vec![0x00, 0xff, 0x67]), "0x00ff67");
  assert_eq!(display_bytes(Vec::new()), "");
}

#[test]
fn parse_keyspace_reads_db_lines() {
  let info =
    "# Keyspace\r\ndb0:keys=13036,expires=12000,avg_ttl=42\r\ndb3:keys=2,expires=0,avg_ttl=0\r\n";
  let used = parse_keyspace(info);
  assert_eq!(used.len(), 2);
  assert_eq!((used[0].db, used[0].keys), (0, 13036.0));
  assert_eq!((used[1].db, used[1].keys), (3, 2.0));
  assert!(parse_keyspace("# Keyspace\r\n").is_empty());
}

// -------- integration, gated on the compose redis --------

fn params_from_env(db: u32) -> Option<RedisParams> {
  let addr = std::env::var("SOQUEL_TEST_REDIS").ok()?;
  let (host, port) = addr
    .split_once(':')
    .expect("SOQUEL_TEST_REDIS is host:port");
  Some(RedisParams {
    host: host.to_string(),
    port: port.parse().unwrap(),
    db,
    username: None,
    tls: false,
    tunnel_id: None,
  })
}

fn profile_from_env(db: u32) -> Option<ConnectionProfile> {
  Some(ConnectionProfile {
    id: String::new(),
    name: "test".to_string(),
    env: Env::Dev,
    group: None,
    agent_access: Default::default(),
    credential: Default::default(),
    params: ConnectorParams::Redis(params_from_env(db)?),
  })
}

async fn connection_from_env(db: u32) -> Option<Box<dyn Connection>> {
  let profile = profile_from_env(db)?;
  Some(
    RedisConnector
      .connect(
        &profile,
        Credentials::fixed(Some("soquel".to_string())),
        None,
      )
      .await
      .unwrap(),
  )
}

/// Raw client for seeding bytes the str-typed kv trait cannot inject.
async fn raw_connection() -> Option<MultiplexedConnection> {
  let params = params_from_env(0)?;
  let client =
    redis::Client::open(format!("redis://:soquel@{}:{}/0", params.host, params.port)).unwrap();
  Some(client.get_multiplexed_tokio_connection().await.unwrap())
}

/// Each test owns a prefix; wiping it first keeps reruns deterministic.
async fn wipe_prefix(kv: &dyn KvBrowse, prefix: &str) {
  let mut cursor: Option<String> = None;
  loop {
    let page = kv
      .scan_keys(&format!("{prefix}*"), cursor.as_deref(), 100)
      .await
      .unwrap();
    for entry in &page.keys {
      kv.delete_key(&entry.key).await.unwrap();
    }
    cursor = page.cursor;
    if cursor.is_none() {
      break;
    }
  }
}

#[tokio::test]
async fn integration_redis_version_and_health() {
  let Some(connection) = connection_from_env(0).await else {
    return;
  };
  connection.health().await.unwrap();
  let version = connection.server_version().unwrap();
  assert!(
    version.chars().next().unwrap().is_ascii_digit(),
    "{version}"
  );
  assert!(connection.sql().is_none());
  assert!(connection.kv().is_some());
}

#[tokio::test]
async fn integration_redis_wrong_password_fails() {
  let Some(profile) = profile_from_env(0) else {
    return;
  };
  let outcome = RedisConnector
    .connect(
      &profile,
      Credentials::fixed(Some("wrong".to_string())),
      None,
    )
    .await;
  assert!(matches!(outcome, Err(Error::Database { .. })));
}

#[tokio::test]
async fn integration_redis_scan_paginates_with_patterns() {
  let Some(connection) = connection_from_env(0).await else {
    return;
  };
  let kv = connection.kv().unwrap();
  let prefix = "soquel_test:scan:";
  wipe_prefix(kv, prefix).await;
  for index in 0..250 {
    kv.set_string(&format!("{prefix}{index}"), "x")
      .await
      .unwrap();
  }

  let mut collected = Vec::new();
  let mut cursor: Option<String> = None;
  let mut pages = 0;
  loop {
    let page = kv
      .scan_keys(&format!("{prefix}*"), cursor.as_deref(), 100)
      .await
      .unwrap();
    collected.extend(page.keys);
    pages += 1;
    cursor = page.cursor;
    if cursor.is_none() {
      break;
    }
    assert!(pages < 50, "cursor never converged");
  }
  assert_eq!(collected.len(), 250);
  assert!(pages > 1, "COUNT 100 over 250 keys must paginate");
  assert!(collected.iter().all(|entry| entry.kind == KeyKind::String));
  assert!(collected.iter().all(|entry| entry.ttl_ms.is_none()));

  // A non-matching pattern converges to an empty result.
  let mut cursor: Option<String> = None;
  loop {
    let page = kv
      .scan_keys("soquel_test:nope:*", cursor.as_deref(), 100)
      .await
      .unwrap();
    assert!(page.keys.is_empty());
    cursor = page.cursor;
    if cursor.is_none() {
      break;
    }
  }
  wipe_prefix(kv, prefix).await;
}

#[tokio::test]
async fn integration_redis_key_detail_per_type() {
  let Some(connection) = connection_from_env(0).await else {
    return;
  };
  let kv = connection.kv().unwrap();
  let prefix = "soquel_test:types:";
  wipe_prefix(kv, prefix).await;

  kv.run_command(&format!("SET {prefix}str héllo"))
    .await
    .unwrap();
  for index in 0..600 {
    kv.run_command(&format!("RPUSH {prefix}list item{index}"))
      .await
      .unwrap();
  }
  kv.run_command(&format!("SADD {prefix}set a b c"))
    .await
    .unwrap();
  kv.run_command(&format!("ZADD {prefix}zset 1.5 one 2.5 two"))
    .await
    .unwrap();
  kv.run_command(&format!("HSET {prefix}hash field1 v1 field2 v2"))
    .await
    .unwrap();
  kv.run_command(&format!("XADD {prefix}stream '*' event boot"))
    .await
    .unwrap();

  let string = kv.key_detail(&format!("{prefix}str")).await.unwrap();
  assert_eq!(string.size, "héllo".len() as f64);
  assert!(matches!(string.value, KeyValue::String { ref value } if value == "héllo"));

  // The sample is capped; size reports the real length.
  let list = kv.key_detail(&format!("{prefix}list")).await.unwrap();
  assert_eq!(list.size, 600.0);
  let KeyValue::List { entries } = &list.value else {
    panic!("expected a list value");
  };
  assert_eq!(entries.len(), VALUE_SAMPLE);
  assert_eq!(entries[0], "item0");

  let set = kv.key_detail(&format!("{prefix}set")).await.unwrap();
  assert_eq!(set.size, 3.0);
  let KeyValue::Set { entries } = &set.value else {
    panic!("expected a set value");
  };
  let mut sorted = entries.clone();
  sorted.sort();
  assert_eq!(sorted, ["a", "b", "c"]);

  let zset = kv.key_detail(&format!("{prefix}zset")).await.unwrap();
  let KeyValue::Zset { entries } = &zset.value else {
    panic!("expected a zset value");
  };
  assert_eq!(entries[0].member, "one");
  assert_eq!(entries[0].score, 1.5);
  assert_eq!(entries[1].score, 2.5);

  let hash = kv.key_detail(&format!("{prefix}hash")).await.unwrap();
  assert_eq!(hash.size, 2.0);
  let KeyValue::Hash { entries } = &hash.value else {
    panic!("expected a hash value");
  };
  assert!(entries
    .iter()
    .any(|field| field.field == "field1" && field.value == "v1"));

  let stream = kv.key_detail(&format!("{prefix}stream")).await.unwrap();
  assert_eq!(stream.size, 1.0);
  let KeyValue::Stream { entries } = &stream.value else {
    panic!("expected a stream value");
  };
  assert_eq!(entries[0].fields[0].field, "event");
  assert_eq!(entries[0].fields[0].value, "boot");

  let missing = kv.key_detail("soquel_test:missing").await;
  assert!(matches!(missing, Err(Error::NotFound { .. })));
  wipe_prefix(kv, prefix).await;
}

#[tokio::test]
async fn integration_redis_ttl_and_string_edit_roundtrip() {
  let Some(connection) = connection_from_env(0).await else {
    return;
  };
  let kv = connection.kv().unwrap();
  let key = "soquel_test:ttl:key";
  wipe_prefix(kv, "soquel_test:ttl:").await;

  kv.set_string(key, "v1").await.unwrap();
  assert_eq!(kv.key_detail(key).await.unwrap().ttl_ms, None);

  kv.set_ttl(key, Some(60_000.0)).await.unwrap();
  let ttl = kv.key_detail(key).await.unwrap().ttl_ms.unwrap();
  assert!(ttl > 0.0 && ttl <= 60_000.0, "{ttl}");

  // Editing the value must keep the expiry (SET KEEPTTL).
  kv.set_string(key, "v2").await.unwrap();
  let detail = kv.key_detail(key).await.unwrap();
  assert!(detail.ttl_ms.is_some());
  assert!(matches!(detail.value, KeyValue::String { ref value } if value == "v2"));

  kv.set_ttl(key, None).await.unwrap();
  assert_eq!(kv.key_detail(key).await.unwrap().ttl_ms, None);

  assert!(matches!(
    kv.set_ttl("soquel_test:ttl:missing", Some(1000.0)).await,
    Err(Error::NotFound { .. })
  ));

  kv.delete_key(key).await.unwrap();
  assert!(matches!(
    kv.delete_key(key).await,
    Err(Error::NotFound { .. })
  ));
}

#[tokio::test]
async fn integration_redis_databases_reports_keyspace() {
  let Some(zero) = connection_from_env(0).await else {
    return;
  };
  let Some(one) = connection_from_env(1).await else {
    return;
  };
  let kv1 = one.kv().unwrap();
  kv1.set_string("soquel_test:dbs:marker", "x").await.unwrap();

  let databases = zero.kv().unwrap().databases().await.unwrap();
  assert_eq!(databases.current, 0);
  assert!(databases.total >= 16, "{}", databases.total);
  let db1 = databases.used.iter().find(|entry| entry.db == 1).unwrap();
  assert!(db1.keys >= 1.0);

  assert_eq!(kv1.databases().await.unwrap().current, 1);
  kv1.delete_key("soquel_test:dbs:marker").await.unwrap();
}

#[tokio::test]
async fn integration_redis_db_indexes_are_isolated() {
  let Some(zero) = connection_from_env(0).await else {
    return;
  };
  let Some(one) = connection_from_env(1).await else {
    return;
  };
  let key = "soquel_test:dbiso:key";
  let kv0 = zero.kv().unwrap();
  let kv1 = one.kv().unwrap();
  wipe_prefix(kv0, "soquel_test:dbiso:").await;
  wipe_prefix(kv1, "soquel_test:dbiso:").await;

  kv0.set_string(key, "only-db-0").await.unwrap();
  assert!(matches!(
    kv1.key_detail(key).await,
    Err(Error::NotFound { .. })
  ));
  kv0.delete_key(key).await.unwrap();
}

#[tokio::test]
async fn integration_redis_console_renders_and_blocks() {
  let Some(connection) = connection_from_env(0).await else {
    return;
  };
  let kv = connection.kv().unwrap();
  let prefix = "soquel_test:console:";
  wipe_prefix(kv, prefix).await;

  assert_eq!(kv.run_command("PING").await.unwrap(), ["PONG"]);
  assert_eq!(
    kv.run_command(&format!("SET {prefix}k \"a b\""))
      .await
      .unwrap(),
    ["OK"]
  );
  assert_eq!(
    kv.run_command(&format!("GET {prefix}k")).await.unwrap(),
    ["\"a b\""]
  );
  kv.run_command(&format!("RPUSH {prefix}l one two"))
    .await
    .unwrap();
  assert_eq!(
    kv.run_command(&format!("LRANGE {prefix}l 0 -1"))
      .await
      .unwrap(),
    ["1) \"one\"", "2) \"two\""]
  );

  let blocked = kv.run_command("SUBSCRIBE chan").await;
  assert!(matches!(blocked, Err(Error::Unsupported { .. })));
  let blocked = kv.run_command("blpop mykey 0").await;
  assert!(matches!(blocked, Err(Error::Unsupported { .. })));
  // The db selector owns the index: console SELECT would drift on reconnect.
  let blocked = kv.run_command("select 1").await;
  assert!(matches!(blocked, Err(Error::Unsupported { .. })));

  // Server errors surface with the server message.
  let err = kv.run_command("NOTACOMMAND x").await.unwrap_err();
  assert!(
    err.to_string().to_lowercase().contains("unknown command"),
    "{err}"
  );
  wipe_prefix(kv, prefix).await;
}

#[tokio::test]
async fn integration_redis_binary_values_render_as_hex() {
  let Some(connection) = connection_from_env(0).await else {
    return;
  };
  let kv = connection.kv().unwrap();
  let mut raw = raw_connection().await.unwrap();
  let prefix = "soquel_test:bin:";
  let mut bin_key = prefix.as_bytes().to_vec();
  bin_key.extend_from_slice(b"\xfekey");
  // wipe_prefix deletes by (lossy) name: the binary key needs the raw client.
  let _: () = raw.del(bin_key.clone()).await.unwrap();
  wipe_prefix(kv, prefix).await;

  let blob: &[u8] = b"\x00\xffgz";
  let hex = "0x00ff677a";
  let _: () = raw.set(format!("{prefix}str"), blob).await.unwrap();
  let _: () = raw.rpush(format!("{prefix}list"), blob).await.unwrap();
  let _: () = raw.sadd(format!("{prefix}set"), blob).await.unwrap();
  let _: () = raw.zadd(format!("{prefix}zset"), blob, 1.5).await.unwrap();
  let _: () = raw.hset(format!("{prefix}hash"), blob, blob).await.unwrap();
  let _: () = redis::cmd("XADD")
    .arg(format!("{prefix}stream"))
    .arg("*")
    .arg("f")
    .arg(blob)
    .query_async(&mut raw)
    .await
    .unwrap();
  let _: () = raw.set(bin_key.clone(), "v").await.unwrap();

  let string = kv.key_detail(&format!("{prefix}str")).await.unwrap();
  assert_eq!(string.size, 4.0);
  assert!(matches!(string.value, KeyValue::String { ref value } if value == hex));

  let list = kv.key_detail(&format!("{prefix}list")).await.unwrap();
  assert!(matches!(list.value, KeyValue::List { ref entries } if entries == &[hex]));

  let set = kv.key_detail(&format!("{prefix}set")).await.unwrap();
  assert!(matches!(set.value, KeyValue::Set { ref entries } if entries == &[hex]));

  let zset = kv.key_detail(&format!("{prefix}zset")).await.unwrap();
  let KeyValue::Zset { entries } = &zset.value else {
    panic!("expected a zset value");
  };
  assert_eq!(entries[0].member, hex);
  assert_eq!(entries[0].score, 1.5);

  let hash = kv.key_detail(&format!("{prefix}hash")).await.unwrap();
  let KeyValue::Hash { entries } = &hash.value else {
    panic!("expected a hash value");
  };
  assert_eq!(entries[0].field, hex);
  assert_eq!(entries[0].value, hex);

  let stream = kv.key_detail(&format!("{prefix}stream")).await.unwrap();
  let KeyValue::Stream { entries } = &stream.value else {
    panic!("expected a stream value");
  };
  assert_eq!(entries[0].fields[0].field, "f");
  assert_eq!(entries[0].fields[0].value, hex);

  // A binary key name still scans: lossy display, raw bytes for TYPE/PTTL.
  let mut lossy = None;
  let mut cursor: Option<String> = None;
  loop {
    let page = kv
      .scan_keys(&format!("{prefix}*"), cursor.as_deref(), 100)
      .await
      .unwrap();
    lossy = lossy.or_else(|| {
      page
        .keys
        .iter()
        .find(|entry| entry.key.contains('\u{FFFD}'))
        .cloned()
    });
    cursor = page.cursor;
    if cursor.is_none() {
      break;
    }
  }
  let lossy = lossy.expect("binary key must appear in the scan");
  assert_eq!(lossy.kind, KeyKind::String);

  let _: () = raw.del(bin_key).await.unwrap();
  wipe_prefix(kv, prefix).await;
}

#[tokio::test]
async fn integration_redis_acl_user_auth() {
  let Some(params) = params_from_env(0) else {
    return;
  };
  let profile = ConnectionProfile {
    id: String::new(),
    name: "test".to_string(),
    env: Env::Dev,
    group: None,
    agent_access: Default::default(),
    credential: Default::default(),
    params: ConnectorParams::Redis(RedisParams {
      username: Some("app".to_string()),
      ..params
    }),
  };
  let connection = RedisConnector
    .connect(
      &profile,
      Credentials::fixed(Some("acl-pw".to_string())),
      None,
    )
    .await
    .unwrap();
  connection.health().await.unwrap();
  let kv = connection.kv().unwrap();
  kv.set_string("soquel_test:acl:key", "v").await.unwrap();
  kv.delete_key("soquel_test:acl:key").await.unwrap();

  let wrong = RedisConnector
    .connect(&profile, Credentials::fixed(Some("nope".to_string())), None)
    .await;
  assert!(matches!(wrong, Err(Error::Database { .. })));
}
