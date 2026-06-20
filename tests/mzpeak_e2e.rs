use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use mzdata::{
    meta::SpectrumType,
    prelude::*,
    spectrum::{ArrayType, ChromatogramType},
};
use mzpeak_prototyping::reader::MzPeakReader;
use predicates::prelude::*;

fn private_fixture(name: &str) -> Option<PathBuf> {
    let root = std::env::var_os("BRFP_TEST_PRIVATE_DATA").map(PathBuf::from)?;
    let input = root.join(name);
    input.is_dir().then_some(input)
}

fn baf_fixture(name: &str) -> Option<PathBuf> {
    let root = std::env::var_os("BRFP_TEST_BAF_DATA").map(PathBuf::from)?;
    let input = root.join(name);
    input.is_dir().then_some(input)
}

fn baf2sql_lib() -> Option<PathBuf> {
    let path = std::env::var_os("BRFP_BAF2SQL_LIB").map(PathBuf::from)?;
    path.is_file().then_some(path)
}

fn mtbls18_uv_cdf_root() -> Option<PathBuf> {
    let root = std::env::var_os("BRFP_TEST_MTBLS18_UV_CDF").map(PathBuf::from)?;
    root.is_dir().then_some(root)
}

#[test]
fn thermo_style_cli_writes_readable_mzpeak_and_metadata() {
    let Some(input) = private_fixture("timsTOF_autoMSMS_Urine_6min_pos.d") else {
        eprintln!("skipping private-data e2e test; BRFP_TEST_PRIVATE_DATA is not set");
        return;
    };
    let tempdir = tempfile::tempdir().unwrap();
    let output = tempdir.path().join("urine-pos.mzpeak");
    let metadata = tempdir.path().join("urine-pos.json");

    Command::cargo_bin("brfp")
        .unwrap()
        .arg("-i")
        .arg(&input)
        .arg("-b")
        .arg(&output)
        .args(["-f", "mzPeak", "-m", "0", "--limit-spectra", "2"])
        .arg("-c")
        .arg(&metadata)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("Wrote metadata")
                .and(predicate::str::contains("Wrote 2 spectra")),
        );

    assert_readable_mzpeak(&output, 2);
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata).unwrap()).unwrap();
    assert_eq!(
        metadata
            .get("input_path")
            .and_then(|value| value.as_str())
            .map(Path::new),
        Some(input.as_path())
    );
}

#[test]
fn thermo_style_output_directory_derives_mzpeak_name() {
    let Some(input) = private_fixture("timsTOF_autoMSMS_Urine_6min_neg.d") else {
        eprintln!("skipping private-data e2e test; BRFP_TEST_PRIVATE_DATA is not set");
        return;
    };
    let tempdir = tempfile::tempdir().unwrap();
    let output_dir = tempdir.path().join("out");
    let output = output_dir.join("timsTOF_autoMSMS_Urine_6min_neg.mzpeak");

    Command::cargo_bin("brfp")
        .unwrap()
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output_dir)
        .args(["-f", "mzPeak", "--limit-spectra", "1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote 1 spectra"));

    assert_readable_mzpeak(&output, 1);
}

