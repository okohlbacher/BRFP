# Docker And Bruker SDK Development

The Bruker SDK is proprietary. BRFP does not redistribute SDK files, and SDK
binaries, headers, example files, or archives must not be committed to Git or
included in release packages.

Users must download the required Bruker SDK/runtime directly from Bruker's Mass
Spectrometry Software Updates page:
<https://www.bruker.com/en/products-and-solutions/mass-spectrometry/ms-software/mass-spectrometry-software-updates.html>.

The local Bruker SDK used during development is not a macOS library. SDK-backed
development should use Linux x86-64 Docker or Windows x86-64.

## Local Paths

Ignored SDK path:

```text
vendor/timsdata
```

Important files:

```text
vendor/timsdata/linux64/libtimsdata.so
vendor/timsdata/win64/timsdata.dll
vendor/timsdata/win64/timsdata.lib
vendor/timsdata/include/c/timsdata.h
vendor/timsdata/include/c/tsfdata.h
vendor/timsdata/examples/py/timsdata.py
vendor/timsdata/examples/py/tsfdata.py
```

BAF conversion uses the separate `libbaf2sql_c` runtime when available. In this
workspace that library is currently available from the explored `tdf2mzml`
checkout:

```text
tmp/tdf2mzml/src/tdf2mzml/libs/libbaf2sql_c.so
```

Ignored private fixture path:

```text
fixtures/private
```

## Host Checks

Check Docker:

```bash
docker --version
docker ps
```

Check SDK binary architecture:

```bash
file vendor/timsdata/linux64/libtimsdata.so
file vendor/timsdata/win64/timsdata.dll
```

Expected:

- Linux library: x86-64 ELF shared object.
- Windows library: x86-64 PE DLL.

## Linux Docker Shell

From the repository root:

```bash
docker run --rm -it --platform linux/amd64 \
  -v "$PWD":/work \
  -w /work \
  -e TIMSDATA_LIB_DIR=/work/vendor/timsdata/linux64 \
  rust:1.96-bookworm bash
```

Inside the container:

```bash
export PATH=/usr/local/cargo/bin:$PATH
apt-get update
apt-get install -y --no-install-recommends cmake
ldd vendor/timsdata/linux64/libtimsdata.so
cargo test
```

For SDK-backed tests later:

```bash
BRFP_TEST_SDK=1 \
BRFP_TEST_PRIVATE_DATA=/work/fixtures/private \
TIMSDATA_LIB_DIR=/work/vendor/timsdata/linux64 \
cargo test --features sdk
```

For BAF-backed conversion tests:

```bash
export PATH=/usr/local/cargo/bin:$PATH
export LD_LIBRARY_PATH=/work/tmp/tdf2mzml/src/tdf2mzml/libs
BRFP_TEST_BAF_DATA=/work/tmp/baf-e2e \
BRFP_BAF2SQL_LIB=/work/tmp/tdf2mzml/src/tdf2mzml/libs/libbaf2sql_c.so \
cargo test baf_cli_writes_readable_mzpeak_and_decoded_uv_spectra --test mzpeak_e2e -- --nocapture
```

For MTBLS18 UV NetCDF reference comparison, download the two public UV CDF
exports to a local directory, then add `BRFP_TEST_MTBLS18_UV_CDF`:

```bash
mkdir -p tmp/mtbls18
curl -L -o tmp/mtbls18/LTI225-41-3neg_1-D__5_01_24321.cdf \
  https://ftp.ebi.ac.uk/pub/databases/metabolights/studies/public/MTBLS18/FILES/LTI225-41-3neg_1-D__5_01_24321.cdf
curl -L -o tmp/mtbls18/LTI225-67-3pos_1-F__2_01_24595.cdf \
  https://ftp.ebi.ac.uk/pub/databases/metabolights/studies/public/MTBLS18/FILES/LTI225-67-3pos_1-F__2_01_24595.cdf

BRFP_TEST_BAF_DATA=/work/tmp/baf-e2e \
BRFP_BAF2SQL_LIB=/work/tmp/tdf2mzml/src/tdf2mzml/libs/libbaf2sql_c.so \
BRFP_TEST_MTBLS18_UV_CDF=/work/tmp/mtbls18 \
cargo test baf_uv_wavelength_spectra_match_mtbls18_netcdf_reference --test mzpeak_e2e -- --nocapture
```

## Non-Interactive Docker Check

Use the helper script:

```bash
scripts/docker-sdk-test.sh
```

It installs `cmake` in the temporary container when needed because the current
mzPeak/mzdata dependency graph builds zlib-ng through `libz-sys`.

The script runs host-independent Rust tests by default and uses
`target-linux/` as the container target directory. If `BRFP_TEST_BAF_DATA` and
`BRFP_BAF2SQL_LIB` are present in the host environment, it also runs the
SDK-backed BAF mzPeak e2e tests. Add `BRFP_TEST_MTBLS18_UV_CDF` to include the
public MTBLS18 NetCDF UV comparison.

Example with the local BAF/UV fixtures:

```bash
LD_LIBRARY_PATH=/work/tmp/tdf2mzml/src/tdf2mzml/libs \
BRFP_TEST_BAF_DATA=/work/tmp/baf-e2e \
BRFP_BAF2SQL_LIB=/work/tmp/tdf2mzml/src/tdf2mzml/libs/libbaf2sql_c.so \
BRFP_TEST_MTBLS18_UV_CDF=/work/tmp/mtbls18 \
scripts/docker-sdk-test.sh
```

BAF smoke conversion:

```bash
docker run --rm --platform linux/amd64 \
  -v "$PWD":/work \
  -w /work \
  -e LD_LIBRARY_PATH=/work/tmp/tdf2mzml/src/tdf2mzml/libs \
  rust:1.96-bookworm \
  bash -lc 'export PATH=/usr/local/cargo/bin:$PATH; \
    cargo run -- convert tmp/baf-e2e/LTI225-67-3pos_1-F,2_01_24595.d \
      --sdk-lib-dir tmp/tdf2mzml/src/tdf2mzml/libs \
      --limit-spectra 3 \
      --vendor-metadata \
      --allDetectors \
      --output tmp/baf-e2e/pos.mzpeak'
```

## Rules

- Do not commit `vendor/timsdata`.
- Do not commit Bruker SDK archives such as `timsdata-*.zip`.
- Do not commit SDK headers, examples, `.so`, `.dll`, `.lib`, or `.dylib`
  binaries.
- Do not redistribute Bruker SDK files with BRFP source or release artifacts.
- Document that users must download proprietary Bruker components from Bruker.
- Do not commit private raw data fixtures.
- Do not make SDK tests run by default.
- Do not claim macOS SDK support.
- Prefer pure-Rust tests in normal CI.
- Use Docker for SDK correctness, native Linux for performance.
