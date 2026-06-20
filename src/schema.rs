//! Centralized mzPeak controlled-vocabulary terms and units.
//!
//! Single source of truth for the CV accessions and units BRFP writes, so unit
//! and term usage cannot drift between code paths. This mirrors the
//! `schema/cv` pattern in the reference converter `mzML2mzPeak` and is what the
//! cv_list conformance test reads from.

use mzdata::{
    meta::SpectrumType,
    params::{ControlledVocabulary, Param, Unit},
};

/// Run-level vendor identity pulled from Bruker `GlobalMetadata` (TSF) or
/// `Properties` (BAF), used to populate mzPeak instrument/software/sample
/// metadata instead of placeholder values (REQ-05).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunVendorMetadata {
    pub instrument_name: Option<String>,
    pub instrument_vendor: Option<String>,
    pub instrument_serial: Option<String>,
    pub acquisition_software: Option<String>,
    pub acquisition_software_version: Option<String>,
    pub sample_name: Option<String>,
}

impl RunVendorMetadata {
    /// Build from a key lookup over a vendor metadata table. `lookup` returns the
    /// trimmed value for a key if present and non-empty.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| {
            lookup(key).and_then(|value| {
                let trimmed = value.trim().to_string();
                (!trimmed.is_empty()).then_some(trimmed)
            })
        };
        Self {
            instrument_name: get("InstrumentName"),
            instrument_vendor: get("InstrumentVendor"),
            instrument_serial: get("InstrumentSerialNumber"),
            acquisition_software: get("AcquisitionSoftware"),
            acquisition_software_version: get("AcquisitionSoftwareVersion"),
            sample_name: get("SampleName"),
        }
    }

    /// True when nothing useful was found (keep placeholder behavior).
    pub fn is_empty(&self) -> bool {
        self.instrument_name.is_none()
            && self.instrument_vendor.is_none()
            && self.instrument_serial.is_none()
            && self.acquisition_software.is_none()
            && self.acquisition_software_version.is_none()
            && self.sample_name.is_none()
    }
}

/// `MS:1000294` "mass spectrum", written as the boolean spectrum-class flag.
pub fn mass_spectrum_param() -> Param {
    ControlledVocabulary::MS.param_val(1000294, "mass spectrum", true)
}

/// Unit for mass-spectral intensity: `MS:1000131` number of detector counts.
pub const MS_INTENSITY_UNIT: Unit = Unit::DetectorCounts;

/// Unit for m/z arrays: `MS:1000040`.
pub const MZ_UNIT: Unit = Unit::MZ;

/// Unit for UV/PDA absorbance arrays: `UO:0000269` absorbance unit.
///
/// This replaces the historical mislabeling of absorbance as detector counts.
pub const ABSORBANCE_UNIT: Unit = Unit::AbsorbanceUnit;

/// Unit for wavelength arrays: `UO:0000018` nanometer.
pub const WAVELENGTH_UNIT: Unit = Unit::Nanometer;

/// Unit for chromatogram time arrays: minutes.
pub const TIME_UNIT: Unit = Unit::Minute;

/// Spectrum-type term for a given MS level (`MS:1000579` MS1 vs `MS:1000580`
/// MSn).
pub fn spectrum_type_for_ms_level(ms_level: u8) -> SpectrumType {
    if ms_level <= 1 {
        SpectrumType::MS1Spectrum
    } else {
        SpectrumType::MSnSpectrum
    }
}

/// Map a Bruker TSF `Frames.MsMsType` value to a one-based MS level.
///
/// Values are documented in `tsf-schema_v5.sql`:
/// `0 = MS`, `2 = MS/MS`, `3 = MSn`, `8 = PASEF`, `9 = DIA`, `10 = PRM`.
/// PASEF/DIA/PRM are fragmentation acquisitions and map to MS2. Unknown values
/// fall back to MS2 and set `recognized = false` so the caller can warn.
pub fn ms_level_from_msms_type(msms_type: i64) -> (u8, bool) {
    match msms_type {
        0 => (1, true),
        2 => (2, true),
        3 => (3, true),
        8..=10 => (2, true),
        _ => (2, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msms_type_mapping_matches_schema() {
        assert_eq!(ms_level_from_msms_type(0), (1, true));
        assert_eq!(ms_level_from_msms_type(2), (2, true));
        assert_eq!(ms_level_from_msms_type(3), (3, true));
        assert_eq!(ms_level_from_msms_type(8), (2, true));
        assert_eq!(ms_level_from_msms_type(9), (2, true));
        assert_eq!(ms_level_from_msms_type(10), (2, true));
        assert_eq!(ms_level_from_msms_type(99), (2, false));
    }

    #[test]
    fn run_vendor_metadata_extracts_and_trims() {
        let map = std::collections::HashMap::from([
            ("InstrumentName".to_string(), " timsTOF fleX ".to_string()),
            ("InstrumentVendor".to_string(), "Bruker".to_string()),
            (
                "InstrumentSerialNumber".to_string(),
                "1859745.00326".to_string(),
            ),
            ("AcquisitionSoftware".to_string(), "timsTOF".to_string()),
            (
                "AcquisitionSoftwareVersion".to_string(),
                "3.1.1".to_string(),
            ),
            ("SampleName".to_string(), "".to_string()),
        ]);
        let meta = RunVendorMetadata::from_lookup(|key| map.get(key).cloned());
        assert_eq!(meta.instrument_name.as_deref(), Some("timsTOF fleX"));
        assert_eq!(meta.instrument_vendor.as_deref(), Some("Bruker"));
        assert_eq!(meta.instrument_serial.as_deref(), Some("1859745.00326"));
        assert_eq!(meta.acquisition_software.as_deref(), Some("timsTOF"));
        assert_eq!(meta.acquisition_software_version.as_deref(), Some("3.1.1"));
        assert_eq!(meta.sample_name, None); // empty trimmed to None
        assert!(!meta.is_empty());

        assert!(RunVendorMetadata::from_lookup(|_| None).is_empty());
    }

    #[test]
    fn spectrum_type_follows_ms_level() {
        assert_eq!(spectrum_type_for_ms_level(0), SpectrumType::MS1Spectrum);
        assert_eq!(spectrum_type_for_ms_level(1), SpectrumType::MS1Spectrum);
        assert_eq!(spectrum_type_for_ms_level(2), SpectrumType::MSnSpectrum);
        assert_eq!(spectrum_type_for_ms_level(3), SpectrumType::MSnSpectrum);
    }
}
