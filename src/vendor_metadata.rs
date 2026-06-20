use std::{
    fs::{self, File},
    io::{Seek, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use arrow::{
    array::{ArrayRef, Float64Array, Int32Array, StringArray, UInt64Array},
    datatypes::{DataType, Field, Schema},
    record_batch::RecordBatch,
};
use mzpeak_prototyping::archive::{DataKind, EntityType, FileEntry, ZipArchiveWriter};
use parquet::arrow::ArrowWriter;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::{
    pipeline::{BrfpError, BrfpResult},
    uv::{UvDetectorInventory, inspect_uv_detector_inventory},
};

const VENDOR_FILE_METADATA_NAME: &str = "vendor_file_metadata.parquet";
const VENDOR_SCAN_METADATA_NAME: &str = "vendor_scan_metadata.parquet";
const MAX_INLINE_TEXT_METADATA_BYTES: u64 = 16 * 1024 * 1024;

/// One verbatim per-spectrum vendor value (REQ-04).
///
/// Keyed by the dense `ordinal` (0..N-1, joins the `spectra_*` facets) and the
/// verbatim `native_id`. `value` is the exact source string; `value_float` is the
/// typed numeric value when the field is numeric.
#[derive(Debug, Clone, PartialEq)]
pub struct VendorScanRow {
    pub ordinal: u64,
    pub native_id: String,
    pub label: String,
    pub value: String,
    pub value_float: Option<f64>,
}

impl VendorScanRow {
    /// Build a row, typing `value_float` from the value string.
    pub fn new(
        ordinal: u64,
        native_id: impl Into<String>,
        label: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let value = value.into();
        let value_float = parse_vendor_float(&value);
        Self {
            ordinal,
            native_id: native_id.into(),
            label: label.into(),
            value,
            value_float,
        }
    }
}

/// The per-spectrum vendor facet: a tall table of [`VendorScanRow`]s written as
/// `vendor_scan_metadata.parquet` and injected as a proprietary archive entry.
#[derive(Debug, Clone, Default)]
pub struct VendorScanMetadata {
    rows: Vec<VendorScanRow>,
}

impl VendorScanMetadata {
    pub fn new(rows: Vec<VendorScanRow>) -> Self {
        Self { rows }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> &[VendorScanRow] {
        &self.rows
    }

    /// Write the facet into the archive as a proprietary (non-CV) parquet entry.
    pub fn write_to_archive<W: Write + Send + Seek>(
        &self,
        zip_writer: &mut ZipArchiveWriter<W>,
    ) -> BrfpResult<()> {
        if self.rows.is_empty() {
            return Ok(());
        }
        let parquet = self.to_parquet()?;
        let entry = proprietary_entry(VENDOR_SCAN_METADATA_NAME);
        zip_writer
            .add_file_from_read(&mut parquet.as_slice(), None::<&String>, Some(entry))
            .map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to add {VENDOR_SCAN_METADATA_NAME} to mzPeak archive: {error}"
                ))
            })
    }

    fn to_parquet(&self) -> BrfpResult<Vec<u8>> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("ordinal", DataType::UInt64, false),
            Field::new("native_id", DataType::Utf8, false),
            Field::new("label", DataType::Utf8, false),
            Field::new("value", DataType::Utf8, false),
            Field::new("value_float", DataType::Float64, true),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(
                self.rows.iter().map(|row| row.ordinal).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                self.rows
                    .iter()
                    .map(|row| row.native_id.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                self.rows
                    .iter()
                    .map(|row| row.label.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                self.rows
                    .iter()
                    .map(|row| row.value.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                self.rows
                    .iter()
                    .map(|row| row.value_float)
                    .collect::<Vec<_>>(),
            )),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).map_err(|error| {
            BrfpError::Writer(format!(
                "failed to build vendor_scan_metadata record batch: {error}"
            ))
        })?;
        let mut buffer = Vec::new();
        {
            let mut writer = ArrowWriter::try_new(&mut buffer, schema, None).map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to create vendor_scan_metadata parquet writer: {error}"
                ))
            })?;
            writer.write(&batch).map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to write vendor_scan_metadata parquet batch: {error}"
                ))
            })?;
            writer.close().map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to close vendor_scan_metadata parquet writer: {error}"
                ))
            })?;
        }
        Ok(buffer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VendorMetadataMode {
    Tall,
    Wide,
    Both,
}

impl VendorMetadataMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tall => "tall",
            Self::Wide => "wide",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VendorMetadataBundle {
    rows: Vec<VendorMetadataRow>,
}

impl VendorMetadataBundle {
    pub fn collect(input: &Path) -> BrfpResult<Self> {
        let mut rows = Vec::new();
        let mut entry_index = 0i32;

        push_row(
            &mut rows,
            "source",
            &mut entry_index,
            "input_path",
            &input.to_string_lossy(),
        );
        if let Some(file_name) = input.file_name().and_then(|value| value.to_str()) {
            push_row(
                &mut rows,
                "source",
                &mut entry_index,
                "input_name",
                file_name,
            );
        }

        if let Some(baf) = input
            .join("analysis.baf")
            .is_file()
            .then(|| input.join("analysis.baf"))
        {
            push_row(
                &mut rows,
                "source",
                &mut entry_index,
                "analysis_baf",
                &relative_or_display(input, &baf),
            );
        }

        // Vendor metadata is opt-in provenance: every facet read below is
        // best-effort. A failing read logs a warning and records a
        // `vendor_warning` row, but never aborts the (often multi-GB)
        // conversion (REQ-02; the "never lose / never abort" rule from the
        // mzPeak4TRFR handoff).
        if let Some(database) = sqlite_analysis_database(input) {
            push_row(
                &mut rows,
                "source",
                &mut entry_index,
                "analysis_database",
                &relative_or_display(input, &database),
            );
            if let Err(error) =
                append_sqlite_metadata_rows(input, &database, &mut rows, &mut entry_index)
            {
                degrade_facet(&mut rows, &mut entry_index, "sqlite_metadata", &error);
            }
        }

        match inspect_uv_detector_inventory(input) {
            Ok(uv_inventory) if !uv_inventory.files.is_empty() => {
                append_uv_inventory_rows(&uv_inventory, &mut rows, &mut entry_index);
            }
            Ok(_) => {}
            Err(error) => degrade_facet(&mut rows, &mut entry_index, "uv_inventory", &error),
        }

        let candidates = match discover_vendor_files(input) {
            Ok(candidates) => candidates,
            Err(error) => {
                degrade_facet(&mut rows, &mut entry_index, "vendor_file_discovery", &error);
                Vec::new()
            }
        };
        for candidate in candidates {
            let rel = relative_or_display(input, &candidate);
            let metadata = match fs::metadata(&candidate) {
                Ok(metadata) => metadata,
                Err(error) => {
                    degrade_facet(
                        &mut rows,
                        &mut entry_index,
                        &format!("vendor_file:{rel}"),
                        &BrfpError::from(error),
                    );
                    continue;
                }
            };
            let role = classify_vendor_file(&candidate, input);
            let detector_data_file = is_detector_data_file(&candidate);

            push_row(
                &mut rows,
                "vendor_file",
                &mut entry_index,
                &format!("{role}:path"),
                &rel,
            );
            push_row(
                &mut rows,
                "vendor_file",
                &mut entry_index,
                &format!("{role}:size_bytes"),
                &metadata.len().to_string(),
            );

            if !detector_data_file
                && metadata.len() <= MAX_INLINE_TEXT_METADATA_BYTES
                && let Err(error) =
                    append_text_file_rows(&candidate, &rel, &mut rows, &mut entry_index)
            {
                degrade_facet(
                    &mut rows,
                    &mut entry_index,
                    &format!("vendor_file_text:{rel}"),
                    &error,
                );
            }
        }

        Ok(Self { rows })
    }

    pub fn rows(&self) -> &[VendorMetadataRow] {
        &self.rows
    }

    pub fn write_json_sidecar(&self, output: &Path) -> BrfpResult<()> {
        if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let file = File::create(output)?;
        serde_json::to_writer_pretty(file, &VendorMetadataJson { rows: &self.rows })?;
        Ok(())
    }

    pub fn write_to_archive<W: Write + Send + Seek>(
        &self,
        zip_writer: &mut ZipArchiveWriter<W>,
        mode: Option<VendorMetadataMode>,
    ) -> BrfpResult<()> {
        if let Some(mode) = mode {
            if matches!(mode, VendorMetadataMode::Wide | VendorMetadataMode::Both) {
                // The wide pivot is one typed column per per-spectrum trailer
                // label; it only becomes meaningful once the per-spectrum vendor
                // facet exists (Phase 2 / REQ-04). Until then `wide`/`both` emit
                // the tall file-level facet. The CLI help states this too, so the
                // flag is honest rather than silently contradicted.
                tracing::warn!(
                    requested = mode.as_str(),
                    "wide vendor layout is not available yet (no per-spectrum trailer facet); writing the tall file-level facet"
                );
            }
            let parquet = self.vendor_file_metadata_parquet()?;
            let entry = proprietary_entry(VENDOR_FILE_METADATA_NAME);
            zip_writer
                .add_file_from_read(&mut parquet.as_slice(), None::<&String>, Some(entry))
                .map_err(|error| {
                    BrfpError::Writer(format!(
                        "failed to add {VENDOR_FILE_METADATA_NAME} to mzPeak archive: {error}"
                    ))
                })?;
        }

        zip_writer
            .add_index_metadata(
                "vendor_metadata",
                &VendorMetadataIndex {
                    vendor: "Bruker",
                    file_metadata_rows: self.rows.len(),
                    raw_payload_count: 0,
                    mode: mode.map(VendorMetadataMode::as_str),
                },
            )
            .map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to add vendor metadata index entry: {error}"
                ))
            })?;

        Ok(())
    }

    fn vendor_file_metadata_parquet(&self) -> BrfpResult<Vec<u8>> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("category", DataType::Utf8, false),
            Field::new("entry_index", DataType::Int32, false),
            Field::new("label", DataType::Utf8, false),
            Field::new("value", DataType::Utf8, false),
            Field::new("value_float", DataType::Float64, true),
        ]));

        let columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(
                self.rows
                    .iter()
                    .map(|row| row.category.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int32Array::from(
                self.rows
                    .iter()
                    .map(|row| row.entry_index)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                self.rows
                    .iter()
                    .map(|row| row.label.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                self.rows
                    .iter()
                    .map(|row| row.value.as_str())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                self.rows
                    .iter()
                    .map(|row| row.value_float)
                    .collect::<Vec<_>>(),
            )),
        ];
        let batch = RecordBatch::try_new(schema.clone(), columns).map_err(|error| {
            BrfpError::Writer(format!(
                "failed to build vendor_file_metadata record batch: {error}"
            ))
        })?;

        let mut buffer = Vec::new();
        {
            let mut writer = ArrowWriter::try_new(&mut buffer, schema, None).map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to create vendor_file_metadata parquet writer: {error}"
                ))
            })?;
            writer.write(&batch).map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to write vendor_file_metadata parquet batch: {error}"
                ))
            })?;
            writer.close().map_err(|error| {
                BrfpError::Writer(format!(
                    "failed to close vendor_file_metadata parquet writer: {error}"
                ))
            })?;
        }
        Ok(buffer)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VendorMetadataRow {
    pub category: String,
    pub entry_index: i32,
    pub label: String,
    pub value: String,
    pub value_float: Option<f64>,
}

