# Bruker Raw File Parser

BRFP is a Rust command line converter for Bruker `.d` raw-data directories.
Its primary output is **mzPeak**.

The project follows the operational shape of ThermoRawFileParser where that
makes sense for automated pipelines, but the internals are Bruker-specific and
mzPeak-first.

## Status

Implemented:

- `inspect` for Bruker TSF, TDF, and BAF `.d` directories using local metadata.
- mzPeak writing for TSF line spectra from `analysis.tsf` and
  `analysis.tsf_bin`, with per-frame MS levels read from `Frames.MsMsType`
  (TSF m/z values remain an approximation).
- mzPeak writing for BAF line/profile spectra through Bruker's
  `libbaf2sql_c` runtime.
- mzPeak and mzML writing for TDF (timsTOF DDA/DIA-PASEF) via the pure-Rust
  `timsrust` reader (no SDK), with MS levels, polarity, and DDA-PASEF precursors
  (selected-ion m/z, charge, inverse reduced ion mobility). Summed spectra; a
  per-peak ion-mobility array is a future refinement.
- mzML output for TSF, BAF, and TDF (`--format mzML`).
- BAF DAD/PDA `.u2` UV records decoded into mzPeak wavelength spectra.
- Method-wavelength absorption chromatograms derived from decoded UV spectra.
- Metadata-only vendor facets through `vendor_file_metadata.parquet`.
- Optional readable vendor metadata JSON sidecars.
- `brfp validate` and `convert --validate` integration with external mzPeak and
  mzML validators.
- ThermoRawFileParser-compatible conversion flags for common workflow options.

Not implemented yet:

- Per-peak ion-mobility array for TDF (precursor mobility is preserved).
- `query`, `xic`, directory batch conversion, and stdout archive streaming.
- Direct `.unt` fixed-wavelength chromatogram decoding.

Raw vendor spectra and chromatogram files are not embedded as proprietary
payloads. Spectra, chromatograms, and UV/DAD/PDA data are decoded into standard
mzPeak data structures; forwarded vendor data is metadata only.

## Build

```bash
cargo build --release
```

The executable is:

```text
target/release/brfp
```

Normal Rust checks:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

## Basic Usage

```bash
brfp inspect /path/to/run.d
brfp convert /path/to/run.d --output /path/to/run.mzpeak
brfp validate /path/to/run.mzpeak --mzpeak-validator /path/to/mzpeak-validate
```

ThermoRawFileParser-style conversion also works:

```bash
brfp -i=/path/to/run.d -b=/path/to/run.mzpeak -f=mzPeak
```

BAF conversion requires the BAF SDK library on a compatible runtime:

```bash
brfp convert /path/to/run.d \
  --output /path/to/run.mzpeak \
  --format mzPeak \
  --baf2sql-lib /path/to/libbaf2sql_c.so \
  --vendor-metadata \
  --allDetectors \
  --calibration-mode auto
```

On macOS, BRFP can build and inspect cached metadata, but SDK-backed BAF
conversion should be run in Linux Docker or on Windows/Linux with the matching
vendor library.

## Docker SDK Development

The repository does not vendor Bruker SDK binaries. The SDK is proprietary and
must not be committed, pushed, packaged into release archives, or redistributed
with BRFP.

Users who need SDK-backed conversion must download the relevant Bruker SDK or
BAF runtime directly from Bruker's Mass Spectrometry Software Updates page:
<https://www.bruker.com/en/products-and-solutions/mass-spectrometry/ms-software/mass-spectrometry-software-updates.html>.

Put local SDKs, fixtures, and generated files under ignored paths such as
`vendor/`, `data/`, `tmp/`, or `fixtures/private/`.

Example Linux container command for the current BAF/UV test path:

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
    cargo test baf_uv_wavelength_spectra_match_mtbls18_netcdf_reference \
      --test mzpeak_e2e -- --nocapture'
```

See [Docker and SDK development](docs/docker-sdk.md) for details.

## Validation

mzPeak output can be validated with
[`okohlbacher/mzPeakValidator`](https://github.com/okohlbacher/mzPeakValidator):

```bash
brfp validate output.mzpeak \
  --mzpeak-validator /path/to/mzpeak-validate \
  --report output.validation.json
```

mzML validator integration is present for future mzML output and external mzML
files. Report generation for mzML remains validator-specific, so run the mzML
validator directly when a report artifact is required.

## Documentation

- [Architecture](docs/architecture.md)
- [Project plan](docs/project-plan.md)
- [Roadmap](docs/roadmap.md)
- [Testing strategy](docs/testing-strategy.md)
- [Docker and SDK development](docs/docker-sdk.md)
- [Release and commit checklist](docs/release.md)
- [BAF UV conversion strategy](docs/research/baf-uv-conversion-strategy.md)

## Licensing

BRFP source code is released under the [MIT License](LICENSE).

Bruker SDK binaries, local raw-data fixtures, generated mzPeak files, and
validator outputs are not part of this repository and must not be committed.
Download proprietary Bruker components separately from Bruker under their own
license terms.