#[test]
fn baf_cli_writes_readable_mzpeak_and_decoded_uv_spectra() {
    let Some(input) = baf_fixture("LTI225-67-3pos_1-F,2_01_24595.d") else {
        eprintln!("skipping BAF e2e test; BRFP_TEST_BAF_DATA is not set");
        return;
    };
    let Some(baf2sql_lib) = baf2sql_lib() else {
        eprintln!("skipping BAF e2e test; BRFP_BAF2SQL_LIB is not set");
        return;
    };
    let tempdir = tempfile::tempdir().unwrap();
    let output = tempdir.path().join("baf-pos.mzpeak");
    let vendor_json = tempdir.path().join("baf-pos.vendor.json");

    let mut command = Command::cargo_bin("brfp").unwrap();
    command
        .arg("convert")
        .arg(&input)
        .arg("-b")
        .arg(&output)
        .arg("--baf2sql-lib")
        .arg(&baf2sql_lib)
        .args([
            "-f",
            "mzPeak",
            "--limit-spectra",
            "2",
            "--vendor-metadata",
            "--allDetectors",
            "--calibration-mode",
            "auto",
        ])
        .arg("--vendor-metadata-json")
        .arg(&vendor_json);
    if let Some(parent) = baf2sql_lib.parent() {
        command.env("LD_LIBRARY_PATH", parent);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote 2 spectra"));

    assert_readable_mzpeak(&output, 2);
    assert!(vendor_json.is_file());

    let mut reader = MzPeakReader::new(&output).unwrap();
    let entries = reader
        .file_index()
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    assert!(entries.contains(&"vendor_file_metadata.parquet"));
    assert!(
        !entries
            .iter()
            .any(|entry| entry.starts_with("vendor_payloads/")),
        "raw vendor detector payloads must not be forwarded into mzPeak"
    );
    assert_eq!(reader.len_wavelength_spectra(), 2);
    let uv_spectrum = reader.get_wavelength_spectrum(0).unwrap();
    assert_eq!(
        uv_spectrum.spectrum_type(),
        Some(SpectrumType::AbsorptionSpectrum)
    );
    assert!(uv_spectrum.start_time().is_finite());
    let uv_arrays = uv_spectrum.raw_arrays().unwrap();
    let wavelengths = uv_arrays
        .get(&ArrayType::WavelengthArray)
        .unwrap()
        .to_f64()
        .unwrap();
    let intensities = uv_arrays
        .get(&ArrayType::IntensityArray)
        .unwrap()
        .to_f64()
        .unwrap();
    assert_eq!(wavelengths.len(), 622);
    assert_eq!(intensities.len(), 622);
    assert!((wavelengths[0] - 190.0).abs() < 1e-9);
    assert!((wavelengths[wavelengths.len() - 1] - 500.0).abs() < 1e-9);
    assert!(intensities.iter().all(|value| value.is_finite()));

    assert_eq!(reader.len_chromatograms(), 3);
    let uv_chromatogram = reader.get_chromatogram(0).unwrap();
    assert_eq!(
        uv_chromatogram.chromatogram_type(),
        ChromatogramType::AbsorptionChromatogram
    );
    let times = uv_chromatogram.time().unwrap();
    let trace = uv_chromatogram.intensity().unwrap();
    assert_eq!(times.len(), 2);
    assert_eq!(trace.len(), 2);
    assert!(times.iter().all(|value| value.is_finite()));
    assert!(trace.iter().all(|value| value.is_finite()));

    let vendor_metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&vendor_json).unwrap()).unwrap();
    let rows = vendor_metadata
        .get("rows")
        .and_then(|value| value.as_array())
        .unwrap();
    assert!(rows.iter().any(|row| {
        row.get("label").and_then(|value| value.as_str()) == Some("channel_wavelength_nm")
            && row.get("value").and_then(|value| value.as_str()) == Some("254")
    }));
    assert!(rows.iter().any(|row| {
        row.get("category").and_then(|value| value.as_str()) == Some("uv_u2_header")
            && row.get("label").and_then(|value| value.as_str()) == Some("intensity_unit")
            && row.get("value").and_then(|value| value.as_str()) == Some("mAU")
    }));
    assert!(rows.iter().any(|row| {
        row.get("category").and_then(|value| value.as_str()) == Some("uv_u2_header")
            && row.get("label").and_then(|value| value.as_str()) == Some("spectrum_count")
            && row
                .get("value")
                .and_then(|value| value.as_str())
                .is_some_and(|value| value.parse::<u32>().is_ok())
    }));
}

