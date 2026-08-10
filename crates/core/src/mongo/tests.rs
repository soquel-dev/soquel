use std::collections::HashSet;

use mongodb::bson::oid::ObjectId;
use mongodb::bson::spec::BinarySubtype;
use mongodb::bson::{doc, Binary, Bson, DateTime, Document};
use mongodb::options::IndexOptions;
use mongodb::IndexModel;

use super::browse::{decode_cursor, parse_extjson_doc, parse_extjson_value, parse_pipeline};
use super::*;
use crate::connectors::{DocCollectionKind, DocFindRequest};
use crate::profiles::{Env, MongoParams};

// -------- pure logic, always on --------

#[test]
fn canonical_id_round_trips_every_bson_type() {
  let cases = vec![
    Bson::ObjectId(ObjectId::new()),
    Bson::String("user-42".to_string()),
    Bson::Int32(7),
    Bson::Int64(9_007_199_254_740_993),
    Bson::Double(1.5),
    Bson::Decimal128("19.99".parse().unwrap()),
    Bson::DateTime(DateTime::from_millis(1_722_000_000_000)),
    Bson::Binary(Binary {
      subtype: BinarySubtype::Generic,
      bytes: vec![0, 159, 146, 150],
    }),
    Bson::Document(doc! { "tenant": "acme", "seq": 12i64 }),
  ];
  for original in cases {
    let encoded = original.clone().into_canonical_extjson().to_string();
    let decoded = parse_extjson_value(&encoded).unwrap();
    assert_eq!(decoded, original, "{encoded}");
  }
}

#[test]
fn extjson_parse_rejects_garbage() {
  assert!(matches!(
    parse_extjson_value("{"),
    Err(Error::Unsupported { .. })
  ));
  assert!(matches!(
    parse_extjson_doc(Some("[1, 2]"), "filter"),
    Err(Error::Unsupported { .. })
  ));
  assert!(parse_extjson_doc(None, "filter").unwrap().is_empty());
  assert!(parse_extjson_doc(Some("  "), "filter").unwrap().is_empty());
}

#[test]
fn page_cursor_decodes_offsets() {
  assert_eq!(decode_cursor(None).unwrap(), 0);
  assert_eq!(decode_cursor(Some("200")).unwrap(), 200);
  assert!(matches!(
    decode_cursor(Some("nope")),
    Err(Error::Unsupported { .. })
  ));
}

