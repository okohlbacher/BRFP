use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
};

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::baf::{BafCalibrationMode, BafProfileMissingMode};
use crate::vendor_metadata::VendorMetadataMode;

#[derive(Debug, Parser)]
#[command(name = "brfp")]
#[command(about = "Convert Bruker .d raw data directories to mzPeak")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[arg(
        short = 'l',
        long = "log",
        alias = "logging",
        global = true,
        default_value = "info",
        value_parser = parse_log_filter,
        help = "Tracing filter or ThermoRawFileParser logging level: 0 silent, 1 verbose, 2 default, 3 warning, 4 error"
    )]
    pub log: String,
}

impl Cli {
    pub fn parse_compat() -> Self {
        Self::parse_from_compat(std::env::args_os())
    }

    pub fn parse_from_compat<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        Self::parse_from(normalize_thermo_args(args))
    }

    pub fn log_filter(&self) -> &str {
        &self.log
    }
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum Command {
    #[command(about = "Convert one Bruker .d directory")]
    Convert(ConvertArgs),
    #[command(about = "Inspect a Bruker .d directory and report detected capabilities")]
    Inspect(InspectArgs),
    #[command(about = "Validate an mzPeak or mzML output file")]
    Validate(ValidateArgs),
    #[command(about = "ThermoRawFileParser-compatible query entry point; not implemented yet")]
    Query(QueryArgs),
    #[command(about = "ThermoRawFileParser-compatible XIC entry point; not implemented yet")]
    Xic(XicArgs),
}