#[test]
fn baf_uv_wavelength_spectra_match_mtbls18_netcdf_reference() {
    let Some(baf_root) = std::env::var_os("BRFP_TEST_BAF_DATA").map(PathBuf::from) else {
        eprintln!("skipping MTBLS18 UV comparison; BRFP_TEST_BAF_DATA is not set");
        return;
    };
    let Some(baf2sql_lib) = baf2sql_lib() else {
        eprintln!("skipping MTBLS18 UV comparison; BRFP_BAF2SQL_LIB is not set");
        return;
    };
    let Some(cdf_root) = mtbls18_uv_cdf_root() else {
        eprintln!("skipping MTBLS18 UV comparison; BRFP_TEST_MTBLS18_UV_CDF is not set");
        return;
    };

    let cases = [
        (
            "LTI225-41-3neg_1-D,5_01_24321.d",
            "LTI225-41-3neg_1-D__5_01_24321.cdf",
        ),
        (
            "LTI225-67-3pos_1-F,2_01_24595.d",
            "LTI225-67-3pos_1-F__2_01_24595.cdf",
        ),
    ];
    let tempdir = tempfile::tempdir().unwrap();
    let spectra_to_compare = 5usize;

    for (fixture_name, cdf_name) in cases {
        let input = baf_root.join(fixture_name);
        assert!(input.is_dir(), "missing BAF fixture {}", input.display());
        let cdf_path = cdf_root.join(cdf_name);
        assert!(
            cdf_path.is_file(),
            "missing MTBLS18 UV NetCDF reference {}",
            cdf_path.display()
        );
        let output = tempdir.path().join(format!("{fixture_name}.mzpeak"));
        run_baf_mzpeak_conversion(&input, &baf2sql_lib, &output, spectra_to_compare, true);
        assert_mzpeak_wavelengths_match_netcdf(&output, &cdf_path, spectra_to_compare);
    }
}

fn assert_readable_mzpeak(output: &Path, expected_spectra: usize) {
    assert!(output.exists(), "{} was not created", output.display());
    let mut reader = MzPeakReader::new(output).unwrap();
    assert_eq!(reader.len(), expected_spectra);
    let spectrum = reader.get_spectrum(0).unwrap();
    let peaks = &spectrum.peaks.as_ref().unwrap().peaks;
    assert!(!peaks.is_empty());
    assert!(
        peaks
            .iter()
            .all(|peak| peak.mz.is_finite() && peak.mz > 0.0)
    );
}

fn run_baf_mzpeak_conversion(
    input: &Path,
    baf2sql_lib: &Path,
    output: &Path,
    limit_spectra: usize,
    include_detectors: bool,
) {
    let mut command = Command::cargo_bin("brfp").unwrap();
    command
        .arg("convert")
        .arg(input)
        .arg("-b")
        .arg(output)
        .arg("--baf2sql-lib")
        .arg(baf2sql_lib)
        .args(["-f", "mzPeak", "--calibration-mode", "auto"])
        .arg("--limit-spectra")
        .arg(limit_spectra.to_string());
    if include_detectors {
        command.arg("--allDetectors");
    }
    if let Some(parent) = baf2sql_lib.parent() {
        command.env("LD_LIBRARY_PATH", parent);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "Wrote {limit_spectra} spectra"
        )));
}

fn assert_mzpeak_wavelengths_match_netcdf(
    output: &Path,
    cdf_path: &Path,
    spectra_to_compare: usize,
) {
    let reference = ClassicNetCdf::open(cdf_path).unwrap();
    let mut reader = MzPeakReader::new(output).unwrap();
    assert_eq!(reader.len_wavelength_spectra(), spectra_to_compare);

    for spectrum_index in 0..spectra_to_compare {
        let expected = reference.wavelength_spectrum(spectrum_index).unwrap();
        let spectrum = reader.get_wavelength_spectrum(spectrum_index).unwrap();
        assert_eq!(
            spectrum.spectrum_type(),
            Some(SpectrumType::AbsorptionSpectrum)
        );
        let arrays = spectrum.raw_arrays().unwrap();
        let wavelengths = arrays
            .get(&ArrayType::WavelengthArray)
            .unwrap()
            .to_f64()
            .unwrap();
        let intensities = arrays
            .get(&ArrayType::IntensityArray)
            .unwrap()
            .to_f64()
            .unwrap();
        assert_close_slices(
            &format!("wavelength spectrum {spectrum_index} wavelength axis"),
            &wavelengths,
            &expected.wavelengths,
            1e-4,
        );
        assert_close_slices(
            &format!("wavelength spectrum {spectrum_index} intensity array"),
            &intensities,
            &expected.intensities,
            1e-5,
        );
    }
}