#[test]
fn pipeline_guard_blocks_write_stages() {
  let parse =
    |raw: &str| parse_pipeline(&serde_json::from_str::<Vec<serde_json::Value>>(raw).unwrap());
  assert!(parse(r#"[{"$match": {}}, {"$group": {"_id": "$plan"}}]"#).is_ok());
  assert!(parse(r#"[{"$facet": {"a": [{"$match": {}}]}}]"#).is_ok());
  assert!(matches!(
    parse(r#"[{"$out": "evil"}]"#),
    Err(Error::Unsupported { .. })
  ));
  assert!(matches!(
    parse(r#"[{"$merge": {"into": "evil"}}]"#),
    Err(Error::Unsupported { .. })
  ));
  assert!(matches!(parse("[42]"), Err(Error::Unsupported { .. })));
}

#[tokio::test]
async fn tls_through_tunnel_is_refused() {
  let mut profile = test_profile(MongoParams {
    host: "db.example.com".to_string(),
    port: 27017,
    database: None,
    username: None,
    auth_source: None,
    tls: true,
    tunnel_id: None,
  });
  profile.name = String::new();
  let Err(err) = MongoConnector
    .connect(
      &profile,
      Credentials::fixed(None),
      Some(LocalForward { port: 1 }),
    )
    .await
  else {
    panic!("tls + tunnel must be refused");
  };
  assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
}

// -------- integration, gated on the compose mongo --------

fn test_profile(params: MongoParams) -> ConnectionProfile {
  ConnectionProfile {
    id: String::new(),
    name: "test".to_string(),
    env: Env::Dev,
    group: None,
    agent_access: Default::default(),
    credential: Default::default(),
    params: ConnectorParams::Mongo(params),
  }
}

fn params_from_env() -> Option<MongoParams> {
  let addr = std::env::var("SOQUEL_TEST_MONGO").ok()?;
  let (host, port) = addr
    .split_once(':')
    .expect("SOQUEL_TEST_MONGO is host:port");
  Some(MongoParams {
    host: host.to_string(),
    port: port.parse().unwrap(),
    database: None,
    username: Some("soquel".to_string()),
    auth_source: Some("admin".to_string()),
    tls: false,
    tunnel_id: None,
  })
}

async fn connection_from_env() -> Option<Box<dyn Connection>> {
  let profile = test_profile(params_from_env()?);
  Some(
    MongoConnector
      .connect(
        &profile,
        Credentials::fixed(Some("soquel".to_string())),
        None,
      )
      .await
      .unwrap(),
  )
}

/// Raw client for seeding; each test owns one `soquel_test_*` db, dropped on both ends.
async fn raw_database(name: &str) -> Option<mongodb::Database> {
  let params = params_from_env()?;
  let uri = format!(
    "mongodb://soquel:soquel@{}:{}/?directConnection=true",
    params.host, params.port
  );
  let client = mongodb::Client::with_uri_str(uri).await.unwrap();
  let db = client.database(name);
  db.drop().await.unwrap();
  Some(db)
}

#[tokio::test]
async fn integration_mongo_version_and_health() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  connection.health().await.unwrap();
  let version = connection
    .server_version()
    .expect("root can read buildInfo");
  assert!(
    version.starts_with(|c: char| c.is_ascii_digit()),
    "{version}"
  );
  assert!(connection.doc().is_some());
  assert!(connection.sql().is_none());
  assert!(connection.kv().is_none());
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_wrong_password_fails() {
  let Some(params) = params_from_env() else {
    return;
  };
  let Err(err) = MongoConnector
    .connect(
      &test_profile(params),
      Credentials::fixed(Some("wrong".to_string())),
      None,
    )
    .await
  else {
    panic!("wrong password must fail at connect");
  };
  assert!(matches!(err, Error::Database { .. }), "{err:?}");
}

#[tokio::test]
async fn integration_mongo_databases_and_collections() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_browse").await.unwrap();
  db.collection::<Document>("users")
    .insert_many(vec![doc! { "n": 1 }, doc! { "n": 2 }])
    .await
    .unwrap();
  db.create_collection("logs")
    .capped(true)
    .size(4096)
    .await
    .unwrap();
  db.run_command(doc! {
    "create": "active_users",
    "viewOn": "users",
    "pipeline": [{ "$match": { "n": 1 } }],
  })
  .await
  .unwrap();
  db.run_command(doc! {
    "create": "no_id",
    "viewOn": "users",
    "pipeline": [{ "$project": { "_id": 0, "n": 1 } }],
  })
  .await
  .unwrap();

  let surface = connection.doc().unwrap();
  let databases = surface.databases().await.unwrap();
  let entry = databases
    .iter()
    .find(|entry| entry.name == "soquel_test_browse")
    .expect("seeded db listed");
  assert!(entry.size_bytes.is_some());
  assert!(!entry.empty);

  let collections = surface.collections("soquel_test_browse").await.unwrap();
  let by_name = |name: &str| {
    collections
      .iter()
      .find(|collection| collection.name == name)
      .unwrap_or_else(|| panic!("missing collection {name}: {collections:?}"))
  };
  let users = by_name("users");
  assert_eq!(users.kind, DocCollectionKind::Collection);
  assert!(!users.capped);
  assert_eq!(users.estimated_docs, Some(2.0));
  assert!(by_name("logs").capped);
  let view = by_name("active_users");
  assert_eq!(view.kind, DocCollectionKind::View);
  assert_eq!(view.estimated_docs, None);

  // A view can project _id away: entries lose their address (UI disables edit/delete).
  let page = surface
    .find_docs(&DocFindRequest {
      db: "soquel_test_browse".to_string(),
      collection: "no_id".to_string(),
      filter: None,
      sort: None,
      limit: 10,
      cursor: None,
    })
    .await
    .unwrap();
  assert!(!page.docs.is_empty());
  assert!(
    page.docs.iter().all(|entry| entry.id.is_none()),
    "{:?}",
    page.docs
  );

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

fn find_request(
  filter: Option<&str>,
  sort: Option<&str>,
  cursor: Option<String>,
) -> DocFindRequest {
  DocFindRequest {
    db: "soquel_test_find".to_string(),
    collection: "items".to_string(),
    filter: filter.map(str::to_string),
    sort: sort.map(str::to_string),
    limit: 100,
    cursor,
  }
}

#[tokio::test]
async fn integration_mongo_find_filter_sort_pagination() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_find").await.unwrap();
  let seeds: Vec<Document> = (0..250)
    .map(|n| doc! { "_id": n, "n": n, "bucket": if n % 2 == 0 { "even" } else { "odd" } })
    .collect();
  db.collection::<Document>("items")
    .insert_many(seeds)
    .await
    .unwrap();
  let surface = connection.doc().unwrap();

  let mut seen = HashSet::new();
  let mut cursor = None;
  let mut pages = 0;
  loop {
    let page = surface
      .find_docs(&find_request(None, Some(r#"{"n": 1}"#), cursor.clone()))
      .await
      .unwrap();
    pages += 1;
    for entry in &page.docs {
      assert!(
        seen.insert(entry.id.clone().unwrap()),
        "duplicate doc across pages"
      );
    }
    match page.cursor {
      Some(next) => cursor = Some(next),
      None => break,
    }
  }
  assert_eq!(pages, 3);
  assert_eq!(seen.len(), 250);

  let filtered = surface
    .find_docs(&find_request(Some(r#"{"bucket": "even"}"#), None, None))
    .await
    .unwrap();
  assert_eq!(filtered.docs.len(), 100);
  assert!(filtered.cursor.is_some());

  let sorted = surface
    .find_docs(&find_request(None, Some(r#"{"n": -1}"#), None))
    .await
    .unwrap();
  let first: serde_json::Value = serde_json::from_str(&sorted.docs[0].doc).unwrap();
  assert_eq!(first["n"], 249);

  for broken in [
    surface
      .find_docs(&find_request(Some("nope"), None, None))
      .await,
    surface
      .find_docs(&find_request(None, Some("[1]"), None))
      .await,
    surface
      .find_docs(&find_request(None, None, Some("x".to_string())))
      .await,
  ] {
    assert!(matches!(broken, Err(Error::Unsupported { .. })));
  }

  // An oversized limit clamps to DOC_PAGE_MAX instead of dumping the collection.
  let clamped = surface
    .find_docs(&DocFindRequest {
      limit: 5000,
      ..find_request(None, None, None)
    })
    .await
    .unwrap();
  assert_eq!(clamped.docs.len(), DOC_PAGE_MAX as usize);
  assert!(clamped.cursor.is_some());

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_replace_rejects_id_mutation() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_idmut").await.unwrap();
  db.collection::<Document>("docs")
    .insert_one(doc! { "_id": "a", "state": "before" })
    .await
    .unwrap();
  let surface = connection.doc().unwrap();
  let id = Bson::String("a".to_string())
    .into_canonical_extjson()
    .to_string();

  let Err(err) = surface
    .replace_doc(
      "soquel_test_idmut",
      "docs",
      &id,
      r#"{"_id": "b", "state": "after"}"#,
    )
    .await
  else {
    panic!("mutating _id through a replace must be rejected");
  };
  assert!(matches!(err, Error::Database { .. }), "{err:?}");

  let detail = surface
    .doc_detail("soquel_test_idmut", "docs", &id)
    .await
    .unwrap();
  assert!(detail.relaxed.contains("before"), "{}", detail.relaxed);

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

/// Scoped read-only user: the credential source falls back to the profile db,
/// and database listing degrades to the authorized ones instead of failing.
#[tokio::test]
async fn integration_mongo_scoped_user() {
  let Some(params) = params_from_env() else {
    return;
  };
  let db = raw_database("soquel_test_scoped").await.unwrap();
  db.collection::<Document>("docs")
    .insert_many(vec![doc! { "n": 1 }, doc! { "n": 2 }])
    .await
    .unwrap();
  let _ = db.run_command(doc! { "dropUser": "scoped" }).await;
  db.run_command(doc! {
    "createUser": "scoped",
    "pwd": "scoped-pw",
    "roles": [{ "role": "read", "db": "soquel_test_scoped" }],
  })
  .await
  .unwrap();

  let profile = test_profile(MongoParams {
    database: Some("soquel_test_scoped".to_string()),
    username: Some("scoped".to_string()),
    auth_source: None,
    ..params
  });
  let connection = MongoConnector
    .connect(
      &profile,
      Credentials::fixed(Some("scoped-pw".to_string())),
      None,
    )
    .await
    .unwrap();
  let surface = connection.doc().unwrap();

  let databases = surface.databases().await.unwrap();
  assert!(
    databases
      .iter()
      .any(|entry| entry.name == "soquel_test_scoped"),
    "{databases:?}"
  );
  assert!(
    !databases.iter().any(|entry| entry.name == "admin"),
    "{databases:?}"
  );

  let collections = surface.collections("soquel_test_scoped").await.unwrap();
  assert_eq!(collections.len(), 1);
  assert_eq!(collections[0].estimated_docs, Some(2.0));

  let page = surface
    .find_docs(&DocFindRequest {
      db: "soquel_test_scoped".to_string(),
      collection: "docs".to_string(),
      filter: None,
      sort: None,
      limit: 10,
      cursor: None,
    })
    .await
    .unwrap();
  assert_eq!(page.docs.len(), 2);

  // Read-only role: writes bounce with the server's message.
  let id = page.docs[0].id.clone().unwrap();
  let Err(err) = surface.delete_doc("soquel_test_scoped", "docs", &id).await else {
    panic!("a read-only user must not delete");
  };
  assert!(matches!(err, Error::Database { .. }), "{err:?}");

  connection.close().await.unwrap();
  let _ = db.run_command(doc! { "dropUser": "scoped" }).await;
  db.drop().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_extjson_round_trip() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_extjson").await.unwrap();
  let original = doc! {
    "_id": ObjectId::new(),
    "when": DateTime::from_millis(1_722_000_000_000),
    "price": Bson::Decimal128("19.99".parse().unwrap()),
    "blob": Bson::Binary(Binary { subtype: BinarySubtype::Generic, bytes: vec![1, 2, 3] }),
    "big": i64::MAX,
    "nested": { "tags": ["a", "b"], "deep": { "n": 1 } },
  };
  db.collection::<Document>("things")
    .insert_one(original.clone())
    .await
    .unwrap();
  let surface = connection.doc().unwrap();

  let page = surface
    .find_docs(&DocFindRequest {
      db: "soquel_test_extjson".to_string(),
      collection: "things".to_string(),
      filter: None,
      sort: None,
      limit: 10,
      cursor: None,
    })
    .await
    .unwrap();
  let entry = &page.docs[0];
  let relaxed: serde_json::Value = serde_json::from_str(&entry.doc).unwrap();
  assert!(relaxed.is_object());

  let detail = surface
    .doc_detail("soquel_test_extjson", "things", entry.id.as_ref().unwrap())
    .await
    .unwrap();
  let canonical: serde_json::Value = serde_json::from_str(&detail.canonical).unwrap();
  let reparsed = match Bson::try_from(canonical).unwrap() {
    Bson::Document(reparsed) => reparsed,
    other => panic!("expected a document, got {other:?}"),
  };
  assert_eq!(reparsed, original);

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_replace_and_delete_by_id() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_edit").await.unwrap();
  let collection = db.collection::<Document>("docs");
  let ids = vec![
    Bson::ObjectId(ObjectId::new()),
    Bson::String("user-42".to_string()),
    Bson::Int64(9_007_199_254_740_993),
    Bson::Document(doc! { "tenant": "acme", "seq": 1 }),
  ];
  for id in &ids {
    collection
      .insert_one(doc! { "_id": id.clone(), "state": "before" })
      .await
      .unwrap();
  }
  let surface = connection.doc().unwrap();
  for id in &ids {
    let encoded = id.clone().into_canonical_extjson().to_string();
    let detail = surface
      .doc_detail("soquel_test_edit", "docs", &encoded)
      .await
      .unwrap();
    assert!(detail.relaxed.contains("before"), "{}", detail.relaxed);

    let replacement = format!(r#"{{"_id": {encoded}, "state": "after"}}"#);
    surface
      .replace_doc("soquel_test_edit", "docs", &encoded, &replacement)
      .await
      .unwrap();
    let detail = surface
      .doc_detail("soquel_test_edit", "docs", &encoded)
      .await
      .unwrap();
    assert!(detail.relaxed.contains("after"), "{}", detail.relaxed);

    surface
      .delete_doc("soquel_test_edit", "docs", &encoded)
      .await
      .unwrap();
    for gone in [
      surface
        .delete_doc("soquel_test_edit", "docs", &encoded)
        .await,
      surface
        .replace_doc("soquel_test_edit", "docs", &encoded, "{}")
        .await,
      surface
        .doc_detail("soquel_test_edit", "docs", &encoded)
        .await
        .map(|_| ()),
    ] {
      assert!(matches!(gone, Err(Error::NotFound { .. })));
    }
  }
  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_indexes() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_indexes").await.unwrap();
  let collection = db.collection::<Document>("users");
  collection
    .insert_one(doc! { "email": "a@b.c", "plan": "pro", "created": 1 })
    .await
    .unwrap();
  collection
    .create_index(
      IndexModel::builder()
        .keys(doc! { "email": 1 })
        .options(IndexOptions::builder().unique(true).build())
        .build(),
    )
    .await
    .unwrap();
  collection
    .create_index(
      IndexModel::builder()
        .keys(doc! { "plan": 1, "created": -1 })
        .build(),
    )
    .await
    .unwrap();

  let indexes = connection
    .doc()
    .unwrap()
    .indexes("soquel_test_indexes", "users")
    .await
    .unwrap();
  let by_name = |name: &str| {
    indexes
      .iter()
      .find(|index| index.name == name)
      .unwrap_or_else(|| panic!("missing index {name}: {indexes:?}"))
  };
  by_name("_id_");
  let email = by_name("email_1");
  assert!(email.unique);
  assert!(email.definition.contains("email"));
  let compound = by_name("plan_1_created_-1");
  assert!(!compound.unique);
  assert!(compound.definition.contains("plan") && compound.definition.contains("created"));

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_count() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_count").await.unwrap();
  let seeds: Vec<Document> = (0..50)
    .map(|n| doc! { "n": n, "bucket": if n % 2 == 0 { "even" } else { "odd" } })
    .collect();
  db.collection::<Document>("items")
    .insert_many(seeds)
    .await
    .unwrap();
  let surface = connection.doc().unwrap();

  let estimate = surface
    .count_docs("soquel_test_count", "items", None)
    .await
    .unwrap();
  assert!(estimate.count >= 50.0, "{estimate:?}");
  assert!(!estimate.exact);

  let exact = surface
    .count_docs("soquel_test_count", "items", Some(r#"{"bucket": "even"}"#))
    .await
    .unwrap();
  assert_eq!(exact.count, 25.0);
  assert!(exact.exact);

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}

#[tokio::test]
async fn integration_mongo_run_query_console() {
  let Some(connection) = connection_from_env().await else {
    return;
  };
  let db = raw_database("soquel_test_console").await.unwrap();
  let seeds: Vec<Document> = (0..250)
    .map(|n| doc! { "n": n, "bucket": if n % 2 == 0 { "even" } else { "odd" } })
    .collect();
  db.collection::<Document>("events")
    .insert_many(seeds)
    .await
    .unwrap();
  let surface = connection.doc().unwrap();

  let all = surface
    .run_query("soquel_test_console", "events", "{}")
    .await
    .unwrap();
  assert_eq!(all.docs.len(), QUERY_SAMPLE);
  assert!(all.truncated);

  let filtered = surface
    .run_query("soquel_test_console", "events", r#"{"n": {"$lt": 5}}"#)
    .await
    .unwrap();
  assert_eq!(filtered.docs.len(), 5);
  assert!(!filtered.truncated);

  let grouped = surface
    .run_query(
      "soquel_test_console",
      "events",
      r#"[{"$group": {"_id": "$bucket", "total": {"$sum": 1}}}]"#,
    )
    .await
    .unwrap();
  assert_eq!(grouped.docs.len(), 2);

  for broken in [
    surface
      .run_query("soquel_test_console", "events", r#"[{"$out": "evil"}]"#)
      .await,
    surface
      .run_query("soquel_test_console", "events", "42")
      .await,
    surface
      .run_query("soquel_test_console", "events", "{")
      .await,
  ] {
    assert!(matches!(broken, Err(Error::Unsupported { .. })));
  }

  db.drop().await.unwrap();
  connection.close().await.unwrap();
}
