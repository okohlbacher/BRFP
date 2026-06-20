use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

use std::sync::Arc;

use arrow::array::{ArrayRef, Float64Array, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use mzdata::prelude::{
    ByteArrayView, IonMobilityMeasure, IonProperties, SpectrumLike, SpectrumSource,
};
use mzdata::{
    ParamList,
    meta::{
        ComponentType, DataProcessing, FileDescription, InstrumentConfiguration,
        MSDataFileMetadata, MassSpectrometerFileFormatTerm, MassSpectrometryRun,
        NativeSpectrumIdentifierFormatTerm, ProcessingMethod, Software, SourceFile, SpectrumType,
        custom_software_name,
    },
    mzpeaks::{CentroidPeak, DeconvolutedPeak},
    params::{ControlledVocabulary, Param, Unit},
    spectrum::{
        Acquisition, ArrayType, BinaryArrayMap, BinaryDataArrayType, Chromatogram,
        ChromatogramDescription, ChromatogramType, DataArray, MultiLayerSpectrum, ScanEvent,
        ScanPolarity, ScanWindow, SignalContinuity, SpectrumDescription,
    },
};
use mzpeak_prototyping::{
    BufferContext, BufferName, MzPeakWriter,
    peak_series::{INTENSITY_ARRAY, MZ_ARRAY},
    writer::{AbstractMzPeakWriter, ArrayBuffersBuilder, CustomBuilderFromParameter},
};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, Encoding, ZstdLevel};
use parquet::file::properties::WriterProperties;
use parquet::schema::types::ColumnPath;

use crate::{
    baf::{BafOpenOptions, BafPolarity, BafProfileMissingMode, BafReader, BafSpectrum},
    pipeline::{BrfpError, BrfpResult},
    schema,
    tsf::{TsfLineReader, TsfPolarity, TsfSpectrum},
    uv::{
        DecodedUvWavelengthRun, DecodedUvWavelengthSpectrum, decode_uv_wavelength_runs,
        inspect_uv_detector_inventory,
    },
    vendor_metadata::{VendorMetadataBundle, VendorMetadataMode, VendorScanMetadata},
};

type BrfpSpectrum = MultiLayerSpectrum<CentroidPeak, DeconvolutedPeak>;

const BRFP_SOFTWARE_ID: &str = "BRFP";
const ACQUISITION_SOFTWARE_ID: &str = "acquisition_software";
const BRFP_DATA_PROCESSING_ID: &str = "BRFP_conversion";
const BRFP_INSTRUMENT_ID: u32 = 1;
const BRFP_SOURCE_FILE_ID: &str = "source_file_0";

#[derive(Debug, Clone)]
pub struct MzPeakWriteOptions {
    pub limit_spectra: Option<usize>,
    pub mask_zero_intensity_runs: bool,
    pub vendor_metadata_mode: Option<VendorMetadataMode>,
    pub vendor_metadata_json: Option<PathBuf>,
    pub include_detector_data: bool,
    pub include_chromatograms: bool,
    /// Extra provenance params recorded on the BRFP data-processing method
    /// (e.g. the BAF calibration mode actually used).
    pub processing_params: Vec<Param>,
    /// Run-level vendor identity (instrument/software/sample) used to populate
    /// mzPeak metadata instead of placeholders (REQ-05).
    pub run_metadata: schema::RunVendorMetadata,
    /// Per-spectrum verbatim vendor facet, written when vendor metadata is
    /// requested (REQ-04). Rows are ordinal-keyed to the written spectra.
    pub vendor_scan: Option<VendorScanMetadata>,
    /// TDF only: merge a PASEF precursor's per-frame MS2 events into one summed
    /// spectrum (one MS2 per unique precursor). Default false = one per frame.
    pub merge_pasef_precursors: bool,
}

impl Default for MzPeakWriteOptions {
    fn default() -> Self {
        Self {
            limit_spectra: None,
            mask_zero_intensity_runs: false,
            vendor_metadata_mode: None,
            vendor_metadata_json: None,
            include_detector_data: false,
            include_chromatograms: true,
            processing_params: Vec::new(),
            run_metadata: schema::RunVendorMetadata::default(),
            vendor_scan: None,
            merge_pasef_precursors: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MzPeakWriteReport {
    pub output: PathBuf,
    pub spectra_written: usize,
}

#[derive(Debug, Clone, Default)]
pub struct BafMzPeakWriteOptions {
    pub mzpeak: MzPeakWriteOptions,
    pub open_options: BafOpenOptions,
    pub prefer_profile: bool,
    pub profile_missing: BafProfileMissingMode,
    pub ms2_only: bool,
    pub ms_level_filter: Option<String>,
    pub start_spectrum_id: Option<i64>,
    pub end_spectrum_id: Option<i64>,
}

pub fn default_mzpeak_output_path(input: &Path) -> PathBuf {
    let mut output = input.to_path_buf();
    let file_name = input
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("output.d");
    let stem = file_name.strip_suffix(".d").unwrap_or(file_name);
    output.set_file_name(format!("{stem}.mzpeak"));
    output
}

pub fn write_tsf_to_mzpeak(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: MzPeakWriteOptions,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let reader = TsfLineReader::open(input_path)?;
    let mut options = options;
    options.run_metadata = reader.run_metadata().clone();
    record_sample_name(&mut options.processing_params, &options.run_metadata);
    let total_spectra = reader.len();
    let spectra_to_write = options
        .limit_spectra
        .unwrap_or(total_spectra)
        .min(total_spectra);

    // Per-spectrum vendor facet (REQ-04), only when vendor metadata is requested.
    // Read is opt-in and bounded by the number of spectra being written, so the
    // facet's rows align 1:1 (by ordinal) with the written spectra.
    if options.vendor_metadata_mode.is_some() {
        let rows = crate::tsf::read_vendor_scan_rows(input_path, Some(spectra_to_write))?;
        options.vendor_scan = Some(VendorScanMetadata::new(rows));
    }

    // Stream spectra straight from the reader (bounded memory) instead of
    // materializing the whole run.
    let spectra = (0..spectra_to_write).map(|index| {
        reader
            .read_spectrum(index)
            .and_then(|s| tsf_spectrum_to_mzdata(&s))
    });

    write_spectra_to_mzpeak(input_path, output_path, spectra, spectra_to_write, &options)?;

    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written: spectra_to_write,
    })
}

pub fn write_baf_to_mzpeak(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    mut options: BafMzPeakWriteOptions,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let reader = BafReader::open(input_path, options.open_options.clone())?;

    // Record the calibration mode actually used so a raw fallback (uncalibrated
    // m/z) is visible in the mzPeak provenance, not just a transient warning.
    if let Some(used) = reader.summary().calibration_mode_used {
        options.mzpeak.processing_params.push(Param::new_key_value(
            "BRFP:baf calibration mode",
            used.as_str(),
        ));
    }

    // Best-effort run-level identity from BAF Properties (REQ-05); absent keys
    // simply leave placeholders in place.
    if let Ok(properties) = reader.properties() {
        let run_metadata =
            schema::RunVendorMetadata::from_lookup(|key| properties.get(key).cloned());
        if !run_metadata.is_empty() {
            options.mzpeak.run_metadata = run_metadata;
        }
    }
    record_sample_name(
        &mut options.mzpeak.processing_params,
        &options.mzpeak.run_metadata,
    );

    let collect_vendor_scan = options.mzpeak.vendor_metadata_mode.is_some();
    let mut spectra = Vec::new();
    let mut vendor_scan_rows = Vec::new();

    for index in 0..reader.len() {
        if options
            .mzpeak
            .limit_spectra
            .is_some_and(|limit| spectra.len() >= limit)
        {
            break;
        }

        let spectrum =
            reader.read_spectrum(index, options.prefer_profile, options.profile_missing)?;
        if !baf_spectrum_selected(&spectrum, &options)? {
            continue;
        }
        // ordinal = written position (post-filter), so the facet joins spectra_*.
        if collect_vendor_scan {
            vendor_scan_rows.extend(reader.vendor_scan_rows_for(index, spectra.len() as u64));
        }
        spectra.push(baf_spectrum_to_mzdata(&spectrum)?);
    }

    if spectra.is_empty() {
        return Err(BrfpError::Writer(
            "no BAF spectra matched the selected filters".to_string(),
        ));
    }
    if collect_vendor_scan {
        options.mzpeak.vendor_scan = Some(VendorScanMetadata::new(vendor_scan_rows));
    }

    let spectra_written = spectra.len();
    write_spectra_to_mzpeak(
        input_path,
        output_path,
        spectra.into_iter().map(Ok),
        spectra_written,
        &options.mzpeak,
    )?;

    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written,
    })
}

/// Open a TDF `.d` run via mzdata's timsrust-backed reader (pure Rust, no SDK).
/// The reader yields summed/sliced spectra as `MultiLayerSpectrum<CentroidPeak,
/// DeconvolutedPeak>` with m/z, intensity, MS level, polarity, retention time,
/// and DDA/DIA-PASEF precursors already populated.
fn open_tdf(input: &Path) -> BrfpResult<mzdata::io::tdf::TDFSpectrumReader> {
    mzdata::io::tdf::TDFSpectrumReader::new(input).map_err(|error| {
        BrfpError::Reader(format!("failed to open TDF {}: {error}", input.display()))
    })
}

/// Stream the first `count` TDF spectra by index, erroring on any missing index
/// rather than silently truncating. mzdata's `iter()` yields `None` on a decode
/// failure, which `.take().map(Ok)` would turn into silent data loss; indexed
/// access surfaces the failure as an error (no-silent-data-loss).
fn tdf_spectra_by_index(
    reader: &mut mzdata::io::tdf::TDFSpectrumReader,
    count: usize,
) -> impl Iterator<Item = BrfpResult<BrfpSpectrum>> + '_ {
    (0..count).map(move |index| {
        reader.get_spectrum_by_index(index).ok_or_else(|| {
            BrfpError::Reader(format!(
                "TDF spectrum at index {index} could not be read (decode error or truncated frame)"
            ))
        })
    })
}

/// Per-peak inverse-reduced-ion-mobility array carried by TDF spectra.
const MOBILITY_ARRAY: ArrayType = ArrayType::MeanInverseReducedIonMobilityArray;
/// Cross-frame peak merge window, matching mzdata's intra-frame value (10 ppm).
const PASEF_MERGE_PPM: f64 = 10.0;

/// Stable identity of a PASEF precursor: selected-ion m/z + 1/K0 + charge, read
/// verbatim from the Precursors table so they are bit-identical across the
/// frames that re-fragment the same precursor. mzdata emits those frame-events
/// scattered (not contiguous), so grouping is global, not consecutive.
#[derive(PartialEq, Eq, Hash, Clone, Copy, PartialOrd, Ord)]
struct PrecursorKey {
    mz_bits: u64,
    mobility_bits: u64,
    charge: i32,
}

fn precursor_key(spec: &BrfpSpectrum) -> Option<PrecursorKey> {
    let ion = spec.precursor()?.ions.first()?;
    Some(PrecursorKey {
        mz_bits: ion.mz().to_bits(),
        mobility_bits: ion.ion_mobility().unwrap_or(f64::NAN).to_bits(),
        charge: ion.charge().unwrap_or(0),
    })
}