#[derive(Debug, Args)]
pub struct ConvertArgs {
    #[arg(
        help = "Input Bruker .d directory",
        conflicts_with_all = ["input_file", "input_directory"]
    )]
    pub input: Option<PathBuf>,

    #[arg(
        short = 'i',
        long = "input",
        value_name = "INPUT",
        conflicts_with_all = ["input", "input_directory"],
        help = "Input Bruker .d directory; ThermoRawFileParser-compatible"
    )]
    pub input_file: Option<PathBuf>,

    #[arg(
        short = 'd',
        long = "input_directory",
        visible_alias = "input-directory",
        value_name = "DIR",
        conflicts_with_all = ["input", "input_file"],
        help = "Input directory. A .d directory is treated as a single Bruker run; batch input is not implemented yet"
    )]
    pub input_directory: Option<PathBuf>,

    #[arg(
        short = 'o',
        long = "output",
        visible_short_alias = 'b',
        visible_alias = "output-file",
        help = "Output file path. Defaults beside the input"
    )]
    pub output: Option<PathBuf>,

    #[arg(
        long = "output_directory",
        visible_alias = "output-directory",
        value_name = "DIR",
        conflicts_with = "output",
        help = "Output directory. ThermoRawFileParser top-level -o/--output_directory is normalized to this option"
    )]
    pub output_directory: Option<PathBuf>,

    #[arg(
        short = 's',
        long,
        default_value_t = false,
        help = "Write output to stdout where the selected format supports streaming"
    )]
    pub stdout: bool,

    #[arg(
        short,
        long,
        default_value = "mzpeak",
        value_parser = parse_output_format,
        help = "Primary output format. Accepts mzPeak plus ThermoRawFileParser values 1 mzML, 2 indexed mzML, 3 Parquet, 4 None"
    )]
    pub format: OutputFormat,

    #[arg(
        short = 'm',
        long,
        default_value = "none",
        value_parser = parse_metadata_format,
        help = "Metadata sidecar format: 0 JSON, 1 TXT, 2 None"
    )]
    pub metadata: MetadataFormat,

    #[arg(
        short = 'c',
        long = "metadata_output_file",
        visible_alias = "metadata-output-file",
        help = "Metadata sidecar output file"
    )]
    pub metadata_output_file: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = SignalLayout::Chunked)]
    pub signal_layout: SignalLayout,

    #[arg(
        long,
        default_value_t = 50.0,
        help = "m/z chunk width for chunked mzPeak output"
    )]
    pub chunk_size: f64,

    #[arg(
        long,
        default_value_t = 3,
        help = "Zstd compression level for mzPeak output"
    )]
    pub compression_level: i32,

    #[arg(long, value_enum, default_value_t = PeakMode::Vendor)]
    pub peak_mode: PeakMode,

    #[arg(
        short = 'p',
        long = "noPeakPicking",
        visible_alias = "no-peak-picking",
        num_args = 0..=1,
        default_missing_value = "all",
        require_equals = false,
        value_parser = parse_ms_level_filter,
        help = "ThermoRawFileParser-compatible peak-picking disable selector; accepted for compatibility"
    )]
    pub no_peak_picking: Option<String>,

    #[arg(
        long,
        help = "Limit the number of spectra written (the first N in reader order; for TDF that is MS1-first, so small limits may omit MS2). Intended for development smoke tests"
    )]
    pub limit_spectra: Option<usize>,

    #[arg(
        short = 'L',
        long = "ms-level",
        visible_alias = "msLevel",
        value_parser = parse_ms_level_filter,
        help = "Select MS levels, for example 1,2, 1-3, or 2-"
    )]
    pub ms_level: Option<String>,

    #[arg(long, help = "Directory containing Bruker timsdata runtime libraries")]
    pub sdk_lib_dir: Option<PathBuf>,

    #[arg(long, help = "Path to libbaf2sql_c.so or baf2sql_c.dll for BAF input")]
    pub baf2sql_lib: Option<PathBuf>,

    #[arg(
        long = "calibration-mode",
        default_value = "auto",
        value_parser = parse_baf_calibration_mode,
        help = "BAF calibration access mode: auto, vendor, or raw"
    )]
    pub calibration_mode: BafCalibrationMode,

    #[arg(
        long = "use_raw_calibration",
        visible_alias = "use-raw-calibration",
        default_value_t = false,
        help = "BAF/TDF-compatible alias forcing raw calibration arrays where supported"
    )]
    pub use_raw_calibration: bool,

    #[arg(
        long = "mode",
        value_enum,
        help = "Bruker-compatible spectrum mode: centroid, profile, or raw"
    )]
    pub mode: Option<BrukerSpectrumMode>,

    #[arg(
        long = "profile-missing",
        default_value = "auto",
        value_parser = parse_baf_profile_missing_mode,
        help = "BAF profile-mode behavior when a spectrum lacks profile arrays: auto, line, or fail"
    )]
    pub profile_missing: BafProfileMissingMode,

    #[arg(long = "ms2_only", visible_alias = "ms2-only", default_value_t = false)]
    pub ms2_only: bool,

    #[arg(
        long = "start-frame",
        help = "Start frame/spectrum id for Bruker inputs"
    )]
    pub start_frame: Option<i64>,

    #[arg(long = "end-frame", help = "End frame/spectrum id for Bruker inputs")]
    pub end_frame: Option<i64>,

    #[arg(
        long = "merge-pasef-precursors",
        visible_alias = "tdf-ms2-per-precursor",
        default_value_t = false,
        help = "TDF DDA-PASEF: emit one summed MS2 per unique precursor instead of one per frame-event (default). Collapses a precursor's ~2.3 frames into a single spectrum"
    )]
    pub merge_pasef_precursors: bool,

    #[arg(
        long = "ims-compact",
        default_value_t = false,
        help = "TDF (PoC): write a compact integer-TOF + implicit-mobility Parquet facet (≈half the size; m/z reconstructed from the stored sqrt calibration). Not a standard mzPeak — opt-in"
    )]
    pub ims_compact: bool,

    #[arg(
        long = "consolidate-ms2",
        default_value_t = false,
        help = "TDF (with --ims-compact): collapse the ion-mobility dimension for MS2 — sum fragment intensity per m/z into one flat peak, dropping per-peak mobility. MS1 keeps its IM profile"
    )]
    pub consolidate_ms2: bool,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub include_chromatograms: bool,

    #[arg(
        short = 'g',
        long,
        default_value_t = false,
        help = "Accepted for ThermoRawFileParser compatibility; mzPeak uses internal archive compression"
    )]
    pub gzip: bool,

    #[arg(
        short = 'z',
        long = "noZlibCompression",
        visible_alias = "no-zlib-compression",
        default_value_t = false,
        help = "Accepted for ThermoRawFileParser compatibility"
    )]
    pub no_zlib_compression: bool,

    #[arg(
        short = 'a',
        long = "allDetectors",
        visible_alias = "all-detectors",
        default_value_t = false,
        help = "Decode supported non-MS detector signals such as UV/PDA into mzPeak detector facets when present"
    )]
    pub all_detectors: bool,

    #[arg(
        long = "vendor-metadata",
        num_args = 0..=1,
        default_missing_value = "tall",
        require_equals = false,
        value_parser = parse_vendor_metadata_mode,
        help = "mzPeak: emit verbatim Bruker vendor metadata facets. Accepts tall, wide, or both; Bruker currently emits the tall file-level facet, plus a per-spectrum facet for TSF and BAF inputs"
    )]
    pub vendor_metadata: Option<VendorMetadataMode>,

    #[arg(
        long = "vendor-metadata-json",
        num_args = 0..=1,
        default_missing_value = "",
        require_equals = false,
        help = "mzPeak: write a readable file-level vendor metadata JSON sidecar; optional path defaults to <output>.vendor.json"
    )]
    pub vendor_metadata_json: Option<String>,

    #[arg(
        short = 'e',
        long = "ignoreInstrumentErrors",
        visible_alias = "ignore-instrument-errors",
        default_value_t = false,
        help = "Accepted for ThermoRawFileParser compatibility"
    )]
    pub ignore_instrument_errors: bool,

    #[arg(
        short = 'x',
        long = "excludeExceptionData",
        visible_alias = "exclude-exception-data",
        default_value_t = false,
        help = "Accepted for ThermoRawFileParser compatibility"
    )]
    pub exclude_exception_data: bool,

    #[arg(
        short = 'N',
        long = "noiseData",
        visible_alias = "noise-data",
        default_value_t = false,
        help = "Accepted for ThermoRawFileParser compatibility"
    )]
    pub noise_data: bool,

    #[arg(
        short = 'C',
        long = "chargeData",
        visible_alias = "charge-data",
        default_value_t = false,
        help = "Accepted for ThermoRawFileParser compatibility"
    )]
    pub charge_data: bool,

    #[arg(
        short = 'w',
        long,
        visible_alias = "warningsAreErrors",
        default_value_t = false
    )]
    pub warnings_are_errors: bool,

    #[arg(
        short = 'u',
        long = "s3_url",
        visible_alias = "s3-url",
        help = "ThermoRawFileParser-compatible S3 URL; not implemented yet"
    )]
    pub s3_url: Option<String>,

    #[arg(
        short = 'k',
        long = "s3_accesskeyid",
        visible_alias = "s3-accesskeyid",
        help = "ThermoRawFileParser-compatible S3 access key; not implemented yet"
    )]
    pub s3_access_key_id: Option<String>,

    #[arg(
        short = 't',
        long = "s3_secretaccesskey",
        visible_alias = "s3-secretaccesskey",
        help = "ThermoRawFileParser-compatible S3 secret key; not implemented yet"
    )]
    pub s3_secret_access_key: Option<String>,

    #[arg(
        short = 'n',
        long = "s3_bucketname",
        visible_alias = "s3-bucketname",
        help = "ThermoRawFileParser-compatible S3 bucket name; not implemented yet"
    )]
    pub s3_bucket_name: Option<String>,

    #[arg(
        long,
        default_value_t = false,
        help = "Validate the output after writing it"
    )]
    pub validate: bool,

    #[arg(
        long,
        action = clap::ArgAction::Set,
        help = "Run semantic validation after conversion; pass false for quick mzPeak validation"
    )]
    pub validation_semantic: Option<bool>,

    #[arg(
        long,
        value_parser = clap::value_parser!(u64).range(1..),
        help = "Maximum seconds to wait for the external validator during conversion; defaults to 600"
    )]
    pub validation_timeout_seconds: Option<u64>,

    #[arg(long, help = "Write the external validation JSON report to this path")]
    pub validation_report: Option<PathBuf>,

    #[arg(long, help = "Path to the mzpeak-validate executable")]
    pub mzpeak_validator: Option<PathBuf>,

    #[arg(long, help = "Path to the mzML validator executable")]
    pub mzml_validator: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct InspectArgs {
    #[arg(help = "Input Bruker .d directory")]
    pub input: PathBuf,

    #[arg(long, value_enum, default_value_t = InspectFormat::Text)]
    pub format: InspectFormat,

    #[arg(
        short = 'w',
        long,
        visible_alias = "warningsAreErrors",
        default_value_t = false
    )]
    pub warnings_are_errors: bool,

    #[arg(
        long,
        default_value_t = 0,
        help = "Preview the first N spectra through the pure-Rust reader when supported"
    )]
    pub preview_spectra: usize,

    #[arg(long, help = "Directory containing Bruker runtime libraries")]
    pub sdk_lib_dir: Option<PathBuf>,

    #[arg(long, help = "Path to libbaf2sql_c.so or baf2sql_c.dll for BAF input")]
    pub baf2sql_lib: Option<PathBuf>,

    #[arg(
        long = "calibration-mode",
        default_value = "auto",
        value_parser = parse_baf_calibration_mode,
        help = "BAF calibration access mode for inspect when SDK access is requested: auto, vendor, or raw"
    )]
    pub calibration_mode: BafCalibrationMode,

    #[arg(
        long = "use_raw_calibration",
        visible_alias = "use-raw-calibration",
        default_value_t = false,
        help = "BAF-compatible alias forcing raw calibration arrays where supported"
    )]
    pub use_raw_calibration: bool,
}

