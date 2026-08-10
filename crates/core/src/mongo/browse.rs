use std::time::Instant;

use futures_util::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::results::CollectionType;
use mongodb::Collection;

use super::{MongoConnection, COUNT_CAP, DOC_PAGE_MAX, QUERY_SAMPLE};
use crate::connectors::{
  DocBrowse, DocCollection, DocCollectionKind, DocCount, DocDatabase, DocDetail, DocEntry,
  DocFindRequest, DocPage, DocQueryResult, IndexInfo,
};
use crate::error::Error;

fn invalid(what: &str, err: impl std::fmt::Display) -> Error {
  Error::Unsupported {
    message: format!("invalid {what}: {err}"),
  }
}

fn not_a_document(what: &str) -> Error {
  Error::Unsupported {
    message: format!("{what} must be a JSON object"),
  }
}

fn doc_not_found() -> Error {
  Error::NotFound {
    message: "document not found; it may have been changed or deleted - refresh and retry"
      .to_string(),
  }
}

pub(super) fn parse_extjson_value(raw: &str) -> Result<Bson, Error> {
  let value: serde_json::Value =
    serde_json::from_str(raw).map_err(|err| invalid("extended JSON", err))?;
  Bson::try_from(value).map_err(|err| invalid("extended JSON", err))
}

/// None/empty parses to {}; anything non-object is refused.
pub(super) fn parse_extjson_doc(raw: Option<&str>, what: &str) -> Result<Document, Error> {
  let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
    return Ok(Document::new());
  };
  match parse_extjson_value(raw)? {
    Bson::Document(doc) => Ok(doc),
    _ => Err(not_a_document(what)),
  }
}

pub(super) fn decode_cursor(cursor: Option<&str>) -> Result<u64, Error> {
  match cursor {
    None => Ok(0),
    Some(raw) => raw.parse().map_err(|_| Error::Unsupported {
      message: "invalid page cursor".to_string(),
    }),
  }
}

/// $out/$merge are only legal as top-level stage keys, so this scan is a complete block.
pub(super) fn parse_pipeline(stages: &[serde_json::Value]) -> Result<Vec<Document>, Error> {
  stages
    .iter()
    .map(|stage| {
      let object = stage
        .as_object()
        .ok_or_else(|| not_a_document("every pipeline stage"))?;
      if let Some(key) = object.keys().find(|key| *key == "$out" || *key == "$merge") {
        return Err(Error::Unsupported {
          message: format!("{key} writes to a collection; run it from a real shell"),
        });
      }
      match Bson::try_from(stage.clone()).map_err(|err| invalid("pipeline", err))? {
        Bson::Document(doc) => Ok(doc),
        _ => Err(not_a_document("every pipeline stage")),
      }
    })
    .collect()
}

fn relaxed_string(doc: Document) -> String {
  Bson::Document(doc).into_relaxed_extjson().to_string()
}

fn canonical_string(value: Bson) -> String {
  value.into_canonical_extjson().to_string()
}

fn doc_entry(doc: Document) -> DocEntry {
  DocEntry {
    id: doc.get("_id").cloned().map(canonical_string),
    doc: relaxed_string(doc),
  }
}

impl MongoConnection {
  fn collection(&self, db: &str, name: &str) -> Collection<Document> {
    self.client.database(db).collection(name)
  }
}

#[async_trait::async_trait]
impl DocBrowse for MongoConnection {
  async fn databases(&self) -> Result<Vec<DocDatabase>, Error> {
    match self.client.list_databases().await {
      Ok(specs) => Ok(
        specs
          .into_iter()
          .map(|spec| DocDatabase {
            name: spec.name,
            size_bytes: Some(spec.size_on_disk as f64),
            empty: spec.empty,
          })
          .collect(),
      ),
      // Restricted users may lack listDatabases; the profile's db still browses.
      Err(err) => match &self.default_database {
        Some(database) => Ok(vec![DocDatabase {
          name: database.clone(),
          size_bytes: None,
          empty: false,
        }]),
        None => Err(err.into()),
      },
    }
  }

  async fn collections(&self, db: &str) -> Result<Vec<DocCollection>, Error> {
    let database = self.client.database(db);
    let specs: Vec<_> = database.list_collections().await?.try_collect().await?;
    let mut collections = Vec::with_capacity(specs.len());
    for spec in specs {
      let kind = match spec.collection_type {
        CollectionType::Collection => DocCollectionKind::Collection,
        CollectionType::View => DocCollectionKind::View,
        CollectionType::Timeseries => DocCollectionKind::Timeseries,
        _ => DocCollectionKind::Other,
      };
      let estimated_docs = match kind {
        // estimatedDocumentCount is invalid on views (it would execute them).
        DocCollectionKind::View => None,
        // One odd namespace must not fail the whole listing.
        _ => database
          .collection::<Document>(&spec.name)
          .estimated_document_count()
          .await
          .ok()
          .map(|count| count as f64),
      };
      collections.push(DocCollection {
        name: spec.name,
        kind,
        estimated_docs,
        capped: spec.options.capped.unwrap_or(false),
      });
    }
    collections.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(collections)
  }