fn assert_close_slices(label: &str, actual: &[f64], expected: &[f64], tolerance: f64) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{label} length mismatch: actual={} expected={}",
        actual.len(),
        expected.len()
    );
    let mut max_delta = 0.0f64;
    let mut max_index = 0usize;
    for (index, (&actual_value, &expected_value)) in actual.iter().zip(expected).enumerate() {
        let delta = (actual_value - expected_value).abs();
        if delta > max_delta {
            max_delta = delta;
            max_index = index;
        }
        assert!(
            delta <= tolerance,
            "{label} mismatch at index {index}: actual={actual_value} expected={expected_value} delta={delta} tolerance={tolerance}"
        );
    }
    assert!(
        max_delta <= tolerance,
        "{label} max delta {max_delta} at index {max_index} exceeds {tolerance}"
    );
}

#[derive(Debug)]
struct ReferenceWavelengthSpectrum {
    wavelengths: Vec<f64>,
    intensities: Vec<f64>,
}

#[derive(Debug)]
struct ClassicNetCdf {
    bytes: Vec<u8>,
    variables: HashMap<String, NetCdfVariable>,
}

#[derive(Debug)]
struct NetCdfVariable {
    type_code: u32,
    shape: Vec<usize>,
    begin: usize,
}

impl ClassicNetCdf {
    fn open(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let mut parser = NetCdfParser::new(&bytes)?;
        parser.parse()
    }

    fn wavelength_spectrum(
        &self,
        spectrum_index: usize,
    ) -> Result<ReferenceWavelengthSpectrum, String> {
        let scan_index = self.read_i32_value("scan_index", spectrum_index)? as usize;
        let point_count = self.read_i32_value("point_count", spectrum_index)? as usize;
        Ok(ReferenceWavelengthSpectrum {
            wavelengths: self.read_f32_slice("mass_values", scan_index, point_count)?,
            intensities: self.read_f32_slice("intensity_values", scan_index, point_count)?,
        })
    }

