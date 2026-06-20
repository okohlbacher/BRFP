use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::baf::{BafOpenOptions, BafReader, BafRunSummary};
use crate::pipeline::{BrfpError, BrfpResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BrukerFormat {
    Baf,
    Tdf,
    Tsf,
}

impl BrukerFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baf => "BAF",
            Self::Tdf => "TDF",
            Self::Tsf => "TSF",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunInspection {
    pub input_path: PathBuf,
    pub format: BrukerFormat,
    pub analysis_database: PathBuf,
    pub analysis_database_size_bytes: u64,
    pub binary_file: Option<PathBuf>,
    pub binary_file_size_bytes: Option<u64>,
    pub tables: Vec<String>,
    pub global_metadata: BTreeMap<String, String>,
    pub frames: Option<FrameSummary>,
    pub baf: Option<BafRunSummary>,
    pub warnings: Vec<String>,
}

impl RunInspection {
    pub fn to_text(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("Input: {}", self.input_path.display()));
        lines.push(format!("Format: {}", self.format.as_str()));
        lines.push(format!(
            "Analysis DB: {} ({})",
            self.analysis_database.display(),
            format_bytes(self.analysis_database_size_bytes)
        ));

        if let Some(path) = &self.binary_file {
            let size = self.binary_file_size_bytes.unwrap_or_default();
            lines.push(format!(
                "Binary data: {} ({})",
                path.display(),
                format_bytes(size)
            ));
        } else {
            lines.push("Binary data: missing".to_string());
        }

        if let Some(frames) = &self.frames {
            lines.push(format!("Frames: {}", frames.frame_count));
            if let Some(total_peaks) = frames.total_peaks {
                lines.push(format!("Peaks: {total_peaks}"));
            }
            if let (Some(start), Some(end)) = (frames.start_time_seconds, frames.end_time_seconds) {
                lines.push(format!("Time range: {start:.3}s to {end:.3}s"));
            }
            if !frames.polarity_counts.is_empty() {
                lines.push(format!(
                    "Polarity counts: {}",
                    join_counts(&frames.polarity_counts)
                ));
            }
            if !frames.msms_type_counts.is_empty() {
                lines.push(format!(
                    "MS/MS type counts: {}",
                    join_counts(&frames.msms_type_counts)
                ));
            }
            if let Some(count) = frames.msms_frame_count {
                lines.push(format!("MS/MS metadata rows: {count}"));
            }
        }

        if let Some(baf) = &self.baf {
            lines.push(format!("Spectra: {}", baf.spectrum_count));
            lines.push(format!("MS1 spectra: {}", baf.ms1_count));
            lines.push(format!("MS2 spectra: {}", baf.ms2_count));
            lines.push(format!("BAF SQLite cache: {}", baf.sqlite_cache.display()));
            lines.push(format!(
                "BAF calibration requested: {}",
                baf.calibration_mode_requested.as_str()
            ));
            if let Some(mode) = baf.calibration_mode_used {
                lines.push(format!("BAF calibration used: {}", mode.as_str()));
            }
            if let Some(path) = &baf.library_path {
                lines.push(format!("BAF SDK library: {}", path.display()));
            }
            if !baf.polarity_counts.is_empty() {
                lines.push(format!(
                    "BAF polarity counts: {}",
                    join_usize_counts(&baf.polarity_counts)
                ));
            }
        }

        for key in [
            "SchemaType",
            "SchemaVersionMajor",
            "SchemaVersionMinor",
            "AcquisitionDateTime",
            "AcquisitionSoftware",
            "AcquisitionSoftwareVersion",
            "InstrumentName",
            "InstrumentVendor",
            "SampleName",
            "MethodName",
            "MzAcqRangeLower",
            "MzAcqRangeUpper",
            "ClosedProperly",
        ] {
            if let Some(value) = self.global_metadata.get(key) {
                lines.push(format!("{key}: {value}"));
            }
        }

        if !self.warnings.is_empty() {
            lines.push("Warnings:".to_string());
            for warning in &self.warnings {
                lines.push(format!("- {warning}"));
            }
        }

        lines.join("\n")
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FrameSummary {
    pub frame_count: u64,
    pub total_peaks: Option<u64>,
    pub start_time_seconds: Option<f64>,
    pub end_time_seconds: Option<f64>,
    pub polarity_counts: BTreeMap<String, u64>,
    pub msms_type_counts: BTreeMap<String, u64>,
    pub msms_frame_count: Option<u64>,
}

pub fn inspect_bruker_run(path: impl AsRef<Path>) -> BrfpResult<RunInspection> {
    inspect_bruker_run_with_baf_options(path, None)
}

pub fn inspect_bruker_run_with_baf_options(
    path: impl AsRef<Path>,
    baf_options: Option<BafOpenOptions>,
) -> BrfpResult<RunInspection> {
    let input_path = path.as_ref();
    if !input_path.is_dir() {
        return Err(BrfpError::InvalidInput(format!(
            "expected a Bruker .d directory, got {}",
            input_path.display()
        )));
    }

    let tdf = input_path.join("analysis.tdf");
    let tsf = input_path.join("analysis.tsf");
    let baf = input_path.join("analysis.baf");
    let (format, database, binary_file) = match (tdf.exists(), tsf.exists(), baf.exists()) {
        (true, false, false) => (
            BrukerFormat::Tdf,
            tdf,
            Some(input_path.join("analysis.tdf_bin")),
        ),
        (false, true, false) => (
            BrukerFormat::Tsf,
            tsf,
            Some(input_path.join("analysis.tsf_bin")),
        ),
        (false, false, true) => return inspect_baf_run(input_path, baf_options),
        (true, true, _) | (true, _, true) | (_, true, true) => {
            return Err(BrfpError::InvalidInput(format!(
                "{} contains multiple Bruker analysis payloads",
                input_path.display()
            )));
        }
        (false, false, false) => {
            return Err(BrfpError::InvalidInput(format!(
                "{} contains neither analysis.tdf, analysis.tsf, nor analysis.baf",
                input_path.display()
            )));
        }
    };

    let database_size = fs::metadata(&database)?.len();
    let conn = open_sqlite_readonly(&database)?;
    let tables = read_table_names(&conn)?;
    let global_metadata = read_global_metadata(&conn)?;
    let mut warnings = Vec::new();
    verify_schema_type(format, &global_metadata, &mut warnings);
    let frames = if has_table(&conn, "Frames")? {
        Some(read_frame_summary(&conn, format)?)
    } else {
        None
    };

    if let Some(value) = global_metadata.get("ClosedProperly") {
        if value != "1" {
            warnings.push(format!("raw acquisition ClosedProperly is {value}"));
        }
    } else {
        warnings.push("GlobalMetadata.ClosedProperly is missing".to_string());
    }

    let (binary_file, binary_file_size_bytes) = match binary_file {
        Some(path) if path.exists() => {
            let size = fs::metadata(&path)?.len();
            (Some(path), Some(size))
        }
        Some(path) => {
            warnings.push(format!("missing binary data file {}", path.display()));
            (None, None)
        }
        None => (None, None),
    };

    Ok(RunInspection {
        input_path: input_path.to_path_buf(),
        format,
        analysis_database: database,
        analysis_database_size_bytes: database_size,
        binary_file,
        binary_file_size_bytes,
        tables,
        global_metadata,
        frames,
        baf: None,
        warnings,
    })
}

fn inspect_baf_run(
    input_path: &Path,
    baf_options: Option<BafOpenOptions>,
) -> BrfpResult<RunInspection> {
    let summary = if let Some(options) = baf_options {
        let reader = BafReader::open(input_path, options)?;
        reader.summary().clone()
    } else {
        BafReader::inspect_existing_cache(input_path)?
    };
    let analysis_baf = input_path.join("analysis.baf");
    let binary_file_size_bytes = fs::metadata(&analysis_baf).ok().map(|meta| meta.len());
    let analysis_database_size_bytes = fs::metadata(&summary.sqlite_cache)
        .ok()
        .map(|meta| meta.len())
        .unwrap_or_default();
    let tables = if summary.sqlite_cache.is_file() {
        let conn = open_sqlite_readonly(&summary.sqlite_cache)?;
        read_table_names(&conn)?
    } else {
        Vec::new()
    };
    let warnings = summary.warnings.clone();

    Ok(RunInspection {
        input_path: input_path.to_path_buf(),
        format: BrukerFormat::Baf,
        analysis_database: summary.sqlite_cache.clone(),
        analysis_database_size_bytes,
        binary_file: Some(analysis_baf),
        binary_file_size_bytes,
        tables,
        global_metadata: summary.properties.clone(),
        frames: None,
        baf: Some(summary),
        warnings,
    })
}

fn open_sqlite_readonly(path: &Path) -> BrfpResult<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(BrfpError::Sqlite)
}

fn read_table_names(conn: &Connection) -> BrfpResult<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view') ORDER BY name")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(BrfpError::Sqlite)
}