#[derive(Debug, Args)]
pub struct ValidateArgs {
    #[arg(help = "Output file to validate")]
    pub input: PathBuf,

    #[arg(long, value_parser = parse_output_format, help = "Output format. Defaults from file extension")]
    pub format: Option<OutputFormat>,

    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub semantic: bool,

    #[arg(
        long,
        default_value_t = 600,
        value_parser = clap::value_parser!(u64).range(1..),
        help = "Maximum seconds to wait for the external validator"
    )]
    pub timeout_seconds: u64,

    #[arg(long, help = "Write the external validation JSON report to this path")]
    pub report: Option<PathBuf>,

    #[arg(long, help = "Path to the mzpeak-validate executable")]
    pub mzpeak_validator: Option<PathBuf>,

    #[arg(long, help = "Path to the mzML validator executable")]
    pub mzml_validator: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct QueryArgs {
    #[arg(short = 'i', long = "input", help = "Input Bruker .d directory")]
    pub input: Option<PathBuf>,

    #[arg(long = "scan", help = "Scan or scan range query")]
    pub scan: Option<String>,

    #[arg(short = 'b', long = "output", help = "Output file")]
    pub output: Option<PathBuf>,

    #[arg(
        short = 'p',
        long = "noPeakPicking",
        visible_alias = "no-peak-picking",
        default_value_t = false
    )]
    pub no_peak_picking: bool,

    #[arg(
        short = 'w',
        long,
        visible_alias = "warningsAreErrors",
        default_value_t = false
    )]
    pub warnings_are_errors: bool,

    #[arg(short = 's', long = "stdout", default_value_t = false)]
    pub stdout: bool,
}