#[derive(Serialize)]
struct VendorMetadataJson<'a> {
    rows: &'a [VendorMetadataRow],
}

#[derive(Serialize)]
struct VendorMetadataIndex<'a> {
    vendor: &'a str,
    file_metadata_rows: usize,
    raw_payload_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
}

fn append_sqlite_metadata_rows(
    input: &Path,
    database: &Path,
    rows: &mut Vec<VendorMetadataRow>,
    entry_index: &mut i32,
) -> BrfpResult<()> {
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    for (table, category) in [
        ("GlobalMetadata", "global_metadata"),
        ("Properties", "baf_properties"),
    ] {
        let exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1",
            [table],
            |row| row.get(0),
        )?;
        if exists == 0 {
            continue;
        }

        let mut stmt =
            connection.prepare(&format!("SELECT Key, Value FROM {table} ORDER BY Key"))?;
        let rows_iter = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        for row in rows_iter {
            let (key, value) = row?;
            push_row(
                rows,
                category,
                entry_index,
                &key,
                value.as_deref().unwrap_or_default(),
            );
        }
    }
    push_row(
        rows,
        "sqlite_metadata_source",
        entry_index,
        "database",
        &relative_or_display(input, database),
    );
    Ok(())
}

fn append_uv_inventory_rows(
    inventory: &UvDetectorInventory,
    rows: &mut Vec<VendorMetadataRow>,
    entry_index: &mut i32,
) {
    for file in &inventory.files {
        push_row(
            rows,
            "uv_detector_file",
            entry_index,
            &format!("{}:path", file.role.as_str()),
            &file.relative_path,
        );
        push_row(
            rows,
            "uv_detector_file",
            entry_index,
            &format!("{}:size_bytes", file.role.as_str()),
            &file.size_bytes.to_string(),
        );
    }

    if let Some(method) = &inventory.lc_method {
        push_optional_float(
            rows,
            entry_index,
            "uv_method",
            "runtime_minutes",
            method.runtime_minutes,
        );
        push_optional_float(
            rows,
            entry_index,
            "uv_method",
            "sample_rate_hz",
            method.sample_rate_hz,
        );
        push_optional_float(
            rows,
            entry_index,
            "uv_method",
            "spectral_start_nm",
            method.spectral_start_nm,
        );
        push_optional_float(
            rows,
            entry_index,
            "uv_method",
            "spectral_end_nm",
            method.spectral_end_nm,
        );
        if let Some(save_spectra) = method.save_spectra {
            push_row(
                rows,
                "uv_method",
                entry_index,
                "save_spectra",
                if save_spectra { "true" } else { "false" },
            );
        }
        for wavelength in &method.channel_wavelengths_nm {
            push_row(
                rows,
                "uv_method",
                entry_index,
                "channel_wavelength_nm",
                &wavelength.to_string(),
            );
        }
        for label in &method.detector_labels {
            push_row(rows, "uv_method", entry_index, "detector_label", label);
        }
    }

    for entry in &inventory.hdx_entries {
        push_row(
            rows,
            "uv_detector_index",
            entry_index,
            &entry.label,
            &entry.target,
        );
    }

    for header in &inventory.u2_headers {
        push_row(
            rows,
            "uv_u2_header",
            entry_index,
            "path",
            &header.relative_path,
        );
        push_row(rows, "uv_u2_header", entry_index, "magic", &header.magic);
        push_optional_u32(
            rows,
            entry_index,
            "uv_u2_header",
            "header_size_bytes",
            header.header_size_bytes,
        );
        push_optional_u32(
            rows,
            entry_index,
            "uv_u2_header",
            "wavelength_count",
            header.wavelength_count,
        );
        push_optional_float(
            rows,
            entry_index,
            "uv_u2_header",
            "wavelength_start_nm",
            header.wavelength_start_nm,
        );
        push_optional_float(
            rows,
            entry_index,
            "uv_u2_header",
            "wavelength_end_nm",
            header.wavelength_end_nm,
        );
        push_optional_float(
            rows,
            entry_index,
            "uv_u2_header",
            "sample_rate_hz",
            header.sample_rate_hz,
        );
        push_optional_u32(
            rows,
            entry_index,
            "uv_u2_header",
            "spectrum_count",
            header.spectrum_count,
        );
        push_optional_u32(
            rows,
            entry_index,
            "uv_u2_header",
            "record_size_bytes",
            header.record_size_bytes,
        );
        if let Some(data_start_bytes) = header.data_start_bytes {
            push_row(
                rows,
                "uv_u2_header",
                entry_index,
                "data_start_bytes",
                &data_start_bytes.to_string(),
            );
        }
        if let Some(unit) = &header.intensity_unit {
            push_row(rows, "uv_u2_header", entry_index, "intensity_unit", unit);
        }
    }

    for warning in &inventory.warnings {
        push_row(rows, "uv_warning", entry_index, "warning", warning);
    }
}