fn read_global_metadata(conn: &Connection) -> BrfpResult<BTreeMap<String, String>> {
    if !has_table(conn, "GlobalMetadata")? {
        return Ok(BTreeMap::new());
    }

    let mut stmt = conn.prepare("SELECT Key, Value FROM GlobalMetadata ORDER BY Key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;

    let mut metadata = BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        metadata.insert(key, value.unwrap_or_default());
    }
    Ok(metadata)
}

fn read_frame_summary(conn: &Connection, format: BrukerFormat) -> BrfpResult<FrameSummary> {
    let (frame_count, start_time_seconds, end_time_seconds, total_peaks): (
        i64,
        Option<f64>,
        Option<f64>,
        Option<i64>,
    ) = conn.query_row(
        "SELECT COUNT(*), MIN(Time), MAX(Time), SUM(NumPeaks) FROM Frames",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;

    Ok(FrameSummary {
        frame_count: frame_count.max(0) as u64,
        total_peaks: total_peaks.map(|value| value.max(0) as u64),
        start_time_seconds,
        end_time_seconds,
        polarity_counts: read_counts(
            conn,
            "SELECT Polarity, COUNT(*) FROM Frames GROUP BY Polarity",
        )?,
        msms_type_counts: read_counts(
            conn,
            "SELECT CAST(MsMsType AS TEXT), COUNT(*) FROM Frames GROUP BY MsMsType",
        )?,
        msms_frame_count: read_msms_metadata_count(conn, format)?,
    })
}

fn read_msms_metadata_count(conn: &Connection, format: BrukerFormat) -> BrfpResult<Option<u64>> {
    let tables = match format {
        BrukerFormat::Baf => return Ok(None),
        BrukerFormat::Tsf => ["FrameMsMsInfo"].as_slice(),
        BrukerFormat::Tdf => ["PasefFrameMsMsInfo", "DiaFrameMsMsWindows"].as_slice(),
    };

    let mut total = 0u64;
    let mut found = false;
    for table in tables {
        if has_table(conn, table)? {
            found = true;
            total += count_known_table(conn, table)?;
        }
    }

    Ok(found.then_some(total))
}

fn read_counts(conn: &Connection, sql: &str) -> BrfpResult<BTreeMap<String, u64>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        let key: Option<String> = row.get(0)?;
        let count: i64 = row.get(1)?;
        Ok((
            key.unwrap_or_else(|| "<null>".to_string()),
            count.max(0) as u64,
        ))
    })?;

    let mut counts = BTreeMap::new();
    for row in rows {
        let (key, count) = row?;
        counts.insert(key, count);
    }
    Ok(counts)
}

