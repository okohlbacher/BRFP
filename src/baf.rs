use std::{
    env,
    ffi::{CString, c_char, c_int},
    path::{Path, PathBuf},
    sync::Arc,
};

use libloading::Library;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use crate::pipeline::{BrfpError, BrfpResult};
use crate::vendor_metadata::VendorScanRow;

const MAX_BAF_ARRAY_ELEMENTS: u64 = 100_000_000;
/// Upper bound on the SQLite-cache path buffer the SDK may request (a filesystem
/// path is far below this); guards against a buggy/hostile library returning a
/// huge size that would exhaust memory.
const MAX_BAF_PATH_BUFFER_BYTES: u32 = 1 << 20; // 1 MiB
/// Upper bound on the SDK error-message buffer; the error path clamps rather than
/// erroring so reporting cannot itself trigger a huge allocation.
const MAX_BAF_ERROR_BUFFER_BYTES: usize = 64 * 1024;

type GetSqliteCacheFilename = unsafe extern "C" fn(*mut c_char, u32, *const c_char, c_int) -> u32;
type ArrayOpenStorage = unsafe extern "C" fn(c_int, *const c_char) -> u64;
type ArrayCloseStorage = unsafe extern "C" fn(u64);
type GetLastErrorString = unsafe extern "C" fn(*mut c_char, u32) -> u32;
type ArrayGetNumElements = unsafe extern "C" fn(u64, u64, *mut u64) -> c_int;
type ArrayReadDouble = unsafe extern "C" fn(u64, u64, *mut f64) -> c_int;

#[derive(Clone)]
struct Baf2SqlApi {
    _library: Arc<Library>,
    get_sqlite_cache_filename: GetSqliteCacheFilename,
    array_open_storage: ArrayOpenStorage,
    array_close_storage: ArrayCloseStorage,
    get_last_error_string: GetLastErrorString,
    array_get_num_elements: ArrayGetNumElements,
    array_read_double: ArrayReadDouble,
    library_path: PathBuf,
}

impl Baf2SqlApi {
    fn load(options: &BafOpenOptions) -> BrfpResult<Self> {
        let library_path = discover_baf2sql_library(options)?;
        // SAFETY: The path is user-controlled but loaded as a dynamic library only
        // after discovery. Symbols are looked up immediately and copied as C
        // function pointers while the Library is kept alive by Arc.
        let library = Arc::new(unsafe { Library::new(&library_path) }.map_err(|error| {
            BrfpError::Reader(format!(
                "failed to load BAF SDK library {}: {error}",
                library_path.display()
            ))
        })?);

        // SAFETY: Symbol names and signatures match the public baf2sql_c C API
        // used by Bruker examples and tdf2mzml. If a vendor library is
        // incompatible, symbol lookup fails before any calls are made.
        unsafe {
            let get_sqlite_cache_filename = *library
                .get::<GetSqliteCacheFilename>(b"baf2sql_get_sqlite_cache_filename_v2\0")
                .map_err(|error| missing_symbol(&library_path, error))?;
            let array_open_storage = *library
                .get::<ArrayOpenStorage>(b"baf2sql_array_open_storage\0")
                .map_err(|error| missing_symbol(&library_path, error))?;
            let array_close_storage = *library
                .get::<ArrayCloseStorage>(b"baf2sql_array_close_storage\0")
                .map_err(|error| missing_symbol(&library_path, error))?;
            let get_last_error_string = *library
                .get::<GetLastErrorString>(b"baf2sql_get_last_error_string\0")
                .map_err(|error| missing_symbol(&library_path, error))?;
            let array_get_num_elements = *library
                .get::<ArrayGetNumElements>(b"baf2sql_array_get_num_elements\0")
                .map_err(|error| missing_symbol(&library_path, error))?;
            let array_read_double = *library
                .get::<ArrayReadDouble>(b"baf2sql_array_read_double\0")
                .map_err(|error| missing_symbol(&library_path, error))?;

            Ok(Self {
                _library: library,
                get_sqlite_cache_filename,
                array_open_storage,
                array_close_storage,
                get_last_error_string,
                array_get_num_elements,
                array_read_double,
                library_path,
            })
        }
    }