#[derive(Debug, Args)]
pub struct XicArgs {
    #[arg(short = 'i', long = "input", help = "Input Bruker .d directory")]
    pub input: Option<PathBuf>,

    #[arg(short = 'j', long = "json", help = "JSON XIC request file")]
    pub json: Option<PathBuf>,

    #[arg(short = 'b', long = "output", help = "Output file")]
    pub output: Option<PathBuf>,

    #[arg(
        short = 'o',
        long = "output_directory",
        visible_alias = "output-directory",
        help = "Output directory"
    )]
    pub output_directory: Option<PathBuf>,

    #[arg(
        short = 'w',
        long,
        visible_alias = "warningsAreErrors",
        default_value_t = false
    )]
    pub warnings_are_errors: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    MzPeak,
    MzMl,
    Parquet,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataFormat {
    Json,
    Text,
    None,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum SignalLayout {
    Point,
    Chunked,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum PeakMode {
    Vendor,
    Generic,
    Both,
    None,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum BrukerSpectrumMode {
    Centroid,
    Profile,
    Raw,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum InspectFormat {
    Text,
    Json,
}

pub fn parse_output_format(value: &str) -> Result<OutputFormat, String> {
    match normalize_choice(value).as_str() {
        "mzpeak" => Ok(OutputFormat::MzPeak),
        "1" | "mzml" => Ok(OutputFormat::MzMl),
        "2" | "indexmzml" | "indexedmzml" => Ok(OutputFormat::MzMl),
        "3" | "parquet" => Ok(OutputFormat::Parquet),
        "4" | "none" | "nooutput" => Ok(OutputFormat::None),
        _ => Err("expected mzPeak, mzML, 1 mzML, 2 indexed mzML, 3 Parquet, or 4 None".to_string()),
    }
}

pub fn parse_metadata_format(value: &str) -> Result<MetadataFormat, String> {
    match normalize_choice(value).as_str() {
        "0" | "json" => Ok(MetadataFormat::Json),
        "1" | "txt" | "text" => Ok(MetadataFormat::Text),
        "2" | "none" | "nooutput" => Ok(MetadataFormat::None),
        _ => Err("expected 0 JSON, 1 TXT, or 2 None".to_string()),
    }
}

pub fn parse_vendor_metadata_mode(value: &str) -> Result<VendorMetadataMode, String> {
    match normalize_choice(value).as_str() {
        "tall" => Ok(VendorMetadataMode::Tall),
        "wide" => Ok(VendorMetadataMode::Wide),
        "both" => Ok(VendorMetadataMode::Both),
        _ => Err("expected tall, wide, or both".to_string()),
    }
}

pub fn parse_baf_calibration_mode(value: &str) -> Result<BafCalibrationMode, String> {
    match normalize_choice(value).as_str() {
        "auto" => Ok(BafCalibrationMode::Auto),
        "vendor" | "calibrated" => Ok(BafCalibrationMode::Vendor),
        "raw" => Ok(BafCalibrationMode::Raw),
        _ => Err("expected auto, vendor, or raw".to_string()),
    }
}

pub fn parse_baf_profile_missing_mode(value: &str) -> Result<BafProfileMissingMode, String> {
    match normalize_choice(value).as_str() {
        "auto" => Ok(BafProfileMissingMode::Auto),
        "line" | "centroid" => Ok(BafProfileMissingMode::Line),
        "fail" | "error" => Ok(BafProfileMissingMode::Fail),
        _ => Err("expected auto, line, or fail".to_string()),
    }
}

pub fn parse_log_filter(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    match normalize_choice(trimmed).as_str() {
        "0" | "silent" | "off" | "none" => Ok("off".to_string()),
        "1" | "verbose" => Ok("debug".to_string()),
        "2" | "default" => Ok("info".to_string()),
        "3" | "warning" | "warn" => Ok("warn".to_string()),
        "4" | "error" => Ok("error".to_string()),
        _ if trimmed.is_empty() => Err("logging filter cannot be empty".to_string()),
        _ => Ok(trimmed.to_string()),
    }
}

pub fn parse_ms_level_filter(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok("all".to_string());
    }
    if trimmed.is_empty() {
        return Err("MS level filter cannot be empty".to_string());
    }

    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err("MS level filter contains an empty element".to_string());
        }
        if let Some((start, end)) = part.split_once('-') {
            parse_optional_ms_level_bound(start)?;
            parse_optional_ms_level_bound(end)?;
            if start.is_empty() && end.is_empty() {
                return Err("MS level interval cannot be '-'".to_string());
            }
        } else {
            parse_ms_level_bound(part)?;
        }
    }

    Ok(trimmed.to_string())
}

fn parse_optional_ms_level_bound(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Ok(())
    } else {
        parse_ms_level_bound(value)
    }
}