fn has_table(conn: &Connection, table: &str) -> BrfpResult<bool> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1 LIMIT 1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn count_known_table(conn: &Connection, table: &str) -> BrfpResult<u64> {
    if !is_safe_sql_identifier(table) {
        return Err(BrfpError::InvalidInput(format!(
            "unsafe SQLite identifier {table:?}"
        )));
    }
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count.max(0) as u64)
}

fn verify_schema_type(
    format: BrukerFormat,
    metadata: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) {
    match metadata.get("SchemaType") {
        Some(schema_type) if schema_type.eq_ignore_ascii_case(format.as_str()) => {}
        Some(schema_type) => warnings.push(format!(
            "GlobalMetadata.SchemaType is {schema_type}, but file layout detected {}",
            format.as_str()
        )),
        None => warnings.push("GlobalMetadata.SchemaType is missing".to_string()),
    }
}

fn is_safe_sql_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn format_bytes(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.2} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.2} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.2} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn join_counts(counts: &BTreeMap<String, u64>) -> String {
    counts
        .iter()
        .map(|(key, count)| format!("{key}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn join_usize_counts(counts: &BTreeMap<String, usize>) -> String {
    counts
        .iter()
        .map(|(key, count)| format!("{key}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspects_minimal_tsf_directory() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("analysis.tsf_bin"), []).unwrap();

        let conn = Connection::open(input.join("analysis.tsf")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE GlobalMetadata (Key TEXT PRIMARY KEY, Value TEXT);
            CREATE TABLE Frames (
                Id INTEGER PRIMARY KEY,
                Time REAL NOT NULL,
                Polarity CHAR(1) NOT NULL,
                ScanMode INTEGER NOT NULL,
                MsMsType INTEGER NOT NULL,
                TimsId INTEGER,
                MaxIntensity INTEGER NOT NULL,
                SummedIntensities INTEGER NOT NULL,
                NumPeaks INTEGER,
                MzCalibration INTEGER NOT NULL,
                T1 REAL NOT NULL,
                T2 REAL NOT NULL,
                PropertyGroup INTEGER
            );
            CREATE TABLE FrameMsMsInfo (
                Frame INTEGER PRIMARY KEY,
                Parent INTEGER,
                TriggerMass REAL NOT NULL,
                IsolationWidth REAL NOT NULL,
                PrecursorCharge INTEGER,
                CollisionEnergy REAL NOT NULL
            );
            INSERT INTO GlobalMetadata VALUES ('SchemaType', 'TSF');
            INSERT INTO GlobalMetadata VALUES ('ClosedProperly', '1');
            INSERT INTO Frames VALUES (1, 0.1, '+', 0, 0, NULL, 10, 100, 3, 1, 0.0, 0.0, NULL);
            INSERT INTO Frames VALUES (2, 0.2, '+', 0, 2, NULL, 20, 200, 4, 1, 0.0, 0.0, NULL);
            INSERT INTO FrameMsMsInfo VALUES (2, 1, 445.1, 1.0, 2, 35.0);
            ",
        )
        .unwrap();
        drop(conn);

        let inspection = inspect_bruker_run(&input).unwrap();
        assert_eq!(inspection.format, BrukerFormat::Tsf);
        let frames = inspection.frames.unwrap();
        assert_eq!(frames.frame_count, 2);
        assert_eq!(frames.total_peaks, Some(7));
        assert_eq!(frames.msms_frame_count, Some(1));
    }

    #[test]
    fn detects_schema_type_mismatch() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("analysis.tsf_bin"), []).unwrap();

        let conn = Connection::open(input.join("analysis.tsf")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE GlobalMetadata (Key TEXT PRIMARY KEY, Value TEXT);
            CREATE TABLE Frames (
                Id INTEGER PRIMARY KEY,
                Time REAL NOT NULL,
                Polarity CHAR(1) NOT NULL,
                ScanMode INTEGER NOT NULL,
                MsMsType INTEGER NOT NULL,
                TimsId INTEGER,
                MaxIntensity INTEGER NOT NULL,
                SummedIntensities INTEGER NOT NULL,
                NumPeaks INTEGER,
                MzCalibration INTEGER NOT NULL,
                T1 REAL NOT NULL,
                T2 REAL NOT NULL,
                PropertyGroup INTEGER
            );
            INSERT INTO GlobalMetadata VALUES ('SchemaType', 'TDF');
            INSERT INTO GlobalMetadata VALUES ('ClosedProperly', '1');
            INSERT INTO Frames VALUES (1, 0.1, '+', 0, 0, NULL, 10, 100, 3, 1, 0.0, 0.0, NULL);
            ",
        )
        .unwrap();
        drop(conn);

        let inspection = inspect_bruker_run(&input).unwrap();
        assert!(inspection.warnings.iter().any(|w| w.contains("SchemaType")));
    }

    #[test]
    fn inspects_baf_with_existing_sqlite_cache() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("analysis.baf"), []).unwrap();

        let conn = Connection::open(input.join("analysis.sqlite")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE Properties (Key TEXT PRIMARY KEY, Value TEXT);
            CREATE TABLE AcquisitionKeys (Id INTEGER PRIMARY KEY, MsLevel INTEGER, Polarity INTEGER);
            CREATE TABLE Spectra (
                Id INTEGER PRIMARY KEY,
                Rt REAL,
                LineMzId INTEGER,
                LineIntensityId INTEGER,
                ProfileMzId INTEGER,
                ProfileIntensityId INTEGER,
                AcquisitionKey INTEGER
            );
            INSERT INTO Properties VALUES ('SchemaType', 'BAF');
            INSERT INTO Properties VALUES ('InstrumentVendor', 'Bruker');
            INSERT INTO AcquisitionKeys VALUES (1, 0, 0);
            INSERT INTO AcquisitionKeys VALUES (2, 0, 1);
            INSERT INTO Spectra VALUES (1, 0.1, 10, 11, NULL, NULL, 1);
            INSERT INTO Spectra VALUES (2, 0.2, 12, 13, NULL, NULL, 2);
            ",
        )
        .unwrap();
        drop(conn);

        let inspection = inspect_bruker_run(&input).unwrap();
        assert_eq!(inspection.format, BrukerFormat::Baf);
        let baf = inspection.baf.unwrap();
        assert_eq!(baf.spectrum_count, 2);
        assert_eq!(baf.ms1_count, 2);
        assert_eq!(baf.polarity_counts.get("positive"), Some(&1));
        assert_eq!(baf.polarity_counts.get("negative"), Some(&1));
    }

    #[test]
    fn inspects_private_tsf_fixtures() {
        let Some(root) = std::env::var_os("BRFP_TEST_PRIVATE_DATA").map(PathBuf::from) else {
            return;
        };

        let pos = inspect_bruker_run(root.join("timsTOF_autoMSMS_Urine_6min_pos.d")).unwrap();
        assert_eq!(pos.format, BrukerFormat::Tsf);
        let pos_frames = pos.frames.unwrap();
        assert_eq!(pos_frames.frame_count, 4819);
        assert_eq!(pos_frames.total_peaks, Some(6_726_191));
        assert_eq!(pos_frames.msms_frame_count, Some(3465));

        let neg = inspect_bruker_run(root.join("timsTOF_autoMSMS_Urine_6min_neg.d")).unwrap();
        assert_eq!(neg.format, BrukerFormat::Tsf);
        let neg_frames = neg.frames.unwrap();
        assert_eq!(neg_frames.frame_count, 4854);
        assert_eq!(neg_frames.total_peaks, Some(7_701_301));
        assert_eq!(neg_frames.msms_frame_count, Some(3486));
    }
}