    fn sqlite_cache_path(&self, baf_file: &Path) -> BrfpResult<PathBuf> {
        let baf_cstring = path_to_cstring(baf_file)?;
        // SAFETY: Passing null buffer follows the SDK two-call pattern. The
        // baf path CString remains alive for the duration of the call.
        let required = unsafe {
            (self.get_sqlite_cache_filename)(std::ptr::null_mut(), 0, baf_cstring.as_ptr(), 0)
        };
        if required == 0 {
            return Err(BrfpError::Reader(format!(
                "baf2sql_get_sqlite_cache_filename_v2 failed: {}",
                self.last_error()
            )));
        }
        if required > MAX_BAF_PATH_BUFFER_BYTES {
            return Err(BrfpError::Reader(format!(
                "baf2sql_get_sqlite_cache_filename_v2 requested an implausibly large path buffer ({required} bytes, limit {MAX_BAF_PATH_BUFFER_BYTES}); refusing to allocate"
            )));
        }

        let mut buffer = vec![0u8; required as usize];
        // SAFETY: Buffer has the size requested by the SDK and is writable.
        let written = unsafe {
            (self.get_sqlite_cache_filename)(
                buffer.as_mut_ptr().cast::<c_char>(),
                required,
                baf_cstring.as_ptr(),
                0,
            )
        };
        if written == 0 {
            return Err(BrfpError::Reader(format!(
                "baf2sql_get_sqlite_cache_filename_v2 failed: {}",
                self.last_error()
            )));
        }

        let nul = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        let path = String::from_utf8_lossy(&buffer[..nul]).to_string();
        Ok(PathBuf::from(path))
    }

    fn open_storage(
        &self,
        baf_file: &Path,
        calibration: BafCalibrationMode,
    ) -> BrfpResult<BafStorage> {
        let baf_cstring = path_to_cstring(baf_file)?;
        let raw_flag = match calibration {
            BafCalibrationMode::Raw => 1,
            BafCalibrationMode::Vendor | BafCalibrationMode::Auto => 0,
        };
        // SAFETY: The C string remains alive for the call. A zero handle is
        // treated as an SDK error and not wrapped.
        let handle = unsafe { (self.array_open_storage)(raw_flag, baf_cstring.as_ptr()) };
        if handle == 0 {
            return Err(BrfpError::Reader(format!(
                "baf2sql_array_open_storage failed for {} calibration: {}",
                calibration.as_str(),
                self.last_error()
            )));
        }
        Ok(BafStorage {
            api: self.clone(),
            handle,
            calibration_used: calibration,
        })
    }

    fn last_error(&self) -> String {
        // SAFETY: Null/0 call follows SDK two-call pattern.
        let required = (unsafe { (self.get_last_error_string)(std::ptr::null_mut(), 0) }.max(1)
            as usize)
            .min(MAX_BAF_ERROR_BUFFER_BYTES);
        let mut buffer = vec![0u8; required];
        // SAFETY: Buffer is valid and writable for the requested size.
        unsafe {
            (self.get_last_error_string)(buffer.as_mut_ptr().cast::<c_char>(), required as u32);
        }
        let nul = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..nul]).to_string()
    }
}

