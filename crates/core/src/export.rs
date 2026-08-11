//! Format query rows as CSV, JSON, SQL inserts, or markdown, streaming to any writer.

use std::io::Write;

use serde::Deserialize;

use crate::connectors::{
  ColumnKind, QueryColumn, RowsChunk, SqlQuery, StreamSummary, TableRowsRequest,
};
use crate::error::Error;
use crate::profiles::ConnectorKind;

// ~5k rows between progress reports keeps the chatter negligible.
const PROGRESS_EVERY_CHUNKS: u64 = 25;

/// Full-table export: streams into `path`, reporting rows-written every
/// `PROGRESS_EVERY_CHUNKS` chunks. A failed or canceled export never leaves a
/// partial file behind.
pub async fn run_export(
  sql: &dyn SqlQuery,
  request: &TableRowsRequest,
  format: ExportFormat,
  kind: ConnectorKind,
  path: &str,
  on_progress: impl Fn(u64) + Send + 'static,
) -> Result<StreamSummary, Error> {
  let table = format!(
    "{}.{}",
    quote_ident(kind, &request.schema),
    quote_ident(kind, &request.table)
  );
  let file = std::io::BufWriter::new(std::fs::File::create(path)?);
  let sink = std::sync::Arc::new(std::sync::Mutex::new(ChunkSink::new(
    file, format, kind, table,
  )));
  let chunk_sink = sink.clone();
  let pushes = std::sync::atomic::AtomicU64::new(0);
  let result = sql
    .stream_rows(
      request,
      Box::new(move |chunk| {
        let mut sink = chunk_sink.lock().unwrap();
        let ok = sink.push(chunk);
        let count = pushes.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        if count % PROGRESS_EVERY_CHUNKS == 0 {
          on_progress(sink.rows());
        }
        ok
      }),
    )
    .await;

  // The stream callback was dropped with stream_rows: this Arc is the last one.
  let mut sink = std::sync::Arc::into_inner(sink)
    .expect("stream callback dropped")
    .into_inner()
    .unwrap();
  let discard = |err: Error| {
    let _ = std::fs::remove_file(path);
    err
  };
  let summary = result.map_err(discard)?;
  if let Some(err) = sink.error.take() {
    return Err(discard(err.into()));
  }
  sink.finish().map_err(|err| discard(err.into()))?;
  Ok(summary)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportFormat {
  Csv,
  Json,
  Sql,
  Markdown,
}

pub struct ExportWriter<W: Write> {
  format: ExportFormat,
  /// Drives ident quoting and literal escaping for SQL inserts.
  kind: ConnectorKind,
  columns: Vec<QueryColumn>,
  /// Quoted target for SQL inserts, e.g. `"public"."users"`.
  table: String,
  out: W,
  rows: u64,
}

impl<W: Write> ExportWriter<W> {
  pub fn new(
    mut out: W,
    format: ExportFormat,
    kind: ConnectorKind,
    columns: Vec<QueryColumn>,
    table: String,
  ) -> std::io::Result<Self> {
    match format {
      ExportFormat::Csv => {
        let header = columns
          .iter()
          .map(|column| csv_field(&column.name))
          .collect::<Vec<_>>()
          .join(",");
        writeln!(out, "{header}")?;
      }
      ExportFormat::Json => write!(out, "[")?,
      ExportFormat::Sql => {}
      ExportFormat::Markdown => {
        let header = columns
          .iter()
          .map(|column| markdown_cell(&column.name))
          .collect::<Vec<_>>()
          .join(" | ");
        writeln!(out, "| {header} |")?;
        writeln!(out, "|{}", " --- |".repeat(columns.len()))?;
      }
    }
    Ok(Self {
      format,
      kind,
      columns,
      table,
      out,
      rows: 0,
    })
  }

  pub fn row(&mut self, row: &[Option<String>]) -> std::io::Result<()> {
    match self.format {
      ExportFormat::Csv => {
        let line = row
          .iter()
          .map(|value| value.as_deref().map(csv_field).unwrap_or_default())
          .collect::<Vec<_>>()
          .join(",");
        writeln!(self.out, "{line}")?;
      }
      ExportFormat::Json => {
        if self.rows > 0 {
          write!(self.out, ",")?;
        }
        write!(self.out, "\n  {{")?;
        for (index, (column, value)) in self.columns.iter().zip(row).enumerate() {
          if index > 0 {
            write!(self.out, ", ")?;
          }
          write!(self.out, "{}: ", json_string(&column.name))?;
          match value {
            None => write!(self.out, "null")?,
            Some(value) => write!(self.out, "{}", json_value(column.kind, value))?,
          }
        }
        write!(self.out, "}}")?;
      }
      ExportFormat::Sql => {
        let names = self
          .columns
          .iter()
          .map(|column| quote_ident(self.kind, &column.name))
          .collect::<Vec<_>>()
          .join(", ");
        let values = row
          .iter()
          .map(|value| match value {
            None => "NULL".to_string(),
            Some(value) => sql_literal(self.kind, value),
          })
          .collect::<Vec<_>>()
          .join(", ");
        writeln!(
          self.out,
          "INSERT INTO {} ({names}) VALUES ({values});",
          self.table
        )?;
      }
      ExportFormat::Markdown => {
        let line = row
          .iter()
          .map(|value| value.as_deref().map(markdown_cell).unwrap_or_default())
          .collect::<Vec<_>>()
          .join(" | ");
        writeln!(self.out, "| {line} |")?;
      }
    }
    self.rows += 1;
    Ok(())
  }

  pub fn finish(mut self) -> std::io::Result<W> {
    if self.format == ExportFormat::Json {
      writeln!(self.out, "\n]")?;
    }
    self.out.flush()?;
    Ok(self.out)
  }
}

/// Adapts `stream_rows` chunks to an `ExportWriter`; the writer is created on
/// the first chunk (which carries the columns). Write failures are stashed so
/// the stream callback can abort and the command surface the error.
pub struct ChunkSink<W: Write> {
  format: ExportFormat,
  kind: ConnectorKind,
  table: String,
  out: Option<W>,
  writer: Option<ExportWriter<W>>,
  pub error: Option<std::io::Error>,
}

impl<W: Write> ChunkSink<W> {
  pub fn new(out: W, format: ExportFormat, kind: ConnectorKind, table: String) -> Self {
    Self {
      format,
      kind,
      table,
      out: Some(out),
      writer: None,
      error: None,
    }
  }

  /// Rows written so far, for progress reporting.
  pub fn rows(&self) -> u64 {
    self.writer.as_ref().map_or(0, |writer| writer.rows)
  }

  pub fn push(&mut self, chunk: RowsChunk) -> bool {
    let result = self.try_push(chunk);
    if let Err(err) = result {
      self.error = Some(err);
      return false;
    }
    true
  }

  fn try_push(&mut self, chunk: RowsChunk) -> std::io::Result<()> {
    if let Some(columns) = chunk.columns {
      let out = self.out.take().expect("columns arrive only once");
      self.writer = Some(ExportWriter::new(
        out,
        self.format,
        self.kind,
        columns,
        self.table.clone(),
      )?);
    }
    let writer = self.writer.as_mut().expect("first chunk carries columns");
    for row in &chunk.rows {
      writer.row(row)?;
    }
    Ok(())
  }

  pub fn finish(self) -> std::io::Result<W> {
    match self.writer {
      Some(writer) => writer.finish(),
      None => Ok(self.out.expect("untouched sink still owns the writer")),
    }
  }
}

pub fn quote_ident(kind: ConnectorKind, ident: &str) -> String {
  match kind {
    // Redis/mongo never reach SQL export; the arm only keeps the match exhaustive.
    ConnectorKind::Postgres
    | ConnectorKind::Sqlite
    | ConnectorKind::Redis
    | ConnectorKind::Mongo => {
      format!("\"{}\"", ident.replace('"', "\"\""))
    }
    ConnectorKind::Mysql => format!("`{}`", ident.replace('`', "``")),
  }
}

fn csv_field(value: &str) -> String {
  if value.contains(['"', ',', '\n', '\r']) {
    format!("\"{}\"", value.replace('"', "\"\""))
  } else {
    value.to_string()
  }
}

fn sql_literal(kind: ConnectorKind, value: &str) -> String {
  match kind {
    // standard_conforming_strings: backslashes are literal in postgres and sqlite.
    ConnectorKind::Postgres
    | ConnectorKind::Sqlite
    | ConnectorKind::Redis
    | ConnectorKind::Mongo => {
      format!("'{}'", value.replace('\'', "''"))
    }
    // mysql treats backslash as an escape character inside strings.
    ConnectorKind::Mysql => {
      format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
    }
  }
}

fn markdown_cell(value: &str) -> String {
  value
    .replace('|', "\\|")
    .replace("\r\n", "<br>")
    .replace(['\n', '\r'], "<br>")
}

fn json_string(value: &str) -> String {
  serde_json::to_string(value).expect("strings always serialize")
}

fn json_value(kind: ColumnKind, value: &str) -> String {
  match kind {
    ColumnKind::Bool if value == "t" => "true".to_string(),
    ColumnKind::Bool if value == "f" => "false".to_string(),
    // NaN/Infinity fall through to a quoted string.
    ColumnKind::Number if is_json_number(value) => value.to_string(),
    // json/jsonb text output is valid JSON already.
    ColumnKind::Json => value.to_string(),
    _ => json_string(value),
  }
}

fn is_json_number(value: &str) -> bool {
  let bytes = value.as_bytes();
  let mut i = usize::from(bytes.first() == Some(&b'-'));
  let digits = |i: &mut usize| {
    let start = *i;
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
      *i += 1;
    }
    *i > start
  };
  if !digits(&mut i) {
    return false;
  }
  if i < bytes.len() && bytes[i] == b'.' {
    i += 1;
    if !digits(&mut i) {
      return false;
    }
  }
  if i < bytes.len() && (bytes[i] | 0x20) == b'e' {
    i += 1;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
      i += 1;
    }
    if !digits(&mut i) {
      return false;
    }
  }
  i == bytes.len()
}

