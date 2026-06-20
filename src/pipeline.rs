use crate::{
    baf::{BafCalibrationMode, BafOpenOptions},
    cli::{
        BrukerSpectrumMode, ConvertArgs, InspectArgs, InspectFormat, MetadataFormat, OutputFormat,
        PeakMode, SignalLayout, ValidateArgs,
    },
    input::{BrukerFormat, RunInspection, inspect_bruker_run, inspect_bruker_run_with_baf_options},
    mzpeak_writer::{
        BafMzPeakWriteOptions, MzPeakWriteOptions, default_mzpeak_output_path, write_baf_to_mzml,
        write_baf_to_mzpeak, write_tdf_to_mzml, write_tdf_to_mzpeak, write_tsf_to_mzml,
        write_tsf_to_mzpeak,
    },
    sdk::SdkDiscovery,
    tsf::{TsfReaderPreview, preview_tsf_spectra},
    validation::{DEFAULT_VALIDATION_TIMEOUT_SECONDS, ValidationOptions, validate_file},
};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::Duration,
};

pub type BrfpResult<T> = Result<T, BrfpError>;

#[derive(Debug, thiserror::Error)]
pub enum BrfpError {
    #[error("{0}")]
    NotImplemented(&'static str),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("reader error: {0}")]
    Reader(String),
    #[error("writer error: {0}")]
    Writer(String),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("{kind} array length {left} does not match {right}")]
    AxisLengthMismatch {
        kind: &'static str,
        left: usize,
        right: usize,
    },
    #[error("invalid {kind} at point {index}: {value}")]
    NonFiniteValue {
        kind: &'static str,
        index: usize,
        value: f64,
    },
    #[error("invalid m/z value at point {index}: {value}")]
    NonPositiveMz { index: usize, value: f64 },
    #[error("unsupported platform: {0}")]
    UnsupportedPlatform(String),
    #[error("warnings treated as errors: {0}")]
    WarningsAsErrors(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn run_convert(args: ConvertArgs) -> BrfpResult<()> {
    let input = resolve_convert_input(&args)?;
    tracing::info!(
        input = %input.display(),
        format = ?args.format,
        signal_layout = ?args.signal_layout,
        "conversion requested"
    );
    validate_convert_validation_args(&args)?;
    validate_convert_compatibility_args(&args)?;

    let inspection = inspect_convert_input(&input, &args)?;
    if inspection.binary_file.is_none() {
        return Err(BrfpError::InvalidInput(format!(
            "{} is missing the required binary payload",
            input.display()
        )));
    }
    fail_on_warnings(args.warnings_are_errors, &inspection.warnings)?;
    handle_accepted_compatibility_noops(&args, inspection.format)?;

    if inspection.format != BrukerFormat::Baf
        && let Some(sdk) = SdkDiscovery::discover(args.sdk_lib_dir.as_deref())?
    {
        tracing::info!(
            sdk_platform = %sdk.platform,
            sdk_library = %sdk.library_path.display(),
            "Bruker SDK discovered"
        );
    }

    write_metadata_sidecar(&args, &input, &inspection)?;

    match (inspection.format, args.format) {
        (BrukerFormat::Tsf, OutputFormat::MzPeak) => {
            let output = resolve_convert_output(&args, &input)?;
            let vendor_metadata_json = resolve_vendor_metadata_json_output(&args, &output)?;
            let report = write_tsf_to_mzpeak(
                &input,
                &output,
                MzPeakWriteOptions {
                    limit_spectra: args.limit_spectra,
                    vendor_metadata_mode: args.vendor_metadata,
                    vendor_metadata_json,
                    include_detector_data: args.all_detectors,
                    include_chromatograms: args.include_chromatograms,
                    ..Default::default()
                },
            )?;
            println!(
                "Wrote {} spectra to {}",
                report.spectra_written,
                report.output.display()
            );
            if args.validate {
                validate_file(ValidationOptions {
                    input: &output,
                    format: Some(args.format),
                    semantic: args.validation_semantic.unwrap_or(true),
                    report: args.validation_report.as_deref(),
                    mzpeak_validator: args.mzpeak_validator.as_deref(),
                    mzml_validator: args.mzml_validator.as_deref(),
                    timeout: Duration::from_secs(
                        args.validation_timeout_seconds
                            .unwrap_or(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
                    ),
                })?;
            }
            Ok(())
        }
        (BrukerFormat::Tdf, OutputFormat::MzPeak) if args.ims_compact => {
            let output = resolve_convert_output(&args, &input)?;
            let report = crate::mzpeak_writer::write_tdf_to_ims_compact(
                &input,
                &output,
                args.limit_spectra,
                args.consolidate_ms2,
            )?;
            println!(
                "Wrote {} spectra (ims-compact) to {}",
                report.spectra_written,
                report.output.display()
            );
            Ok(())
        }
        (BrukerFormat::Tdf, OutputFormat::MzPeak) => {
            let output = resolve_convert_output(&args, &input)?;
            let vendor_metadata_json = resolve_vendor_metadata_json_output(&args, &output)?;
            let report = write_tdf_to_mzpeak(
                &input,
                &output,
                MzPeakWriteOptions {
                    limit_spectra: args.limit_spectra,
                    vendor_metadata_mode: args.vendor_metadata,
                    vendor_metadata_json,
                    include_chromatograms: args.include_chromatograms,
                    merge_pasef_precursors: args.merge_pasef_precursors,
                    ..Default::default()
                },
            )?;
            println!(
                "Wrote {} spectra to {}",
                report.spectra_written,
                report.output.display()
            );
            maybe_validate_output(&args, &output)?;
            Ok(())
        }
        (BrukerFormat::Baf, OutputFormat::MzPeak) => {
            let output = resolve_convert_output(&args, &input)?;
            let vendor_metadata_json = resolve_vendor_metadata_json_output(&args, &output)?;
            let report = write_baf_to_mzpeak(
                &input,
                &output,
                BafMzPeakWriteOptions {
                    mzpeak: MzPeakWriteOptions {
                        limit_spectra: args.limit_spectra,
                        vendor_metadata_mode: args.vendor_metadata,
                        vendor_metadata_json,
                        include_detector_data: args.all_detectors,
                        include_chromatograms: args.include_chromatograms,
                        ..Default::default()
                    },
                    open_options: baf_open_options_from_convert(&args)?,
                    prefer_profile: prefer_baf_profile(&args),
                    profile_missing: args.profile_missing,
                    ms2_only: args.ms2_only,
                    ms_level_filter: args.ms_level.clone(),
                    start_spectrum_id: args.start_frame,
                    end_spectrum_id: args.end_frame,
                },
            )?;
            println!(
                "Wrote {} spectra to {}",
                report.spectra_written,
                report.output.display()
            );
            if args.validate {
                validate_file(ValidationOptions {
                    input: &output,
                    format: Some(args.format),
                    semantic: args.validation_semantic.unwrap_or(true),
                    report: args.validation_report.as_deref(),
                    mzpeak_validator: args.mzpeak_validator.as_deref(),
                    mzml_validator: args.mzml_validator.as_deref(),
                    timeout: Duration::from_secs(
                        args.validation_timeout_seconds
                            .unwrap_or(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
                    ),
                })?;
            }
            Ok(())
        }
        (BrukerFormat::Tsf, OutputFormat::MzMl) => {
            let output = mzml_output_path(&args, &input);
            let report = write_tsf_to_mzml(&input, &output, args.limit_spectra)?;
            println!(
                "Wrote {} spectra to {}",
                report.spectra_written,
                report.output.display()
            );
            maybe_validate_output(&args, &output)?;
            Ok(())
        }
        (BrukerFormat::Baf, OutputFormat::MzMl) => {
            let output = mzml_output_path(&args, &input);
            let report = write_baf_to_mzml(
                &input,
                &output,
                BafMzPeakWriteOptions {
                    mzpeak: MzPeakWriteOptions {
                        limit_spectra: args.limit_spectra,
                        ..Default::default()
                    },
                    open_options: baf_open_options_from_convert(&args)?,
                    prefer_profile: prefer_baf_profile(&args),
                    profile_missing: args.profile_missing,
                    ms2_only: args.ms2_only,
                    ms_level_filter: args.ms_level.clone(),
                    start_spectrum_id: args.start_frame,
                    end_spectrum_id: args.end_frame,
                },
            )?;
            println!(
                "Wrote {} spectra to {}",
                report.spectra_written,
                report.output.display()
            );
            maybe_validate_output(&args, &output)?;
            Ok(())
        }
        (BrukerFormat::Tdf, OutputFormat::MzMl) => {
            let output = mzml_output_path(&args, &input);
            let report = write_tdf_to_mzml(
                &input,
                &output,
                args.limit_spectra,
                args.merge_pasef_precursors,
            )?;
            println!(
                "Wrote {} spectra to {}",
                report.spectra_written,
                report.output.display()
            );
            maybe_validate_output(&args, &output)?;
            Ok(())
        }
        (_, OutputFormat::Parquet) => Err(BrfpError::NotImplemented(
            "raw Parquet output is accepted for ThermoRawFileParser CLI compatibility but is not implemented; use mzPeak for Parquet-backed output",
        )),
        (_, OutputFormat::None) => {
            println!(
                "No spectra output requested for {}; metadata output {}",
                input.display(),
                if args.metadata == MetadataFormat::None {
                    "was not requested"
                } else {
                    "completed"
                }
            );
            Ok(())
        }
    }
}

pub fn inspect_run(args: InspectArgs) -> BrfpResult<()> {
    tracing::info!(input = %args.input.display(), format = ?args.format, "inspect requested");

    let inspection =
        inspect_bruker_run_with_baf_options(&args.input, inspect_baf_options_from_inspect(&args)?)?;
    fail_on_warnings(args.warnings_are_errors, &inspection.warnings)?;
    let preview = if args.preview_spectra > 0 {
        match inspection.format {
            BrukerFormat::Tsf => Some(preview_tsf_spectra(&args.input, args.preview_spectra)?),
            BrukerFormat::Baf => {
                return Err(BrfpError::NotImplemented(
                    "BAF spectrum preview is not wired for inspect yet; use convert --limit-spectra for a smoke test",
                ));
            }
            BrukerFormat::Tdf => {
                return Err(BrfpError::NotImplemented(
                    "TDF spectrum preview is not wired yet",
                ));
            }
        }
    } else {
        None
    };

    match args.format {
        InspectFormat::Text => {
            println!("{}", inspection.to_text());
            if let Some(preview) = preview {
                println!();
                print_tsf_preview(&preview);
            }
        }
        InspectFormat::Json => {
            let response = InspectResponse {
                inspection,
                spectrum_preview: preview,
            };
            serde_json::to_writer_pretty(std::io::stdout(), &response)?;
            println!();
        }
    }
    Ok(())
}

pub fn validate_output(args: ValidateArgs) -> BrfpResult<()> {
    tracing::info!(input = %args.input.display(), semantic = args.semantic, "validation requested");

    validate_file(ValidationOptions {
        input: &args.input,
        format: args.format,
        semantic: args.semantic,
        report: args.report.as_deref(),
        mzpeak_validator: args.mzpeak_validator.as_deref(),
        mzml_validator: args.mzml_validator.as_deref(),
        timeout: Duration::from_secs(args.timeout_seconds),
    })?;

    Ok(())
}

fn validate_convert_validation_args(args: &ConvertArgs) -> BrfpResult<()> {
    if args.validate {
        return Ok(());
    }

    let mut validation_only_args = Vec::new();
    if args.validation_report.is_some() {
        validation_only_args.push("--validation-report");
    }
    if args.validation_semantic.is_some() {
        validation_only_args.push("--validation-semantic");
    }
    if args.validation_timeout_seconds.is_some() {
        validation_only_args.push("--validation-timeout-seconds");
    }
    if args.mzpeak_validator.is_some() {
        validation_only_args.push("--mzpeak-validator");
    }
    if args.mzml_validator.is_some() {
        validation_only_args.push("--mzml-validator");
    }

    if validation_only_args.is_empty() {
        Ok(())
    } else {
        let verb = if validation_only_args.len() == 1 {
            "requires"
        } else {
            "require"
        };
        Err(BrfpError::InvalidInput(format!(
            "{} {verb} --validate",
            validation_only_args.join(", ")
        )))
    }
}

fn validate_convert_compatibility_args(args: &ConvertArgs) -> BrfpResult<()> {
    if args.stdout {
        return Err(BrfpError::NotImplemented(
            "stdout conversion output is not implemented yet; use -b/--output or an output directory",
        ));
    }
    if args.s3_url.is_some()
        || args.s3_access_key_id.is_some()
        || args.s3_secret_access_key.is_some()
        || args.s3_bucket_name.is_some()
    {
        return Err(BrfpError::NotImplemented(
            "S3 output options are accepted for ThermoRawFileParser CLI compatibility but are not implemented yet",
        ));
    }
    if args.metadata_output_file.is_some() && args.metadata == MetadataFormat::None {
        return Err(BrfpError::InvalidInput(
            "--metadata-output-file requires --metadata json or --metadata txt".to_string(),
        ));
    }
    if args.format == OutputFormat::None && args.metadata == MetadataFormat::None {
        return Err(BrfpError::InvalidInput(
            "--format none requires --metadata json or --metadata txt".to_string(),
        ));
    }
    if let (Some(start), Some(end)) = (args.start_frame, args.end_frame)
        && start > end
    {
        return Err(BrfpError::InvalidInput(format!(
            "--start-frame ({start}) must be less than or equal to --end-frame ({end})"
        )));
    }
    Ok(())
}

fn inspect_convert_input(input: &Path, args: &ConvertArgs) -> BrfpResult<RunInspection> {
    if is_baf_run_directory(input) {
        inspect_bruker_run_with_baf_options(input, Some(baf_open_options_from_convert(args)?))
    } else {
        inspect_bruker_run(input)
    }
}

fn resolve_convert_input(args: &ConvertArgs) -> BrfpResult<PathBuf> {
    if let Some(input) = &args.input {
        return Ok(input.clone());
    }
    if let Some(input) = &args.input_file {
        return Ok(input.clone());
    }
    if let Some(input_directory) = &args.input_directory {
        if is_bruker_run_directory(input_directory) {
            return Ok(input_directory.clone());
        }
        return Err(BrfpError::NotImplemented(
            "ThermoRawFileParser batch input-directory conversion is not implemented yet; pass one Bruker .d directory with -i/--input, or use -d only when the path itself is a .d run",
        ));
    }

    Err(BrfpError::InvalidInput(
        "specify an input Bruker .d directory as a positional argument, -i/--input, or -d/--input_directory".to_string(),
    ))
}

fn is_bruker_run_directory(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("d"))
        || path.join("analysis.tsf").is_file()
        || path.join("analysis.tdf").is_file()
        || path.join("analysis.baf").is_file()
}

fn is_baf_run_directory(path: &Path) -> bool {
    path.join("analysis.baf").is_file()
}

fn resolve_convert_output(args: &ConvertArgs, input: &Path) -> BrfpResult<PathBuf> {
    if let Some(output) = &args.output {
        return Ok(output.clone());
    }

    if let Some(output_directory) = &args.output_directory {
        let default_output = default_mzpeak_output_path(input);
        let file_name = default_output.file_name().ok_or_else(|| {
            BrfpError::InvalidInput(format!(
                "cannot derive output file name from input {}",
                input.display()
            ))
        })?;
        return Ok(output_directory.join(file_name));
    }

    Ok(default_mzpeak_output_path(input))
}

/// Output path for mzML: honor an explicit `-o`, else default beside the input
/// with a `.mzML` extension.
/// Run the configured validator over `output` when `--validate` was requested.
fn maybe_validate_output(args: &ConvertArgs, output: &Path) -> BrfpResult<()> {
    if !args.validate {
        return Ok(());
    }
    validate_file(ValidationOptions {
        input: output,
        format: Some(args.format),
        semantic: args.validation_semantic.unwrap_or(true),
        report: args.validation_report.as_deref(),
        mzpeak_validator: args.mzpeak_validator.as_deref(),
        mzml_validator: args.mzml_validator.as_deref(),
        timeout: Duration::from_secs(
            args.validation_timeout_seconds
                .unwrap_or(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
        ),
    })?;
    Ok(())
}

fn mzml_output_path(args: &ConvertArgs, input: &Path) -> PathBuf {
    if let Some(output) = &args.output {
        return output.clone();
    }
    let default = default_mzpeak_output_path(input).with_extension("mzML");
    if let Some(dir) = &args.output_directory {
        if let Some(name) = default.file_name() {
            return dir.join(name);
        }
    }
    default
}

fn write_metadata_sidecar(
    args: &ConvertArgs,
    input: &Path,
    inspection: &RunInspection,
) -> BrfpResult<()> {
    let metadata_format = args.metadata;
    if metadata_format == MetadataFormat::None {
        return Ok(());
    }

    let output = metadata_output_path(args, input, metadata_format)?;
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    match metadata_format {
        MetadataFormat::Json => {
            let file = File::create(&output)?;
            serde_json::to_writer_pretty(file, inspection)?;
        }
        MetadataFormat::Text => {
            fs::write(&output, inspection.to_text())?;
        }
        MetadataFormat::None => unreachable!("handled above"),
    }

    println!("Wrote metadata to {}", output.display());
    Ok(())
}

fn metadata_output_path(
    args: &ConvertArgs,
    input: &Path,
    metadata_format: MetadataFormat,
) -> BrfpResult<PathBuf> {
    if let Some(output) = &args.metadata_output_file {
        return Ok(output.clone());
    }

    let extension = match metadata_format {
        MetadataFormat::Json => "json",
        MetadataFormat::Text => "txt",
        MetadataFormat::None => unreachable!("metadata output path requested for none"),
    };
    let stem = input
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            BrfpError::InvalidInput(format!(
                "cannot derive metadata file name from input {}",
                input.display()
            ))
        })?;
    let file_name = format!("{stem}.{extension}");

    if let Some(output_directory) = &args.output_directory {
        return Ok(output_directory.join(file_name));
    }
    if let Some(output) = &args.output
        && let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty())
    {
        return Ok(parent.join(file_name));
    }
    if let Some(parent) = input.parent().filter(|path| !path.as_os_str().is_empty()) {
        return Ok(parent.join(file_name));
    }

    Ok(PathBuf::from(file_name))
}

fn handle_accepted_compatibility_noops(args: &ConvertArgs, format: BrukerFormat) -> BrfpResult<()> {
    let warnings = accepted_compatibility_noop_warnings(args, format);
    for warning in &warnings {
        tracing::warn!("{warning}");
    }

    if args.warnings_are_errors && !warnings.is_empty() {
        return Err(BrfpError::WarningsAsErrors(warnings.join("; ")));
    }

    Ok(())
}

fn accepted_compatibility_noop_warnings(
    args: &ConvertArgs,
    format: BrukerFormat,
) -> Vec<&'static str> {
    let mut warnings = Vec::new();