fn push_optional_float(
    rows: &mut Vec<VendorMetadataRow>,
    entry_index: &mut i32,
    category: &str,
    label: &str,
    value: Option<f64>,
) {
    if let Some(value) = value {
        push_row(rows, category, entry_index, label, &value.to_string());
    }
}

fn push_optional_u32(
    rows: &mut Vec<VendorMetadataRow>,
    entry_index: &mut i32,
    category: &str,
    label: &str,
    value: Option<u32>,
) {
    if let Some(value) = value {
        push_row(rows, category, entry_index, label, &value.to_string());
    }
}

fn append_text_file_rows(
    path: &Path,
    relative_path: &str,
    rows: &mut Vec<VendorMetadataRow>,
    entry_index: &mut i32,
) -> BrfpResult<()> {
    let Some(text) = read_text_lossless(path)? else {
        push_row(
            rows,
            "vendor_text",
            entry_index,
            &format!("{relative_path}:encoding"),
            "binary_or_unsupported_text_encoding",
        );
        return Ok(());
    };

    let category = text_category(path);
    push_row(rows, category, entry_index, relative_path, &text);
    for (line_index, line) in text.lines().enumerate() {
        if line_index >= 256 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((key, value)) = split_metadata_line(trimmed) {
            push_row(
                rows,
                category,
                entry_index,
                &format!("{relative_path}:{key}"),
                value,
            );
        }
    }
    Ok(())
}

