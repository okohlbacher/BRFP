# Release And Commit Checklist

Use this checklist before the initial GitHub commit and before release builds.

## Commit Contents

Commit source and documentation:

```bash
git add .github .gitignore Cargo.toml Cargo.lock LICENSE README.md docs scripts src tests
git status --short
```

Do not commit:

- Bruker SDK binaries or headers under `vendor/`.
- Bruker SDK archives such as `timsdata-*.zip`.
- Bruker runtime libraries such as `libtimsdata.so`, `timsdata.dll`,
  `libbaf2sql_c.so`, or `baf2sql_c.dll`.
- Private or public raw-data fixtures under `data/` or `fixtures/private/`.
- Generated mzPeak, mzML, Parquet, validation, or vendor sidecar files.
- Local Docker build output under `target-linux/`.
- Temporary exploration clones under `tmp/`.

The `.gitignore` is intentionally conservative so local SDK and data work can
stay beside the source tree without leaking into the repository.

Users who need SDK-backed conversion must download the relevant Bruker SDK or
BAF runtime directly from Bruker's Mass Spectrometry Software Updates page:
<https://www.bruker.com/en/products-and-solutions/mass-spectrometry/ms-software/mass-spectrometry-software-updates.html>.
BRFP source and release artifacts must not redistribute proprietary Bruker SDK
components.

## Required Checks

Run on the host:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release
target/release/brfp --help
```

The release executable path after a local host build is:

```text
target/release/brfp
```

## Optional SDK/BAF Checks

BAF conversion and UV extraction require `libbaf2sql_c` on Linux or Windows.
From the repository root, with local fixtures and SDK libraries under ignored
paths:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD:/work" -w /work \
  -e CARGO_TARGET_DIR=/work/target-linux \
  -e LD_LIBRARY_PATH=/work/tmp/tdf2mzml/src/tdf2mzml/libs \
  -e BRFP_TEST_BAF_DATA=/work/tmp/baf-e2e \
  -e BRFP_BAF2SQL_LIB=/work/tmp/tdf2mzml/src/tdf2mzml/libs/libbaf2sql_c.so \
  -e BRFP_TEST_MTBLS18_UV_CDF=/work/tmp/mtbls18 \
  rust:1.96-bookworm \
  bash -lc 'export PATH=/usr/local/cargo/bin:$PATH; \
    apt-get update >/dev/null && \
    apt-get install -y --no-install-recommends cmake >/dev/null && \
    cargo test baf_cli_writes_readable_mzpeak_and_decoded_uv_spectra \
      --test mzpeak_e2e -- --nocapture && \
    cargo test baf_uv_wavelength_spectra_match_mtbls18_netcdf_reference \
      --test mzpeak_e2e -- --nocapture'
```

## Example Full Conversion

```bash
brfp convert /path/to/run.d \
  --output /path/to/run.mzpeak \
  --format mzPeak \
  --baf2sql-lib /path/to/libbaf2sql_c.so \
  --vendor-metadata \
  --vendor-metadata-json \
  --allDetectors \
  --calibration-mode auto \
  --validate \
  --mzpeak-validator /path/to/mzpeak-validate \
  --validation-report /path/to/run.validation.json
```

Expected high-level structure for BAF/PDA data:

- `spectra_*` facets contain MS spectra.
- `wavelength_spectra_*` facets contain decoded UV/DAD/PDA spectra.
- `chromatograms_*` facets contain TIC/BPC and derived absorption
  chromatograms where available.
- `vendor_file_metadata.parquet` contains vendor metadata.
- `vendor_payload_entries` remains empty for raw spectra/chromatogram files.

## GitHub Release Notes

For the first public release, state the supported scope explicitly:

- mzPeak is the primary implemented output.
- TSF and BAF conversion are implemented; TDF conversion is planned.
- BAF conversion depends on a user-provided Bruker BAF SDK runtime.
- macOS is supported for Rust development and metadata inspection, not for
  Linux/Windows Bruker SDK execution.
- The project is MIT licensed; Bruker SDK artifacts are not redistributed.
- Users must obtain proprietary Bruker SDK/runtime components directly from
  Bruker.
