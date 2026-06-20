use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::pipeline::{BrfpError, BrfpResult};

const U2_INTENSITY_SCALE: f64 = 0.01;

#[derive(Debug, Clone, Serialize, Default)]
pub struct UvDetectorInventory {
    pub files: Vec<UvDetectorFile>,
    pub lc_method: Option<LcMethodSummary>,
    pub hdx_entries: Vec<HdxEntry>,
    pub u2_headers: Vec<U2Header>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UvDetectorFile {
    pub relative_path: String,
    pub role: UvDetectorFileRole,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UvDetectorFileRole {
    Method,
    Hdx,
    Hss,
    Chromatogram,
    WavelengthSpectra,
    UnknownDetectorData,
}

impl UvDetectorFileRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::Hdx => "hdx",
            Self::Hss => "hss",
            Self::Chromatogram => "chromatogram",
            Self::WavelengthSpectra => "wavelength_spectra",
            Self::UnknownDetectorData => "unknown_detector_data",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct LcMethodSummary {
    pub runtime_minutes: Option<f64>,
    pub sample_rate_hz: Option<f64>,
    pub channel_wavelengths_nm: Vec<f64>,
    pub spectral_start_nm: Option<f64>,
    pub spectral_end_nm: Option<f64>,
    pub save_spectra: Option<bool>,
    pub detector_labels: Vec<String>,
    pub key_values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HdxEntry {
    pub label: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct U2Header {
    pub relative_path: String,
    pub magic: String,
    pub header_size_bytes: Option<u32>,
    pub wavelength_count: Option<u32>,
    pub wavelength_start_nm: Option<f64>,
    pub wavelength_end_nm: Option<f64>,
    pub sample_rate_hz: Option<f64>,
    pub spectrum_count: Option<u32>,
    pub record_size_bytes: Option<u32>,
    pub data_start_bytes: Option<u64>,
    pub intensity_unit: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedUvWavelengthRun {
    pub relative_path: String,
    pub wavelengths_nm: Vec<f64>,
    pub spectra: Vec<DecodedUvWavelengthSpectrum>,
    pub intensity_unit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecodedUvWavelengthSpectrum {
    pub source_index: usize,
    pub time_minutes: f64,
    pub intensities: Vec<f64>,
}

pub fn inspect_uv_detector_inventory(input: &Path) -> BrfpResult<UvDetectorInventory> {
    let mut inventory = UvDetectorInventory::default();
    if !input.is_dir() {
        return Ok(inventory);
    }

    let mut files = Vec::new();
    visit_files(input, &mut files)?;
    files.sort();

    for path in files {
        let Some(role) = classify_uv_file(&path) else {
            continue;
        };
        let relative_path = relative_or_display(input, &path);
        let size_bytes = fs::metadata(&path)?.len();
        inventory.files.push(UvDetectorFile {
            relative_path: relative_path.clone(),
            role,
            size_bytes,
        });

        match role {
            UvDetectorFileRole::Method => match parse_lcparms(&path) {
                Ok(summary) => merge_lc_method(&mut inventory.lc_method, summary),
                Err(error) => inventory.warnings.push(format!(
                    "failed to parse UV/LC method {}: {error}",
                    relative_path
                )),
            },
            UvDetectorFileRole::Hdx => match parse_hdx(&path) {
                Ok(entries) => inventory.hdx_entries.extend(entries),
                Err(error) => inventory.warnings.push(format!(
                    "failed to parse detector index {}: {error}",
                    relative_path
                )),
            },
            UvDetectorFileRole::WavelengthSpectra => match parse_u2_header(&path, &relative_path) {
                Ok(Some(header)) => inventory.u2_headers.push(header),
                Ok(None) => inventory.warnings.push(format!(
                    "{relative_path} is not a recognized Bruker U2/DAD header"
                )),
                Err(error) => inventory.warnings.push(format!(
                    "failed to parse U2/DAD header {}: {error}",
                    relative_path
                )),
            },
            _ => {}
        }
    }

    Ok(inventory)
}

pub fn decode_uv_wavelength_runs(
    input: &Path,
    limit_spectra: Option<usize>,
) -> BrfpResult<Vec<DecodedUvWavelengthRun>> {
    let inventory = inspect_uv_detector_inventory(input)?;
    let mut runs = Vec::new();
    for file in inventory
        .files
        .iter()
        .filter(|file| file.role == UvDetectorFileRole::WavelengthSpectra)
    {
        let path = input.join(&file.relative_path);
        runs.push(decode_u2_wavelength_run(
            &path,
            &file.relative_path,
            limit_spectra,
        )?);
    }
    Ok(runs)
}

fn visit_files(path: &Path, files: &mut Vec<PathBuf>) -> BrfpResult<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            visit_files(&path, files)?;
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn classify_uv_file(path: &Path) -> Option<UvDetectorFileRole> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    let ext = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase());
    if name == "lcparms.txt" {
        Some(UvDetectorFileRole::Method)
    } else if ext.as_deref() == Some("hdx") {
        Some(UvDetectorFileRole::Hdx)
    } else if ext.as_deref() == Some("hss") {
        Some(UvDetectorFileRole::Hss)
    } else if ext.as_deref() == Some("unt") {
        Some(UvDetectorFileRole::Chromatogram)
    } else if ext.as_deref() == Some("u2") {
        Some(UvDetectorFileRole::WavelengthSpectra)
    } else if matches!(ext.as_deref(), Some("uv" | "pda" | "dad")) {
        Some(UvDetectorFileRole::UnknownDetectorData)
    } else {
        None
    }
}

fn parse_lcparms(path: &Path) -> BrfpResult<LcMethodSummary> {
    let text = read_text_lossy(path)?;
    let mut summary = LcMethodSummary::default();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((key, value)) = split_metadata_line(trimmed) {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            update_lc_method_from_pair(&mut summary, &key, &value);
            summary.key_values.insert(key, value);
        } else {
            update_lc_method_from_pair(&mut summary, trimmed, trimmed);
            let lower = trimmed.to_ascii_lowercase();
            if lower.contains("pda") || lower.contains("dad") || lower.contains("detector") {
                push_unique_string(&mut summary.detector_labels, trimmed);
            }
        }
    }

    summary.channel_wavelengths_nm.sort_by(f64::total_cmp);
    summary
        .channel_wavelengths_nm
        .dedup_by(|a, b| (*a - *b).abs() < 0.001);
    summary.detector_labels.sort();
    summary.detector_labels.dedup();

    Ok(summary)
}

fn update_lc_method_from_pair(summary: &mut LcMethodSummary, key: &str, value: &str) {
    let key_lower = key.to_ascii_lowercase();
    let value_lower = value.to_ascii_lowercase();
    let numeric_value = first_number(value);

    if summary.runtime_minutes.is_none()
        && (key_lower.contains("runtime")
            || key_lower.contains("run time")
            || key_lower.contains("stop time"))
    {
        summary.runtime_minutes = numeric_value;
    }
    if summary.sample_rate_hz.is_none()
        && (key_lower.contains("sample rate")
            || key_lower.contains("sampling rate")
            || key_lower.contains("frequency"))
    {
        summary.sample_rate_hz = numeric_value;
    }
    if key_lower.contains("save") && key_lower.contains("spectra") {
        summary.save_spectra = parse_bool(value);
    }

    let is_spectral_bound = key_lower.contains("spectrum")
        || key_lower.contains("spectral")
        || key_lower.contains("scan");
    if summary.spectral_start_nm.is_none()
        && is_spectral_bound
        && (key_lower.contains("start") || key_lower.contains("from") || key_lower.contains("low"))
    {
        summary.spectral_start_nm = numeric_value;
    }
    if summary.spectral_end_nm.is_none()
        && is_spectral_bound
        && (key_lower.contains("end") || key_lower.contains("to") || key_lower.contains("high"))
    {
        summary.spectral_end_nm = numeric_value;
    }

    if key_lower.contains("wavelength")
        && !key_lower.contains("start")
        && !key_lower.contains("end")
        && !key_lower.contains("range")
        && let Some(value) = numeric_value
        && (100.0..=1000.0).contains(&value)
    {
        summary.channel_wavelengths_nm.push(value);
    }

    if key_lower.contains("detector")
        || value_lower.contains("pda")
        || value_lower.contains("dad")
        || value_lower.contains("acquity")
    {
        push_unique_string(&mut summary.detector_labels, value);
    }
}

fn merge_lc_method(target: &mut Option<LcMethodSummary>, source: LcMethodSummary) {
    let Some(target) = target else {
        *target = Some(source);
        return;
    };

    target.runtime_minutes = target.runtime_minutes.or(source.runtime_minutes);
    target.sample_rate_hz = target.sample_rate_hz.or(source.sample_rate_hz);
    target.spectral_start_nm = target.spectral_start_nm.or(source.spectral_start_nm);
    target.spectral_end_nm = target.spectral_end_nm.or(source.spectral_end_nm);
    target.save_spectra = target.save_spectra.or(source.save_spectra);
    target
        .channel_wavelengths_nm
        .extend(source.channel_wavelengths_nm);
    target.channel_wavelengths_nm.sort_by(f64::total_cmp);
    target
        .channel_wavelengths_nm
        .dedup_by(|a, b| (*a - *b).abs() < 0.001);
    target.detector_labels.extend(source.detector_labels);
    target.detector_labels.sort();
    target.detector_labels.dedup();
    target.key_values.extend(source.key_values);
}

fn parse_hdx(path: &Path) -> BrfpResult<Vec<HdxEntry>> {
    let text = read_text_lossy(path)?;
    let mut entries = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((label, target)) = split_metadata_line(trimmed)
            && looks_like_detector_reference(target)
        {
            entries.push(HdxEntry {
                label: label.trim().to_string(),
                target: target.trim().to_string(),
            });
            continue;
        }
        if looks_like_detector_reference(trimmed) {
            entries.push(HdxEntry {
                label: format!("line_{}", entries.len()),
                target: trimmed.to_string(),
            });
        }
    }
    Ok(entries)
}

fn parse_u2_header(path: &Path, relative_path: &str) -> BrfpResult<Option<U2Header>> {
    let bytes = fs::read(path)?;
    if bytes.len() < 64 || !bytes.starts_with(b"#BFALCCHROM#") {
        return Ok(None);
    }

    let magic_bytes = &bytes[..b"#BFALCCHROM#".len()];
    let magic_len = magic_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(magic_bytes.len());
    let magic = String::from_utf8_lossy(&magic_bytes[..magic_len]).to_string();

    let wavelength_count = read_u32_le(&bytes, 0x24);
    let spectrum_count = read_nonzero_u32_le(&bytes, 0x154);
    let record_size_bytes = wavelength_count
        .filter(|count| *count > 0)
        .and_then(|count| count.checked_add(4))
        .and_then(|words| words.checked_mul(2));
    let data_start_bytes =
        spectrum_count
            .zip(record_size_bytes)
            .and_then(|(count, record_size)| {
                (count as u64)
                    .checked_mul(record_size as u64)
                    .and_then(|payload_size| (bytes.len() as u64).checked_sub(payload_size))
            });

    Ok(Some(U2Header {
        relative_path: relative_path.to_string(),
        magic,
        header_size_bytes: read_u32_le(&bytes, 0x20),
        wavelength_count,
        wavelength_start_nm: read_f64_le(&bytes, 0x28),
        wavelength_end_nm: read_f64_le(&bytes, 0x30),
        sample_rate_hz: read_f64_le(&bytes, 0x38),
        spectrum_count,
        record_size_bytes,
        data_start_bytes,
        intensity_unit: find_ascii_unit(&bytes),
    }))
}

fn decode_u2_wavelength_run(
    path: &Path,
    relative_path: &str,
    limit_spectra: Option<usize>,
) -> BrfpResult<DecodedUvWavelengthRun> {
    let bytes = fs::read(path)?;
    let header = parse_u2_header_from_bytes(&bytes, relative_path)?.ok_or_else(|| {
        BrfpError::Reader(format!(
            "{} is not a recognized Bruker U2/DAD wavelength file",
            path.display()
        ))
    })?;

    let wavelength_count = header.wavelength_count.ok_or_else(|| {
        BrfpError::Reader(format!(
            "missing wavelength count in U2/DAD header {}",
            path.display()
        ))
    })? as usize;
    if wavelength_count < 2 {
        return Err(BrfpError::Reader(format!(
            "invalid wavelength count {wavelength_count} in {}",
            path.display()
        )));
    }

    let wavelength_start_nm =
        require_finite_header_value(header.wavelength_start_nm, "wavelength start", path)?;
    let wavelength_end_nm =
        require_finite_header_value(header.wavelength_end_nm, "wavelength end", path)?;
    if wavelength_start_nm >= wavelength_end_nm {
        return Err(BrfpError::Reader(format!(
            "invalid wavelength bounds {wavelength_start_nm}..{wavelength_end_nm} in {}",
            path.display()
        )));
    }

    let spectrum_count = header.spectrum_count.ok_or_else(|| {
        BrfpError::Reader(format!(
            "missing wavelength spectrum count in U2/DAD header {}",
            path.display()
        ))
    })? as usize;
    let record_size_bytes = header.record_size_bytes.ok_or_else(|| {
        BrfpError::Reader(format!(
            "missing wavelength record size in U2/DAD header {}",
            path.display()
        ))
    })? as usize;
    let data_start = header.data_start_bytes.ok_or_else(|| {
        BrfpError::Reader(format!(
            "could not derive U2/DAD data start for {}",
            path.display()
        ))
    })? as usize;

    if data_start < header.header_size_bytes.unwrap_or(0) as usize {
        return Err(BrfpError::Reader(format!(
            "derived U2/DAD data start {data_start} precedes header in {}",
            path.display()
        )));
    }
    let expected_len = data_start
        .checked_add(
            spectrum_count
                .checked_mul(record_size_bytes)
                .ok_or_else(|| {
                    BrfpError::Reader(format!(
                        "U2/DAD payload size overflow in {}",
                        path.display()
                    ))
                })?,
        )
        .ok_or_else(|| BrfpError::Reader(format!("U2/DAD size overflow in {}", path.display())))?;
    if expected_len != bytes.len() {
        return Err(BrfpError::Reader(format!(
            "U2/DAD file size mismatch for {}: expected {expected_len} bytes, got {}",
            path.display(),
            bytes.len()
        )));
    }

    let expected_block_size = ((wavelength_count + 2) * 2) as u16;
    let spectra_to_decode = limit_spectra.unwrap_or(spectrum_count).min(spectrum_count);
    let mut spectra = Vec::with_capacity(spectra_to_decode);
    for source_index in 0..spectra_to_decode {
        let offset = data_start + source_index * record_size_bytes;
        let time_ms = read_u32_pair_le(&bytes, offset)?;
        let block_size = read_u16_le_required(&bytes, offset + 4)?;
        if block_size != expected_block_size {
            return Err(BrfpError::Reader(format!(
                "unexpected U2/DAD record size word {block_size} at spectrum {source_index} in {}; expected {expected_block_size}",
                path.display()
            )));
        }

        let mut intensities = Vec::with_capacity(wavelength_count);
        let mut current_intensity = read_i16_le_required(&bytes, offset + 6)? as i32;
        intensities.push(current_intensity as f64 * U2_INTENSITY_SCALE);
        for point_index in 1..wavelength_count {
            let delta = read_i16_le_required(&bytes, offset + 10 + (point_index - 1) * 2)? as i32;
            current_intensity -= delta;
            intensities.push(current_intensity as f64 * U2_INTENSITY_SCALE);
        }
        spectra.push(DecodedUvWavelengthSpectrum {
            source_index,
            time_minutes: time_ms as f64 / 60_000.0,
            intensities,
        });
    }

    Ok(DecodedUvWavelengthRun {
        relative_path: relative_path.to_string(),
        wavelengths_nm: evenly_spaced(wavelength_start_nm, wavelength_end_nm, wavelength_count),
        spectra,
        intensity_unit: header.intensity_unit.unwrap_or_else(|| "mAU".to_string()),
    })
}

fn parse_u2_header_from_bytes(bytes: &[u8], relative_path: &str) -> BrfpResult<Option<U2Header>> {
    if bytes.len() < 64 || !bytes.starts_with(b"#BFALCCHROM#") {
        return Ok(None);
    }

    let magic_bytes = &bytes[..b"#BFALCCHROM#".len()];
    let magic_len = magic_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(magic_bytes.len());
    let magic = String::from_utf8_lossy(&magic_bytes[..magic_len]).to_string();

    let wavelength_count = read_u32_le(bytes, 0x24);
    let spectrum_count = read_nonzero_u32_le(bytes, 0x154);
    let record_size_bytes = wavelength_count
        .filter(|count| *count > 0)
        .and_then(|count| count.checked_add(4))
        .and_then(|words| words.checked_mul(2));
    let data_start_bytes =
        spectrum_count
            .zip(record_size_bytes)
            .and_then(|(count, record_size)| {
                (count as u64)
                    .checked_mul(record_size as u64)
                    .and_then(|payload_size| (bytes.len() as u64).checked_sub(payload_size))
            });

    Ok(Some(U2Header {
        relative_path: relative_path.to_string(),
        magic,
        header_size_bytes: read_u32_le(bytes, 0x20),
        wavelength_count,
        wavelength_start_nm: read_f64_le(bytes, 0x28),
        wavelength_end_nm: read_f64_le(bytes, 0x30),
        sample_rate_hz: read_f64_le(bytes, 0x38),
        spectrum_count,
        record_size_bytes,
        data_start_bytes,
        intensity_unit: find_ascii_unit(bytes),
    }))
}

fn read_text_lossy(path: &Path) -> BrfpResult<String> {
    let bytes = fs::read(path)?;
    if bytes.starts_with(&[0xFF, 0xFE]) && bytes.len() % 2 == 0 {
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&words).map_err(|error| {
            BrfpError::Reader(format!(
                "invalid UTF-16 text in {}: {error}",
                path.display()
            ))
        });
    }
    if bytes.starts_with(&[0xFE, 0xFF]) && bytes.len() % 2 == 0 {
        let words = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&words).map_err(|error| {
            BrfpError::Reader(format!(
                "invalid UTF-16 text in {}: {error}",
                path.display()
            ))
        });
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
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

fn first_number(value: &str) -> Option<f64> {
    for token in
        value.split(|ch: char| !(ch.is_ascii_digit() || matches!(ch, '.' | '-' | '+' | 'e' | 'E')))
    {
        if token.is_empty() || matches!(token, "-" | "+" | "." | "-." | "+.") {
            continue;
        }
        if let Ok(value) = token.parse::<f64>()
            && value.is_finite()
        {
            return Some(value);
        }
    }
    None
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn looks_like_detector_reference(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains(".unt")
        || lower.contains(".u2")
        || lower.contains("chromatogram")
        || lower.contains("uv.dad")
        || lower.contains("dad")
}

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn read_nonzero_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    read_u32_le(bytes, offset).filter(|value| *value > 0)
}

fn read_u32_pair_le(bytes: &[u8], offset: usize) -> BrfpResult<u32> {
    let low = read_u16_le_required(bytes, offset)? as u32;
    let high = read_u16_le_required(bytes, offset + 2)? as u32;
    Ok(low | (high << 16))
}

fn read_u16_le_required(bytes: &[u8], offset: usize) -> BrfpResult<u16> {
    let slice = bytes.get(offset..offset + 2).ok_or_else(|| {
        BrfpError::Reader(format!("unexpected end of U2/DAD data at byte {offset}"))
    })?;
    Ok(u16::from_le_bytes(slice.try_into().map_err(|_| {
        BrfpError::Reader(format!("invalid U2/DAD u16 at byte {offset}"))
    })?))
}

fn read_i16_le_required(bytes: &[u8], offset: usize) -> BrfpResult<i16> {
    let slice = bytes.get(offset..offset + 2).ok_or_else(|| {
        BrfpError::Reader(format!("unexpected end of U2/DAD data at byte {offset}"))
    })?;
    Ok(i16::from_le_bytes(slice.try_into().map_err(|_| {
        BrfpError::Reader(format!("invalid U2/DAD i16 at byte {offset}"))
    })?))
}

fn read_f64_le(bytes: &[u8], offset: usize) -> Option<f64> {
    let slice = bytes.get(offset..offset + 8)?;
    let value = f64::from_le_bytes(slice.try_into().ok()?);
    value.is_finite().then_some(value)
}

fn find_ascii_unit(bytes: &[u8]) -> Option<String> {
    for unit in ["mAU", "AU", "counts"] {
        if bytes
            .windows(unit.len())
            .any(|window| window == unit.as_bytes())
        {
            return Some(unit.to_string());
        }
    }
    None
}

fn require_finite_header_value(value: Option<f64>, label: &str, path: &Path) -> BrfpResult<f64> {
    let value = value.ok_or_else(|| {
        BrfpError::Reader(format!(
            "missing {label} in U2/DAD header {}",
            path.display()
        ))
    })?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(BrfpError::Reader(format!(
            "non-finite {label} in U2/DAD header {}",
            path.display()
        )))
    }
}

fn evenly_spaced(start: f64, end: f64, count: usize) -> Vec<f64> {
    if count == 1 {
        return vec![start];
    }
    let step = (end - start) / (count - 1) as f64;
    (0..count)
        .map(|index| start + index as f64 * step)
        .collect()
}

fn push_unique_string(values: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() && !values.iter().any(|existing| existing == trimmed) {
        values.push(trimmed.to_string());
    }
}

fn relative_or_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lcparms_uv_method_hints() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("LCParms.txt");
        fs::write(
            &path,
            "\
Runtime: 15.02 min
Sample Rate: 50 Hz
Wavelength A: 220 nm
Wavelength B: 254 nm
Wavelength C: 360 nm
Spectrum Start: 190 nm
Spectrum End: 500 nm
DoSaveSpectra: no
Detector: Waters ACQUITY PDA
",
        )
        .unwrap();