  async fn find_docs(&self, request: &DocFindRequest) -> Result<DocPage, Error> {
    let filter = parse_extjson_doc(request.filter.as_deref(), "filter")?;
    let sort = parse_extjson_doc(request.sort.as_deref(), "sort")?;
    let offset = decode_cursor(request.cursor.as_deref())?;
    let limit = request.limit.clamp(1, DOC_PAGE_MAX);
    let collection = self.collection(&request.db, &request.collection);
    // limit+1 probes for a next page without a second round-trip.
    let mut find = collection
      .find(filter)
      .skip(offset)
      .limit(i64::from(limit) + 1);
    if !sort.is_empty() {
      find = find.sort(sort);
    }
    let mut cursor = find.await?;
    let mut docs = Vec::with_capacity(limit as usize);
    let mut more = false;
    while let Some(doc) = cursor.try_next().await? {
      if docs.len() == limit as usize {
        more = true;
        break;
      }
      docs.push(doc_entry(doc));
    }
    Ok(DocPage {
      docs,
      cursor: more.then(|| (offset + u64::from(limit)).to_string()),
    })
  }

  async fn doc_detail(&self, db: &str, collection: &str, id: &str) -> Result<DocDetail, Error> {
    let filter = doc! { "_id": parse_extjson_value(id)? };
    let document = self
      .collection(db, collection)
      .find_one(filter)
      .await?
      .ok_or_else(doc_not_found)?;
    Ok(DocDetail {
      id: document.get("_id").cloned().map(canonical_string),
      relaxed: relaxed_string(document.clone()),
      canonical: canonical_string(Bson::Document(document)),
    })
  }

  async fn replace_doc(
    &self,
    db: &str,
    collection: &str,
    id: &str,
    doc: &str,
  ) -> Result<(), Error> {
    let replacement = match parse_extjson_value(doc)? {
      Bson::Document(replacement) => replacement,
      _ => return Err(not_a_document("the replacement")),
    };
    let filter = doc! { "_id": parse_extjson_value(id)? };
    let result = self
      .collection(db, collection)
      .replace_one(filter, replacement)
      .await?;
    if result.matched_count == 0 {
      return Err(doc_not_found());
    }
    Ok(())
  }

  async fn delete_doc(&self, db: &str, collection: &str, id: &str) -> Result<(), Error> {
    let filter = doc! { "_id": parse_extjson_value(id)? };
    let result = self.collection(db, collection).delete_one(filter).await?;
    if result.deleted_count == 0 {
      return Err(doc_not_found());
    }
    Ok(())
  }

  async fn indexes(&self, db: &str, collection: &str) -> Result<Vec<IndexInfo>, Error> {
    let models: Vec<_> = self
      .collection(db, collection)
      .list_indexes()
      .await?
      .try_collect()
      .await?;
    Ok(
      models
        .into_iter()
        .map(|model| {
          let definition = relaxed_string(model.keys.clone());
          IndexInfo {
            name: model
              .options
              .as_ref()
              .and_then(|options| options.name.clone())
              .unwrap_or_else(|| definition.clone()),
            unique: model
              .options
              .as_ref()
              .and_then(|options| options.unique)
              .unwrap_or(false),
            definition,
          }
        })
        .collect(),
    )
  }

  async fn count_docs(
    &self,
    db: &str,
    collection: &str,
    filter: Option<&str>,
  ) -> Result<DocCount, Error> {
    match filter.map(str::trim).filter(|raw| !raw.is_empty()) {
      None => {
        let count = self
          .collection(db, collection)
          .estimated_document_count()
          .await?;
        Ok(DocCount {
          count: count as f64,
          exact: false,
        })
      }
      Some(raw) => {
        let filter = parse_extjson_doc(Some(raw), "filter")?;
        let count = self
          .collection(db, collection)
          .count_documents(filter)
          .limit(COUNT_CAP)
          .await?;
        Ok(DocCount {
          count: count as f64,
          exact: count < COUNT_CAP,
        })
      }
    }
  }

  async fn run_query(
    &self,
    db: &str,
    collection: &str,
    source: &str,
  ) -> Result<DocQueryResult, Error> {
    let value: serde_json::Value =
      serde_json::from_str(source).map_err(|err| invalid("query", err))?;
    let collection = self.collection(db, collection);
    let started = Instant::now();
    let mut cursor = match value {
      serde_json::Value::Object(_) => {
        let filter = match Bson::try_from(value).map_err(|err| invalid("query", err))? {
          Bson::Document(filter) => filter,
          _ => return Err(not_a_document("the filter")),
        };
        collection
          .find(filter)
          .limit(QUERY_SAMPLE as i64 + 1)
          .await?
      }
      serde_json::Value::Array(stages) => collection.aggregate(parse_pipeline(&stages)?).await?,
      _ => {
        return Err(Error::Unsupported {
          message: "expected a filter object or a pipeline array".to_string(),
        })
      }
    };
    let mut docs = Vec::new();
    let mut truncated = false;
    while let Some(doc) = cursor.try_next().await? {
      if docs.len() == QUERY_SAMPLE {
        truncated = true;
        break;
      }
      docs.push(relaxed_string(doc));
    }
    Ok(DocQueryResult {
      docs,
      truncated,
      duration_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
  }
}
