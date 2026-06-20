# Testing Strategy

BRFP needs more than unit tests because raw file conversion failures are usually
metadata and edge-case failures. The test suite should combine small deterministic
fixtures, semantic invariants, round-trip readers, and optional SDK-backed tests.

## Test Layers

Unit tests:

- CLI parsing and option normalization.
- `.d` input detection.
- Output path derivation.
- Acquisition mode classification.
- MS-level filter parsing.
- Error and warning classification.
- BAF SDK library discovery and cache inspection.
- UV/PDA method/header parsing.

Reader tests:

- BAF SQLite cache parsing against synthetic cache fixtures.
- BAF array extraction through `libbaf2sql_c` in Docker/Linux.
- TDF SQL metadata parsing against small SQLite fixtures.
- Spectrum index construction for MS1, DDA-PASEF, and DIA-PASEF.
- Parent precursor resolution.
- Scan range handling.
- TIC/BPC generation.

Writer tests:

- mzPeak archive member presence.
- `mzpeak_index.json` schema validity.
- Array index presence and correctness.
- Correct placement of centroid data in `spectra_peaks.parquet`.
- Correct use of `spectra_data.parquet` only for profile data.
- CV list includes all used prefixes.
- BRFP software and data-processing provenance is present.

Integration tests:

- Convert small TSF and BAF `.d` fixtures to mzPeak.
- Convert a small TDF `.d` fixture to mzPeak.
- Reopen output with the mzPeak Rust reader.
- Compare spectrum count, chromatogram count, representative spectra, precursor
  metadata, and ion mobility arrays.
- Verify decoded BAF UV/PDA wavelength spectra, derived absorption
  chromatograms, and vendor metadata facets are present when
  `--allDetectors --vendor-metadata` is used, and verify raw detector files are
  not embedded as mzPeak sidecars.
- Run semantic validation.

Cross-platform tests:

- Linux CI without Bruker SDK.
- Windows CI without Bruker SDK.
- Optional Linux/Windows SDK job only when secrets or local runner paths provide
  SDK libraries.

Performance tests:

- Large-run conversion throughput.
- Peak count and byte size by layout/compression mode.
- Memory ceiling under bounded streaming.
- Random access query timing on generated mzPeak output.

## Fixtures

Fixture categories:

- Minimal valid TDF with a few MS1 frames.
- Local TSF positive/negative examples for smoke conversion.
- Local BAF positive/negative examples with Waters/HyStar PDA sidecars.
- DDA-PASEF with precursor links.
- DIA-PASEF with isolation windows.
- Empty or malformed `.d` directories for error tests.
- Private large files for performance and SDK tests.

Public fixture candidates must be checked for redistribution permissions before
committing them. Until then, keep large/proprietary samples in `fixtures/private`
or outside the repository.

## mzPeak Validation

Validation should use the external mzPeak validator as the conformance oracle:

- `okohlbacher/mzPeakValidator`
- CLI: `mzpeak-validate <archive.mzpeak | unpacked_dir/>`
- JSON report mode: `mzpeak-validate output.mzpeak --json report.json`

BRFP's own validation code should primarily be an adapter around this tool plus
fast preflight checks that produce BRFP-specific diagnostics. It should not
duplicate the full conformance rule set unless the external validator is
unavailable in a given environment.

Implemented adapter entry points:

- `brfp validate output.mzpeak --mzpeak-validator /path/to/mzpeak-validate`
- `brfp validate output.mzpeak --report report.json`
- `brfp validate output.mzpeak --timeout-seconds 600`
- `brfp convert input.d --validate --validation-report report.json`
- `brfp convert input.d --validate --validation-semantic false`
- `brfp convert input.d --validate --validation-timeout-seconds 120`
- `BRFP_MZPEAK_VALIDATOR=/path/to/mzpeak-validate`

If `--semantic false` is used, BRFP passes `--quick` to the mzPeak validator.
If `--validation-semantic false` is used with `convert --validate`, BRFP does
the same after writing output. External validator processes are killed after
the configured timeout.

