use std::{
    collections::HashMap,
    io::{Cursor, Read},
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::pipeline::{BrfpError, BrfpResult};
use crate::vendor_metadata::VendorScanRow;

const TSF_CHUNK_HEADER_BYTES: usize = 8;
const TSF_BYTES_PER_PEAK: usize = 12;

#[derive(Debug, Clone, Serialize)]
pub struct TsfSpectrumPreview {
    pub index: usize,
    pub point_count: usize,
    pub mz_min: Option<f64>,
    pub mz_max: Option<f64>,
    pub base_peak_mz: Option<f64>,
    pub base_peak_intensity: Option<f64>,
    pub total_ion_current: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TsfReaderPreview {
    pub spectrum_count: usize,
    pub spectra: Vec<TsfSpectrumPreview>,
}

#[derive(Debug, Clone)]
pub struct TsfSpectrum {
    pub index: usize,
    pub frame_id: i64,
    pub retention_time_seconds: f64,
    pub polarity: TsfPolarity,
    pub ms_level: u8,
    pub mz_values: Vec<f64>,
    pub intensities: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TsfPolarity {
    Positive,
    Negative,
    Unknown,
}

#[derive(Debug, Clone)]
struct TsfFrame {
    frame_id: i64,
    retention_time_seconds: f64,
    polarity: TsfPolarity,
    ms_level: u8,
    num_peaks: usize,
    offset: usize,
}

#[derive(Debug)]
pub struct TsfLineReader {
    frames: Vec<TsfFrame>,
    bin_data: Vec<u8>,
    mz_converter: Tof2MzConverter,
    run_metadata: crate::schema::RunVendorMetadata,
}

impl TsfLineReader {
    pub fn open(path: impl AsRef<Path>) -> BrfpResult<Self> {
        let paths = TsfPaths::resolve(path.as_ref())?;
        let connection = Connection::open(&paths.tsf)?;
        ensure_line_spectra(&connection)?;
        let global_metadata = read_global_metadata(&connection)?;
        let mz_converter = Tof2MzConverter::from_metadata(&global_metadata)?;
        let run_metadata =
            crate::schema::RunVendorMetadata::from_lookup(|key| global_metadata.get(key).cloned());
        let frames = read_frames(&connection)?;
        let bin_data = std::fs::read(&paths.tsf_bin)?;

        Ok(Self {
            frames,
            bin_data,
            mz_converter,
            run_metadata,
        })
    }

    /// Run-level vendor identity extracted from `GlobalMetadata` (REQ-05).
    pub fn run_metadata(&self) -> &crate::schema::RunVendorMetadata {
        &self.run_metadata
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn read_spectrum(&self, index: usize) -> BrfpResult<TsfSpectrum> {
        let frame = self
            .frames
            .get(index)
            .ok_or_else(|| BrfpError::Reader(format!("spectrum index {index} out of bounds")))?;
        // A frame with no peaks has no (or a null) binary offset; reading a chunk
        // at the defaulted offset 0 would decode the first chunk's bytes. Emit an
        // empty spectrum instead.
        if frame.num_peaks == 0 {
            return Ok(TsfSpectrum {
                index,
                frame_id: frame.frame_id,
                retention_time_seconds: frame.retention_time_seconds,
                polarity: frame.polarity,
                ms_level: frame.ms_level,
                mz_values: Vec::new(),
                intensities: Vec::new(),
            });
        }
        let chunk = self.read_chunk(frame.offset, frame.num_peaks)?;
        let mz_values = chunk
            .tof_indices
            .into_iter()
            .map(|tof| self.mz_converter.convert(tof))
            .collect();

        Ok(TsfSpectrum {
            index,
            frame_id: frame.frame_id,
            retention_time_seconds: frame.retention_time_seconds,
            polarity: frame.polarity,
            ms_level: frame.ms_level,
            mz_values,
            intensities: chunk.intensities,
        })
    }

    fn read_chunk(&self, offset: usize, num_peaks: usize) -> BrfpResult<TsfSpectrumChunk> {
        let header = self.read_header(offset)?;
        let compressed_start = offset + TSF_CHUNK_HEADER_BYTES;
        let compressed_end = compressed_start
            .checked_add(header.compressed_len)
            .ok_or_else(|| BrfpError::Reader("TSF chunk offset overflow".to_string()))?;
        let compressed = self.bin_data.get(compressed_start..compressed_end).ok_or_else(|| {
            BrfpError::Reader(format!(
                "TSF chunk range {compressed_start}..{compressed_end} is outside analysis.tsf_bin"
            ))
        })?;

        if compressed.is_empty() {
            return Ok(TsfSpectrumChunk {
                tof_indices: Vec::new(),
                intensities: Vec::new(),
            });
        }

        let expected = num_peaks
            .checked_mul(TSF_BYTES_PER_PEAK)
            .ok_or_else(|| BrfpError::Reader("TSF peak payload size overflow".to_string()))?;
        let decode_limit = expected.checked_add(1).ok_or_else(|| {
            BrfpError::Reader("TSF decompression size limit overflow".to_string())
        })?;
        let mut decoder =
            zstd::stream::read::Decoder::new(Cursor::new(compressed)).map_err(|error| {
                BrfpError::Reader(format!("failed to initialize TSF zstd decoder: {error}"))
            })?;
        let mut decompressed = Vec::with_capacity(expected);
        decoder
            .by_ref()
            .take(decode_limit as u64)
            .read_to_end(&mut decompressed)
            .map_err(|error| {
                BrfpError::Reader(format!("failed to decompress TSF chunk: {error}"))
            })?;
        if decompressed.len() < expected {
            return Err(BrfpError::Reader(format!(
                "TSF chunk is shorter than expected: expected at least {expected} bytes, got {}",
                decompressed.len()
            )));
        }
        if decompressed.len() > expected {
            return Err(BrfpError::Reader(format!(
                "TSF chunk is longer than expected: expected {expected} bytes, got at least {}",
                decompressed.len()
            )));
        }

        let (tof_indices_bytes, intensity_bytes) = decompressed[..expected].split_at(num_peaks * 8);
        let tof_indices = tof_indices_bytes
            .chunks_exact(8)
            .map(|chunk| {
                let tof_index = f64::from_le_bytes(chunk.try_into().expect("chunk size checked"));
                if !tof_index.is_finite() || tof_index < 0.0 {
                    return Err(BrfpError::Reader(format!(
                        "invalid TSF TOF index value: {tof_index}"
                    )));
                }
                Ok(tof_index)
            })
            .collect::<BrfpResult<Vec<_>>>()?;
        let intensities = intensity_bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("chunk size checked")) as f64)
            .collect();
        Ok(TsfSpectrumChunk {
            tof_indices,
            intensities,
        })
    }

    fn read_header(&self, offset: usize) -> BrfpResult<TsfChunkHeader> {
        let header_end = offset
            .checked_add(TSF_CHUNK_HEADER_BYTES)
            .ok_or_else(|| BrfpError::Reader("TSF chunk header offset overflow".to_string()))?;
        let header_bytes = self.bin_data.get(offset..header_end).ok_or_else(|| {
            BrfpError::Reader(format!(
                "TSF chunk header range {offset}..{header_end} is outside analysis.tsf_bin"
            ))
        })?;
        let chunk_padded =
            u32::from_le_bytes(header_bytes[0..4].try_into().expect("header size checked"))
                as usize;
        let compressed_len =
            u32::from_le_bytes(header_bytes[4..8].try_into().expect("header size checked"))
                as usize;
        if chunk_padded < TSF_CHUNK_HEADER_BYTES || chunk_padded < compressed_len {
            return Err(BrfpError::Reader(format!(
                "invalid TSF chunk header at {offset}: padded={chunk_padded}, compressed={compressed_len}"
            )));
        }
        Ok(TsfChunkHeader { compressed_len })
    }
}

pub fn preview_tsf_spectra(path: impl AsRef<Path>, limit: usize) -> BrfpResult<TsfReaderPreview> {
    let reader = TsfLineReader::open(path)?;
    let spectrum_count = reader.len();
    let spectra = (0..spectrum_count.min(limit))
        .map(|index| {
            reader
                .read_spectrum(index)
                .map(|spectrum| preview_spectrum(&spectrum))
        })
        .collect::<BrfpResult<Vec<_>>>()?;

    Ok(TsfReaderPreview {
        spectrum_count,
        spectra,
    })
}

fn preview_spectrum(spectrum: &TsfSpectrum) -> TsfSpectrumPreview {
    let mz_min = spectrum.mz_values.iter().copied().reduce(f64::min);
    let mz_max = spectrum.mz_values.iter().copied().reduce(f64::max);
    let total_ion_current = spectrum.intensities.iter().sum::<f64>();

    let (base_peak_mz, base_peak_intensity) = spectrum
        .mz_values
        .iter()
        .copied()
        .zip(spectrum.intensities.iter().copied())
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(mz, intensity)| (Some(mz), Some(intensity)))
        .unwrap_or((None, None));

    TsfSpectrumPreview {
        index: spectrum.index,
        point_count: spectrum.mz_values.len(),
        mz_min,
        mz_max,
        base_peak_mz,
        base_peak_intensity,
        total_ion_current,
    }
}

#[derive(Debug, Clone)]
struct TsfPaths {
    tsf: PathBuf,
    tsf_bin: PathBuf,
}

impl TsfPaths {
    fn resolve(path: &Path) -> BrfpResult<Self> {
        let root = if path.is_dir() {
            path.to_path_buf()
        } else if path.file_name().and_then(|name| name.to_str()) == Some("analysis.tsf") {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf()
        } else {
            return Err(BrfpError::InvalidInput(format!(
                "{} is not a TSF .d directory or analysis.tsf file",
                path.display()
            )));
        };
        let tsf = root.join("analysis.tsf");
        let tsf_bin = root.join("analysis.tsf_bin");
        if !tsf.is_file() || !tsf_bin.is_file() {
            return Err(BrfpError::InvalidInput(format!(
                "{} is missing analysis.tsf or analysis.tsf_bin",
                root.display()
            )));
        }
        Ok(Self { tsf, tsf_bin })
    }
}

/// Best-effort run-level vendor identity from a Bruker analysis DB's
/// `GlobalMetadata` table (shared by TSF and TDF). Returns the default (empty)
/// metadata if the DB can't be opened or read.
pub fn read_run_metadata_from_analysis_db(db_path: &Path) -> crate::schema::RunVendorMetadata {
    let Ok(connection) = Connection::open(db_path) else {
        return crate::schema::RunVendorMetadata::default();
    };
    match read_global_metadata(&connection) {
        Ok(map) => crate::schema::RunVendorMetadata::from_lookup(|key| map.get(key).cloned()),
        Err(_) => crate::schema::RunVendorMetadata::default(),
    }
}

/// True when a TDF analysis DB is DDA-PASEF — i.e. it has a non-empty
/// `PasefFrameMsMsInfo` table. DIA-PASEF leaves that table empty (it uses
/// `DiaFrameMsMs*`), so per-precursor merging (which assumes one selected
/// precursor per fragment spectrum) must refuse it rather than mis-merge.
pub fn tdf_is_dda_pasef(db_path: &Path) -> BrfpResult<bool> {
    let connection = Connection::open(db_path).map_err(|error| {
        BrfpError::Reader(format!("failed to open {}: {error}", db_path.display()))
    })?;
    // DIA-PASEF omits the table entirely, so check existence first (a missing
    // table reads as "not DDA-PASEF" rather than a raw SQLite error).
    let table_exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='PasefFrameMsMsInfo')",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if table_exists == 0 {
        return Ok(false);
    }
    let has_rows = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM PasefFrameMsMsInfo)",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(has_rows != 0)
}

