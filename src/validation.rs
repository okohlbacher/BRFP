use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    time::Duration,
};

use wait_timeout::ChildExt;

use crate::{
    cli::OutputFormat,
    pipeline::{BrfpError, BrfpResult},
};

const MZPEAK_VALIDATOR_ENV: &str = "BRFP_MZPEAK_VALIDATOR";
const MZML_VALIDATOR_ENV: &str = "BRFP_MZML_VALIDATOR";
pub const DEFAULT_VALIDATION_TIMEOUT_SECONDS: u64 = 600;

#[derive(Debug, Clone, Copy)]
pub struct ValidationOptions<'a> {
    pub input: &'a Path,
    pub format: Option<OutputFormat>,
    pub semantic: bool,
    pub report: Option<&'a Path>,
    pub mzpeak_validator: Option<&'a Path>,
    pub mzml_validator: Option<&'a Path>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationReport {
    pub format: OutputFormat,
    pub validator: PathBuf,
}

pub fn validate_file(options: ValidationOptions<'_>) -> BrfpResult<ValidationReport> {
    if !options.input.exists() {
        return Err(BrfpError::InvalidInput(format!(
            "{} does not exist",
            options.input.display()
        )));
    }

    let format = options
        .format
        .map(Ok)
        .unwrap_or_else(|| infer_validation_format(options.input))?;

    match format {
        OutputFormat::MzPeak => validate_mzpeak(options, format),
        OutputFormat::MzMl => validate_mzml(options, format),
        OutputFormat::Parquet | OutputFormat::None => Err(BrfpError::InvalidInput(
            "validation supports mzPeak and mzML outputs only".to_string(),
        )),
    }
}

fn validate_mzpeak(
    options: ValidationOptions<'_>,
    format: OutputFormat,
) -> BrfpResult<ValidationReport> {
    let validator = resolve_validator(
        options.mzpeak_validator,
        MZPEAK_VALIDATOR_ENV,
        &["mzpeak-validate"],
    )?;

    let mut command = Command::new(&validator);
    command.arg(options.input);
    if !options.semantic {
        command.arg("--quick");
    }
    if let Some(report) = options.report {
        command.arg("--json").arg(report);
    }

    run_validator(command, &validator, "mzPeak", options.timeout)?;
    println!(
        "Validation passed: {} ({})",
        options.input.display(),
        format_name(format)
    );

    Ok(ValidationReport { format, validator })
}

fn validate_mzml(
    options: ValidationOptions<'_>,
    format: OutputFormat,
) -> BrfpResult<ValidationReport> {
    if let Some(report) = options.report {
        return Err(BrfpError::Validation(format!(
            "mzML report output is not wired because validator report arguments are tool-specific; remove --report or run the mzML validator directly for {}",
            report.display()
        )));
    }

    let validator = resolve_validator(
        options.mzml_validator,
        MZML_VALIDATOR_ENV,
        &["mzml-validator", "mzMLValidator", "jmzml-validator"],
    )?;

    let mut command = Command::new(&validator);
    command.arg(options.input);

    run_validator(command, &validator, "mzML", options.timeout)?;
    println!(
        "Validation passed: {} ({})",
        options.input.display(),
        format_name(format)
    );

    Ok(ValidationReport { format, validator })
}

fn run_validator(
    mut command: Command,
    validator: &Path,
    label: &str,
    timeout: Duration,
) -> BrfpResult<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            BrfpError::Validation(format!(
                "failed to execute {label} validator {}: {error}",
                validator.display()
            ))
        })?;

    if child
        .wait_timeout(timeout)
        .map_err(|error| {
            BrfpError::Validation(format!(
                "failed while waiting for {label} validator {}: {error}",
                validator.display()
            ))
        })?
        .is_none()
    {
        if let Err(error) = child.kill() {
            tracing::warn!(
                validator = %validator.display(),
                %label,
                %error,
                "failed to terminate timed-out validator"
            );
        }
        let output = child.wait_with_output().map_err(|error| {
            BrfpError::Validation(format!(
                "failed to collect timed-out {label} validator output from {}: {error}",
                validator.display()
            ))
        })?;
        forward_output(&output);
        return Err(BrfpError::Validation(format!(
            "{label} validator {} timed out after {}{}",
            validator.display(),
            format_timeout(timeout),
            validation_output_summary(&output)
        )));
    }

    let output = child.wait_with_output().map_err(|error| {
        BrfpError::Validation(format!(
            "failed to collect {label} validator output from {}: {error}",
            validator.display()
        ))
    })?;

    forward_output(&output);

    if !output.status.success() {
        return Err(BrfpError::Validation(format!(
            "{label} validator {} failed with status {}{}",
            validator.display(),
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "terminated by signal".to_string()),
            validation_output_summary(&output)
        )));
    }

    Ok(())
}