Validation should run in two modes:

- Syntactic: JSON schema validation for index/metadata and Parquet schema checks.
- Semantic: equal parallel column lengths, ascending coordinate arrays, resolving
  foreign keys, non-overlapping chunks, and CV declaration coverage.

Integration tests and release gates should call `mzpeak-validate` on generated
mzPeak output and archive the JSON report with conversion artifacts.

Current BAF smoke-validation command shape:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD:/work" -w /work \
  -e LD_LIBRARY_PATH=/work/tmp/tdf2mzml/src/tdf2mzml/libs \
  rust:1.96-bookworm \
  bash -lc 'export PATH=/usr/local/cargo/bin:$PATH; \
    cargo run -- convert tmp/baf-e2e/LTI225-67-3pos_1-F,2_01_24595.d \
      --sdk-lib-dir tmp/tdf2mzml/src/tdf2mzml/libs \
      --limit-spectra 3 \
      --vendor-metadata \
      --allDetectors \
      --output tmp/baf-e2e/pos.mzpeak'
cargo run -- validate tmp/baf-e2e/pos.mzpeak \
  --mzpeak-validator /path/to/mzpeak-validate \
  --report tmp/baf-e2e/pos.validation.json
```

## mzML Validation

mzML output should be validated with the HUPO-PSI mzML repository tooling and
examples:

- `HUPO-PSI/mzML`

When mzML output is implemented, integration tests should validate generated
mzML with the HUPO-PSI validator/tooling rather than relying only on round-trip
reads through `mzdata`.

The current adapter supports `brfp validate output.mzML --mzml-validator
/path/to/validator` and `BRFP_MZML_VALIDATOR=/path/to/validator`. Report-file
arguments for mzML remain tool-specific, so BRFP rejects `--report` for mzML
and uses the validator exit status as the conformance gate.

## Golden Data Policy

Golden mzPeak files are useful but should not be the primary assertion because
Parquet metadata, row groups, and compression can legitimately change. Prefer
semantic comparisons:

- Same spectrum count.
- Same native IDs and times within tolerance.
- Same MS levels and spectrum representation.
- Same precursor links and isolation windows.
- Same m/z, intensity, and ion mobility arrays within configured precision.
- Same chromatogram lengths and values within tolerance.

Use byte-for-byte golden files only for tiny synthetic fixtures where all writer
settings are pinned.

## Optional SDK Tests

SDK tests must be skipped unless the environment explicitly opts in.

Implemented environment variables:

- `TIMSDATA_LIB_DIR=/path/to/sdk/lib`
- `BRFP_TEST_PRIVATE_DATA=/path/to/private/fixtures`
- `BRFP_TEST_BAF_DATA=/path/to/baf/fixtures`
- `BRFP_BAF2SQL_LIB=/path/to/libbaf2sql_c.so`
- `BRFP_TEST_MTBLS18_UV_CDF=/path/to/MTBLS18/UV/cdf/files`

Future SDK-specific tests may add an explicit opt-in flag for calibration
comparisons. Current tests are gated by fixture/library paths.

The BAF integration tests in `tests/mzpeak_e2e.rs` are gated by
`BRFP_TEST_BAF_DATA` and `BRFP_BAF2SQL_LIB`. The MTBLS18 UV wavelength
comparison additionally requires `BRFP_TEST_MTBLS18_UV_CDF` and compares
`wavelength_spectra_*` arrays against the public UV NetCDF exports:

- `https://ftp.ebi.ac.uk/pub/databases/metabolights/studies/public/MTBLS18/FILES/LTI225-41-3neg_1-D__5_01_24321.cdf`
- `https://ftp.ebi.ac.uk/pub/databases/metabolights/studies/public/MTBLS18/FILES/LTI225-67-3pos_1-F__2_01_24595.cdf`

SDK-backed BAF tests should be run in Linux/Windows SDK environments, not on
macOS with an ELF or DLL vendor library.