fn read_global_metadata(connection: &Connection) -> BrfpResult<HashMap<String, String>> {
    let mut statement = connection.prepare("SELECT Key, Value FROM GlobalMetadata")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect::<Result<HashMap<_, _>, _>>()
        .map_err(Into::into)
}

fn ensure_line_spectra(connection: &Connection) -> BrfpResult<()> {
    let has_line_spectra = connection
        .query_row(
            "SELECT Value FROM GlobalMetadata WHERE Key = 'HasLineSpectra'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| value.trim() == "1")
        .unwrap_or(false);
    if !has_line_spectra {
        return Err(BrfpError::Reader(
            "TSF dataset does not advertise line spectra".to_string(),
        ));
    }
    Ok(())
}

fn read_frames(connection: &Connection) -> BrfpResult<Vec<TsfFrame>> {
    let mut statement = connection
        .prepare("SELECT Id, Time, Polarity, NumPeaks, TimsId, MsMsType FROM Frames ORDER BY Id")?;
    let mut rows = statement.query([])?;
    let mut frames = Vec::new();

    while let Some(row) = rows.next()? {
        let polarity = match row.get::<_, String>(2)?.as_str() {
            "+" => TsfPolarity::Positive,
            "-" => TsfPolarity::Negative,
            _ => TsfPolarity::Unknown,
        };
        let frame_id = row.get::<_, i64>(0)?;
        let msms_type = row.get::<_, i64>(5)?;
        let (ms_level, recognized) = crate::schema::ms_level_from_msms_type(msms_type);
        if !recognized {
            tracing::warn!(
                frame_id,
                msms_type,
                "unrecognized TSF MsMsType; defaulting to MS level 2"
            );
        }
        let raw_num_peaks = row.get::<_, Option<i64>>(3)?.unwrap_or(0);
        if raw_num_peaks < 0 {
            return Err(BrfpError::Reader(format!(
                "TSF frame {frame_id} has a negative NumPeaks value: {raw_num_peaks}"
            )));
        }
        let raw_offset = row.get::<_, Option<i64>>(4)?;
        if raw_num_peaks > 0 && raw_offset.is_none() {
            return Err(BrfpError::Reader(format!(
                "TSF frame {frame_id} has peaks but no TimsId binary offset"
            )));
        }
        let raw_offset = raw_offset.unwrap_or(0);
        if raw_offset < 0 {
            return Err(BrfpError::Reader(format!(
                "TSF frame {frame_id} has a negative TimsId binary offset: {raw_offset}"
            )));
        }

        frames.push(TsfFrame {
            frame_id: row.get(0)?,
            retention_time_seconds: row.get(1)?,
            polarity,
            ms_level,
            num_peaks: raw_num_peaks as usize,
            offset: raw_offset as usize,
        });
    }

    Ok(frames)
}

/// Read verbatim per-frame vendor codes for a TSF run, ordinal-keyed to the
/// written spectra (REQ-04). Opt-in: only called when vendor metadata is
/// requested, and bounded by `limit` (the number of spectra being written) so a
/// default conversion never pays for it. `native_id` matches the spectrum
/// description (`frame={Id}`).
pub fn read_vendor_scan_rows(
    path: impl AsRef<Path>,
    limit: Option<usize>,
) -> BrfpResult<Vec<VendorScanRow>> {
    let paths = TsfPaths::resolve(path.as_ref())?;
    let connection = Connection::open(&paths.tsf)?;
    read_vendor_scan_rows_from_connection(&connection, limit)
}

fn read_vendor_scan_rows_from_connection(
    connection: &Connection,
    limit: Option<usize>,
) -> BrfpResult<Vec<VendorScanRow>> {
    // SQLite treats LIMIT -1 as unbounded.
    let sql_limit: i64 = limit.and_then(|n| i64::try_from(n).ok()).unwrap_or(-1);
    let mut statement = connection.prepare(
        "SELECT Id, ScanMode, MsMsType, MaxIntensity, SummedIntensities \
         FROM Frames ORDER BY Id LIMIT ?1",
    )?;
    let mut query = statement.query([sql_limit])?;
    let mut rows = Vec::new();
    let mut ordinal = 0u64;
    while let Some(row) = query.next()? {
        let id = row.get::<_, i64>(0)?;
        let native_id = format!("frame={id}");
        // Read each vendor field as optional so a NULL/variant column in a
        // malformed cache degrades (skips that field) rather than aborting the
        // facet — best-effort, matching the vendor-metadata philosophy.
        for (column, label) in [
            (1usize, "ScanMode"),
            (2, "MsMsType"),
            (3, "MaxIntensity"),
            (4, "SummedIntensities"),
        ] {
            if let Some(value) = row.get::<_, Option<i64>>(column)? {
                rows.push(VendorScanRow::new(
                    ordinal,
                    &native_id,
                    label,
                    value.to_string(),
                ));
            }
        }
        ordinal += 1;
    }
    Ok(rows)
}

#[derive(Debug, Clone, Copy)]
struct Tof2MzConverter {
    tof_intercept: f64,
    tof_slope: f64,
}

/// Acquisition-software string that requires the m/z boundary correction below.
const OTOF_CONTROL_SOFTWARE: &str = "Bruker otofControl";

impl Tof2MzConverter {
    /// Build the TOF-index → m/z converter for a TSF run.
    ///
    /// TSF line spectra use a two-point square-law model anchored on the
    /// acquisition m/z range and the digitizer sample count. This matches the
    /// authoritative pure-Rust reference reader (`timsrust-tsf`'s
    /// `Tof2MzConverter`); it is *not* an approximation to be replaced with a
    /// polynomial — the per-frame `MzCalibration`/`T1`/`T2` columns apply to TDF,
    /// not TSF. Like the reference, runs acquired with "Bruker otofControl" widen
    /// the m/z bounds by 5 Th on each side before fitting the model.
    fn from_metadata(metadata: &HashMap<String, String>) -> BrfpResult<Self> {
        let mut mz_min = parse_metadata_f64(metadata, "MzAcqRangeLower")?;
        let mut mz_max = parse_metadata_f64(metadata, "MzAcqRangeUpper")?;
        if metadata
            .get("AcquisitionSoftware")
            .map(|value| value.trim() == OTOF_CONTROL_SOFTWARE)
            .unwrap_or(false)
        {
            mz_min -= 5.0;
            mz_max += 5.0;
        }
        let tof_max_index = parse_metadata_f64(metadata, "DigitizerNumSamples")?;
        if !mz_min.is_finite() || mz_min <= 0.0 {
            return Err(BrfpError::Reader(format!(
                "MzAcqRangeLower must be finite and positive for TSF m/z conversion, got {mz_min}"
            )));
        }
        if !mz_max.is_finite() || mz_max < mz_min {
            return Err(BrfpError::Reader(format!(
                "MzAcqRangeUpper must be finite and >= MzAcqRangeLower for TSF m/z conversion, got {mz_max}"
            )));
        }
        if !tof_max_index.is_finite() || tof_max_index <= 0.0 {
            return Err(BrfpError::Reader(
                "DigitizerNumSamples must be positive for TSF m/z conversion".to_string(),
            ));
        }
        let tof_intercept = mz_min.sqrt();
        let tof_slope = (mz_max.sqrt() - tof_intercept) / tof_max_index;
        Ok(Self {
            tof_intercept,
            tof_slope,
        })
    }

    fn convert(&self, tof_index: f64) -> f64 {
        let mz = self.tof_intercept + self.tof_slope * tof_index;
        mz * mz
    }
}

fn parse_metadata_f64(metadata: &HashMap<String, String>, key: &str) -> BrfpResult<f64> {
    let value = metadata
        .get(key)
        .ok_or_else(|| BrfpError::Reader(format!("missing TSF metadata key {key}")))?;
    value.trim().parse::<f64>().map_err(|error| {
        BrfpError::Reader(format!("invalid TSF metadata value {key}={value}: {error}"))
    })
}

#[derive(Debug, Clone)]
struct TsfSpectrumChunk {
    tof_indices: Vec<f64>,
    intensities: Vec<f64>,
}

#[derive(Debug, Clone, Copy)]
struct TsfChunkHeader {
    compressed_len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otof_control_widens_mz_bounds_like_reference() {
        let base = || {
            HashMap::from([
                ("MzAcqRangeLower".to_string(), "100".to_string()),
                ("MzAcqRangeUpper".to_string(), "1000".to_string()),
                ("DigitizerNumSamples".to_string(), "400000".to_string()),
            ])
        };

        // Without otofControl, tof index 0 maps to the lower acquisition bound.
        let plain = Tof2MzConverter::from_metadata(&base()).unwrap();
        assert!((plain.convert(0.0) - 100.0).abs() < 1e-9);

        // With otofControl, the lower bound is widened by 5 Th (matches
        // timsrust-tsf's get_mz_bounds correction).
        let mut otof = base();
        otof.insert(
            "AcquisitionSoftware".to_string(),
            "Bruker otofControl".to_string(),
        );
        let otof = Tof2MzConverter::from_metadata(&otof).unwrap();
        assert!((otof.convert(0.0) - 95.0).abs() < 1e-9);
    }

    #[test]
    fn vendor_scan_rows_are_ordinal_keyed_and_ordered() {
        // REQ-04 keying: rows are emitted in Frames `Id` order with a dense
        // 0-based ordinal and native_id `frame={Id}`, even for sparse/unsorted
        // ids; the limit bounds the number of frames (4 vendor fields each).
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE Frames (
                    Id INTEGER PRIMARY KEY,
                    ScanMode INTEGER NOT NULL,
                    MsMsType INTEGER NOT NULL,
                    MaxIntensity INTEGER NOT NULL,
                    SummedIntensities INTEGER NOT NULL
                 );
                 INSERT INTO Frames VALUES (5, 1, 0, 100, 1000);
                 INSERT INTO Frames VALUES (9, 1, 2, 50, 500);
                 INSERT INTO Frames VALUES (12, 8, 8, 7, 70);",
            )
            .unwrap();

        let all = read_vendor_scan_rows_from_connection(&connection, None).unwrap();
        assert_eq!(all.len(), 3 * 4); // 3 frames x 4 fields
        // Frame Id 5 is ordinal 0 (Id order), with the matching native id.
        assert_eq!(all[0].ordinal, 0);
        assert_eq!(all[0].native_id, "frame=5");
        assert_eq!(all[0].label, "ScanMode");
        // Frame Id 9 is ordinal 1.
        assert_eq!(all[4].ordinal, 1);
        assert_eq!(all[4].native_id, "frame=9");
        // MsMsType value is typed.
        assert_eq!(all[5].label, "MsMsType");
        assert_eq!(all[5].value_float, Some(2.0));

        // Limit bounds the frame count: limit 2 -> first 2 frames -> 8 rows,
        // max ordinal 1.
        let limited = read_vendor_scan_rows_from_connection(&connection, Some(2)).unwrap();
        assert_eq!(limited.len(), 2 * 4);
        assert_eq!(limited.iter().map(|r| r.ordinal).max(), Some(1));
    }

    #[test]
    fn read_frames_maps_msms_type_to_ms_level() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE Frames (
                    Id INTEGER PRIMARY KEY,
                    Time REAL NOT NULL,
                    Polarity CHAR(1) NOT NULL,
                    NumPeaks INTEGER,
                    TimsId INTEGER,
                    MsMsType INTEGER NOT NULL
                 );
                 INSERT INTO Frames VALUES (1, 0.1, '+', 0, NULL, 0);
                 INSERT INTO Frames VALUES (2, 0.2, '+', 0, NULL, 2);
                 INSERT INTO Frames VALUES (3, 0.3, '-', 0, NULL, 3);
                 INSERT INTO Frames VALUES (4, 0.4, '+', 0, NULL, 8);",
            )
            .unwrap();

        let frames = read_frames(&connection).unwrap();
        let levels: Vec<u8> = frames.iter().map(|frame| frame.ms_level).collect();
        assert_eq!(levels, vec![1, 2, 3, 2]);
        assert_eq!(frames[2].polarity, TsfPolarity::Negative);
    }

    #[test]
    fn previews_private_tsf_fixture() {
        let Some(root) = std::env::var_os("BRFP_TEST_PRIVATE_DATA").map(std::path::PathBuf::from)
        else {
            return;
        };

        let preview =
            preview_tsf_spectra(root.join("timsTOF_autoMSMS_Urine_6min_pos.d"), 3).unwrap();

        assert_eq!(preview.spectrum_count, 4819);
        assert_eq!(preview.spectra.len(), 3);
        assert!(preview.spectra[0].point_count > 0);
        assert!(preview.spectra[0].mz_min.unwrap() >= 0.0);
        assert!(preview.spectra[0].mz_max.unwrap() > preview.spectra[0].mz_min.unwrap());
        assert!(preview.spectra[0].total_ion_current > 0.0);
    }
}