        let summary = parse_lcparms(&path).unwrap();
        assert_eq!(summary.runtime_minutes, Some(15.02));
        assert_eq!(summary.sample_rate_hz, Some(50.0));
        assert_eq!(summary.channel_wavelengths_nm, vec![220.0, 254.0, 360.0]);
        assert_eq!(summary.spectral_start_nm, Some(190.0));
        assert_eq!(summary.spectral_end_nm, Some(500.0));
        assert_eq!(summary.save_spectra, Some(false));
        assert!(
            summary
                .detector_labels
                .iter()
                .any(|label| label.contains("PDA"))
        );
    }

    #[test]
    fn parses_u2_header_hints() {
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().join("sample.u2");
        let mut bytes = vec![0u8; 512];
        bytes[..12].copy_from_slice(b"#BFALCCHROM#");
        bytes[0x20..0x24].copy_from_slice(&512u32.to_le_bytes());
        bytes[0x24..0x28].copy_from_slice(&622u32.to_le_bytes());
        bytes[0x28..0x30].copy_from_slice(&190.0f64.to_le_bytes());
        bytes[0x30..0x38].copy_from_slice(&500.0f64.to_le_bytes());
        bytes[0x38..0x40].copy_from_slice(&50.0f64.to_le_bytes());
        bytes[128..131].copy_from_slice(b"mAU");
        fs::write(&path, bytes).unwrap();

        let header = parse_u2_header(&path, "sample.u2").unwrap().unwrap();
        assert_eq!(header.magic, "#BFALCCHROM#");
        assert_eq!(header.header_size_bytes, Some(512));
        assert_eq!(header.wavelength_count, Some(622));
        assert_eq!(header.wavelength_start_nm, Some(190.0));
        assert_eq!(header.wavelength_end_nm, Some(500.0));
        assert_eq!(header.sample_rate_hz, Some(50.0));
        assert_eq!(header.spectrum_count, None);
        assert_eq!(header.record_size_bytes, Some(1252));
        assert_eq!(header.intensity_unit.as_deref(), Some("mAU"));
    }

    #[test]
    fn decodes_u2_wavelength_records() {
        let tempdir = tempfile::tempdir().unwrap();
        let input = tempdir.path().join("sample.d");
        fs::create_dir(&input).unwrap();
        let path = input.join("sample.u2");
        write_synthetic_u2(&path);

        let runs = decode_uv_wavelength_runs(&input, Some(1)).unwrap();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(run.relative_path, "sample.u2");
        assert_eq!(run.wavelengths_nm, vec![200.0, 201.0, 202.0]);
        assert_eq!(run.intensity_unit, "mAU");
        assert_eq!(run.spectra.len(), 1);
        assert_eq!(run.spectra[0].source_index, 0);
        assert!((run.spectra[0].time_minutes - (1000.0 / 60_000.0)).abs() < 1e-12);
        assert_eq!(run.spectra[0].intensities, vec![1.0, -2.0, 3.0]);
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
            &[100, -200, 300],
        );
        write_u2_record(
            &mut bytes,
            data_start + record_size,
            wavelength_count,
            1050,
            &[400, 500, 600],
        );
        fs::write(path, bytes).unwrap();
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