/// Pull the (m/z, intensity, 1/K0) peak triples out of one frame's raw arrays.
/// An empty frame's unstacked map has no m/z array — it yields nothing (the
/// normal writer tolerates zero-peak spectra likewise).
fn extract_peaks(spec: &BrfpSpectrum) -> BrfpResult<Vec<(f64, f32, f64)>> {
    let Some(arrays) = spec.raw_arrays() else {
        return Ok(Vec::new());
    };
    let Ok(mz) = arrays.mzs() else {
        return Ok(Vec::new());
    };
    let intensity = arrays
        .intensities()
        .map_err(|error| BrfpError::Writer(format!("PASEF merge intensity: {error}")))?;
    let mobility = arrays
        .get(&MOBILITY_ARRAY)
        .map(ByteArrayView::to_f64)
        .transpose()
        .map_err(|error| BrfpError::Writer(format!("PASEF merge mobility: {error}")))?;
    let mut peaks = Vec::with_capacity(mz.len());
    for i in 0..mz.len() {
        let mob = mobility.as_ref().map(|values| values[i]).unwrap_or(0.0);
        peaks.push((mz[i], intensity[i], mob));
    }
    Ok(peaks)
}

/// One unique precursor accumulating its scattered frame-events' peaks, the
/// description of the first frame seen (precursor/RT metadata), and the index of
/// the most recent frame-event (drives window eviction).
struct Ms2Open {
    peaks: Vec<(f64, f32, f64)>,
    description: SpectrumDescription,
    last_seen: usize,
}

/// Build the single summed MS2 spectrum for a precursor: sort the pooled peaks
/// by m/z and merge those within `PASEF_MERGE_PPM` (intensity-weighted m/z &
/// mobility, summed intensity). Always emits the m/z, intensity and ion-mobility
/// arrays so the schema matches the unmerged spectra.
fn finalize_pasef_ms2(
    mut peaks: Vec<(f64, f32, f64)>,
    description: SpectrumDescription,
) -> BrfpResult<BrfpSpectrum> {
    peaks.sort_by(|left, right| left.0.total_cmp(&right.0));

    let mut out_mz = Vec::with_capacity(peaks.len());
    let mut out_intensity = Vec::with_capacity(peaks.len());
    let mut out_mobility = Vec::with_capacity(peaks.len());
    let mut i = 0;
    while i < peaks.len() {
        // ponytail: greedy left-anchored 10ppm cluster; tighten if distinct
        // near-isobaric fragments coalesce.
        let anchor = peaks[i].0;
        let window = anchor * PASEF_MERGE_PPM * 1e-6;
        let (mut mz_w, mut mob_w, mut isum) = (0.0f64, 0.0f64, 0.0f64);
        let mut j = i;
        while j < peaks.len() && peaks[j].0 - anchor <= window {
            let weight = peaks[j].1 as f64;
            mz_w += peaks[j].0 * weight;
            mob_w += peaks[j].2 * weight;
            isum += weight;
            j += 1;
        }
        if isum > 0.0 {
            out_mz.push(mz_w / isum);
            out_mobility.push(mob_w / isum);
        } else {
            out_mz.push(anchor);
            out_mobility.push(peaks[i].2);
        }
        out_intensity.push(isum as f32);
        i = j;
    }

    let mut arrays = BinaryArrayMap::new();
    let mut mz_array =
        DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
    mz_array
        .extend(&out_mz)
        .map_err(|error| BrfpError::Writer(format!("PASEF merge build m/z: {error}")))?;
    arrays.add(mz_array);
    let mut intensity_array =
        DataArray::from_name_and_type(&ArrayType::IntensityArray, BinaryDataArrayType::Float32);
    intensity_array
        .extend(&out_intensity)
        .map_err(|error| BrfpError::Writer(format!("PASEF merge build intensity: {error}")))?;
    arrays.add(intensity_array);
    let mut mobility_array =
        DataArray::from_name_and_type(&MOBILITY_ARRAY, BinaryDataArrayType::Float64);
    mobility_array
        .extend(&out_mobility)
        .map_err(|error| BrfpError::Writer(format!("PASEF merge build mobility: {error}")))?;
    arrays.add(mobility_array);

    Ok(MultiLayerSpectrum::from_arrays_and_description(
        arrays,
        description,
    ))
}

/// Indices a precursor's frame-events may span in the reader's (frame-monotonic)
/// emission order before it is considered complete and flushed. Measured max
/// intra-precursor span on DDA-PASEF data is 92 (MS1+MS2 indices); 512 is a
/// ~5.5x margin. A precursor whose events legitimately span more than this would
/// flush early and re-open as a *visible* second spectrum (same precursor m/z),
/// never silently corrupted intensities.
const PASEF_FLUSH_WINDOW: usize = 512;
/// Hard ceiling on simultaneously-open precursors. The window keeps this in the
/// low hundreds in practice; exceeding it means the ordering assumption broke, so
/// abort with a diagnostic rather than force-flush incomplete (corrupt) data.
const MAX_OPEN_PRECURSORS: usize = 200_000;

/// Iterator adaptor: emit one summed MS2 per unique precursor (DDA-PASEF), with
/// memory bounded by the acquisition window rather than the file size.
///
/// The reader emits frame-events in frame-monotonic order, and a precursor's
/// events fall within a bounded index window. So MS2 are pooled by precursor key
/// and each precursor is flushed (merged + emitted) once the stream has advanced
/// `PASEF_FLUSH_WINDOW` indices past its last event — at which point no further
/// events can arrive. MS1 / non-precursor spectra stream through in place, so
/// merged MS2 land near their acquisition position. Only precursors active within
/// the trailing window are held (the open set, not the whole run).
struct MergePasefPrecursors<I> {
    inner: I,
    open: std::collections::HashMap<PrecursorKey, Ms2Open>,
    /// Lazy (last_seen, key) min-heap for window eviction; stale entries (whose
    /// `last_seen` no longer matches the open precursor) are skipped on pop.
    evict: std::collections::BinaryHeap<std::cmp::Reverse<(usize, PrecursorKey)>>,
    pending: std::collections::VecDeque<BrfpResult<BrfpSpectrum>>,
    index: usize,
    max_open: usize,
    done: bool,
}

impl<I: Iterator<Item = BrfpResult<BrfpSpectrum>>> MergePasefPrecursors<I> {
    fn new(inner: I) -> Self {
        Self {
            inner,
            open: std::collections::HashMap::new(),
            evict: std::collections::BinaryHeap::new(),
            pending: std::collections::VecDeque::new(),
            index: 0,
            max_open: 0,
            done: false,
        }
    }

    /// Flush precursors whose last event is more than `PASEF_FLUSH_WINDOW` behind
    /// the current index — they can receive no further events.
    fn flush_stale(&mut self) {
        while let Some(&std::cmp::Reverse((last_seen, key))) = self.evict.peek() {
            if self.index.saturating_sub(last_seen) <= PASEF_FLUSH_WINDOW {
                break;
            }
            self.evict.pop();
            // Skip stale heap entries: a newer event updated `last_seen`, or the
            // precursor was already flushed.
            if self
                .open
                .get(&key)
                .is_some_and(|open| open.last_seen == last_seen)
            {
                let open = self.open.remove(&key).unwrap();
                self.pending
                    .push_back(finalize_pasef_ms2(open.peaks, open.description));
            }
        }
    }
}

impl<I: Iterator<Item = BrfpResult<BrfpSpectrum>>> Iterator for MergePasefPrecursors<I> {
    type Item = BrfpResult<BrfpSpectrum>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Some(item);
            }
            if self.done {
                return None;
            }
            match self.inner.next() {
                None => {
                    // Drain remaining open precursors (window not yet elapsed at
                    // EOF, or partial under --limit-spectra) in acquisition order.
                    let mut rest: Vec<Ms2Open> = self.open.drain().map(|(_, v)| v).collect();
                    rest.sort_by_key(|open| open.last_seen);
                    self.pending.extend(
                        rest.into_iter()
                            .map(|o| finalize_pasef_ms2(o.peaks, o.description)),
                    );
                    tracing::info!(
                        max_open_precursors = self.max_open,
                        "PASEF merge window high-water mark"
                    );
                    self.done = true;
                }
                Some(Err(error)) => return Some(Err(error)),
                Some(Ok(spectrum)) => {
                    let index = self.index;
                    self.index += 1;
                    match precursor_key(&spectrum) {
                        None => {
                            self.flush_stale();
                            self.pending.push_back(Ok(spectrum));
                        }
                        Some(key) => {
                            let peaks = match extract_peaks(&spectrum) {
                                Ok(peaks) => peaks,
                                Err(error) => return Some(Err(error)),
                            };
                            match self.open.get_mut(&key) {
                                Some(open) => {
                                    open.peaks.extend(peaks);
                                    open.last_seen = index;
                                }
                                None => {
                                    self.open.insert(
                                        key,
                                        Ms2Open {
                                            peaks,
                                            description: spectrum.description().clone(),
                                            last_seen: index,
                                        },
                                    );
                                }
                            }
                            self.evict.push(std::cmp::Reverse((index, key)));
                            self.max_open = self.max_open.max(self.open.len());
                            if self.open.len() > MAX_OPEN_PRECURSORS {
                                return Some(Err(BrfpError::Writer(format!(
                                    "PASEF merge: {} precursors open at once (>{}) — frame ordering \
                                     assumption broke; aborting rather than emitting partial spectra",
                                    self.open.len(),
                                    MAX_OPEN_PRECURSORS
                                ))));
                            }
                            self.flush_stale();
                        }
                    }
                }
            }
        }
    }
}

/// Global TOF→m/z sqrt model (one per run): `m/z = (a + b·tof)²`, the exact model
/// timsrust uses (`Tof2MzConverter::from_boundaries`). Lets us invert each decoded
/// m/z back to its integer TOF index losslessly.
struct ImsCalibration {
    a: f64,
    b: f64,
}

fn read_ims_calibration(analysis_tdf: &Path) -> BrfpResult<ImsCalibration> {
    let connection = rusqlite::Connection::open(analysis_tdf)
        .map_err(|error| BrfpError::Reader(format!("open {}: {error}", analysis_tdf.display())))?;
    let get = |key: &str| -> BrfpResult<f64> {
        let value: String = connection
            .query_row(
                "SELECT Value FROM GlobalMetadata WHERE Key = ?1",
                [key],
                |row| row.get(0),
            )
            .map_err(|error| BrfpError::Reader(format!("GlobalMetadata {key}: {error}")))?;
        value
            .trim()
            .parse::<f64>()
            .map_err(|error| BrfpError::Reader(format!("parse {key}: {error}")))
    };
    let lower = get("MzAcqRangeLower")?;
    let upper = get("MzAcqRangeUpper")?;
    let digitizer = get("DigitizerNumSamples")?;
    let a = lower.sqrt();
    let b = (upper.sqrt() - a) / digitizer;
    Ok(ImsCalibration { a, b })
}