    if args.gzip {
        warnings.push(
            "--gzip/-g accepted for CLI compatibility; mzPeak archive compression is controlled internally",
        );
    }
    if args.no_zlib_compression {
        warnings.push(
            "--noZlibCompression/-z accepted for CLI compatibility; zlib output is not used by the current mzPeak writer",
        );
    }
    if args.ignore_instrument_errors {
        warnings.push(
            "--ignoreInstrumentErrors/-e accepted for CLI compatibility; Bruker instrument-error filtering is not implemented yet",
        );
    }
    if args.exclude_exception_data {
        warnings.push(
            "--excludeExceptionData/-x accepted for CLI compatibility; exception-data filtering is not implemented yet",
        );
    }
    if args.noise_data {
        warnings.push(
            "--noiseData/-N accepted for CLI compatibility; noise arrays are not exported by the current writer",
        );
    }
    if args.charge_data {
        warnings.push(
            "--chargeData/-C accepted for CLI compatibility; charge-state export is not implemented by the current writer",
        );
    }
    if args.signal_layout != SignalLayout::Chunked {
        warnings.push(
            "--signal-layout is accepted but the current mzPeak writer path still uses its default layout",
        );
    }
    if (args.chunk_size - 50.0).abs() > f64::EPSILON {
        warnings.push(
            "--chunk-size is accepted but the current mzPeak writer path still uses its default chunking",
        );
    }
    if args.compression_level != 3 {
        warnings.push(
            "--compression-level is accepted but the current mzPeak writer path still uses its default compression settings",
        );
    }
    if args.peak_mode != PeakMode::Vendor {
        warnings.push(
            "--peak-mode is accepted for CLI compatibility but alternate peak-picking modes are not implemented yet",
        );
    }
    if args.no_peak_picking.is_some() && format != BrukerFormat::Baf {
        warnings.push(
            "--noPeakPicking/-p accepted for CLI compatibility; peak picking mode is not applied by the current TSF writer",
        );
    }
    if args.ms_level.is_some() && format != BrukerFormat::Baf {
        warnings.push(
            "--ms-level/-L accepted for CLI compatibility; MS-level filtering is not applied by the current TSF writer",
        );
    }
    if (args.start_frame.is_some() || args.end_frame.is_some()) && format != BrukerFormat::Baf {
        warnings.push(
            "--start-frame/--end-frame accepted for CLI compatibility; frame-range filtering is currently applied only by the BAF writer",
        );
    }

