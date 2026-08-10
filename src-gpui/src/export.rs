use soquel_core::export::ExportFormat;

pub const EXPORT_FORMATS: [ExportFormat; 4] = [
  ExportFormat::Csv,
  ExportFormat::Json,
  ExportFormat::Sql,
  ExportFormat::Markdown,
];

pub fn format_label(format: ExportFormat) -> &'static str {
  match format {
    ExportFormat::Csv => "CSV",
    ExportFormat::Json => "JSON",
    ExportFormat::Sql => "SQL inserts",
    ExportFormat::Markdown => "Markdown",
  }
}

pub fn format_extension(format: ExportFormat) -> &'static str {
  match format {
    ExportFormat::Csv => "csv",
    ExportFormat::Json => "json",
    ExportFormat::Sql => "sql",
    ExportFormat::Markdown => "md",
  }
}