/// PoC `--ims-compact` writer. Stores the timsTOF signal `.d`-style as integer
/// columns — TOF index (m/z reconstructable via the global sqrt calibration),
/// integer intensity, and ion mobility — with peaks sorted mobility-major so the
/// mobility column RLE-compresses to near-free (mobility implicit). Output is one
/// Parquet facet + a `.ims.json` calibration sidecar. Lossless vs the standard f64
/// conversion (round-trip exact), but the signal needs the calibration to read as
/// m/z — opt-in, not a standard mzPeak archive.
pub fn write_tdf_to_ims_compact(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    limit_spectra: Option<usize>,
    consolidate_ms2: bool,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let calib = read_ims_calibration(&input_path.join("analysis.tdf"))?;
    let mut reader = open_tdf(input_path)?;
    let total = reader.len();
    let count = limit_spectra.unwrap_or(total).min(total);

    let schema = Arc::new(Schema::new(vec![
        Field::new("spectrum_index", DataType::UInt32, false),
        Field::new("tof", DataType::UInt32, false),
        Field::new("intensity", DataType::UInt32, false),
        Field::new("mobility", DataType::Float64, false),
    ]));
    // tof: PLAIN + zstd (beats DELTA/dict on the high-cardinality mass axis).
    // intensity + mobility: RLE-dictionary (low cardinality; mobility runs in
    // mobility-major order collapse to near-nothing). spectrum_index: delta.
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(9).map_err(|e| BrfpError::Writer(format!("zstd level: {e}")))?,
        ))
        .set_column_encoding(
            ColumnPath::from("spectrum_index"),
            Encoding::DELTA_BINARY_PACKED,
        )
        .set_column_dictionary_enabled(ColumnPath::from("spectrum_index"), false)
        .set_column_encoding(ColumnPath::from("tof"), Encoding::DELTA_BINARY_PACKED)
        .set_column_dictionary_enabled(ColumnPath::from("tof"), false)
        .set_column_dictionary_enabled(ColumnPath::from("intensity"), true)
        .set_column_dictionary_enabled(ColumnPath::from("mobility"), true)
        .build();
    let file = File::create(output_path)?;
    let mut writer = ArrowWriter::try_new(file, schema.clone(), Some(props))
        .map_err(|e| BrfpError::Writer(format!("parquet init: {e}")))?;

    let (mut si, mut tofs, mut ints, mut mobs) = (
        Vec::<u32>::new(),
        Vec::<u32>::new(),
        Vec::<u32>::new(),
        Vec::<f64>::new(),
    );
    let mut written = 0usize;
    for index in 0..count {
        let spec = reader
            .get_spectrum_by_index(index)
            .ok_or_else(|| BrfpError::Reader(format!("TDF spectrum {index} could not be read")))?;
        let mut rows: Vec<(u32, u32, f64)> = extract_peaks(&spec)?
            .into_iter()
            .map(|(mz, intensity, mobility)| {
                let tof = (((mz.sqrt() - calib.a) / calib.b).round()).max(0.0) as u32;
                (tof, intensity.max(0.0).round() as u32, mobility)
            })
            .collect();
        // MS2 IM-collapse: sum fragment intensity over the mobility dimension into
        // one flat peak per TOF bin, dropping per-peak mobility (PASEF fragments
        // co-migrate with the precursor, so mobility ≈ constant). MS1 untouched —
        // its same-TOF multiplicity is the real ion-mobility profile.
        if consolidate_ms2 && spec.ms_level() == 2 {
            rows.sort_unstable_by_key(|row| row.0);
            let mut flat: Vec<(u32, u32, f64)> = Vec::with_capacity(rows.len());
            for (tof, intensity, _) in rows {
                match flat.last_mut() {
                    Some(last) if last.0 == tof => last.1 = last.1.saturating_add(intensity),
                    _ => flat.push((tof, intensity, 0.0)),
                }
            }
            rows = flat;
        }
        // mobility-major: mobility forms long runs (RLE ≈ free), tof ascending within.
        rows.sort_by(|x, y| x.2.total_cmp(&y.2).then_with(|| x.0.cmp(&y.0)));
        for (tof, intensity, mobility) in rows {
            si.push(index as u32);
            tofs.push(tof);
            ints.push(intensity);
            mobs.push(mobility);
        }
        written += 1;
        if si.len() >= 4_000_000 {
            write_ims_batch(
                &mut writer,
                &schema,
                &mut si,
                &mut tofs,
                &mut ints,
                &mut mobs,
            )?;
        }
    }
    write_ims_batch(
        &mut writer,
        &schema,
        &mut si,
        &mut tofs,
        &mut ints,
        &mut mobs,
    )?;
    writer
        .close()
        .map_err(|e| BrfpError::Writer(format!("parquet close: {e}")))?;

    let mut sidecar = output_path.as_os_str().to_os_string();
    sidecar.push(".ims.json");
    let sidecar = PathBuf::from(sidecar);
    std::fs::write(
        &sidecar,
        format!(
            "{{\"codec\":\"ims-compact\",\"mz_from_tof\":\"(a+b*tof)^2\",\"a\":{},\"b\":{}}}\n",
            calib.a, calib.b
        ),
    )?;
    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written: written,
    })
}

fn write_ims_batch(
    writer: &mut ArrowWriter<File>,
    schema: &Arc<Schema>,
    si: &mut Vec<u32>,
    tofs: &mut Vec<u32>,
    ints: &mut Vec<u32>,
    mobs: &mut Vec<f64>,
) -> BrfpResult<()> {
    if si.is_empty() {
        return Ok(());
    }
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt32Array::from(std::mem::take(si))) as ArrayRef,
            Arc::new(UInt32Array::from(std::mem::take(tofs))),
            Arc::new(UInt32Array::from(std::mem::take(ints))),
            Arc::new(Float64Array::from(std::mem::take(mobs))),
        ],
    )
    .map_err(|e| BrfpError::Writer(format!("record batch: {e}")))?;
    writer
        .write(&batch)
        .map_err(|e| BrfpError::Writer(format!("parquet write: {e}")))?;
    Ok(())
}

/// Convert a TDF `.d` run to mzPeak by streaming the reader's spectra straight
/// into the shared writer (so precursors/ion-mobility survive untouched).
pub fn write_tdf_to_mzpeak(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: MzPeakWriteOptions,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let mut options = options;
    options.run_metadata =
        crate::tsf::read_run_metadata_from_analysis_db(&input_path.join("analysis.tdf"));
    record_sample_name(&mut options.processing_params, &options.run_metadata);

    if options.merge_pasef_precursors
        && !crate::tsf::tdf_is_dda_pasef(&input_path.join("analysis.tdf"))?
    {
        return Err(BrfpError::Writer(
            "--merge-pasef-precursors requires DDA-PASEF data (empty PasefFrameMsMsInfo: \
             likely DIA-PASEF, which has no single precursor per fragment spectrum)"
                .to_string(),
        ));
    }

    let mut reader = open_tdf(input_path)?;
    let total = reader.len();
    let count = options.limit_spectra.unwrap_or(total).min(total);
    let spectra = tdf_spectra_by_index(&mut reader, count);
    let written = if options.merge_pasef_precursors {
        // Stream: MS1 pass through, MS2 pooled by precursor and flushed once the
        // acquisition window elapses (bounded memory). `count` is only a hint
        // (over-estimate is fine); the counter reports the exact post-merge total.
        let counter = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let tally = counter.clone();
        let merged = MergePasefPrecursors::new(spectra).inspect(move |item| {
            if item.is_ok() {
                tally.set(tally.get() + 1);
            }
        });
        write_spectra_to_mzpeak(input_path, output_path, merged, count, &options)?;
        counter.get()
    } else {
        write_spectra_to_mzpeak(input_path, output_path, spectra, count, &options)?;
        count
    };

    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written: written,
    })
}

/// Convert a TDF `.d` run to mzML.
pub fn write_tdf_to_mzml(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    limit_spectra: Option<usize>,
    merge_pasef_precursors: bool,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let run_metadata =
        crate::tsf::read_run_metadata_from_analysis_db(&input_path.join("analysis.tdf"));

    if merge_pasef_precursors {
        // mzML writes the spectrumList count up front (no patch on close), but the
        // post-merge total is only known after draining the whole run, and the MS1
        // spectra are too large to buffer just to learn the count. Per-precursor
        // merge is therefore mzPeak-only; convert to mzPeak, then mzPeak->mzML.
        return Err(BrfpError::Writer(
            "--merge-pasef-precursors is supported for mzPeak output only (not mzML)".to_string(),
        ));
    }

    let mut reader = open_tdf(input_path)?;
    let total = reader.len();
    let count = limit_spectra.unwrap_or(total).min(total);
    let spectra = tdf_spectra_by_index(&mut reader, count);
    write_spectra_to_mzml(input_path, output_path, spectra, count, &run_metadata)?;

    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written: count,
    })
}

/// Convert a TSF `.d` run to mzML, reusing the same spectrum decoding as the
/// mzPeak path and streaming into mzdata's mzML writer.
pub fn write_tsf_to_mzml(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    limit_spectra: Option<usize>,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let reader = TsfLineReader::open(input_path)?;
    let total = reader.len();
    let count = limit_spectra.unwrap_or(total).min(total);
    let spectra = (0..count).map(|i| {
        reader
            .read_spectrum(i)
            .and_then(|s| tsf_spectrum_to_mzdata(&s))
    });
    write_spectra_to_mzml(
        input_path,
        output_path,
        spectra,
        count,
        reader.run_metadata(),
    )?;
    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written: count,
    })
}

/// Convert a BAF `.d` run to mzML.
pub fn write_baf_to_mzml(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: BafMzPeakWriteOptions,
) -> BrfpResult<MzPeakWriteReport> {
    let input_path = input.as_ref();
    let output_path = output.as_ref();
    let reader = BafReader::open(input_path, options.open_options.clone())?;
    let run_metadata = reader
        .properties()
        .ok()
        .map(|p| schema::RunVendorMetadata::from_lookup(|k| p.get(k).cloned()))
        .unwrap_or_default();

    let mut spectra = Vec::new();
    for index in 0..reader.len() {
        if options
            .mzpeak
            .limit_spectra
            .is_some_and(|limit| spectra.len() >= limit)
        {
            break;
        }
        let spectrum =
            reader.read_spectrum(index, options.prefer_profile, options.profile_missing)?;
        if !baf_spectrum_selected(&spectrum, &options)? {
            continue;
        }
        spectra.push(baf_spectrum_to_mzdata(&spectrum)?);
    }
    if spectra.is_empty() {
        return Err(BrfpError::Writer(
            "no BAF spectra matched the selected filters".to_string(),
        ));
    }
    let count = spectra.len();
    write_spectra_to_mzml(
        input_path,
        output_path,
        spectra.into_iter().map(Ok),
        count,
        &run_metadata,
    )?;
    Ok(MzPeakWriteReport {
        output: output_path.to_path_buf(),
        spectra_written: count,
    })
}

