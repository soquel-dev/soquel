//! Redis connector: multiplexed async connection, key-value browse surface.

use std::sync::Arc;

use redis::aio::MultiplexedConnection;
use redis::{AsyncCommands, ConnectionAddr, ConnectionInfo, RedisConnectionInfo, Value};

use crate::connectors::{
  Capability, Connection, Connector, HashField, KeyDetail, KeyEntry, KeyKind, KeyScanPage,
  KeyValue, KvBrowse, KvDatabaseKeys, KvDatabases, LocalForward, StreamEntry, ZsetMember,
};
use crate::credentials::Credentials;
use crate::error::Error;
use crate::profiles::{ConnectionProfile, ConnectorParams};

/// Bounded sample per collection value; `size` carries the real total.
const VALUE_SAMPLE: usize = 500;

pub struct RedisConnector;

#[async_trait::async_trait]
impl Connector for RedisConnector {
  fn capabilities(&self) -> &'static [Capability] {
    &[Capability::KvBrowse]
  }

  async fn connect(
    &self,
    profile: &ConnectionProfile,
    secret: Arc<Credentials>,
    forward: Option<LocalForward>,
  ) -> Result<Box<dyn Connection>, Error> {
    let ConnectorParams::Redis(params) = &profile.params else {
      return Err(Error::Unsupported {
        message: "this connector needs a redis profile".to_string(),
      });
    };
    let (host, port) = match forward {
      Some(forward) => ("127.0.0.1".to_string(), forward.port),
      None => (params.host.clone(), params.port),
    };
    let addr = if params.tls {
      ConnectionAddr::TcpTls {
        // TLS still verifies the logical hostname through a tunnel.
        host: params.host.clone(),
        port,
        insecure: false,
        tls_params: None,
      }
    } else {
      ConnectionAddr::Tcp(host, port)
    };
    let info = ConnectionInfo {
      addr,
      redis: RedisConnectionInfo {
        db: i64::from(params.db),
        username: params.username.clone(),
        // One multiplexed socket, authenticated once: nothing to refresh later.
        password: secret.resolve().await?,
        ..Default::default()
      },
    };
    let client = redis::Client::open(info)?;
    let mut conn = client.get_multiplexed_tokio_connection().await?;
    let info_raw: String = redis::cmd("INFO")
      .arg("server")
      .query_async(&mut conn)
      .await?;
    Ok(Box::new(RedisConnection {
      conn,
      server_version: parse_info_version(&info_raw),
      db: params.db,
    }))
  }
}

pub struct RedisConnection {
  /// Cheap to clone; commands multiplex over one socket.
  conn: MultiplexedConnection,
  server_version: Option<String>,
  db: u32,
}

/// "redis_version:7.4.1" -> "7.4.1"; Valkey ships both lines, its own wins.
fn parse_info_version(info: &str) -> Option<String> {
  let field = |name: &str| {
    info
      .lines()
      .find_map(|line| line.strip_prefix(name))
      .map(|value| value.trim().to_string())
  };
  match field("valkey_version:") {
    Some(version) => Some(format!("{version}-valkey")),
    None => field("redis_version:"),
  }
}

#[async_trait::async_trait]
impl Connection for RedisConnection {
  async fn health(&self) -> Result<(), Error> {
    let mut conn = self.conn.clone();
    redis::cmd("PING").query_async::<()>(&mut conn).await?;
    Ok(())
  }

  async fn close(&self) -> Result<(), Error> {
    // The multiplexed socket closes when the last clone drops.
    Ok(())
  }

  fn server_version(&self) -> Option<String> {
    self.server_version.clone()
  }

  fn kv(&self) -> Option<&dyn KvBrowse> {
    Some(self)
  }
}

fn key_kind(type_name: &str) -> KeyKind {
  match type_name {
    "string" => KeyKind::String,
    "list" => KeyKind::List,
    "set" => KeyKind::Set,
    "zset" => KeyKind::Zset,
    "hash" => KeyKind::Hash,
    "stream" => KeyKind::Stream,
    _ => KeyKind::Other,
  }
}

/// PTTL: -1 = no expiry, -2 = gone; both map to None.
fn ttl_ms(pttl: i64) -> Option<f64> {
  (pttl >= 0).then_some(pttl as f64)
}

/// UTF-8 text as-is; binary payloads as 0x.. hex, like sql blob cells.
fn display_bytes(bytes: Vec<u8>) -> String {
  match String::from_utf8(bytes) {
    Ok(text) => text,
    Err(err) => {
      let hex: String = err
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
      format!("0x{hex}")
    }
  }
}