fn read_text_lossless(path: &Path) -> BrfpResult<Option<String>> {
    let bytes = fs::read(path)?;
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(bytes[3..].to_vec())
            .map(Some)
            .map_err(|error| {
                BrfpError::Reader(format!("invalid UTF-8 in {}: {error}", path.display()))
            });
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&bytes[2..], true, path).map(Some);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&bytes[2..], false, path).map(Some);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Ok(None),
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool, path: &Path) -> BrfpResult<String> {
    if bytes.len() % 2 != 0 {
        return Err(BrfpError::Reader(format!(
            "odd UTF-16 byte count in {}",
            path.display()
        )));
    }
    let words = bytes
        .chunks_exact(2)
        .map(|chunk| {
            if little_endian {
                u16::from_le_bytes([chunk[0], chunk[1]])
            } else {
                u16::from_be_bytes([chunk[0], chunk[1]])
            }
        })
        .collect::<Vec<_>>();
    String::from_utf16(&words).map_err(|error| {
        BrfpError::Reader(format!(
            "invalid UTF-16 text in {}: {error}",
            path.display()
        ))
    })
}

fn split_metadata_line(line: &str) -> Option<(&str, &str)> {
    for delimiter in ["=", ":", "\t"] {
        if let Some((key, value)) = line.split_once(delimiter) {
            let key = key.trim();
            let value = value.trim();
            if !key.is_empty() && !value.is_empty() {
                return Some((key, value));
            }
        }
    }
    None
}