    fn read_i32_value(&self, name: &str, index: usize) -> Result<i32, String> {
        let variable = self.variable(name)?;
        if variable.type_code != NC_INT {
            return Err(format!("NetCDF variable {name} is not NC_INT"));
        }
        let offset = variable
            .begin
            .checked_add(index.checked_mul(4).ok_or("NetCDF index overflow")?)
            .ok_or("NetCDF offset overflow")?;
        let bytes = self
            .bytes
            .get(offset..offset + 4)
            .ok_or_else(|| format!("NetCDF variable {name} index {index} is out of bounds"))?;
        Ok(i32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_f32_slice(&self, name: &str, start: usize, len: usize) -> Result<Vec<f64>, String> {
        let variable = self.variable(name)?;
        if variable.type_code != NC_FLOAT {
            return Err(format!("NetCDF variable {name} is not NC_FLOAT"));
        }
        let total_len = variable.shape.iter().product::<usize>();
        let end = start
            .checked_add(len)
            .ok_or("NetCDF variable slice overflow")?;
        if end > total_len {
            return Err(format!(
                "NetCDF variable {name} slice {start}..{end} exceeds length {total_len}"
            ));
        }
        let byte_start = variable
            .begin
            .checked_add(start.checked_mul(4).ok_or("NetCDF slice overflow")?)
            .ok_or("NetCDF offset overflow")?;
        let byte_len = len.checked_mul(4).ok_or("NetCDF byte length overflow")?;
        let bytes = self
            .bytes
            .get(byte_start..byte_start + byte_len)
            .ok_or_else(|| format!("NetCDF variable {name} slice is out of bounds"))?;
        let mut values = Vec::with_capacity(len);
        for chunk in bytes.chunks_exact(4) {
            values.push(f32::from_be_bytes(chunk.try_into().unwrap()) as f64);
        }
        Ok(values)
    }

    fn variable(&self, name: &str) -> Result<&NetCdfVariable, String> {
        self.variables
            .get(name)
            .ok_or_else(|| format!("missing NetCDF variable {name}"))
    }
}

struct NetCdfParser<'a> {
    bytes: &'a [u8],
    offset: usize,
    version: u8,
}

impl<'a> NetCdfParser<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() < 8 || bytes.get(0..3) != Some(b"CDF") {
            return Err("not a classic NetCDF file".to_string());
        }
        let version = bytes[3];
        if version != 1 && version != 2 {
            return Err(format!("unsupported NetCDF CDF version {version}"));
        }
        Ok(Self {
            bytes,
            offset: 4,
            version,
        })
    }

    fn parse(&mut self) -> Result<ClassicNetCdf, String> {
        let _num_records = self.read_u32()?;
        let dimensions = self.read_dimensions()?;
        self.skip_attributes()?;
        let variables = self.read_variables(&dimensions)?;
        Ok(ClassicNetCdf {
            bytes: self.bytes.to_vec(),
            variables,
        })
    }

    fn read_dimensions(&mut self) -> Result<Vec<usize>, String> {
        let tag = self.read_u32()?;
        let count = self.read_u32()?;
        if tag == NC_ABSENT {
            if count != 0 {
                return Err("invalid absent NetCDF dimension list".to_string());
            }
            return Ok(Vec::new());
        }
        if tag != NC_DIMENSION {
            return Err(format!("expected NetCDF dimension list tag, got {tag}"));
        }
        let mut dimensions = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let _name = self.read_string()?;
            dimensions.push(self.read_u32()? as usize);
        }
        Ok(dimensions)
    }

    fn read_variables(
        &mut self,
        dimensions: &[usize],
    ) -> Result<HashMap<String, NetCdfVariable>, String> {
        let tag = self.read_u32()?;
        let count = self.read_u32()?;
        if tag == NC_ABSENT {
            if count != 0 {
                return Err("invalid absent NetCDF variable list".to_string());
            }
            return Ok(HashMap::new());
        }
        if tag != NC_VARIABLE {
            return Err(format!("expected NetCDF variable list tag, got {tag}"));
        }

        let mut variables = HashMap::with_capacity(count as usize);
        for _ in 0..count {
            let name = self.read_string()?;
            let dimension_count = self.read_u32()? as usize;
            let mut shape = Vec::with_capacity(dimension_count);
            for _ in 0..dimension_count {
                let dimension_index = self.read_u32()? as usize;
                let Some(&dimension_len) = dimensions.get(dimension_index) else {
                    return Err(format!("invalid NetCDF dimension index {dimension_index}"));
                };
                shape.push(dimension_len);
            }
            self.skip_attributes()?;
            let type_code = self.read_u32()?;
            let _value_size = self.read_u32()?;
            let begin = if self.version == 1 {
                self.read_u32()? as usize
            } else {
                self.read_u64()? as usize
            };
            variables.insert(
                name,
                NetCdfVariable {
                    type_code,
                    shape,
                    begin,
                },
            );
        }
        Ok(variables)
    }

    fn skip_attributes(&mut self) -> Result<(), String> {
        let tag = self.read_u32()?;
        let count = self.read_u32()?;
        if tag == NC_ABSENT {
            if count != 0 {
                return Err("invalid absent NetCDF attribute list".to_string());
            }
            return Ok(());
        }
        if tag != NC_ATTRIBUTE {
            return Err(format!("expected NetCDF attribute list tag, got {tag}"));
        }
        for _ in 0..count {
            let _name = self.read_string()?;
            let type_code = self.read_u32()?;
            let value_count = self.read_u32()? as usize;
            let byte_count = netcdf_type_size(type_code)?
                .checked_mul(value_count)
                .ok_or("NetCDF attribute size overflow")?;
            self.skip_padded(byte_count)?;
        }
        Ok(())
    }

    fn read_string(&mut self) -> Result<String, String> {
        let len = self.read_u32()? as usize;
        let bytes = self
            .bytes
            .get(self.offset..self.offset + len)
            .ok_or("truncated NetCDF string")?;
        let value = std::str::from_utf8(bytes)
            .map_err(|error| format!("invalid NetCDF UTF-8 name: {error}"))?
            .to_string();
        self.skip_padded(len)?;
        Ok(value)
    }

    fn read_u32(&mut self) -> Result<u32, String> {
        let bytes = self
            .bytes
            .get(self.offset..self.offset + 4)
            .ok_or("truncated NetCDF u32")?;
        self.offset += 4;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> Result<u64, String> {
        let bytes = self
            .bytes
            .get(self.offset..self.offset + 8)
            .ok_or("truncated NetCDF u64")?;
        self.offset += 8;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn skip_padded(&mut self, len: usize) -> Result<(), String> {
        let padded = align4(len);
        let next = self
            .offset
            .checked_add(padded)
            .ok_or("NetCDF offset overflow")?;
        if next > self.bytes.len() {
            return Err("truncated NetCDF payload".to_string());
        }
        self.offset = next;
        Ok(())
    }
}

fn align4(len: usize) -> usize {
    len + ((4 - (len % 4)) % 4)
}

fn netcdf_type_size(type_code: u32) -> Result<usize, String> {
    match type_code {
        NC_BYTE | NC_CHAR => Ok(1),
        NC_SHORT => Ok(2),
        NC_INT | NC_FLOAT => Ok(4),
        NC_DOUBLE => Ok(8),
        _ => Err(format!("unsupported NetCDF type code {type_code}")),
    }
}

const NC_ABSENT: u32 = 0;
const NC_DIMENSION: u32 = 10;
const NC_VARIABLE: u32 = 11;
const NC_ATTRIBUTE: u32 = 12;
const NC_BYTE: u32 = 1;
const NC_CHAR: u32 = 2;
const NC_SHORT: u32 = 3;
const NC_INT: u32 = 4;
const NC_FLOAT: u32 = 5;
const NC_DOUBLE: u32 = 6;

// --- Full-workflow cross-validation -----------------------------------------
// Bruker .d --(BRFP)--> mzPeak and mzML; validate each with its own validator;
// then mzML --(mzML2mzPeak)--> mzPeak and check it agrees with the direct mzPeak.
// Gated on external tools via env vars so CI without them skips cleanly:
//   BRFP_TEST_PRIVATE_DATA  - Bruker TSF .d fixtures (existing)
//   BRFP_MZPEAK_VALIDATOR   - mzpeak-validate binary (else ~/Claude/mzPeakValidator)
//   BRFP_MZML2MZPEAK        - mzml2mzpeak binary (else ~/Claude/mzML2mzPeak build)
//   BRFP_MZML_VALIDATOR     - (optional) path to an mzML validator

fn tool_from_env(var: &str) -> Option<PathBuf> {
    let path = std::env::var_os(var).map(PathBuf::from)?;
    path.is_file().then_some(path)
}

/// Locate the mzml2mzpeak binary: `BRFP_MZML2MZPEAK` wins, else the sibling
/// project's built binary under ~/Claude/mzML2mzPeak (release preferred).
fn mzml2mzpeak_tool() -> Option<PathBuf> {
    if let Some(path) = tool_from_env("BRFP_MZML2MZPEAK") {
        return Some(path);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    ["target/release/mzml2mzpeak", "target/debug/mzml2mzpeak"]
        .into_iter()
        .map(|rel| home.join("Claude/mzML2mzPeak").join(rel))
        .find(|p| p.is_file())
}

/// How to invoke the mzPeak validator: an explicit binary, or the source package
/// at ~/Claude/mzPeakValidator run as `python3 -m mzpeak_validator`.
enum MzPeakValidator {
    Bin(PathBuf),
    Module(PathBuf),
}

impl MzPeakValidator {
    /// `BRFP_MZPEAK_VALIDATOR` wins; else the sibling ~/Claude/mzPeakValidator package.
    fn discover() -> Option<Self> {
        if let Some(bin) = tool_from_env("BRFP_MZPEAK_VALIDATOR") {
            return Some(Self::Bin(bin));
        }
        let dir = std::env::var_os("HOME")
            .map(PathBuf::from)?
            .join("Claude/mzPeakValidator");
        dir.join("mzpeak_validator/__main__.py")
            .is_file()
            .then_some(Self::Module(dir))
    }

    fn assert_valid(&self, file: &Path) {
        let mut cmd = match self {
            Self::Bin(bin) => {
                let mut c = std::process::Command::new(bin);
                c.arg(file);
                c
            }
            Self::Module(dir) => {
                let mut c = std::process::Command::new("python3");
                c.arg("-m")
                    .arg("mzpeak_validator")
                    .arg(file)
                    .current_dir(dir);
                c
            }
        };
        let out = cmd.output().expect("failed to run the mzPeak validator");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && (stdout.contains("PASS") || stdout.contains("0 errors")),
            "mzPeak validation did not pass for {}:\n{stdout}{}",
            file.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn full_workflow_mzpeak_and_mzml_cross_check() {
    let Some(input) = private_fixture("timsTOF_autoMSMS_Urine_6min_pos.d") else {
        eprintln!("skipping workflow e2e; BRFP_TEST_PRIVATE_DATA is not set");
        return;
    };
    let Some(validator) = MzPeakValidator::discover() else {
        eprintln!("skipping workflow e2e; mzPeak validator not found (set BRFP_MZPEAK_VALIDATOR)");
        return;
    };
    let Some(mzml2mzpeak) = mzml2mzpeak_tool() else {
        eprintln!("skipping workflow e2e; mzml2mzpeak not found (set BRFP_MZML2MZPEAK)");
        return;
    };

    let tempdir = tempfile::tempdir().unwrap();
    let direct = tempdir.path().join("direct.mzpeak");
    let mzml = tempdir.path().join("run.mzML");
    let via_mzml = tempdir.path().join("via_mzml.mzpeak");
    let limit = "30";

    // 1. BRFP: .d -> mzPeak (direct) and .d -> mzML.
    Command::cargo_bin("brfp")
        .unwrap()
        .args(["convert"])
        .arg(&input)
        .args(["-b"])
        .arg(&direct)
        .args(["-f", "mzPeak", "--limit-spectra", limit])
        .assert()
        .success();
    Command::cargo_bin("brfp")
        .unwrap()
        .args(["convert"])
        .arg(&input)
        .args(["-b"])
        .arg(&mzml)
        .args(["-f", "mzML", "--limit-spectra", limit])
        .assert()
        .success();

    // 2. Validate each output with its own validator.
    validator.assert_valid(&direct);
    if let Some(mzml_validator) = tool_from_env("BRFP_MZML_VALIDATOR") {
        let out = std::process::Command::new(&mzml_validator)
            .arg(&mzml)
            .output()
            .expect("failed to run mzML validator");
        assert!(
            out.status.success(),
            "mzML validator failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 3. mzML -> mzPeak via the reference converter mzML2mzPeak.
    let out = std::process::Command::new(&mzml2mzpeak)
        .arg(&mzml)
        .arg(&via_mzml)
        .output()
        .expect("failed to run mzml2mzpeak");
    assert!(
        out.status.success() && via_mzml.exists(),
        "mzml2mzpeak failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    validator.assert_valid(&via_mzml);

    // 4. Consistency: the direct mzPeak and the mzML->mzPeak must agree.
    let mut a = MzPeakReader::new(&direct).unwrap();
    let mut b = MzPeakReader::new(&via_mzml).unwrap();
    assert_eq!(
        a.len(),
        b.len(),
        "spectrum count differs: direct {} vs via-mzML {}",
        a.len(),
        b.len()
    );

    // Compare a sample of spectra peak-for-peak (same TSF source both paths;
    // m/z is lossless f64, intensity f32, so values should match closely).
    let n = a.len();
    for i in [0, n / 2, n.saturating_sub(1)] {
        let sa = a.get_spectrum(i).unwrap();
        let sb = b.get_spectrum(i).unwrap();
        let pa = &sa.peaks.as_ref().unwrap().peaks;
        let pb = &sb.peaks.as_ref().unwrap().peaks;
        assert_eq!(pa.len(), pb.len(), "peak count differs at spectrum {i}");
        for (x, y) in pa.iter().zip(pb.iter()) {
            assert!(
                (x.mz - y.mz).abs() < 1e-4,
                "m/z mismatch at spectrum {i}: {} vs {}",
                x.mz,
                y.mz
            );
            let denom = x.intensity.abs().max(1.0);
            assert!(
                ((x.intensity - y.intensity).abs() / denom) < 1e-3,
                "intensity mismatch at spectrum {i}: {} vs {}",
                x.intensity,
                y.intensity
            );
        }
    }
}

// Real-world data sweep over ~/Claude/mzML2mzPeak/data (gated on env paths):
//   BRFP_TEST_TDF_D        - a Bruker TDF .d directory
//   BRFP_TEST_NONBRUKER_D  - a non-Bruker .d directory (e.g. Agilent AcqData)

fn dir_from_env(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

#[test]
fn tdf_inspects_and_converts() {
    let Some(tdf) = dir_from_env("BRFP_TEST_TDF_D") else {
        eprintln!("skipping TDF e2e; BRFP_TEST_TDF_D is not set");
        return;
    };

    // inspect detects TDF and reports frames.
    Command::cargo_bin("brfp")
        .unwrap()
        .arg("inspect")
        .arg(&tdf)
        .assert()
        .success()
        .stdout(predicate::str::contains("Format: TDF").and(predicate::str::contains("Frames:")));

    // convert a subset to mzPeak; output must be readable. Use enough spectra to
    // include DDA-PASEF MS2 (the reader front-loads MS1 frames).
    let tempdir = tempfile::tempdir().unwrap();
    let mzpeak = tempdir.path().join("tdf.mzpeak");
    Command::cargo_bin("brfp")
        .unwrap()
        .arg("convert")
        .arg(&tdf)
        .arg("-o")
        .arg(&mzpeak)
        .args(["-f", "mzPeak", "--limit-spectra", "2000"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote 2000 spectra"));
    let mut reader = MzPeakReader::new(&mzpeak).unwrap();
    assert_eq!(reader.len(), 2000);
    // MS2 (DDA-PASEF) spectra must round-trip through mzPeak.
    let has_msn = (0..reader.len())
        .filter_map(|i| reader.get_spectrum(i))
        .any(|s| s.description().ms_level >= 2);
    assert!(has_msn, "expected at least one MS2 spectrum");
    if let Some(validator) = MzPeakValidator::discover() {
        validator.assert_valid(&mzpeak);
    }

    // convert to mzML too.
    let mzml = tempdir.path().join("tdf.mzML");
    Command::cargo_bin("brfp")
        .unwrap()
        .arg("convert")
        .arg(&tdf)
        .arg("-o")
        .arg(&mzml)
        .args(["-f", "mzML", "--limit-spectra", "200"])
        .assert()
        .success();
    assert!(mzml.is_file());
}

#[test]
fn non_bruker_directory_is_rejected() {
    let Some(dir) = dir_from_env("BRFP_TEST_NONBRUKER_D") else {
        eprintln!("skipping non-Bruker e2e; BRFP_TEST_NONBRUKER_D is not set");
        return;
    };
    Command::cargo_bin("brfp")
        .unwrap()
        .arg("inspect")
        .arg(&dir)
        .assert()
        .failure()
        .stderr(predicate::str::contains("analysis.baf"));
}