/// "db0:keys=13036,expires=…" lines from INFO keyspace.
fn parse_keyspace(info: &str) -> Vec<KvDatabaseKeys> {
  info
    .lines()
    .filter_map(|line| {
      let (name, rest) = line.split_once(':')?;
      let db = name.strip_prefix("db")?.parse().ok()?;
      let keys = rest
        .split(',')
        .find_map(|field| field.strip_prefix("keys="))?
        .parse()
        .ok()?;
      Some(KvDatabaseKeys { db, keys })
    })
    .collect()
}

#[async_trait::async_trait]
impl KvBrowse for RedisConnection {
  async fn databases(&self) -> Result<KvDatabases, Error> {
    let mut conn = self.conn.clone();
    let info: String = redis::cmd("INFO")
      .arg("keyspace")
      .query_async(&mut conn)
      .await?;
    // CONFIG may be renamed or ACL-blocked on managed instances: assume 16.
    let total = redis::cmd("CONFIG")
      .arg("GET")
      .arg("databases")
      .query_async::<Vec<String>>(&mut conn)
      .await
      .ok()
      .and_then(|pair| pair.get(1)?.parse().ok())
      .unwrap_or(16);
    Ok(KvDatabases {
      current: self.db,
      total,
      used: parse_keyspace(&info),
    })
  }

  async fn scan_keys(
    &self,
    pattern: &str,
    cursor: Option<&str>,
    count: u32,
  ) -> Result<KeyScanPage, Error> {
    let mut conn = self.conn.clone();
    let cursor: u64 = cursor
      .unwrap_or("0")
      .parse()
      .map_err(|_| Error::Unsupported {
        message: "invalid scan cursor".to_string(),
      })?;
    let pattern = if pattern.is_empty() { "*" } else { pattern };
    // Key names can be arbitrary bytes; display lossy, address by raw bytes.
    let (next, keys): (u64, Vec<Vec<u8>>) = redis::cmd("SCAN")
      .arg(cursor)
      .arg("MATCH")
      .arg(pattern)
      .arg("COUNT")
      .arg(count)
      .query_async(&mut conn)
      .await?;

    let mut pipe = redis::pipe();
    for key in &keys {
      pipe
        .cmd("TYPE")
        .arg(key.as_slice())
        .cmd("PTTL")
        .arg(key.as_slice());
    }
    let meta: Vec<(String, i64)> = pipe.query_async(&mut conn).await?;
    let entries = keys
      .into_iter()
      .zip(meta)
      .map(|(key, (type_name, pttl))| KeyEntry {
        key: String::from_utf8_lossy(&key).into_owned(),
        kind: key_kind(&type_name),
        ttl_ms: ttl_ms(pttl),
      })
      .collect();
    Ok(KeyScanPage {
      keys: entries,
      cursor: (next != 0).then(|| next.to_string()),
    })
  }