fn format_timeout(timeout: Duration) -> String {
    if timeout.as_secs() > 0 {
        format!("{}s", timeout.as_secs())
    } else {
        format!("{}ms", timeout.as_millis())
    }
}

fn forward_output(output: &Output) {
    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
}

fn validation_output_summary(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let summary = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    if summary.is_empty() {
        String::new()
    } else {
        format!(": {summary}")
    }
}

fn infer_validation_format(input: &Path) -> BrfpResult<OutputFormat> {
    if input.is_dir() && input.join("mzpeak_index.json").is_file() {
        return Ok(OutputFormat::MzPeak);
    }

    let extension = input
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match extension.as_deref() {
        Some("mzpeak") => Ok(OutputFormat::MzPeak),
        Some("mzml") => Ok(OutputFormat::MzMl),
        _ => Err(BrfpError::InvalidInput(format!(
            "cannot infer validation format from {}; pass --format mz-peak or --format mz-ml",
            input.display()
        ))),
    }
}

fn resolve_validator(
    explicit: Option<&Path>,
    env_var: &str,
    command_names: &[&str],
) -> BrfpResult<PathBuf> {
    if let Some(path) = explicit {
        return resolve_explicit_validator(path);
    }

    if let Some(path) = env::var_os(env_var).filter(|value| !value.is_empty()) {
        return resolve_explicit_validator(Path::new(&path));
    }

    for command_name in command_names {
        if let Some(path) = find_in_path(command_name) {
            return Ok(path);
        }
    }

    Err(BrfpError::Validation(format!(
        "external validator not found; set {env_var}, pass an explicit validator path, or install one of: {}",
        command_names.join(", ")
    )))
}

fn resolve_explicit_validator(path: &Path) -> BrfpResult<PathBuf> {
    if path.components().count() == 1 {
        return find_in_path(path.to_string_lossy().as_ref()).ok_or_else(|| {
            BrfpError::Validation(format!(
                "validator executable {} was not found on PATH",
                path.display()
            ))
        });
    }

    if is_executable_file(path) {
        Ok(path.to_path_buf())
    } else if path.is_file() {
        Err(BrfpError::Validation(format!(
            "validator executable {} is not executable",
            path.display()
        )))
    } else {
        Err(BrfpError::Validation(format!(
            "validator executable {} does not exist or is not a file",
            path.display()
        )))
    }
}