fn discover_vendor_files(input: &Path) -> BrfpResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    visit_files(input, &mut files)?;
    files.sort();
    files.retain(|path| is_vendor_metadata_file(path, input) || is_detector_data_file(path));
    Ok(files)
}

fn visit_files(path: &Path, files: &mut Vec<PathBuf>) -> BrfpResult<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            visit_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn is_vendor_metadata_file(path: &Path, root: &Path) -> bool {
    if path == root.join("analysis.tsf")
        || path == root.join("analysis.tdf")
        || path == root.join("analysis.baf")
    {
        return false;
    }

    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let name_lower = name.to_ascii_lowercase();
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    matches!(
        name_lower.as_str(),
        "sampleinfo.xml"
            | "lcparms.txt"
            | "analysis.content"
            | "analysis.baf_idx"
            | "analysis.baf_xtr"
            | "analysis.sqlite"
    ) || name_lower.contains("method")
        || matches!(
            ext.as_deref(),
            Some("xml" | "txt" | "content" | "hdx" | "hss")
        )
}

fn is_detector_data_file(path: &Path) -> bool {
    let Some(ext) = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        "u2" | "unt" | "uv" | "pda" | "dad" | "hdx" | "hss"
    )
}

fn classify_vendor_file(path: &Path, root: &Path) -> &'static str {
    let rel = relative_or_display(root, path).to_ascii_lowercase();
    if rel.ends_with("analysis.baf_idx") {
        "baf_index"
    } else if rel.ends_with("analysis.baf_xtr") {
        "baf_extra"
    } else if rel.ends_with("analysis.sqlite") {
        "baf_sqlite_cache"
    } else if is_detector_data_file(path) {
        "detector_file"
    } else if rel.ends_with("sampleinfo.xml") {
        "sample_info"
    } else if rel.ends_with("lcparms.txt") {
        "lc_parameters"
    } else if rel.ends_with("analysis.content") {
        "analysis_content"
    } else if rel.contains("method") {
        "method"
    } else {
        "metadata_file"
    }
}