fn write_spectra_to_mzml<I>(
    input: &Path,
    output: &Path,
    spectra: I,
    spectrum_count: usize,
    run_metadata: &schema::RunVendorMetadata,
) -> BrfpResult<()>
where
    I: Iterator<Item = BrfpResult<BrfpSpectrum>>,
{
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let result = (|| -> BrfpResult<()> {
        let file = File::create(output)?;
        let mut writer = mzdata::io::mzml::MzMLWriter::new(file);
        *writer.file_description_mut() = FileDescription::new(
            vec![SpectrumType::MassSpectrum.into()],
            vec![source_file_metadata(input)],
        );
        writer.softwares_mut().push(Software::new(
            BRFP_SOFTWARE_ID.to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            vec![custom_software_name("BRFP")],
        ));
        let model = match &run_metadata.instrument_name {
            Some(name) => {
                ControlledVocabulary::MS.param_val(1000031, "instrument model", name.as_str())
            }
            None => ControlledVocabulary::MS.param(1000031, "instrument model"),
        };
        // Instrument config id MUST be 0: the scan events reference instrument 0,
        // and mzML emits a dangling instrumentConfigurationRef otherwise. A valid
        // mzML componentList needs a source/analyzer/detector, so populate the
        // generic category terms (the specifics are unknown at this layer).
        let mut instrument = InstrumentConfiguration {
            id: 0,
            software_reference: BRFP_SOFTWARE_ID.to_string(),
            params: vec![model],
            ..Default::default()
        };
        instrument
            .new_component(ComponentType::IonSource)
            .params
            .push(ControlledVocabulary::MS.param(1000008, "ionization type"));
        instrument
            .new_component(ComponentType::Analyzer)
            .params
            .push(ControlledVocabulary::MS.param(1000443, "mass analyzer type"));
        instrument
            .new_component(ComponentType::Detector)
            .params
            .push(ControlledVocabulary::MS.param(1000026, "detector type"));
        writer.instrument_configurations_mut().insert(0, instrument);

        // mzML requires a non-empty dataProcessingList referenced as the default.
        let mut method = ProcessingMethod {
            order: 0,
            software_reference: BRFP_SOFTWARE_ID.to_string(),
            ..Default::default()
        };
        method
            .params
            .push(ControlledVocabulary::MS.param(1000544, "Conversion to mzML"));
        writer.data_processings_mut().push(DataProcessing {
            id: BRFP_DATA_PROCESSING_ID.to_string(),
            methods: vec![method],
        });

        if let Some(run) = writer.run_description_mut() {
            *run = MassSpectrometryRun::new(
                Some("run_0".to_string()),
                Some(BRFP_DATA_PROCESSING_ID.to_string()),
                Some(0),
                Some(BRFP_SOURCE_FILE_ID.to_string()),
                None,
            );
        }
        writer.set_spectrum_count(spectrum_count as u64);
        for (index, spectrum) in spectra.enumerate() {
            writer.write_spectrum(&spectrum?).map_err(|e| {
                BrfpError::Writer(format!("failed to write mzML spectrum {index}: {e}"))
            })?;
        }
        writer
            .close()
            .map_err(|e| BrfpError::Writer(format!("failed to finalize mzML: {e}")))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

/// Record the vendor sample name as run-level provenance when known (REQ-05),
/// so the `SampleName` from `GlobalMetadata`/`Properties` is not silently dropped.
fn record_sample_name(
    processing_params: &mut Vec<Param>,
    run_metadata: &schema::RunVendorMetadata,
) {
    if let Some(sample) = &run_metadata.sample_name {
        processing_params.push(Param::new_key_value("BRFP:sample name", sample.as_str()));
    }
}

/// Number of leading spectra buffered to infer the file-wide array schema before
/// the remaining spectra are streamed one at a time. Array types are homogeneous
/// within a Bruker backend, so a small sample is sufficient and keeps peak
/// memory bounded (sample + one live spectrum) rather than materializing the run.
const ARRAY_SAMPLE_LIMIT: usize = 16;

/// Write spectra to an mzPeak archive, deleting a partial output on failure.
///
/// `File::create` truncates the target up front and the streaming writer can fail
/// mid-run, so on any error we remove the half-written `.mzpeak` rather than leave
/// a corrupt archive behind (the "delete-on-failure / no corrupt output" rule from
/// the mzPeak4TRFR handoff).
fn write_spectra_to_mzpeak<I>(
    input: &Path,
    output: &Path,
    spectra: I,
    spectrum_count: usize,
    options: &MzPeakWriteOptions,
) -> BrfpResult<()>
where
    I: Iterator<Item = BrfpResult<BrfpSpectrum>>,
{
    let result = write_spectra_to_mzpeak_inner(input, output, spectra, spectrum_count, options);
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

/// Build the peaks-file schema for ion-mobility data: index + m/z + intensity +
/// the per-peak ion-mobility column (using the array's actual type/unit, e.g.
/// MS:1003006 mean inverse reduced ion mobility for TDF). This routes the mobility
/// array to the streamed `spectra_peaks` facet as a sliceable signal column — the
/// spec's signal-data SHOULD — instead of letting it fall into the metadata
/// `auxiliary_arrays` builder, which buffers every spectrum's mobility blob until
/// EOF and dominated peak RSS. Returns `None` when the sample has no mobility array
/// (TSF/BAF/UV), leaving the default peaks schema untouched.
fn ion_mobility_peak_schema(sample: &[BrfpSpectrum]) -> Option<ArrayBuffersBuilder> {
    let (im_type, im_unit) = sample.iter().find_map(|spectrum| {
        let arrays = spectrum.raw_arrays()?;
        arrays
            .iter()
            .find(|(array_type, _)| array_type.is_ion_mobility())
            .map(|(array_type, data)| (array_type.clone(), data.unit()))
    })?;
    let mobility_field = BufferName::new(
        BufferContext::Spectrum,
        im_type,
        BinaryDataArrayType::Float64,
    )
    .with_unit(im_unit)
    .to_field();
    Some(
        ArrayBuffersBuilder::default()
            .prefix("point")
            .with_context(BufferContext::Spectrum)
            .add_field(BufferContext::Spectrum.index_field())
            .add_field(MZ_ARRAY.to_field())
            .add_field(INTENSITY_ARRAY.to_field())
            .add_field(mobility_field),
    )
}

fn write_spectra_to_mzpeak_inner<I>(
    input: &Path,
    output: &Path,
    mut spectra: I,
    spectrum_count: usize,
    options: &MzPeakWriteOptions,
) -> BrfpResult<()>
where
    I: Iterator<Item = BrfpResult<BrfpSpectrum>>,
{
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }

    let file = File::create(output)?;
    let uv_wavelength_runs = if options.include_detector_data {
        decode_uv_wavelength_runs(input, options.limit_spectra)?
    } else {
        Vec::new()
    };
    let wavelength_spectrum_count = uv_wavelength_runs
        .iter()
        .map(|run| run.spectra.len())
        .sum::<usize>();
    let uv_chromatograms = if options.include_detector_data && options.include_chromatograms {
        uv_chromatograms_from_wavelength_runs(input, &uv_wavelength_runs)?
    } else {
        Vec::new()
    };

    // Buffer a bounded sample to infer the array schema, then stream the rest.
    let mut sample = Vec::with_capacity(ARRAY_SAMPLE_LIMIT.min(spectrum_count));
    for item in spectra.by_ref() {
        sample.push(item?);
        if sample.len() >= ARRAY_SAMPLE_LIMIT {
            break;
        }
    }

    let mut sample_stream = mzdata::io::StreamingSpectrumIterator::new(sample.iter().cloned());
    let mut builder = MzPeakWriter::<File>::builder()
        .sample_array_types_from_spectrum_stream(&mut sample_stream)
        .add_spectrum_param_field(CustomBuilderFromParameter::from_spec(
            mzdata::curie!(MS:1000294),
            "mass spectrum",
            DataType::Boolean,
        ));
    // Ion-mobility data: register the per-peak mobility array as a primary peaks-file
    // column (spec signal-data SHOULD) so it streams instead of accumulating in the
    // metadata `auxiliary_arrays` builder until EOF — the latter dominated peak RSS.
    if let Some(peak_schema) = ion_mobility_peak_schema(&sample) {
        builder = builder.store_peaks_and_profiles_apart(Some(peak_schema));
    }
    if !uv_chromatograms.is_empty() {
        builder = builder.sample_array_types_from_chromatograms(uv_chromatograms.iter().cloned());
    }
    let mut writer = builder.build(file, options.mask_zero_intensity_runs);
    configure_mzpeak_metadata(
        &mut writer,
        input,
        spectrum_count,
        wavelength_spectrum_count,
        uv_chromatograms.len(),
        &options.processing_params,
        &options.run_metadata,
    );

    // Write the buffered sample, then stream the remaining spectra one at a time.
    for spectrum in &sample {
        writer.write_spectrum(spectrum)?;
    }
    drop(sample);
    for item in spectra {
        writer.write_spectrum(&item?)?;
    }
    for run in &uv_wavelength_runs {
        for spectrum in uv_run_to_mzdata(run)? {
            writer.write_spectrum(&spectrum)?;
        }
    }
    for chromatogram in &uv_chromatograms {
        writer.write_chromatogram(chromatogram)?;
    }

    let vendor_metadata = if options.vendor_metadata_mode.is_some()
        || options.vendor_metadata_json.is_some()
        || options.include_detector_data
    {
        let bundle = VendorMetadataBundle::collect(input)?;
        if let Some(path) = &options.vendor_metadata_json {
            bundle.write_json_sidecar(path)?;
        }
        Some(bundle)
    } else {
        None
    };

    writer.copy_metadata_to_index().map_err(|error| {
        BrfpError::Writer(format!("failed to write mzPeak index metadata: {error}"))
    })?;
    let mut zip_writer = writer.finish_parquet().map_err(|error| {
        BrfpError::Writer(format!("failed to finish mzPeak parquet facets: {error}"))
    })?;
    if let Some(bundle) = vendor_metadata.as_ref() {
        bundle.write_to_archive(&mut zip_writer, options.vendor_metadata_mode)?;
    }
    if let Some(scan) = options.vendor_scan.as_ref() {
        scan.write_to_archive(&mut zip_writer)?;
    }
    zip_writer
        .flush()
        .map_err(|error| BrfpError::Writer(format!("failed to flush mzPeak file: {error}")))?;
    zip_writer
        .finish()
        .map_err(|error| BrfpError::Writer(format!("failed to finish mzPeak file: {error}")))?;

    Ok(())
}

fn configure_mzpeak_metadata(
    writer: &mut mzpeak_prototyping::writer::MzPeakWriterType<File>,
    input: &Path,
    spectrum_count: usize,
    wavelength_spectrum_count: usize,
    chromatogram_count: usize,
    processing_params: &[Param],
    run_metadata: &schema::RunVendorMetadata,
) {
    // Provenance params (sample name, BAF calibration mode) ride on the source
    // file rather than a data_processing method: a method would need to satisfy
    // the profile's `processingmethod_must` rule, which the current writer's
    // method-CV serialization does not, producing a spurious warning. The source
    // file is a clean, conformant provenance home.
    let mut source_file = source_file_metadata(input);
    source_file.params.extend(processing_params.iter().cloned());
    let mut content_types = vec![
        SpectrumType::MassSpectrum.into(),
        MassSpectrometerFileFormatTerm::MzPeak.into(),
    ];
    if wavelength_spectrum_count > 0 {
        content_types.push(SpectrumType::AbsorptionSpectrum.into());
    }
    if chromatogram_count > 0 {
        content_types.push(ControlledVocabulary::MS.param(1000812, "absorption chromatogram"));
    }
    *writer.file_description_mut() = FileDescription::new(content_types, vec![source_file]);

    writer.softwares_mut().push(Software::new(
        BRFP_SOFTWARE_ID.to_string(),
        env!("CARGO_PKG_VERSION").to_string(),
        vec![custom_software_name("BRFP")],
    ));

    // Acquisition software from the vendor metadata, when known (REQ-05). The
    // instrument was controlled by this software, so reference it from the
    // instrument configuration.
    let instrument_software_ref = if let Some(name) = &run_metadata.acquisition_software {
        writer.softwares_mut().push(Software::new(
            ACQUISITION_SOFTWARE_ID.to_string(),
            run_metadata
                .acquisition_software_version
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
            vec![custom_software_name(name)],
        ));
        ACQUISITION_SOFTWARE_ID.to_string()
    } else {
        BRFP_SOFTWARE_ID.to_string()
    };

    // Instrument model/serial/vendor from the vendor metadata, falling back to a
    // bare "instrument model" term when unknown.
    let mut instrument_params = match &run_metadata.instrument_name {
        Some(name) => {
            vec![ControlledVocabulary::MS.param_val(1000031, "instrument model", name.as_str())]
        }
        None => vec![ControlledVocabulary::MS.param(1000031, "instrument model")],
    };
    if let Some(serial) = &run_metadata.instrument_serial {
        instrument_params.push(ControlledVocabulary::MS.param_val(
            1000529,
            "instrument serial number",
            serial.as_str(),
        ));
    }
    if let Some(vendor) = &run_metadata.instrument_vendor {
        instrument_params.push(Param::new_key_value(
            "BRFP:instrument vendor",
            vendor.as_str(),
        ));
    }
    writer.instrument_configurations_mut().insert(
        BRFP_INSTRUMENT_ID,
        InstrumentConfiguration {
            id: BRFP_INSTRUMENT_ID,
            software_reference: instrument_software_ref,
            params: instrument_params,
            ..Default::default()
        },
    );

    writer.data_processings_mut().push(DataProcessing {
        id: BRFP_DATA_PROCESSING_ID.to_string(),
        methods: Vec::new(),
    });

    if let Some(run) = writer.run_description_mut() {
        *run = MassSpectrometryRun::new(
            Some("run_0".to_string()),
            Some(BRFP_DATA_PROCESSING_ID.to_string()),
            Some(BRFP_INSTRUMENT_ID),
            Some(BRFP_SOURCE_FILE_ID.to_string()),
            None,
        );
    }
    writer.set_spectrum_count_hint(Some(spectrum_count as u64));
}

fn source_file_metadata(input: &Path) -> SourceFile {
    let name = input
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "input.d".to_string());
    let location = input
        .canonicalize()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
        .map(|parent| format!("file://{}", parent.to_string_lossy()))
        .unwrap_or_else(|| "file://".to_string());

    let (file_format, id_format) = source_file_format_terms(input);

    SourceFile {
        name,
        location,
        id: BRFP_SOURCE_FILE_ID.to_string(),
        file_format: Some(file_format.into()),
        id_format: Some(id_format.into()),
        params: ParamList::default(),
    }
}

fn source_file_format_terms(
    input: &Path,
) -> (
    MassSpectrometerFileFormatTerm,
    NativeSpectrumIdentifierFormatTerm,
) {
    if input.join("analysis.baf").is_file()
        || input.file_name().and_then(|value| value.to_str()) == Some("analysis.baf")
    {
        (
            MassSpectrometerFileFormatTerm::BrukerBAF,
            NativeSpectrumIdentifierFormatTerm::BrukerBAFNativeIDFormat,
        )
    } else if input.join("analysis.tdf").is_file() {
        (
            MassSpectrometerFileFormatTerm::BrukerTDF,
            NativeSpectrumIdentifierFormatTerm::BrukerTDFNativeIDFormat,
        )
    } else {
        (
            MassSpectrometerFileFormatTerm::BrukerTSF,
            NativeSpectrumIdentifierFormatTerm::BrukerTSFNativeIDFormat,
        )
    }
}

fn tsf_spectrum_to_mzdata(spectrum: &TsfSpectrum) -> BrfpResult<BrfpSpectrum> {
    let mut description = SpectrumDescription::new(
        format!("frame={}", spectrum.frame_id),
        spectrum.index,
        spectrum.ms_level,
        scan_polarity(spectrum.polarity),
        SignalContinuity::Centroid,
        ParamList::default(),
        Acquisition {
            scans: vec![ScanEvent::new(
                spectrum.retention_time_seconds / 60.0,
                0.0,
                Vec::new(),
                0,
                None,
            )],
            ..Default::default()
        },
        None,
    );
    description.set_spectrum_type(schema::spectrum_type_for_ms_level(spectrum.ms_level));
    description.params.push(schema::mass_spectrum_param());
    let arrays = arrays_from_mz_and_intensity(&spectrum.mz_values, &spectrum.intensities)?;

    Ok(MultiLayerSpectrum::from_arrays_and_description(
        arrays,
        description,
    ))
}

fn baf_spectrum_to_mzdata(spectrum: &BafSpectrum) -> BrfpResult<BrfpSpectrum> {
    let mut description = SpectrumDescription::new(
        format!("scan={}", spectrum.id),
        spectrum.index,
        spectrum.ms_level,
        scan_polarity_from_baf(spectrum.polarity),
        if spectrum.centroided {
            SignalContinuity::Centroid
        } else {
            SignalContinuity::Profile
        },
        ParamList::default(),
        Acquisition {
            scans: vec![ScanEvent::new(
                spectrum.retention_time_seconds / 60.0,
                0.0,
                Vec::new(),
                0,
                None,
            )],
            ..Default::default()
        },
        None,
    );
    description.set_spectrum_type(schema::spectrum_type_for_ms_level(spectrum.ms_level));
    description.params.push(schema::mass_spectrum_param());
    let arrays = arrays_from_mz_and_intensity(&spectrum.mz_values, &spectrum.intensities)?;

    Ok(MultiLayerSpectrum::from_arrays_and_description(
        arrays,
        description,
    ))
}

fn uv_run_to_mzdata(run: &DecodedUvWavelengthRun) -> BrfpResult<Vec<BrfpSpectrum>> {
    let mut spectra = Vec::with_capacity(run.spectra.len());
    for spectrum in &run.spectra {
        spectra.push(uv_spectrum_to_mzdata(run, spectrum)?);
    }
    Ok(spectra)
}

fn uv_spectrum_to_mzdata(
    run: &DecodedUvWavelengthRun,
    spectrum: &DecodedUvWavelengthSpectrum,
) -> BrfpResult<BrfpSpectrum> {
    let lower_wavelength = *run.wavelengths_nm.first().ok_or_else(|| {
        BrfpError::Reader(format!(
            "UV wavelength run {} has no wavelength axis",
            run.relative_path
        ))
    })? as f32;
    let upper_wavelength = *run.wavelengths_nm.last().unwrap() as f32;
    let mut description = SpectrumDescription::new(
        format!(
            "uv spectrum={} index={}",
            run.relative_path, spectrum.source_index
        ),
        spectrum.source_index,
        0,
        ScanPolarity::Unknown,
        SignalContinuity::Profile,
        ParamList::default(),
        Acquisition {
            scans: vec![ScanEvent::new(
                spectrum.time_minutes,
                0.0,
                vec![ScanWindow::new(lower_wavelength, upper_wavelength)],
                BRFP_INSTRUMENT_ID,
                None,
            )],
            ..Default::default()
        },
        None,
    );
    description.set_spectrum_type(SpectrumType::AbsorptionSpectrum);
    description.params.push(Param::new_key_value(
        "BRFP:uv source file",
        run.relative_path.clone(),
    ));
    description.params.push(Param::new_key_value(
        "BRFP:uv intensity unit",
        run.intensity_unit.clone(),
    ));
    description
        .params
        .push(ControlledVocabulary::UO.param(269, "absorbance unit"));

    let arrays = arrays_from_wavelength_and_absorbance(&run.wavelengths_nm, &spectrum.intensities)?;
    Ok(MultiLayerSpectrum::from_arrays_and_description(
        arrays,
        description,
    ))
}

fn uv_chromatograms_from_wavelength_runs(
    input: &Path,
    runs: &[DecodedUvWavelengthRun],
) -> BrfpResult<Vec<Chromatogram>> {
    if runs.is_empty() {
        return Ok(Vec::new());
    }

    let inventory = inspect_uv_detector_inventory(input)?;
    let Some(method) = inventory.lc_method else {
        return Ok(Vec::new());
    };
    let mut channel_wavelengths = method.channel_wavelengths_nm;
    channel_wavelengths.retain(|value| value.is_finite() && *value > 0.0);
    channel_wavelengths.sort_by(f64::total_cmp);
    channel_wavelengths.dedup_by(|left, right| (*left - *right).abs() < 0.001);

    let mut chromatograms = Vec::new();
    for run in runs {
        for &wavelength_nm in &channel_wavelengths {
            let Some(first) = run.wavelengths_nm.first() else {
                continue;
            };
            let Some(last) = run.wavelengths_nm.last() else {
                continue;
            };
            if wavelength_nm < *first || wavelength_nm > *last {
                continue;
            }
            let index = chromatograms.len();
            chromatograms.push(uv_chromatogram_to_mzdata(run, wavelength_nm, index)?);
        }
    }

    Ok(chromatograms)
}

fn uv_chromatogram_to_mzdata(
    run: &DecodedUvWavelengthRun,
    wavelength_nm: f64,
    index: usize,
) -> BrfpResult<Chromatogram> {
    let mut times = Vec::with_capacity(run.spectra.len());
    let mut intensities = Vec::with_capacity(run.spectra.len());
    for spectrum in &run.spectra {
        times.push(spectrum.time_minutes);
        intensities.push(interpolate_wavelength_intensity(
            &run.wavelengths_nm,
            &spectrum.intensities,
            wavelength_nm,
        )?);
    }

    let mut description = ChromatogramDescription {
        id: format!(
            "uv chromatogram wavelength={wavelength_nm:.3} source={}",
            run.relative_path
        ),
        index,
        ms_level: None,
        polarity: ScanPolarity::Unknown,
        chromatogram_type: ChromatogramType::AbsorptionChromatogram,
        params: ParamList::default(),
        precursor: Vec::new(),
    };
    description.params.push(Param::new_key_value(
        "BRFP:uv source file",
        run.relative_path.clone(),
    ));
    description
        .params
        .push(Param::new_key_value("BRFP:uv wavelength nm", wavelength_nm));
    description.params.push(Param::new_key_value(
        "BRFP:uv intensity unit",
        run.intensity_unit.clone(),
    ));
    description
        .params
        .push(ControlledVocabulary::UO.param(269, "absorbance unit"));

    let arrays = arrays_from_time_and_uv_intensity(&times, &intensities)?;
    Ok(Chromatogram::new(description, arrays))
}

fn interpolate_wavelength_intensity(
    wavelengths_nm: &[f64],
    intensities: &[f64],
    wavelength_nm: f64,
) -> BrfpResult<f64> {
    if wavelengths_nm.len() != intensities.len() {
        return Err(BrfpError::Reader(format!(
            "wavelength array length {} does not match intensity array length {}",
            wavelengths_nm.len(),
            intensities.len()
        )));
    }
    if wavelengths_nm.is_empty() {
        return Err(BrfpError::Reader(
            "cannot interpolate UV chromatogram from an empty wavelength axis".to_string(),
        ));
    }

    match wavelengths_nm.binary_search_by(|probe| probe.total_cmp(&wavelength_nm)) {
        Ok(index) => Ok(intensities[index]),
        Err(index) if index == 0 || index >= wavelengths_nm.len() => {
            Err(BrfpError::Reader(format!(
                "UV chromatogram wavelength {wavelength_nm} nm falls outside decoded axis {}..{} nm",
                wavelengths_nm.first().unwrap(),
                wavelengths_nm.last().unwrap()
            )))
        }
        Err(index) => {
            let lower_index = index - 1;
            let upper_index = index;
            let lower_wavelength = wavelengths_nm[lower_index];
            let upper_wavelength = wavelengths_nm[upper_index];
            let span = upper_wavelength - lower_wavelength;
            if span <= 0.0 || !span.is_finite() {
                return Err(BrfpError::Reader(format!(
                    "invalid UV wavelength interpolation span {lower_wavelength}..{upper_wavelength}"
                )));
            }
            let fraction = (wavelength_nm - lower_wavelength) / span;
            Ok(intensities[lower_index]
                + fraction * (intensities[upper_index] - intensities[lower_index]))
        }
    }
}

fn scan_polarity(polarity: TsfPolarity) -> ScanPolarity {
    match polarity {
        TsfPolarity::Positive => ScanPolarity::Positive,
        TsfPolarity::Negative => ScanPolarity::Negative,
        TsfPolarity::Unknown => ScanPolarity::Unknown,
    }
}

fn scan_polarity_from_baf(polarity: BafPolarity) -> ScanPolarity {
    match polarity {
        BafPolarity::Positive => ScanPolarity::Positive,
        BafPolarity::Negative => ScanPolarity::Negative,
        BafPolarity::Unknown => ScanPolarity::Unknown,
    }
}

fn baf_spectrum_selected(
    spectrum: &BafSpectrum,
    options: &BafMzPeakWriteOptions,
) -> BrfpResult<bool> {
    if options.ms2_only && spectrum.ms_level != 2 {
        return Ok(false);
    }
    if let Some(start) = options.start_spectrum_id
        && spectrum.id < start
    {
        return Ok(false);
    }
    if let Some(end) = options.end_spectrum_id
        && spectrum.id > end
    {
        return Ok(false);
    }
    ms_level_matches(options.ms_level_filter.as_deref(), spectrum.ms_level)
}

fn ms_level_matches(filter: Option<&str>, level: u8) -> BrfpResult<bool> {
    let Some(filter) = filter else {
        return Ok(true);
    };
    let filter = filter.trim();
    if filter.eq_ignore_ascii_case("all") {
        return Ok(true);
    }

    for part in filter.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_optional_level_bound(start)?;
            let end = parse_optional_level_bound(end)?;
            if start.is_none_or(|start| level >= start) && end.is_none_or(|end| level <= end) {
                return Ok(true);
            }
        } else if level == parse_level_bound(part)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_optional_level_bound(value: &str) -> BrfpResult<Option<u8>> {
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        parse_level_bound(value).map(Some)
    }
}

fn parse_level_bound(value: &str) -> BrfpResult<u8> {
    value.trim().parse::<u8>().map_err(|_| {
        BrfpError::InvalidInput(format!(
            "invalid MS level filter component {value:?}; expected positive integer"
        ))
    })
}

fn arrays_from_mz_and_intensity(
    mz_values: &[f64],
    intensities: &[f64],
) -> BrfpResult<BinaryArrayMap> {
    if mz_values.len() != intensities.len() {
        return Err(BrfpError::AxisLengthMismatch {
            kind: "m/z",
            left: mz_values.len(),
            right: intensities.len(),
        });
    }
    for (index, mz) in mz_values.iter().enumerate() {
        if !mz.is_finite() || *mz <= 0.0 {
            return Err(BrfpError::NonPositiveMz { index, value: *mz });
        }
    }
    for (index, intensity) in intensities.iter().enumerate() {
        // Intensity is stored as f32 (the mzPeak convention, matching the
        // reference converters and mzdata's CentroidPeak / the spectra_peaks
        // facet). Reject non-finite, negative, or out-of-f32-range values so the
        // cast cannot silently saturate to infinity.
        if !intensity.is_finite() || *intensity < 0.0 || intensity.abs() > f32::MAX as f64 {
            return Err(BrfpError::NonFiniteValue {
                kind: "intensity",
                index,
                value: *intensity,
            });
        }
    }

    // m/z stays lossless Float64; intensity is Float32 (mzPeak convention).
    let mut mz_array =
        DataArray::from_name_and_type(&ArrayType::MZArray, BinaryDataArrayType::Float64);
    mz_array.unit = schema::MZ_UNIT;
    mz_array.update_buffer(mz_values).map_err(|error| {
        BrfpError::Writer(format!(
            "failed to encode m/z array for mzPeak output: {error}"
        ))
    })?;

    let intensity_values = intensities
        .iter()
        .map(|value| *value as f32)
        .collect::<Vec<_>>();
    let mut intensity_array =
        DataArray::from_name_and_type(&ArrayType::IntensityArray, BinaryDataArrayType::Float32);
    intensity_array
        .update_buffer(&intensity_values)
        .map_err(|error| {
            BrfpError::Writer(format!(
                "failed to encode intensity array for mzPeak output: {error}"
            ))
        })?;
    intensity_array.unit = schema::MS_INTENSITY_UNIT;

    let mut arrays = BinaryArrayMap::new();
    arrays.add(mz_array);
    arrays.add(intensity_array);
    Ok(arrays)
}

fn arrays_from_wavelength_and_absorbance(
    wavelengths_nm: &[f64],
    intensities_mau: &[f64],
) -> BrfpResult<BinaryArrayMap> {
    if wavelengths_nm.len() != intensities_mau.len() {
        return Err(BrfpError::Reader(format!(
            "wavelength array length {} does not match UV intensity array length {}",
            wavelengths_nm.len(),
            intensities_mau.len()
        )));
    }
    for (index, wavelength) in wavelengths_nm.iter().enumerate() {
        if !wavelength.is_finite() || *wavelength <= 0.0 {
            return Err(BrfpError::Writer(format!(
                "invalid wavelength at point {index}: {wavelength}"
            )));
        }
        if index > 0 && *wavelength <= wavelengths_nm[index - 1] {
            return Err(BrfpError::Writer(format!(
                "wavelength axis is not strictly increasing at point {index}: {wavelength}"
            )));
        }
    }
    for (index, intensity) in intensities_mau.iter().enumerate() {
        if !intensity.is_finite() {
            return Err(BrfpError::Writer(format!(
                "invalid UV intensity at point {index}: {intensity}"
            )));
        }
    }

    let mut wavelength_array =
        DataArray::from_name_and_type(&ArrayType::WavelengthArray, BinaryDataArrayType::Float64);
    wavelength_array.unit = Unit::Nanometer;
    wavelength_array
        .update_buffer(wavelengths_nm)
        .map_err(|error| {
            BrfpError::Writer(format!(
                "failed to encode wavelength array for mzPeak output: {error}"
            ))
        })?;

    let mut intensity_array =
        DataArray::from_name_and_type(&ArrayType::IntensityArray, BinaryDataArrayType::Float64);
    // UV/PDA values are absorbance, not detector counts: label the array with the
    // proper absorbance unit (UO:0000269) and keep the raw vendor unit string as
    // metadata for provenance.
    intensity_array.unit = schema::ABSORBANCE_UNIT;
    intensity_array.params = Some(Box::new(vec![Param::new_key_value(
        "BRFP:raw UV intensity unit",
        "mAU",
    )]));
    intensity_array
        .update_buffer(intensities_mau)
        .map_err(|error| {
            BrfpError::Writer(format!(
                "failed to encode UV intensity array for mzPeak output: {error}"
            ))
        })?;

    let mut arrays = BinaryArrayMap::new();
    arrays.add(wavelength_array);
    arrays.add(intensity_array);
    Ok(arrays)
}

fn arrays_from_time_and_uv_intensity(
    times_minutes: &[f64],
    intensities_mau: &[f64],
) -> BrfpResult<BinaryArrayMap> {
    if times_minutes.len() != intensities_mau.len() {
        return Err(BrfpError::Reader(format!(
            "time array length {} does not match UV chromatogram intensity array length {}",
            times_minutes.len(),
            intensities_mau.len()
        )));
    }
    for (index, time) in times_minutes.iter().enumerate() {
        if !time.is_finite() || *time < 0.0 {
            return Err(BrfpError::Writer(format!(
                "invalid UV chromatogram time at point {index}: {time}"
            )));
        }
        if index > 0 && *time <= times_minutes[index - 1] {
            return Err(BrfpError::Writer(format!(
                "UV chromatogram time axis is not strictly increasing at point {index}: {time}"
            )));
        }
    }
    for (index, intensity) in intensities_mau.iter().enumerate() {
        // Absorbance may be negative (baseline drift), so only reject non-finite
        // values and magnitudes outside the f32 range this column is encoded in.
        // (The previous `< f32::MIN` lower bound was a no-op: f32::MIN is the
        // most-negative f32, which every finite f64 exceeds.)
        if !intensity.is_finite() || intensity.abs() > f32::MAX as f64 {
            return Err(BrfpError::NonFiniteValue {
                kind: "UV chromatogram intensity",
                index,
                value: *intensity,
            });
        }
    }

    let mut time_array =
        DataArray::from_name_and_type(&ArrayType::TimeArray, BinaryDataArrayType::Float64);
    time_array.unit = Unit::Minute;
    time_array.update_buffer(times_minutes).map_err(|error| {
        BrfpError::Writer(format!(
            "failed to encode UV chromatogram time array for mzPeak output: {error}"
        ))
    })?;

    let intensity_values = intensities_mau
        .iter()
        .map(|value| *value as f32)
        .collect::<Vec<_>>();
    let mut intensity_array =
        DataArray::from_name_and_type(&ArrayType::IntensityArray, BinaryDataArrayType::Float32);
    // Absorbance chromatogram: label with the absorbance unit (UO:0000269), not
    // detector counts; keep the raw vendor unit string as provenance metadata.
    intensity_array.unit = schema::ABSORBANCE_UNIT;
    intensity_array.params = Some(Box::new(vec![Param::new_key_value(
        "BRFP:raw UV intensity unit",
        "mAU",
    )]));
    intensity_array
        .update_buffer(&intensity_values)
        .map_err(|error| {
            BrfpError::Writer(format!(
                "failed to encode UV chromatogram intensity array for mzPeak output: {error}"
            ))
        })?;

    let mut arrays = BinaryArrayMap::new();
    arrays.add(time_array);
    arrays.add(intensity_array);
    Ok(arrays)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mzdata::prelude::*;
    use mzpeak_prototyping::reader::MzPeakReader;

    #[test]
    fn writes_and_reads_synthetic_mzpeak_file() {
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("synthetic.mzpeak");
        let spectrum = synthetic_spectrum();

        write_spectra_to_mzpeak(
            Path::new("synthetic.d"),
            &output,
            vec![spectrum].into_iter().map(Ok),
            1,
            &MzPeakWriteOptions::default(),
        )
        .unwrap();

        let mut reader = MzPeakReader::new(&output).unwrap();
        assert_eq!(reader.len(), 1);
        let spectrum = reader.get_spectrum(0).unwrap();
        let peaks = &spectrum.peaks.as_ref().unwrap().peaks;
        assert_eq!(peaks.len(), 3);
        assert_eq!(peaks[0].mz, 100.0);
        assert_eq!(peaks[1].mz, 101.0);
        assert_eq!(peaks[0].intensity, 10.0);
        assert_eq!(peaks[1].intensity, 20.0);
    }

    #[test]
    fn writes_vendor_metadata_facet_without_raw_detector_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("synthetic.d");
        std::fs::create_dir(&input).unwrap();
        std::fs::write(
            input.join("SampleInfo.xml"),
            "<Sample><Name>demo</Name></Sample>",
        )
        .unwrap();
        std::fs::write(input.join("LCParms.txt"), "WavelengthA=254\nFlow=0.2").unwrap();
        std::fs::write(input.join("uv.u2"), [1u8, 2, 3, 4]).unwrap();
        let output = tempdir.path().join("synthetic.mzpeak");
        let json = tempdir.path().join("synthetic.vendor.json");

        write_spectra_to_mzpeak(
            &input,
            &output,
            vec![synthetic_spectrum()].into_iter().map(Ok),
            1,
            &MzPeakWriteOptions {
                vendor_metadata_mode: Some(VendorMetadataMode::Tall),
                vendor_metadata_json: Some(json.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(json.is_file());
        let reader = MzPeakReader::new(&output).unwrap();
        assert_eq!(reader.len(), 1);
        assert!(reader.file_index().iter().any(|entry| {
            entry.name == "vendor_file_metadata.parquet"
                && matches!(
                    entry.data_kind,
                    mzpeak_prototyping::archive::DataKind::Proprietary
                )
        }));
        assert!(
            !reader
                .file_index()
                .iter()
                .any(|entry| entry.name.starts_with("vendor_payloads/"))
        );
        let batches = reader
            .open_parquet("vendor_file_metadata.parquet")
            .unwrap()
            .build()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(!batches.is_empty());
        assert!(batches.iter().map(|batch| batch.num_rows()).sum::<usize>() > 0);
    }

    #[test]
    fn wide_vendor_mode_still_writes_tall_file_facet() {
        // REQ-03: `wide` is honest, not silently contradicted — it falls back to
        // the tall file-level facet (warning emitted) until a per-spectrum trailer
        // facet exists. The archive must still carry vendor_file_metadata.parquet.
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("synthetic.d");
        std::fs::create_dir(&input).unwrap();
        std::fs::write(
            input.join("SampleInfo.xml"),
            "<Sample><Name>demo</Name></Sample>",
        )
        .unwrap();
        let output = tempdir.path().join("wide.mzpeak");

        write_spectra_to_mzpeak(
            &input,
            &output,
            vec![synthetic_spectrum()].into_iter().map(Ok),
            1,
            &MzPeakWriteOptions {
                vendor_metadata_mode: Some(VendorMetadataMode::Wide),
                ..Default::default()
            },
        )
        .unwrap();

        let reader = MzPeakReader::new(&output).unwrap();
        assert!(reader.file_index().iter().any(|entry| {
            entry.name == "vendor_file_metadata.parquet"
                && matches!(
                    entry.data_kind,
                    mzpeak_prototyping::archive::DataKind::Proprietary
                )
        }));
    }

    #[test]
    fn writes_per_spectrum_vendor_scan_facet() {
        // REQ-04: the per-spectrum vendor facet is injected as a proprietary
        // parquet entry, keyed by ordinal + native id with typed value_float.
        use crate::vendor_metadata::{VendorScanMetadata, VendorScanRow};

        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("scan.mzpeak");
        let scan = VendorScanMetadata::new(vec![
            VendorScanRow::new(0, "frame=1", "MsMsType", "2"),
            VendorScanRow::new(0, "frame=1", "SummedIntensities", "12345"),
        ]);
        // value_float is typed from the numeric strings.
        assert_eq!(scan.rows()[0].value_float, Some(2.0));

        write_spectra_to_mzpeak(
            Path::new("scan.d"),
            &output,
            vec![synthetic_spectrum()].into_iter().map(Ok),
            1,
            &MzPeakWriteOptions {
                vendor_scan: Some(scan),
                ..Default::default()
            },
        )
        .unwrap();

        let reader = MzPeakReader::new(&output).unwrap();
        assert!(reader.file_index().iter().any(|entry| {
            entry.name == "vendor_scan_metadata.parquet"
                && matches!(
                    entry.data_kind,
                    mzpeak_prototyping::archive::DataKind::Proprietary
                )
        }));
        let rows: usize = reader
            .open_parquet("vendor_scan_metadata.parquet")
            .unwrap()
            .build()
            .unwrap()
            .map(|b| b.unwrap().num_rows())
            .sum();
        assert_eq!(rows, 2);
    }

    #[test]
    fn per_spectrum_vendor_facet_joins_real_tsf_spectra() {
        // REQ-04 end-to-end join check (Claude review Finding 1): convert the real
        // TSF fixture with the vendor facet and assert the facet's ordinals are
        // dense 0..N-1 and each native_id matches the corresponding spectrum's id.
        let Some(root) = std::env::var_os("BRFP_TEST_PRIVATE_DATA").map(PathBuf::from) else {
            return;
        };
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("join.mzpeak");
        let limit = 20usize;
        write_tsf_to_mzpeak(
            root.join("timsTOF_autoMSMS_Urine_6min_pos.d"),
            &output,
            MzPeakWriteOptions {
                limit_spectra: Some(limit),
                vendor_metadata_mode: Some(VendorMetadataMode::Tall),
                ..Default::default()
            },
        )
        .unwrap();

        let mut reader = MzPeakReader::new(&output).unwrap();
        // native_id per written spectrum, indexed by spectrum_index (== ordinal).
        let spectrum_ids: Vec<String> = (0..reader.len())
            .map(|i| reader.get_spectrum(i).unwrap().description().id.clone())
            .collect();

        use arrow::array::{StringArray, UInt64Array};
        let batches = reader
            .open_parquet("vendor_scan_metadata.parquet")
            .unwrap()
            .build()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let mut max_ordinal = 0u64;
        for batch in &batches {
            let ords = batch
                .column(0)
                .as_any()
                .downcast_ref::<UInt64Array>()
                .unwrap();
            let nids = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for row in 0..batch.num_rows() {
                let ordinal = ords.value(row);
                max_ordinal = max_ordinal.max(ordinal);
                // Every vendor row's native_id must equal the spectrum it joins to.
                assert_eq!(nids.value(row), spectrum_ids[ordinal as usize]);
            }
        }
        // Dense coverage of exactly the written spectra.
        assert_eq!(max_ordinal as usize, limit - 1);
    }

    #[test]
    fn writes_decoded_uv_wavelength_spectra() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("synthetic.d");
        std::fs::create_dir(&input).unwrap();
        write_synthetic_u2(&input.join("uv.u2"));
        std::fs::write(input.join("LCParms.txt"), "Wavelength A: 201 nm\n").unwrap();
        let output = tempdir.path().join("synthetic.mzpeak");

        write_spectra_to_mzpeak(
            &input,
            &output,
            vec![synthetic_spectrum()].into_iter().map(Ok),
            1,
            &MzPeakWriteOptions {
                limit_spectra: Some(2),
                include_detector_data: true,
                ..Default::default()
            },
        )
        .unwrap();

        let mut reader = MzPeakReader::new(&output).unwrap();
        assert_eq!(reader.len(), 1);
        assert_eq!(reader.len_wavelength_spectra(), 2);
        assert_eq!(reader.len_chromatograms(), 1);
        assert!(
            !reader
                .file_index()
                .iter()
                .any(|entry| entry.name.starts_with("vendor_payloads/"))
        );

        let spectrum = reader.get_wavelength_spectrum(0).unwrap();
        assert_eq!(
            spectrum.spectrum_type(),
            Some(SpectrumType::AbsorptionSpectrum)
        );
        assert!((spectrum.start_time() - (1000.0 / 60_000.0)).abs() < 1e-9);
        let arrays = spectrum.raw_arrays().unwrap();
        let wavelengths = arrays
            .get(&ArrayType::WavelengthArray)
            .unwrap()
            .to_f64()
            .unwrap();
        let intensity_array = arrays.get(&ArrayType::IntensityArray).unwrap();
        // Phase C: absorbance is labeled with the absorbance unit, not detector
        // counts, and the unit survives the write/read round trip.
        assert_eq!(intensity_array.unit, Unit::AbsorbanceUnit);
        let intensities = intensity_array.to_f64().unwrap();
        assert_eq!(&wavelengths[..], &[200.0, 201.0, 202.0]);
        assert_eq!(&intensities[..], &[1.0, 2.0, 3.0]);

        let chromatogram = reader.get_chromatogram(0).unwrap();
        assert_eq!(
            chromatogram.chromatogram_type(),
            ChromatogramType::AbsorptionChromatogram
        );
        assert_eq!(
            chromatogram.time().unwrap().as_ref(),
            &[1000.0 / 60_000.0, 1050.0 / 60_000.0]
        );
        assert_eq!(chromatogram.intensity().unwrap().as_ref(), &[2.0f32, 5.0]);
    }

    #[test]
    fn writes_private_tsf_fixture_to_mzpeak() {
        let Some(root) = std::env::var_os("BRFP_TEST_PRIVATE_DATA").map(PathBuf::from) else {
            return;
        };

        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("urine-pos.mzpeak");
        let report = write_tsf_to_mzpeak(
            root.join("timsTOF_autoMSMS_Urine_6min_pos.d"),
            &output,
            MzPeakWriteOptions {
                limit_spectra: Some(2),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.spectra_written, 2);
        let mut reader = MzPeakReader::new(&output).unwrap();
        assert_eq!(reader.len(), 2);
        let spectrum = reader.get_spectrum(0).unwrap();
        assert!(!spectrum.peaks.as_ref().unwrap().peaks.is_empty());
    }

    fn synthetic_spectrum() -> BrfpSpectrum {
        let description = SpectrumDescription::new(
            "scan=1".to_string(),
            0,
            1,
            ScanPolarity::Positive,
            SignalContinuity::Centroid,
            ParamList::default(),
            Acquisition::default(),
            None,
        );
        MultiLayerSpectrum::from_arrays_and_description(
            arrays_from_mz_and_intensity(&[100.0, 101.0, 102.0], &[10.0, 20.0, 0.0]).unwrap(),
            description,
        )
    }

    fn ms2_with_precursor(mzs: &[f64], intensities: &[f64], precursor_mz: f64) -> BrfpSpectrum {
        let ion = mzdata::spectrum::SelectedIon {
            mz: precursor_mz,
            ..Default::default()
        };
        let precursor = mzdata::spectrum::Precursor {
            ions: vec![ion],
            ..Default::default()
        };
        let description = SpectrumDescription::new(
            "scan=1".to_string(),
            0,
            2,
            ScanPolarity::Positive,
            SignalContinuity::Centroid,
            ParamList::default(),
            Acquisition::default(),
            Some(precursor),
        );
        MultiLayerSpectrum::from_arrays_and_description(
            arrays_from_mz_and_intensity(mzs, intensities).unwrap(),
            description,
        )
    }

    #[test]
    fn merge_pasef_group_sums_same_precursor_peaks() {
        // Two frames of one precursor: the ~200 peak (within 10 ppm = 0.002 Da)
        // merges with summed intensity and weighted m/z; 300 and 400 stay distinct.
        let a = ms2_with_precursor(&[200.0, 300.0], &[10.0, 5.0], 500.0);
        let b = ms2_with_precursor(&[200.0009, 400.0], &[20.0, 7.0], 500.0);
        let out: Vec<_> = MergePasefPrecursors::new(vec![Ok(a), Ok(b)].into_iter())
            .collect::<BrfpResult<Vec<_>>>()
            .unwrap();
        assert_eq!(
            out.len(),
            1,
            "same precursor across two frames merges to one"
        );
        let arrays = out[0].raw_arrays().unwrap();
        let mz = arrays.mzs().unwrap();
        let intensity = arrays.intensities().unwrap();
        assert_eq!(mz.len(), 3, "200 cluster collapses; 300 and 400 stay");
        assert!((intensity[0] - 30.0).abs() < 1e-4);
        let expected_mz = (200.0 * 10.0 + 200.0009 * 20.0) / 30.0;
        assert!((mz[0] - expected_mz).abs() < 1e-6);
        assert!(
            arrays.get(&MOBILITY_ARRAY).is_some(),
            "mobility array emitted"
        );
    }

    #[test]
    fn merge_pasef_precursors_groups_consecutive_by_precursor() {
        let spectra = vec![
            Ok(ms2_with_precursor(&[100.0], &[1.0], 500.0)),
            Ok(ms2_with_precursor(&[101.0], &[1.0], 500.0)), // same precursor → merge
            Ok(ms2_with_precursor(&[200.0], &[1.0], 600.0)), // new precursor
        ];
        let out: Vec<_> = MergePasefPrecursors::new(spectra.into_iter())
            .collect::<BrfpResult<Vec<_>>>()
            .unwrap();
        assert_eq!(
            out.len(),
            2,
            "two unique precursors from three frame-events"
        );
    }

    #[test]
    fn ims_calibration_tof_roundtrip_is_exact() {
        // m/z = (a + b·tof)² for integer tof must invert back to the same integer.
        let calib = ImsCalibration {
            a: 9.9996966453988,
            b: 0.0000778611663727645,
        };
        for tof in [0u32, 1, 12_345, 200_000, 401_112] {
            let mz = (calib.a + calib.b * tof as f64).powi(2);
            let recovered = ((mz.sqrt() - calib.a) / calib.b).round() as u32;
            assert_eq!(recovered, tof, "tof {tof} → m/z {mz} → {recovered}");
        }
    }

    #[test]
    fn merge_pasef_precursors_groups_non_consecutively() {
        // Precursor 500's two frame-events are separated by other precursors
        // (the real reader scatters them); within the flush window they must
        // still merge to one spectrum, not split.
        let spectra = vec![
            Ok(ms2_with_precursor(&[100.0], &[1.0], 500.0)),
            Ok(ms2_with_precursor(&[200.0], &[1.0], 600.0)),
            Ok(ms2_with_precursor(&[300.0], &[1.0], 700.0)),
            Ok(ms2_with_precursor(&[101.0], &[1.0], 500.0)), // precursor 500 again
        ];
        let out: Vec<_> = MergePasefPrecursors::new(spectra.into_iter())
            .collect::<BrfpResult<Vec<_>>>()
            .unwrap();
        assert_eq!(
            out.len(),
            3,
            "500 merges across the gap; 600 and 700 stand alone"
        );
    }

    /// Locate the mzPeak validator, preferring `BRFP_MZPEAK_VALIDATOR` then PATH.
    /// Returns `None` so the conformance test skips when the validator is absent
    /// (e.g. CI without the tool installed).
    fn locate_mzpeak_validator() -> Option<String> {
        // ponytail: explicit env only, no PATH probing. A PATH probe behaved
        // inconsistently across host/container, so the gate is now deterministic:
        // set BRFP_MZPEAK_VALIDATOR=/path/to/mzpeak-validate to run the check.
        std::env::var_os("BRFP_MZPEAK_VALIDATOR").map(|v| v.to_string_lossy().to_string())
    }

    #[test]
    fn synthetic_mzpeak_passes_mzpeak_validate() {
        // Conformance gate (REQ-01): BRFP-written mzPeak must validate clean. Skips
        // when the validator is unavailable; runs in CI when it is installed.
        let Some(validator) = locate_mzpeak_validator() else {
            // Make the skip visible rather than silently green (the H4 lesson).
            eprintln!(
                "SKIP synthetic_mzpeak_passes_mzpeak_validate: \
                 set BRFP_MZPEAK_VALIDATOR=/path/to/mzpeak-validate to run it"
            );
            return;
        };

        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("conformance.mzpeak");
        let spectra = (0..8usize).map(|index| {
            let description = SpectrumDescription::new(
                format!("scan={index}"),
                index,
                if index % 2 == 0 { 1 } else { 2 },
                ScanPolarity::Positive,
                SignalContinuity::Centroid,
                ParamList::default(),
                Acquisition::default(),
                None,
            );
            let mz = [
                100.0 + index as f64,
                200.5 + index as f64,
                300.25 + index as f64,
            ];
            let intensity = [10.0, 20.0 + index as f64, 5.0];
            arrays_from_mz_and_intensity(&mz, &intensity)
                .map(|arrays| MultiLayerSpectrum::from_arrays_and_description(arrays, description))
        });

        write_spectra_to_mzpeak(
            Path::new("conformance.d"),
            &output,
            spectra,
            8,
            &MzPeakWriteOptions::default(),
        )
        .unwrap();

        let result = std::process::Command::new(&validator)
            .arg(&output)
            .output()
            .expect("failed to run mzpeak-validate");
        let stdout = String::from_utf8_lossy(&result.stdout);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            result.status.success(),
            "mzpeak-validate failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(
            stdout.contains("PASS") || stdout.contains("0 errors"),
            "mzpeak-validate did not report a clean pass:\n{stdout}"
        );
    }

    #[test]
    fn failed_conversion_deletes_partial_output() {
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("broken.mzpeak");

        // An iterator whose first item is an error simulates a mid-conversion
        // reader failure. The partial output must not be left behind.
        let spectra = std::iter::once(Err(BrfpError::Reader("synthetic decode failure".into())));
        let result = write_spectra_to_mzpeak(
            Path::new("broken.d"),
            &output,
            spectra,
            1,
            &MzPeakWriteOptions::default(),
        );

        assert!(result.is_err());
        assert!(
            !output.exists(),
            "partial .mzpeak should be deleted on failure"
        );
    }

    #[test]
    fn streams_more_spectra_than_the_sample_buffer() {
        // Exercise the sample-then-stream boundary: write well past
        // ARRAY_SAMPLE_LIMIT spectra through the iterator path and read them all
        // back, including spectra emitted from the streamed (post-sample) tail.
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("streamed.mzpeak");
        let count = ARRAY_SAMPLE_LIMIT * 3 + 5;

        let spectra = (0..count).map(|index| {
            let description = SpectrumDescription::new(
                format!("scan={index}"),
                index,
                1,
                ScanPolarity::Positive,
                SignalContinuity::Centroid,
                ParamList::default(),
                Acquisition::default(),
                None,
            );
            let mz = [100.0 + index as f64, 200.0 + index as f64];
            let intensity = [10.0 + index as f64, 20.0 + index as f64];
            arrays_from_mz_and_intensity(&mz, &intensity)
                .map(|arrays| MultiLayerSpectrum::from_arrays_and_description(arrays, description))
        });

        write_spectra_to_mzpeak(
            Path::new("streamed.d"),
            &output,
            spectra,
            count,
            &MzPeakWriteOptions::default(),
        )
        .unwrap();

        let mut reader = MzPeakReader::new(&output).unwrap();
        assert_eq!(reader.len(), count);
        // A spectrum from the streamed tail round-trips correctly.
        let last = reader.get_spectrum(count - 1).unwrap();
        let peaks = &last.peaks.as_ref().unwrap().peaks;
        assert_eq!(peaks.len(), 2);
        assert_eq!(peaks[0].mz, 100.0 + (count - 1) as f64);
    }

    #[test]
    fn writes_mzml_that_round_trips() {
        use mzdata::io::MZReader;
        let tempdir = tempfile::tempdir().unwrap();
        let output = tempdir.path().join("out.mzML");
        let spectra = (0..3usize).map(|i| {
            let description = SpectrumDescription::new(
                format!("scan={i}"),
                i,
                1,
                ScanPolarity::Positive,
                SignalContinuity::Centroid,
                ParamList::default(),
                Acquisition::default(),
                None,
            );
            arrays_from_mz_and_intensity(&[100.0 + i as f64, 200.0], &[5.0, 6.0])
                .map(|a| MultiLayerSpectrum::from_arrays_and_description(a, description))
        });
        write_spectra_to_mzml(
            Path::new("synthetic.d"),
            &output,
            spectra,
            3,
            &schema::RunVendorMetadata::default(),
        )
        .unwrap();

        let mut reader = MZReader::open_path(&output).unwrap();
        assert_eq!(reader.iter().count(), 3);
    }

    #[test]
    fn arrays_use_f64_mz_and_f32_intensity() {
        // mzPeak convention (matches the reference converters and the spectra_peaks
        // facet): m/z lossless Float64, intensity Float32 — for every backend.
        let arrays = arrays_from_mz_and_intensity(&[100.0, 200.0], &[1.0, 2.0]).unwrap();
        assert_eq!(
            arrays.get(&ArrayType::MZArray).unwrap().dtype,
            BinaryDataArrayType::Float64
        );
        assert_eq!(
            arrays.get(&ArrayType::IntensityArray).unwrap().dtype,
            BinaryDataArrayType::Float32
        );
    }

    fn write_synthetic_u2(path: &Path) {
        let wavelength_count = 3u32;
        let spectrum_count = 2u32;
        let record_size = ((wavelength_count + 4) * 2) as usize;
        let data_start = 512usize;
        let mut bytes = vec![0u8; data_start + spectrum_count as usize * record_size];
        bytes[..12].copy_from_slice(b"#BFALCCHROM#");
        write_u32(&mut bytes, 0x20, 512);
        write_u32(&mut bytes, 0x24, wavelength_count);
        write_f64(&mut bytes, 0x28, 200.0);
        write_f64(&mut bytes, 0x30, 202.0);
        write_f64(&mut bytes, 0x38, 20.0);
        bytes[0x40..0x43].copy_from_slice(b"mAU");
        write_u32(&mut bytes, 0x154, spectrum_count);

        write_u2_record(
            &mut bytes,
            data_start,
            wavelength_count,
            1000,
            &[100, 200, 300],
        );
        write_u2_record(
            &mut bytes,
            data_start + record_size,
            wavelength_count,
            1050,
            &[400, 500, 600],
        );
        std::fs::write(path, bytes).unwrap();
    }

    fn write_u2_record(
        bytes: &mut [u8],
        offset: usize,
        wavelength_count: u32,
        time_ms: u32,
        intensities_centimau: &[i16],
    ) {
        assert_eq!(intensities_centimau.len(), wavelength_count as usize);
        write_u16(bytes, offset, (time_ms & 0xffff) as u16);
        write_u16(bytes, offset + 2, (time_ms >> 16) as u16);
        write_u16(bytes, offset + 4, ((wavelength_count + 2) * 2) as u16);
        write_i16(bytes, offset + 6, intensities_centimau[0]);
        write_i16(bytes, offset + 8, 0);
        for (index, window) in intensities_centimau.windows(2).enumerate() {
            let delta = window[0] - window[1];
            write_i16(bytes, offset + 10 + index * 2, delta);
        }
    }

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_i16(bytes: &mut [u8], offset: usize, value: i16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_f64(bytes: &mut [u8], offset: usize, value: f64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
}