fn find_in_path(command_name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        for candidate in executable_candidates(&directory, command_name) {
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn executable_candidates(directory: &Path, command_name: &str) -> Vec<PathBuf> {
    let command = Path::new(command_name);
    if command.components().count() > 1 {
        return vec![command.to_path_buf()];
    }

    let base = directory.join(command_name);

    #[cfg(windows)]
    {
        let mut candidates = vec![base.clone()];
        if base.extension().is_none() {
            let pathext = env::var_os("PATHEXT")
                .unwrap_or_else(|| std::ffi::OsString::from(".COM;.EXE;.BAT;.CMD"));
            for extension in pathext.to_string_lossy().split(';') {
                let extension = extension.trim();
                if extension.is_empty() {
                    continue;
                }
                let extension = extension.trim_start_matches('.');
                candidates.push(base.with_extension(extension));
            }
        }
        candidates
    }

    #[cfg(not(windows))]
    {
        vec![base]
    }
}

fn format_name(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::MzPeak => "mzPeak",
        OutputFormat::MzMl => "mzML",
        OutputFormat::Parquet => "Parquet",
        OutputFormat::None => "None",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn infers_mzpeak_archive_and_unpacked_directory() {
        assert_eq!(
            infer_validation_format(Path::new("sample.mzpeak")).unwrap(),
            OutputFormat::MzPeak
        );
        assert_eq!(
            infer_validation_format(Path::new("sample.mzML")).unwrap(),
            OutputFormat::MzMl
        );

        let tempdir = tempfile::tempdir().unwrap();
        std::fs::write(tempdir.path().join("mzpeak_index.json"), "{}").unwrap();
        assert_eq!(
            infer_validation_format(tempdir.path()).unwrap(),
            OutputFormat::MzPeak
        );
    }

    #[test]
    fn runs_mzpeak_validator_and_writes_report() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.mzpeak");
        let report = tempdir.path().join("report.json");
        std::fs::write(&input, b"archive").unwrap();
        let validator = fake_validator(tempdir.path(), "ok", 0);

        let result = validate_file(ValidationOptions {
            input: &input,
            format: None,
            semantic: true,
            report: Some(&report),
            mzpeak_validator: Some(&validator),
            mzml_validator: None,
            timeout: Duration::from_secs(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
        })
        .unwrap();

        assert_eq!(result.format, OutputFormat::MzPeak);
        assert!(report.is_file());
        let report_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert_eq!(report_json["valid"], true);
    }

    #[test]
    fn fails_when_validator_fails() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.mzpeak");
        std::fs::write(&input, b"archive").unwrap();
        let validator = fake_validator(tempdir.path(), "invalid", 7);

        let error = validate_file(ValidationOptions {
            input: &input,
            format: None,
            semantic: true,
            report: None,
            mzpeak_validator: Some(&validator),
            mzml_validator: None,
            timeout: Duration::from_secs(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
        })
        .unwrap_err();

        assert!(error.to_string().contains("failed with status 7"));
    }

    #[test]
    fn rejects_mzml_report_path_until_tool_mapping_exists() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.mzML");
        let report = tempdir.path().join("report.json");
        std::fs::write(&input, b"mzml").unwrap();

        let error = validate_file(ValidationOptions {
            input: &input,
            format: None,
            semantic: true,
            report: Some(&report),
            mzpeak_validator: None,
            mzml_validator: None,
            timeout: Duration::from_secs(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
        })
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("mzML report output is not wired")
        );
    }

    #[cfg(unix)]
    #[test]
    fn explicit_validator_must_be_executable() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.mzpeak");
        let validator = tempdir.path().join("not-executable");
        std::fs::write(&input, b"archive").unwrap();
        std::fs::write(&validator, b"#!/usr/bin/env sh\nexit 0\n").unwrap();

        let error = validate_file(ValidationOptions {
            input: &input,
            format: None,
            semantic: true,
            report: None,
            mzpeak_validator: Some(&validator),
            mzml_validator: None,
            timeout: Duration::from_secs(DEFAULT_VALIDATION_TIMEOUT_SECONDS),
        })
        .unwrap_err();

        assert!(error.to_string().contains("is not executable"));
    }

    #[cfg(unix)]
    #[test]
    fn times_out_hung_validator() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.mzpeak");
        let validator = sleeping_validator(tempdir.path());
        std::fs::write(&input, b"archive").unwrap();

        let error = validate_file(ValidationOptions {
            input: &input,
            format: None,
            semantic: true,
            report: None,
            mzpeak_validator: Some(&validator),
            mzml_validator: None,
            timeout: Duration::from_millis(10),
        })
        .unwrap_err();

        assert!(error.to_string().contains("timed out"));
    }

    #[cfg(unix)]
    fn fake_validator(directory: &Path, message: &str, exit_code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("fake-validator");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "#!/usr/bin/env sh\nset -eu\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--json\" ]; then\n    shift\n    printf '{{\"valid\":true}}\\n' > \"$1\"\n  fi\n  shift || true\ndone\necho {message}\nexit {exit_code}\n"
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(unix)]
    fn sleeping_validator(directory: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join("sleeping-validator");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "#!/usr/bin/env sh\nsleep 5\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(windows)]
    fn fake_validator(directory: &Path, message: &str, exit_code: i32) -> PathBuf {
        let path = directory.join("fake-validator.cmd");
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            "@echo off\r\n:loop\r\nif \"%1\"==\"\" goto done\r\nif \"%1\"==\"--json\" (\r\n  shift\r\n  echo {{\"valid\":true}}>\"%1\"\r\n)\r\nshift\r\ngoto loop\r\n:done\r\necho {message}\r\nexit /b {exit_code}\r\n"
        )
        .unwrap();
        path
    }
}