fn text_category(path: &Path) -> &'static str {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return "vendor_text";
    };
    let lower = name.to_ascii_lowercase();
    if lower == "sampleinfo.xml" {
        "sample_info"
    } else if lower == "lcparms.txt" {
        "lc_parameters"
    } else if lower == "analysis.content" {
        "analysis_content"
    } else if lower.contains("method") {
        "instrument_method"
    } else {
        "vendor_text"
    }
}

fn sqlite_analysis_database(input: &Path) -> Option<PathBuf> {
    ["analysis.tsf", "analysis.tdf", "analysis.sqlite"]
        .iter()
        .map(|name| input.join(name))
        .find(|path| path.is_file())
}

fn relative_or_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn proprietary_entry(name: &str) -> FileEntry {
    FileEntry::new(
        name.to_string(),
        EntityType::Other("proprietary".to_string()),
        DataKind::Proprietary,
    )
}

/// Record a best-effort vendor-facet failure: warn and emit a `vendor_warning`
/// row, so the conversion continues with that facet degraded rather than aborting.
fn degrade_facet(
    rows: &mut Vec<VendorMetadataRow>,
    entry_index: &mut i32,
    facet: &str,
    error: &BrfpError,
) {
    let message = error.to_string();
    tracing::warn!(facet, error = %message, "vendor metadata facet degraded");
    push_row(rows, "vendor_warning", entry_index, facet, &message);
}

fn push_row(
    rows: &mut Vec<VendorMetadataRow>,
    category: &str,
    entry_index: &mut i32,
    label: &str,
    value: &str,
) {
    rows.push(VendorMetadataRow {
        category: category.to_string(),
        entry_index: *entry_index,
        label: label.to_string(),
        value: value.to_string(),
        value_float: parse_vendor_float(value),
    });
    *entry_index += 1;
}