fn parse_ms_level_bound(value: &str) -> Result<(), String> {
    let parsed = value
        .trim()
        .parse::<u8>()
        .map_err(|_| format!("invalid MS level '{value}'"))?;
    if parsed == 0 {
        Err("MS levels are 1-based".to_string())
    } else {
        Ok(())
    }
}

fn normalize_choice(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(['-', '_'], "")
}

fn normalize_thermo_args<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.len() <= 1 {
        return args;
    }

    let Some(first) = args.get(1).and_then(|value| value.to_str()) else {
        return args;
    };

    if is_help_or_version(first) || is_known_subcommand(first) {
        return args;
    }

    if !looks_like_thermo_convert_invocation(&args[1..]) {
        return args;
    }

    let mut normalized = Vec::with_capacity(args.len() + 1);
    normalized.push(args[0].clone());
    normalized.push(OsString::from("convert"));

    let mut index = 1;
    while index < args.len() {
        if arg_eq(&args[index], "-o") || arg_eq(&args[index], "--output_directory") {
            normalized.push(OsString::from("--output_directory"));
            if let Some(value) = args.get(index + 1) {
                if value_looks_like_option(value) {
                    index += 1;
                } else {
                    normalized.push(value.clone());
                    index += 2;
                }
            } else {
                index += 1;
            }
            continue;
        }

        if let Some(value) = split_prefixed_value(&args[index], "-o=") {
            normalized.push(join_os("--output_directory=", &value));
            index += 1;
            continue;
        }

        if let Some(value) = split_prefixed_value(&args[index], "--output_directory=") {
            normalized.push(join_os("--output_directory=", &value));
            index += 1;
            continue;
        }

        normalized.push(args[index].clone());
        index += 1;
    }

    normalized
}