  async fn key_detail(&self, key: &str) -> Result<KeyDetail, Error> {
    let mut conn = self.conn.clone();
    let (type_name, pttl): (String, i64) = redis::pipe()
      .cmd("TYPE")
      .arg(key)
      .cmd("PTTL")
      .arg(key)
      .query_async(&mut conn)
      .await?;
    if type_name == "none" {
      return Err(Error::NotFound {
        message: format!("key {key} does not exist"),
      });
    }

    let (size, value) = match key_kind(&type_name) {
      KeyKind::String => {
        let bytes: Vec<u8> = conn.get(key).await?;
        (
          bytes.len() as f64,
          KeyValue::String {
            value: display_bytes(bytes),
          },
        )
      }
      KeyKind::List => {
        let size: i64 = conn.llen(key).await?;
        let raw: Vec<Vec<u8>> = conn.lrange(key, 0, VALUE_SAMPLE as isize - 1).await?;
        let entries = raw.into_iter().map(display_bytes).collect();
        (size as f64, KeyValue::List { entries })
      }
      KeyKind::Set => {
        let size: i64 = conn.scard(key).await?;
        let raw = scan_collection(&mut conn, "SSCAN", key).await?;
        let entries = raw.into_iter().map(display_bytes).collect();
        (size as f64, KeyValue::Set { entries })
      }
      KeyKind::Zset => {
        let size: i64 = conn.zcard(key).await?;
        let flat: Vec<(Vec<u8>, f64)> = redis::cmd("ZRANGE")
          .arg(key)
          .arg(0)
          .arg(VALUE_SAMPLE as isize - 1)
          .arg("WITHSCORES")
          .query_async(&mut conn)
          .await?;
        let entries = flat
          .into_iter()
          .map(|(member, score)| ZsetMember {
            member: display_bytes(member),
            score,
          })
          .collect();
        (size as f64, KeyValue::Zset { entries })
      }
      KeyKind::Hash => {
        let size: i64 = conn.hlen(key).await?;
        let flat = scan_collection(&mut conn, "HSCAN", key).await?;
        let entries = flat
          .chunks_exact(2)
          .map(|pair| HashField {
            field: display_bytes(pair[0].clone()),
            value: display_bytes(pair[1].clone()),
          })
          .collect();
        (size as f64, KeyValue::Hash { entries })
      }
      KeyKind::Stream => {
        let size: i64 = redis::cmd("XLEN").arg(key).query_async(&mut conn).await?;
        let raw: Vec<(String, Vec<Vec<u8>>)> = redis::cmd("XRANGE")
          .arg(key)
          .arg("-")
          .arg("+")
          .arg("COUNT")
          .arg(VALUE_SAMPLE)
          .query_async(&mut conn)
          .await?;
        let entries = raw
          .into_iter()
          .map(|(id, flat)| StreamEntry {
            id,
            fields: flat
              .chunks_exact(2)
              .map(|pair| HashField {
                field: display_bytes(pair[0].clone()),
                value: display_bytes(pair[1].clone()),
              })
              .collect(),
          })
          .collect();
        (size as f64, KeyValue::Stream { entries })
      }
      _ => (
        0.0,
        KeyValue::Other {
          type_name: type_name.clone(),
        },
      ),
    };

    Ok(KeyDetail {
      key: key.to_string(),
      ttl_ms: ttl_ms(pttl),
      size,
      value,
    })
  }

  async fn set_string(&self, key: &str, value: &str) -> Result<(), Error> {
    let mut conn = self.conn.clone();
    // KEEPTTL: editing the value must not silently clear an expiry.
    redis::cmd("SET")
      .arg(key)
      .arg(value)
      .arg("KEEPTTL")
      .query_async::<()>(&mut conn)
      .await?;
    Ok(())
  }

  async fn delete_key(&self, key: &str) -> Result<(), Error> {
    let mut conn = self.conn.clone();
    let removed: i64 = conn.del(key).await?;
    if removed == 0 {
      return Err(Error::NotFound {
        message: format!("key {key} does not exist"),
      });
    }
    Ok(())
  }

  async fn set_ttl(&self, key: &str, ttl_ms: Option<f64>) -> Result<(), Error> {
    let mut conn = self.conn.clone();
    let applied: i64 = match ttl_ms {
      Some(ms) if ms >= 1.0 => {
        redis::cmd("PEXPIRE")
          .arg(key)
          .arg(ms as i64)
          .query_async(&mut conn)
          .await?
      }
      Some(_) => {
        return Err(Error::Unsupported {
          message: "ttl must be at least 1ms".to_string(),
        })
      }
      None => {
        redis::cmd("PERSIST")
          .arg(key)
          .query_async(&mut conn)
          .await?
      }
    };
    // PERSIST returns 0 when there was no expiry: not an error.
    if applied == 0 && ttl_ms.is_some() {
      return Err(Error::NotFound {
        message: format!("key {key} does not exist"),
      });
    }
    Ok(())
  }

  async fn run_command(&self, command: &str) -> Result<Vec<String>, Error> {
    let args = split_command_line(command)?;
    let (first, rest) = args.split_first().ok_or_else(|| Error::Unsupported {
      message: "empty command".to_string(),
    })?;
    let upper = first.to_uppercase();
    if BLOCKED_COMMANDS.contains(&upper.as_str()) {
      return Err(Error::Unsupported {
        message: format!("{upper} would block or hijack the shared connection"),
      });
    }
    let mut cmd = redis::cmd(&upper);
    for arg in rest {
      cmd.arg(arg);
    }
    let mut conn = self.conn.clone();
    let value: Value = cmd.query_async(&mut conn).await?;
    let mut lines = Vec::new();
    render_value(&value, 0, None, &mut lines);
    Ok(lines)
  }
}