fn parse_vendor_float(value: &str) -> Option<f64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_bruker_file_metadata_without_raw_payload_manifest() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        fs::write(
            input.join("SampleInfo.xml"),
            "<Sample><Name>demo</Name></Sample>",
        )
        .unwrap();
        fs::write(input.join("LCParms.txt"), "WavelengthA=254\nFlow=0.2").unwrap();
        fs::write(input.join("sample.u2"), [1u8, 2, 3, 4]).unwrap();

        let conn = Connection::open(input.join("analysis.tsf")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE GlobalMetadata (Key TEXT PRIMARY KEY, Value TEXT);
            INSERT INTO GlobalMetadata VALUES ('SchemaType', 'TSF');
            INSERT INTO GlobalMetadata VALUES ('MzAcqRangeLower', '100.0');
            ",
        )
        .unwrap();
        drop(conn);

        let bundle = VendorMetadataBundle::collect(&input).unwrap();
        assert!(bundle.rows().iter().any(|row| {
            row.category == "global_metadata"
                && row.label == "MzAcqRangeLower"
                && row.value_float == Some(100.0)
        }));
        assert!(
            bundle.rows().iter().any(|row| {
                row.category == "lc_parameters" && row.label.ends_with("WavelengthA")
            })
        );
        assert!(bundle.rows().iter().any(|row| {
            row.category == "vendor_file"
                && row.label == "detector_file:path"
                && row.value == "sample.u2"
        }));
        assert!(
            !bundle
                .rows()
                .iter()
                .any(|row| row.category == "vendor_text" && row.label == "sample.u2")
        );
        assert!(
            !bundle
                .rows()
                .iter()
                .any(|row| row.label.ends_with(":archive_name"))
        );

        let parquet = bundle.vendor_file_metadata_parquet().unwrap();
        assert!(parquet.starts_with(b"PAR1"));
    }

    #[test]
    fn inventories_detector_files_without_payload_copy() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("sample.u2"), [1u8, 2, 3, 4]).unwrap();

        let bundle = VendorMetadataBundle::collect(&input).unwrap();
        assert!(
            bundle
                .rows()
                .iter()
                .any(|row| row.value.ends_with("sample.u2"))
        );
        assert!(
            !bundle
                .rows()
                .iter()
                .any(|row| row.label.ends_with(":archive_name"))
        );
    }

    #[test]
    fn does_not_open_baf_as_sqlite_metadata() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("analysis.baf"), [0u8, 1, 2, 3]).unwrap();
        fs::write(
            input.join("SampleInfo.xml"),
            "<Sample><Name>baf</Name></Sample>",
        )
        .unwrap();

        let bundle = VendorMetadataBundle::collect(&input).unwrap();
        assert!(bundle.rows().iter().any(|row| {
            row.category == "source" && row.label == "analysis_baf" && row.value == "analysis.baf"
        }));
        assert!(bundle.rows().iter().any(|row| {
            row.category == "sample_info" && row.value.contains("<Name>baf</Name>")
        }));
    }

    #[test]
    fn corrupt_sqlite_degrades_facet_without_aborting() {
        // REQ-02: a per-facet read failure (here a non-SQLite analysis.tsf) must
        // degrade to a vendor_warning row and let other facets through, never
        // abort the whole collection.
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("broken.d");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("analysis.tsf"), b"not a sqlite database").unwrap();
        fs::write(
            input.join("SampleInfo.xml"),
            "<Sample><Name>ok</Name></Sample>",
        )
        .unwrap();

        // Must NOT return Err — collection degrades rather than aborting.
        let bundle = VendorMetadataBundle::collect(&input).unwrap();
        assert!(
            bundle
                .rows()
                .iter()
                .any(|row| row.category == "vendor_warning" && row.label == "sqlite_metadata"),
            "expected a vendor_warning row for the failed SQLite facet"
        );
        // The healthy facet still made it in.
        assert!(
            bundle.rows().iter().any(|row| {
                row.category == "sample_info" && row.value.contains("<Name>ok</Name>")
            })
        );
    }
}