fn looks_like_thermo_convert_invocation(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.to_str(),
            Some("-i" | "--input" | "-d" | "--input_directory" | "--input-directory")
        ) || has_any_prefix(
            arg,
            &[
                "-i=",
                "--input=",
                "-d=",
                "--input_directory=",
                "--input-directory=",
            ],
        )
    })
}

fn is_help_or_version(value: &str) -> bool {
    matches!(value, "-h" | "--help" | "-V" | "--version")
}

fn is_known_subcommand(value: &str) -> bool {
    matches!(
        value,
        "convert" | "inspect" | "validate" | "query" | "xic" | "help"
    )
}

fn arg_eq(arg: &OsStr, value: &str) -> bool {
    arg.to_str() == Some(value)
}

fn has_any_prefix(arg: &OsStr, prefixes: &[&str]) -> bool {
    prefixes
        .iter()
        .any(|prefix| arg.to_str().is_some_and(|value| value.starts_with(prefix)))
}

fn split_prefixed_value(arg: &OsStr, prefix: &str) -> Option<OsString> {
    arg.to_str()
        .and_then(|value| value.strip_prefix(prefix))
        .map(OsString::from)
}

fn value_looks_like_option(value: &OsStr) -> bool {
    value
        .to_str()
        .is_some_and(|value| value.starts_with('-') && value != "-")
}