/// SSCAN/HSCAN pages until the sample cap; COUNT is a hint, not a limit.
async fn scan_collection(
  conn: &mut MultiplexedConnection,
  command: &str,
  key: &str,
) -> Result<Vec<Vec<u8>>, Error> {
  let mut cursor = 0u64;
  let mut out: Vec<Vec<u8>> = Vec::new();
  loop {
    let (next, mut chunk): (u64, Vec<Vec<u8>>) = redis::cmd(command)
      .arg(key)
      .arg(cursor)
      .arg("COUNT")
      .arg(200)
      .query_async(conn)
      .await?;
    out.append(&mut chunk);
    cursor = next;
    if cursor == 0 || out.len() >= VALUE_SAMPLE {
      break;
    }
  }
  out.truncate(VALUE_SAMPLE);
  Ok(out)
}

/// Would monopolize or re-mode the multiplexed connection.
/// SELECT: the workspace db selector owns the index; a console SELECT would
/// silently revert on reconnect.
const BLOCKED_COMMANDS: &[&str] = &[
  "SUBSCRIBE",
  "PSUBSCRIBE",
  "SSUBSCRIBE",
  "MONITOR",
  "BLPOP",
  "BRPOP",
  "BLMOVE",
  "BLMPOP",
  "BRPOPLPUSH",
  "BZPOPMIN",
  "BZPOPMAX",
  "BZMPOP",
  "XREAD",
  "WAIT",
  "SELECT",
  "RESET",
];

/// redis-cli style tokenizer: whitespace-separated, quotes group words.
fn split_command_line(command: &str) -> Result<Vec<String>, Error> {
  let mut args = Vec::new();
  let mut current = String::new();
  let mut quote: Option<char> = None;
  let mut chars = command.chars();
  let mut in_token = false;
  while let Some(ch) = chars.next() {
    match quote {
      Some(q) => {
        if ch == q {
          quote = None;
        } else if ch == '\\' && q == '"' {
          match chars.next() {
            Some('n') => current.push('\n'),
            Some('t') => current.push('\t'),
            Some(other) => current.push(other),
            None => break,
          }
        } else {
          current.push(ch);
        }
      }
      None if ch == '\'' || ch == '"' => {
        quote = Some(ch);
        in_token = true;
      }
      None if ch.is_whitespace() => {
        if in_token {
          args.push(std::mem::take(&mut current));
          in_token = false;
        }
      }
      None => {
        current.push(ch);
        in_token = true;
      }
    }
  }
  if quote.is_some() {
    return Err(Error::Unsupported {
      message: "unbalanced quote in command".to_string(),
    });
  }
  if in_token {
    args.push(current);
  }
  Ok(args)
}

/// redis-cli style rendering, one line per scalar, indented nesting.
fn render_value(value: &Value, depth: usize, index: Option<usize>, lines: &mut Vec<String>) {
  let prefix = format!(
    "{}{}",
    "  ".repeat(depth),
    index.map(|i| format!("{}) ", i + 1)).unwrap_or_default()
  );
  match value {
    Value::Nil => lines.push(format!("{prefix}(nil)")),
    Value::Okay => lines.push(format!("{prefix}OK")),
    Value::Int(int) => lines.push(format!("{prefix}(integer) {int}")),
    Value::Double(double) => lines.push(format!("{prefix}(double) {double}")),
    Value::Boolean(boolean) => lines.push(format!("{prefix}(boolean) {boolean}")),
    Value::BigNumber(number) => lines.push(format!("{prefix}(big number) {number}")),
    Value::SimpleString(text) => lines.push(format!("{prefix}{text}")),
    Value::BulkString(bytes) => lines.push(format!(
      "{prefix}\"{}\"",
      String::from_utf8_lossy(bytes).replace('"', "\\\"")
    )),
    Value::VerbatimString { text, .. } => lines.push(format!("{prefix}{text}")),
    Value::Array(items) | Value::Set(items) => {
      if items.is_empty() {
        lines.push(format!("{prefix}(empty array)"));
      } else {
        for (position, item) in items.iter().enumerate() {
          render_value(
            item,
            depth + usize::from(index.is_some()),
            Some(position),
            lines,
          );
        }
      }
    }
    Value::Map(pairs) => {
      for (position, (field, entry)) in pairs.iter().enumerate() {
        render_value(
          field,
          depth + usize::from(index.is_some()),
          Some(position),
          lines,
        );
        render_value(entry, depth + 1, None, lines);
      }
    }
    other => lines.push(format!("{prefix}{other:?}")),
  }
}

#[cfg(test)]
mod tests;