    warnings
}

fn baf_open_options_from_convert(args: &ConvertArgs) -> BrfpResult<BafOpenOptions> {
    Ok(BafOpenOptions {
        sdk_lib_dir: args.sdk_lib_dir.clone(),
        baf2sql_lib: args.baf2sql_lib.clone(),
        calibration_mode: effective_baf_calibration_mode(
            args.calibration_mode,
            args.use_raw_calibration,
        )?,
    })
}

fn inspect_baf_options_from_inspect(args: &InspectArgs) -> BrfpResult<Option<BafOpenOptions>> {
    if args.sdk_lib_dir.is_none()
        && args.baf2sql_lib.is_none()
        && args.calibration_mode == BafCalibrationMode::Auto
        && !args.use_raw_calibration
    {
        return Ok(None);
    }
    Ok(Some(BafOpenOptions {
        sdk_lib_dir: args.sdk_lib_dir.clone(),
        baf2sql_lib: args.baf2sql_lib.clone(),
        calibration_mode: effective_baf_calibration_mode(
            args.calibration_mode,
            args.use_raw_calibration,
        )?,
    }))
}

fn effective_baf_calibration_mode(
    calibration_mode: BafCalibrationMode,
    use_raw_calibration: bool,
) -> BrfpResult<BafCalibrationMode> {
    if use_raw_calibration && calibration_mode == BafCalibrationMode::Vendor {
        return Err(BrfpError::InvalidInput(
            "--use_raw_calibration conflicts with --calibration-mode vendor".to_string(),
        ));
    }
    if use_raw_calibration {
        Ok(BafCalibrationMode::Raw)
    } else {
        Ok(calibration_mode)
    }
}