fn join_os(prefix: &str, value: &OsStr) -> OsString {
    let mut combined = OsString::from(prefix);
    combined.push(value);
    combined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_convert_command() {
        let cli = Cli::parse_from(["brfp", "convert", "sample.d"]);
        match cli.command {
            Command::Convert(args) => {
                assert_eq!(args.input, Some(PathBuf::from("sample.d")));
                assert_eq!(args.format, OutputFormat::MzPeak);
                assert_eq!(args.signal_layout, SignalLayout::Chunked);
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn parses_thermo_style_top_level_convert_command() {
        let cli = Cli::parse_from_compat([
            "brfp",
            "-i=sample.d",
            "-b=out.mzpeak",
            "-f=mzPeak",
            "-m=0",
            "-c=meta.json",
            "-L=1-2",
            "-w",
            "-l=3",
        ]);
        assert_eq!(cli.log, "warn");
        match cli.command {
            Command::Convert(args) => {
                assert_eq!(args.input_file, Some(PathBuf::from("sample.d")));
                assert_eq!(args.output, Some(PathBuf::from("out.mzpeak")));
                assert_eq!(args.metadata, MetadataFormat::Json);
                assert_eq!(args.metadata_output_file, Some(PathBuf::from("meta.json")));
                assert_eq!(args.ms_level, Some("1-2".to_string()));
                assert!(args.warnings_are_errors);
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn normalizes_thermo_output_directory_short_option() {
        let cli = Cli::parse_from_compat(["brfp", "-i", "sample.d", "-o", "outdir"]);
        match cli.command {
            Command::Convert(args) => {
                assert_eq!(args.input_file, Some(PathBuf::from("sample.d")));
                assert_eq!(args.output_directory, Some(PathBuf::from("outdir")));
                assert_eq!(args.output, None);
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn top_level_output_directory_does_not_consume_following_flag() {
        let normalized = normalize_thermo_args(["brfp", "-i", "sample.d", "-o", "-f", "mzPeak"]);
        let error = Cli::try_parse_from(normalized).unwrap_err();
        assert!(error.to_string().contains("--output_directory"));
    }

    #[test]
    fn parses_convert_validation_options() {
        let cli = Cli::parse_from([
            "brfp",
            "convert",
            "sample.d",
            "--validate",
            "--validation-semantic",
            "false",
            "--validation-timeout-seconds",
            "42",
            "--validation-report",
            "report.json",
        ]);
        match cli.command {
            Command::Convert(args) => {
                assert!(args.validate);
                assert_eq!(args.validation_semantic, Some(false));
                assert_eq!(args.validation_timeout_seconds, Some(42));
                assert_eq!(args.validation_report, Some(PathBuf::from("report.json")));
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn parses_validate_report_and_validator_path() {
        let cli = Cli::parse_from([
            "brfp",
            "validate",
            "sample.mzpeak",
            "--format",
            "mz-peak",
            "--report",
            "report.json",
            "--timeout-seconds",
            "12",
            "--mzpeak-validator",
            "mzpeak-validate",
        ]);
        match cli.command {
            Command::Validate(args) => {
                assert_eq!(args.input, PathBuf::from("sample.mzpeak"));
                assert_eq!(args.format, Some(OutputFormat::MzPeak));
                assert_eq!(args.report, Some(PathBuf::from("report.json")));
                assert_eq!(args.timeout_seconds, 12);
                assert_eq!(
                    args.mzpeak_validator,
                    Some(PathBuf::from("mzpeak-validate"))
                );
            }
            _ => panic!("expected validate command"),
        }
    }

    #[test]
    fn parses_thermo_numeric_formats() {
        assert_eq!(parse_output_format("1").unwrap(), OutputFormat::MzMl);
        assert_eq!(parse_output_format("2").unwrap(), OutputFormat::MzMl);
        assert_eq!(parse_output_format("3").unwrap(), OutputFormat::Parquet);
        assert_eq!(parse_output_format("4").unwrap(), OutputFormat::None);
        assert_eq!(parse_metadata_format("1").unwrap(), MetadataFormat::Text);
        assert_eq!(
            parse_vendor_metadata_mode("both").unwrap(),
            VendorMetadataMode::Both
        );
        assert_eq!(parse_log_filter("1").unwrap(), "debug");
    }

    #[test]
    fn parses_vendor_metadata_flags() {
        let cli = Cli::parse_from([
            "brfp",
            "convert",
            "sample.d",
            "--vendor-metadata",
            "--vendor-metadata-json",
        ]);
        match cli.command {
            Command::Convert(args) => {
                assert_eq!(args.vendor_metadata, Some(VendorMetadataMode::Tall));
                assert_eq!(args.vendor_metadata_json, Some(String::new()));
            }
            _ => panic!("expected convert command"),
        }

        let cli = Cli::parse_from([
            "brfp",
            "convert",
            "sample.d",
            "--vendor-metadata=wide",
            "--vendor-metadata-json=meta.json",
        ]);
        match cli.command {
            Command::Convert(args) => {
                assert_eq!(args.vendor_metadata, Some(VendorMetadataMode::Wide));
                assert_eq!(args.vendor_metadata_json, Some("meta.json".to_string()));
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn rejects_mgf_output_format() {
        assert!(parse_output_format("0").is_err());
        assert!(parse_output_format("mgf").is_err());
    }
}