fn missing_symbol(path: &Path, error: libloading::Error) -> BrfpError {
    BrfpError::Reader(format!(
        "BAF SDK library {} is missing a required symbol: {error}",
        path.display()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BafCalibrationMode {
    #[default]
    Auto,
    Vendor,
    Raw,
}

impl BafCalibrationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Vendor => "vendor",
            Self::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BafProfileMissingMode {
    #[default]
    Auto,
    Line,
    Fail,
}

impl BafProfileMissingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Line => "line",
            Self::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct BafOpenOptions {
    pub sdk_lib_dir: Option<PathBuf>,
    pub baf2sql_lib: Option<PathBuf>,
    pub calibration_mode: BafCalibrationMode,
}

#[derive(Debug, Clone, Serialize)]
pub struct BafRunSummary {
    pub spectrum_count: usize,
    pub ms1_count: usize,
    pub ms2_count: usize,
    pub polarity_counts: std::collections::BTreeMap<String, usize>,
    pub properties: std::collections::BTreeMap<String, String>,
    pub sqlite_cache: PathBuf,
    pub library_path: Option<PathBuf>,
    pub calibration_mode_requested: BafCalibrationMode,
    pub calibration_mode_used: Option<BafCalibrationMode>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BafSpectrum {
    pub index: usize,
    pub id: i64,
    pub retention_time_seconds: f64,
    pub ms_level: u8,
    pub polarity: BafPolarity,
    pub centroided: bool,
    pub mz_values: Vec<f64>,
    pub intensities: Vec<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BafPolarity {
    Positive,
    Negative,
    Unknown,
}

pub struct BafReader {
    root: PathBuf,
    baf_file: PathBuf,
    connection: Connection,
    rows: Vec<BafSpectrumRow>,
    storage: BafStorage,
    summary: BafRunSummary,
}

impl BafReader {
    pub fn open(path: impl AsRef<Path>, options: BafOpenOptions) -> BrfpResult<Self> {
        let paths = BafPaths::resolve(path.as_ref())?;
        let api = Baf2SqlApi::load(&options)?;
        let sqlite_cache = api.sqlite_cache_path(&paths.baf_file)?;
        let connection = Connection::open_with_flags(
            &sqlite_cache,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let rows = read_spectrum_rows(&connection)?;
        let mut warnings = stale_cache_warnings(&paths, &sqlite_cache);
        let storage = open_storage_with_fallback(&api, &paths.baf_file, &options, &mut warnings)?;
        let summary = summarize_baf(
            &connection,
            &rows,
            &sqlite_cache,
            Some(api.library_path.clone()),
            options.calibration_mode,
            Some(storage.calibration_used),
            warnings.clone(),
        )?;

        Ok(Self {
            root: paths.root,
            baf_file: paths.baf_file,
            connection,
            rows,
            storage,
            summary,
        })
    }

    pub fn inspect_existing_cache(path: impl AsRef<Path>) -> BrfpResult<BafRunSummary> {
        let paths = BafPaths::resolve(path.as_ref())?;
        let sqlite_cache = paths.root.join("analysis.sqlite");
        if !sqlite_cache.is_file() {
            return Err(BrfpError::Reader(format!(
                "{} has analysis.baf but no analysis.sqlite cache; pass --sdk-lib-dir/--baf2sql-lib or run inside the Linux/Windows SDK environment",
                paths.root.display()
            )));
        }
        let connection = Connection::open_with_flags(
            &sqlite_cache,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let rows = read_spectrum_rows(&connection)?;
        let warnings = stale_cache_warnings(&paths, &sqlite_cache);
        summarize_baf(
            &connection,
            &rows,
            &sqlite_cache,
            None,
            BafCalibrationMode::Auto,
            None,
            warnings,
        )
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn baf_file(&self) -> &Path {
        &self.baf_file
    }

    pub fn summary(&self) -> &BafRunSummary {
        &self.summary
    }

    pub fn read_spectrum(
        &self,
        index: usize,
        prefer_profile: bool,
        profile_missing: BafProfileMissingMode,
    ) -> BrfpResult<BafSpectrum> {
        let row = self.rows.get(index).ok_or_else(|| {
            BrfpError::Reader(format!("BAF spectrum index {index} out of bounds"))
        })?;
        let (mz_id, intensity_id, centroided) = if prefer_profile
            && row.profile_mz_id.is_some()
            && row.profile_intensity_id.is_some()
        {
            (row.profile_mz_id, row.profile_intensity_id, false)
        } else if prefer_profile && matches!(profile_missing, BafProfileMissingMode::Fail) {
            return Err(BrfpError::Reader(format!(
                "BAF spectrum {} has no profile arrays and --profile-missing=fail was requested",
                row.id
            )));
        } else {
            (row.line_mz_id, row.line_intensity_id, true)
        };

        let ms_level = checked_baf_ms_level_to_public(row.ms_level)?;
        let Some(mz_id) = mz_id else {
            return Ok(row.empty_spectrum(index, centroided, ms_level));
        };
        let Some(intensity_id) = intensity_id else {
            return Ok(row.empty_spectrum(index, centroided, ms_level));
        };

        let mz_values = self.storage.read_array_double(mz_id)?;
        let intensities = self.storage.read_array_double(intensity_id)?;
        if mz_values.len() != intensities.len() {
            return Err(BrfpError::Reader(format!(
                "BAF spectrum {} m/z array length {} does not match intensity array length {}",
                row.id,
                mz_values.len(),
                intensities.len()
            )));
        }

        Ok(BafSpectrum {
            index,
            id: row.id,
            retention_time_seconds: row.retention_time_seconds,
            ms_level,
            polarity: baf_polarity(row.polarity),
            centroided,
            mz_values,
            intensities,
        })
    }

    pub fn properties(&self) -> BrfpResult<std::collections::BTreeMap<String, String>> {
        read_properties(&self.connection)
    }

    /// Verbatim per-spectrum vendor codes for the row at `index`, keyed to the
    /// written `ordinal` and `scan={id}` native id (REQ-04, BAF side). Captures
    /// the raw cache codes behind the CV-mapped values (0-based MsLevel, raw
    /// polarity code) so the original vendor values are preserved.
    ///
    /// ponytail: just the two raw acquisition codes for now — BAF's per-spectrum
    /// vendor surface is thin. Expand if a `PerSpectrumVariables`-style table is
    /// found worth forwarding.
    pub fn vendor_scan_rows_for(&self, index: usize, ordinal: u64) -> Vec<VendorScanRow> {
        let Some(row) = self.rows.get(index) else {
            return Vec::new();
        };
        let native_id = format!("scan={}", row.id);
        vec![
            VendorScanRow::new(ordinal, &native_id, "MsLevelRaw", row.ms_level.to_string()),
            VendorScanRow::new(ordinal, &native_id, "PolarityRaw", row.polarity.to_string()),
        ]
    }
}

#[derive(Debug)]
struct BafPaths {
    root: PathBuf,
    baf_file: PathBuf,
}

impl BafPaths {
    fn resolve(path: &Path) -> BrfpResult<Self> {
        if path.file_name().and_then(|value| value.to_str()) == Some("analysis.baf")
            && path.is_file()
        {
            let root = path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            return Ok(Self {
                root,
                baf_file: path.to_path_buf(),
            });
        }

        let baf_file = path.join("analysis.baf");
        if !baf_file.is_file() {
            return Err(BrfpError::InvalidInput(format!(
                "analysis.baf not found in {}",
                path.display()
            )));
        }
        Ok(Self {
            root: path.to_path_buf(),
            baf_file,
        })
    }
}

#[derive(Debug, Clone)]
struct BafSpectrumRow {
    id: i64,
    retention_time_seconds: f64,
    line_mz_id: Option<u64>,
    line_intensity_id: Option<u64>,
    profile_mz_id: Option<u64>,
    profile_intensity_id: Option<u64>,
    ms_level: i64,
    polarity: i64,
}

impl BafSpectrumRow {
    fn empty_spectrum(&self, index: usize, centroided: bool, ms_level: u8) -> BafSpectrum {
        BafSpectrum {
            index,
            id: self.id,
            retention_time_seconds: self.retention_time_seconds,
            ms_level,
            polarity: baf_polarity(self.polarity),
            centroided,
            mz_values: Vec::new(),
            intensities: Vec::new(),
        }
    }
}

struct BafStorage {
    api: Baf2SqlApi,
    handle: u64,
    calibration_used: BafCalibrationMode,
}

impl BafStorage {
    fn read_array_double(&self, array_id: u64) -> BrfpResult<Vec<f64>> {
        let mut count = 0u64;
        // SAFETY: The storage handle is valid while self is alive. The SDK
        // writes one u64 count to the provided pointer.
        let ok = unsafe {
            (self.api.array_get_num_elements)(self.handle, array_id, &mut count as *mut u64)
        };
        if ok == 0 {
            return Err(BrfpError::Reader(format!(
                "baf2sql_array_get_num_elements failed for array {array_id}: {}",
                self.api.last_error()
            )));
        }
        if count == 0 {
            return Ok(Vec::new());
        }
        if count > MAX_BAF_ARRAY_ELEMENTS {
            return Err(BrfpError::Reader(format!(
                "BAF array {array_id} reports {count} elements, exceeding safety limit {MAX_BAF_ARRAY_ELEMENTS}"
            )));
        }
        let len = usize::try_from(count).map_err(|_| {
            BrfpError::Reader(format!(
                "BAF array {array_id} has too many elements for this platform: {count}"
            ))
        })?;
        let mut values = vec![0.0f64; len];
        // SAFETY: values has len f64 elements, which matches the count returned
        // by the SDK for this array ID.
        let ok =
            unsafe { (self.api.array_read_double)(self.handle, array_id, values.as_mut_ptr()) };
        if ok == 0 {
            return Err(BrfpError::Reader(format!(
                "baf2sql_array_read_double failed for array {array_id}: {}",
                self.api.last_error()
            )));
        }
        Ok(values)
    }
}

impl Drop for BafStorage {
    fn drop(&mut self) {
        if self.handle != 0 {
            // SAFETY: The handle was returned by baf2sql_array_open_storage and
            // is closed exactly once here.
            unsafe {
                (self.api.array_close_storage)(self.handle);
            }
            self.handle = 0;
        }
    }
}

fn open_storage_with_fallback(
    api: &Baf2SqlApi,
    baf_file: &Path,
    options: &BafOpenOptions,
    warnings: &mut Vec<String>,
) -> BrfpResult<BafStorage> {
    match options.calibration_mode {
        BafCalibrationMode::Raw => api.open_storage(baf_file, BafCalibrationMode::Raw),
        BafCalibrationMode::Vendor => api.open_storage(baf_file, BafCalibrationMode::Vendor),
        BafCalibrationMode::Auto => match api.open_storage(baf_file, BafCalibrationMode::Vendor) {
            Ok(storage) => Ok(storage),
            Err(error) => {
                warnings.push(format!(
                    "calibrated BAF array access failed; falling back to raw arrays: {error}"
                ));
                api.open_storage(baf_file, BafCalibrationMode::Raw)
                    .map_err(|raw_error| {
                        BrfpError::Reader(format!(
                            "BAF array access failed in auto calibration mode; calibrated error: {error}; raw fallback error: {raw_error}"
                        ))
                    })
            }
        },
    }
}

fn read_spectrum_rows(connection: &Connection) -> BrfpResult<Vec<BafSpectrumRow>> {
    let mut stmt = connection.prepare(
        "SELECT s.Id, s.Rt, s.LineMzId, s.LineIntensityId, \
         s.ProfileMzId, s.ProfileIntensityId, ak.MsLevel, ak.Polarity \
         FROM Spectra s JOIN AcquisitionKeys ak ON s.AcquisitionKey = ak.Id \
         ORDER BY s.Id",
    )?;
    let rows = stmt.query_map([], |row| {
        let id: i64 = row.get(0)?;
        Ok(BafSpectrumRow {
            id,
            retention_time_seconds: row.get::<_, Option<f64>>(1)?.unwrap_or(0.0),
            line_mz_id: sql_array_id(row.get::<_, Option<i64>>(2)?, id, "LineMzId"),
            line_intensity_id: sql_array_id(row.get::<_, Option<i64>>(3)?, id, "LineIntensityId"),
            profile_mz_id: sql_array_id(row.get::<_, Option<i64>>(4)?, id, "ProfileMzId"),
            profile_intensity_id: sql_array_id(
                row.get::<_, Option<i64>>(5)?,
                id,
                "ProfileIntensityId",
            ),
            ms_level: row.get(6)?,
            polarity: row.get(7)?,
        })
    })?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(BrfpError::Sqlite)
}

/// Convert a SQLite array-id cell to a positive storage id.
///
/// `NULL` legitimately means "this spectrum has no array of this kind" and maps
/// to `None` silently. A *present but non-positive* id is a corruption signal in
/// the cache (storage ids are positive); we warn rather than silently dropping
/// the peak data so a stale/damaged cache cannot masquerade as an empty scan.
fn sql_array_id(value: Option<i64>, spectrum_id: i64, field: &str) -> Option<u64> {
    match value {
        None => None,
        Some(raw) => match u64::try_from(raw) {
            Ok(0) => {
                tracing::warn!(
                    spectrum_id,
                    field,
                    "BAF cache has a zero {field} array id; treating as no data"
                );
                None
            }
            Ok(id) => Some(id),
            Err(_) => {
                tracing::warn!(
                    spectrum_id,
                    field,
                    raw,
                    "BAF cache has an invalid (negative) {field} array id; peak data may be missing"
                );
                None
            }
        },
    }
}

fn summarize_baf(
    connection: &Connection,
    rows: &[BafSpectrumRow],
    sqlite_cache: &Path,
    library_path: Option<PathBuf>,
    requested: BafCalibrationMode,
    used: Option<BafCalibrationMode>,
    mut warnings: Vec<String>,
) -> BrfpResult<BafRunSummary> {
    let mut polarity_counts = std::collections::BTreeMap::new();
    let mut ms1_count = 0usize;
    let mut ms2_count = 0usize;
    for row in rows {
        *polarity_counts
            .entry(match baf_polarity(row.polarity) {
                BafPolarity::Positive => "positive".to_string(),
                BafPolarity::Negative => "negative".to_string(),
                BafPolarity::Unknown => format!("unknown({})", row.polarity),
            })
            .or_insert(0) += 1;
        match checked_baf_ms_level_to_public(row.ms_level) {
            Ok(1) => ms1_count += 1,
            Ok(2) => ms2_count += 1,
            Ok(_) => {}
            Err(error) => warnings.push(format!(
                "invalid BAF MS level for spectrum {}: {error}",
                row.id
            )),
        }
    }

    Ok(BafRunSummary {
        spectrum_count: rows.len(),
        ms1_count,
        ms2_count,
        polarity_counts,
        properties: read_properties(connection)?,
        sqlite_cache: sqlite_cache.to_path_buf(),
        library_path,
        calibration_mode_requested: requested,
        calibration_mode_used: used,
        warnings,
    })
}

fn read_properties(
    connection: &Connection,
) -> BrfpResult<std::collections::BTreeMap<String, String>> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = 'Properties' LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(std::collections::BTreeMap::new());
    }

    let mut stmt = connection.prepare("SELECT Key, Value FROM Properties ORDER BY Key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let mut properties = std::collections::BTreeMap::new();
    for row in rows {
        let (key, value) = row?;
        properties.insert(key, value.unwrap_or_default());
    }
    Ok(properties)
}

fn checked_baf_ms_level_to_public(value: i64) -> BrfpResult<u8> {
    if value < 0 {
        return Err(BrfpError::Reader(format!(
            "BAF AcquisitionKeys.MsLevel is negative ({value})"
        )));
    }
    if value == 0 {
        return Ok(1);
    }
    (value + 1).try_into().map_err(|_| {
        BrfpError::Reader(format!(
            "BAF AcquisitionKeys.MsLevel is too large ({value})"
        ))
    })
}

fn baf_polarity(value: i64) -> BafPolarity {
    match value {
        0 => BafPolarity::Positive,
        1 => BafPolarity::Negative,
        _ => BafPolarity::Unknown,
    }
}

fn stale_cache_warnings(paths: &BafPaths, sqlite_cache: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(cache_meta) = std::fs::metadata(sqlite_cache) else {
        return warnings;
    };
    let Ok(cache_modified) = cache_meta.modified() else {
        return warnings;
    };
    let sources = [
        paths.baf_file.clone(),
        paths.root.join("analysis.baf_idx"),
        paths.root.join("analysis.baf_xtr"),
    ];
    for source in &sources {
        if let Ok(source_meta) = std::fs::metadata(source)
            && let Ok(source_modified) = source_meta.modified()
            && source_modified > cache_modified
        {
            warnings.push(format!(
                "BAF SQLite cache {} is older than source file {}",
                sqlite_cache.display(),
                source.display()
            ));
        }
    }
    warnings
}

fn discover_baf2sql_library(options: &BafOpenOptions) -> BrfpResult<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = &options.baf2sql_lib {
        candidates.push(path.clone());
    }
    if let Some(path) = env::var_os("BRFP_BAF2SQL_LIB").map(PathBuf::from) {
        candidates.push(path);
    }
    if let Some(root) = &options.sdk_lib_dir {
        candidates.extend(baf_library_candidates_from_root(root));
    }
    if let Some(root) = env::var_os("BRFP_BRUKER_SDK_DIR").map(PathBuf::from) {
        candidates.extend(baf_library_candidates_from_root(&root));
    }
    if let Some(root) = env::var_os("TIMSDATA_LIB_DIR").map(PathBuf::from) {
        candidates.extend(baf_library_candidates_from_root(&root));
    }

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(BrfpError::InvalidInput(
        "BAF conversion requires libbaf2sql_c.so on Linux or baf2sql_c.dll on Windows; pass --baf2sql-lib, --sdk-lib-dir, BRFP_BAF2SQL_LIB, or BRFP_BRUKER_SDK_DIR".to_string(),
    ))
}

fn baf_library_candidates_from_root(root: &Path) -> Vec<PathBuf> {
    if root.is_file() {
        return vec![root.to_path_buf()];
    }
    let mut candidates = Vec::new();
    for name in [
        "libbaf2sql_c.so",
        "baf2sql_c.dll",
        "linux64/libbaf2sql_c.so",
        "win64/baf2sql_c.dll",
    ] {
        candidates.push(root.join(name));
    }
    if let Some(parent) = root.parent() {
        if root.file_name().and_then(|name| name.to_str()) == Some("linux64") {
            candidates.push(parent.join("linux64/libbaf2sql_c.so"));
        }
        if root.file_name().and_then(|name| name.to_str()) == Some("win64") {
            candidates.push(parent.join("win64/baf2sql_c.dll"));
        }
    }
    candidates
}

fn path_to_cstring(path: &Path) -> BrfpResult<CString> {
    CString::new(path.to_string_lossy().as_bytes()).map_err(|_| {
        BrfpError::InvalidInput(format!(
            "path contains an interior NUL byte: {}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_baf_ms_levels_from_zero_based_cache_values() {
        assert_eq!(checked_baf_ms_level_to_public(0).unwrap(), 1);
        assert_eq!(checked_baf_ms_level_to_public(1).unwrap(), 2);
        assert_eq!(checked_baf_ms_level_to_public(2).unwrap(), 3);
        assert!(checked_baf_ms_level_to_public(-1).is_err());
    }

    #[test]
    fn builds_baf_library_candidates_from_plain_directory() {
        let candidates = baf_library_candidates_from_root(Path::new("/sdk"));
        assert!(candidates.contains(&PathBuf::from("/sdk/libbaf2sql_c.so")));
        assert!(candidates.contains(&PathBuf::from("/sdk/linux64/libbaf2sql_c.so")));
        assert!(candidates.contains(&PathBuf::from("/sdk/win64/baf2sql_c.dll")));
    }

    #[test]
    fn inspects_existing_baf_sqlite_cache() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        std::fs::create_dir(&input).unwrap();
        std::fs::write(input.join("analysis.baf"), []).unwrap();
        let conn = Connection::open(input.join("analysis.sqlite")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE Properties (Key TEXT, Value TEXT);
            CREATE TABLE AcquisitionKeys (
                Id INTEGER PRIMARY KEY,
                Polarity INTEGER,
                ScanMode INTEGER,
                AcquisitionMode INTEGER,
                MsLevel INTEGER
            );
            CREATE TABLE Spectra (
                Id INTEGER PRIMARY KEY,
                Rt REAL,
                AcquisitionKey INTEGER,
                LineMzId INTEGER,
                LineIntensityId INTEGER,
                ProfileMzId INTEGER,
                ProfileIntensityId INTEGER
            );
            INSERT INTO Properties VALUES ('InstrumentName', 'micrOTOF-Q');
            INSERT INTO AcquisitionKeys VALUES (1, 0, 0, 0, 0);
            INSERT INTO Spectra VALUES (1, 0.5, 1, 10, 11, NULL, NULL);
            INSERT INTO Spectra VALUES (2, 0.8, 1, 12, 13, NULL, NULL);
            ",
        )
        .unwrap();
        drop(conn);

        let summary = BafReader::inspect_existing_cache(&input).unwrap();
        assert_eq!(summary.spectrum_count, 2);
        assert_eq!(summary.ms1_count, 2);
        assert_eq!(
            summary.properties.get("InstrumentName"),
            Some(&"micrOTOF-Q".to_string())
        );
    }
}