/// One statement's rows to a file; the caller owns the path choice.
pub fn export_statement(
  columns: Vec<QueryColumn>,
  rows: &[Vec<Option<String>>],
  format: ExportFormat,
  kind: ConnectorKind,
  table: &str,
  path: &str,
) -> Result<(), Error> {
  let file = std::io::BufWriter::new(std::fs::File::create(path)?);
  let mut writer = ExportWriter::new(file, format, kind, columns, quote_ident(kind, table))?;
  for row in rows {
    writer.row(row)?;
  }
  writer.finish()?;
  Ok(())
}

/// Clipboard copy: same formats, returned as a string.
pub fn format_statement(
  columns: Vec<QueryColumn>,
  rows: &[Vec<Option<String>>],
  format: ExportFormat,
  kind: ConnectorKind,
  table: &str,
) -> Result<String, Error> {
  let mut out = Vec::new();
  let mut writer = ExportWriter::new(&mut out, format, kind, columns, quote_ident(kind, table))?;
  for row in rows {
    writer.row(row)?;
  }
  writer.finish()?;
  Ok(String::from_utf8(out).expect("formats emit utf-8"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn column(name: &str, kind: ColumnKind) -> QueryColumn {
    QueryColumn {
      name: name.to_string(),
      data_type: None,
      kind,
    }
  }

  fn render(
    format: ExportFormat,
    columns: Vec<QueryColumn>,
    rows: &[Vec<Option<String>>],
  ) -> String {
    render_as(
      ConnectorKind::Postgres,
      "\"public\".\"users\"",
      format,
      columns,
      rows,
    )
  }

  fn render_as(
    kind: ConnectorKind,
    table: &str,
    format: ExportFormat,
    columns: Vec<QueryColumn>,
    rows: &[Vec<Option<String>>],
  ) -> String {
    let mut out = Vec::new();
    let mut writer = ExportWriter::new(&mut out, format, kind, columns, table.to_string()).unwrap();
    for row in rows {
      writer.row(row).unwrap();
    }
    writer.finish().unwrap();
    String::from_utf8(out).unwrap()
  }

  fn cell(value: &str) -> Option<String> {
    Some(value.to_string())
  }

  #[test]
  fn csv_escapes_and_renders_null_as_empty() {
    let out = render(
      ExportFormat::Csv,
      vec![
        column("id", ColumnKind::Number),
        column("name,x", ColumnKind::Text),
      ],
      &[
        vec![cell("1"), cell("plain")],
        vec![cell("2"), cell("has \"quotes\", commas\nand newlines")],
        vec![cell("3"), None],
      ],
    );
    assert_eq!(
      out,
      "id,\"name,x\"\n1,plain\n2,\"has \"\"quotes\"\", commas\nand newlines\"\n3,\n"
    );
  }

  #[test]
  fn json_types_by_column_kind() {
    let out = render(
      ExportFormat::Json,
      vec![
        column("id", ColumnKind::Number),
        column("ok", ColumnKind::Bool),
        column("meta", ColumnKind::Json),
        column("note", ColumnKind::Text),
      ],
      &[
        vec![
          cell("-1.5e3"),
          cell("t"),
          cell("{\"a\":1}"),
          cell("line\n\"two\""),
        ],
        vec![cell("NaN"), cell("f"), None, None],
      ],
    );
    assert_eq!(
      out,
      "[\n  {\"id\": -1.5e3, \"ok\": true, \"meta\": {\"a\":1}, \"note\": \"line\\n\\\"two\\\"\"},\n  {\"id\": \"NaN\", \"ok\": false, \"meta\": null, \"note\": null}\n]\n"
    );
  }

  #[test]
  fn sql_inserts_quote_idents_and_literals() {
    let out = render(
      ExportFormat::Sql,
      vec![
        column("id", ColumnKind::Number),
        column("na\"me", ColumnKind::Text),
      ],
      &[vec![cell("1"), cell("it's")], vec![cell("2"), None]],
    );
    assert_eq!(
      out,
      "INSERT INTO \"public\".\"users\" (\"id\", \"na\"\"me\") VALUES ('1', 'it''s');\nINSERT INTO \"public\".\"users\" (\"id\", \"na\"\"me\") VALUES ('2', NULL);\n"
    );
  }

  #[test]
  fn sql_inserts_follow_the_mysql_dialect() {
    let out = render_as(
      ConnectorKind::Mysql,
      "`app`.`users`",
      ExportFormat::Sql,
      vec![
        column("na`me", ColumnKind::Text),
        column("note", ColumnKind::Text),
      ],
      &[vec![cell("it's"), cell("C:\\path")]],
    );
    // Backtick idents, doubled quotes, escaped backslashes.
    assert_eq!(
      out,
      "INSERT INTO `app`.`users` (`na``me`, `note`) VALUES ('it''s', 'C:\\\\path');\n"
    );
  }

  #[test]
  fn sql_inserts_follow_the_sqlite_dialect() {
    let out = render_as(
      ConnectorKind::Sqlite,
      "\"main\".\"users\"",
      ExportFormat::Sql,
      vec![column("note", ColumnKind::Text)],
      &[vec![cell("C:\\path it's")]],
    );
    // Double-quoted idents, doubled quotes, literal backslashes.
    assert_eq!(
      out,
      "INSERT INTO \"main\".\"users\" (\"note\") VALUES ('C:\\path it''s');\n"
    );
  }

  #[test]
  fn markdown_escapes_pipes_and_newlines() {
    let out = render(
      ExportFormat::Markdown,
      vec![
        column("a|b", ColumnKind::Text),
        column("c", ColumnKind::Text),
      ],
      &[vec![cell("x|y"), cell("l1\r\nl2")], vec![None, cell("z")]],
    );
    assert_eq!(
      out,
      "| a\\|b | c |\n| --- | --- |\n| x\\|y | l1<br>l2 |\n|  | z |\n"
    );
  }

  #[test]
  fn json_number_syntax() {
    for valid in ["0", "-1", "1.5", "-1.5e3", "2E+10", "42e-1"] {
      assert!(is_json_number(valid), "{valid}");
    }
    for invalid in [
      "", "-", "1.", ".5", "1e", "1e+", "NaN", "Infinity", "1a", "--1",
    ] {
      assert!(!is_json_number(invalid), "{invalid}");
    }
  }

  #[test]
  fn chunk_sink_creates_writer_on_first_chunk_and_stashes_errors() {
    struct FailAfter(usize);
    impl Write for FailAfter {
      fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0 == 0 {
          return Err(std::io::Error::other("disk full"));
        }
        self.0 -= 1;
        Ok(buf.len())
      }
      fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
      }
    }

    // writeln! issues two writes (payload, newline): budget covers the header only.
    let mut sink = ChunkSink::new(
      FailAfter(2),
      ExportFormat::Csv,
      ConnectorKind::Postgres,
      String::new(),
    );
    assert!(sink.push(RowsChunk {
      columns: Some(vec![column("id", ColumnKind::Number)]),
      rows: vec![],
    }));
    assert!(!sink.push(RowsChunk {
      columns: None,
      rows: vec![vec![cell("1")]],
    }));
    assert!(sink.error.is_some());
  }
}