fn prefer_baf_profile(args: &ConvertArgs) -> bool {
    matches!(
        args.mode,
        Some(BrukerSpectrumMode::Profile | BrukerSpectrumMode::Raw)
    ) || args.no_peak_picking.is_some()
}

fn resolve_vendor_metadata_json_output(
    args: &ConvertArgs,
    mzpeak_output: &Path,
) -> BrfpResult<Option<PathBuf>> {
    let Some(raw_path) = &args.vendor_metadata_json else {
        return Ok(None);
    };

    if !raw_path.is_empty() {
        return Ok(Some(PathBuf::from(raw_path)));
    }

    let stem = mzpeak_output
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            BrfpError::InvalidInput(format!(
                "cannot derive vendor metadata JSON file name from output {}",
                mzpeak_output.display()
            ))
        })?;
    let file_name = format!("{stem}.vendor.json");
    let output = mzpeak_output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .map(|parent| parent.join(&file_name))
        .unwrap_or_else(|| PathBuf::from(file_name));
    Ok(Some(output))
}

fn fail_on_warnings(warnings_are_errors: bool, warnings: &[String]) -> BrfpResult<()> {
    if warnings_are_errors && !warnings.is_empty() {
        return Err(BrfpError::WarningsAsErrors(warnings.join("; ")));
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct InspectResponse {
    #[serde(flatten)]
    inspection: RunInspection,
    #[serde(skip_serializing_if = "Option::is_none")]
    spectrum_preview: Option<TsfReaderPreview>,
}

fn print_tsf_preview(preview: &TsfReaderPreview) {
    println!(
        "Spectrum preview: first {} of {}",
        preview.spectra.len(),
        preview.spectrum_count
    );
    for spectrum in &preview.spectra {
        let mz_range = match (spectrum.mz_min, spectrum.mz_max) {
            (Some(min), Some(max)) => format!("{min:.4}-{max:.4}"),
            _ => "n/a".to_string(),
        };
        let base_peak = match (spectrum.base_peak_mz, spectrum.base_peak_intensity) {
            (Some(mz), Some(intensity)) => format!("{mz:.4} @ {intensity:.0}"),
            _ => "n/a".to_string(),
        };
        println!(
            "- #{}: points={}, mz={}, base_peak={}, tic={:.0}",
            spectrum.index, spectrum.point_count, mz_range, base_peak, spectrum.total_ion_current
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{PeakMode, SignalLayout};

    #[test]
    fn warnings_are_errors_rejects_compatibility_noops() {
        let mut args = base_convert_args();
        args.warnings_are_errors = true;
        args.ms_level = Some("1".to_string());

        let error = handle_accepted_compatibility_noops(&args, BrukerFormat::Tsf).unwrap_err();
        assert!(matches!(error, BrfpError::WarningsAsErrors(_)));
        assert!(
            error
                .to_string()
                .contains("MS-level filtering is not applied")
        );
    }

    #[test]
    fn rejects_inverted_frame_range() {
        let mut args = base_convert_args();
        args.start_frame = Some(10);
        args.end_frame = Some(2);

        let error = validate_convert_compatibility_args(&args).unwrap_err();
        assert!(matches!(error, BrfpError::InvalidInput(_)));
        assert!(error.to_string().contains("--start-frame"));
    }

    fn base_convert_args() -> ConvertArgs {
        ConvertArgs {
            input: Some(PathBuf::from("sample.d")),
            input_file: None,
            input_directory: None,
            output: None,
            output_directory: None,
            stdout: false,
            format: OutputFormat::MzPeak,
            metadata: MetadataFormat::None,
            metadata_output_file: None,
            signal_layout: SignalLayout::Chunked,
            chunk_size: 50.0,
            compression_level: 3,
            peak_mode: PeakMode::Vendor,
            no_peak_picking: None,
            limit_spectra: None,
            ms_level: None,
            sdk_lib_dir: None,
            baf2sql_lib: None,
            calibration_mode: BafCalibrationMode::Auto,
            use_raw_calibration: false,
            mode: None,
            profile_missing: Default::default(),
            ms2_only: false,
            merge_pasef_precursors: false,
            ims_compact: false,
            consolidate_ms2: false,
            start_frame: None,
            end_frame: None,
            include_chromatograms: true,
            gzip: false,
            no_zlib_compression: false,
            all_detectors: false,
            vendor_metadata: None,
            vendor_metadata_json: None,
            ignore_instrument_errors: false,
            exclude_exception_data: false,
            noise_data: false,
            charge_data: false,
            warnings_are_errors: false,
            s3_url: None,
            s3_access_key_id: None,
            s3_secret_access_key: None,
            s3_bucket_name: None,
            validate: false,
            validation_semantic: None,
            validation_timeout_seconds: None,
            validation_report: None,
            mzpeak_validator: None,
            mzml_validator: None,
        }
    }
}
