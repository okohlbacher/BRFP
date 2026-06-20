# Work Breakdown

This file chunks the project into implementation pieces that can become issues,
branches, or delegated subagent tasks.

## Track 1: Project Hygiene

T1.1 Commit initial baseline.

- Inputs: current repo state.
- Output: first git commit.
- Dependencies: none.

T1.2 Add CI skeleton.

- Commands: `cargo fmt --check`, `cargo test`, `cargo clippy`.
- Platforms: Linux and Windows.
- Status: implemented as `.github/workflows/ci.yml`.
- Dependencies: T1.1.

T1.3 Add contribution and SDK policy docs.

- Include proprietary SDK handling.
- Include fixture policy.
- Dependencies: T1.1.

T1.4 Document SDK redistribution policy.

- State that Bruker SDK artifacts are proprietary and not redistributed.
- Document user responsibility for downloading SDK/runtime components directly
  from Bruker.
- Keep release artifacts SDK-free unless a future legal review explicitly
  permits otherwise.
- Status: implemented in README, Docker SDK docs, release checklist, and
  `.gitignore`.
- Dependencies: none.

T1.5 Add dependency pinning policy.

- Define how git dependencies are pinned and updated.
- Include Arrow/Parquet conflict checks.
- Dependencies: none.

## Track 2: Docker And SDK

T2.1 Add Docker helper script.

- Script: `scripts/docker-sdk-test.sh`.
- Runs `ldd` and `cargo test` in `linux/amd64`.
- Dependencies: none.

T2.2 Add SDK discovery module.

- Detect `--sdk-lib-dir`.
- Detect `TIMSDATA_LIB_DIR`.
- Validate expected platform library.
- Dependencies: none.

T2.2b Add BAF SDK discovery.

- Detect `--baf2sql-lib`, `BRFP_BAF2SQL_LIB`, and `--sdk-lib-dir`.
- Search Linux/Windows library names without requiring macOS loading.
- Status: implemented.
- Dependencies: none.

T2.3 Add SDK smoke test.

- Gated by `BRFP_TEST_SDK=1`.
- Runs only in Linux/Windows compatible environments.
- Dependencies: T2.2.

T2.4 Add SDK integrity/version checks.

- Record checksum of local SDK files in ignored local report.
- Read SDK version/changelog where available.
- Fail SDK tests if expected library is absent or incompatible with platform.
- Dependencies: T2.2.

## Track 3: Input Inspection

T3.1 Harden current SQLite inspection.

- Validate `SchemaType` against file name.
- Report schema version.
- Report missing binary file as warning.
- Dependencies: current implementation.

T3.2 Add private fixture integration tests.

- Gated by `BRFP_TEST_PRIVATE_DATA`.
- Assert positive/negative TSF counts.
- Dependencies: current implementation.

T3.2b Add BAF inspection tests.

- Use synthetic `analysis.sqlite` for pure host tests.
- Use local BAF/PDA fixtures for Docker-backed smoke tests.
- Status: implemented.
- Dependencies: BAF cache schema.

T3.3 Add TDF inspection fixture support.

- Requires a TDF fixture.
- Assert TDF table summaries.
- Dependencies: fixture availability.

T3.4 Acquire or create TDF fixture.

- Locate a redistributable TDF `.d` directory or document private fixture path.
- Include DDA-PASEF if possible; add DIA-PASEF later.
- Dependencies: none.

T3.5 Add malformed input fixtures.

- Missing database.
- Multiple analysis payloads.
- Missing binary payload.
- `ClosedProperly=0`.
- SchemaType mismatch.
- Dependencies: none.

## Track 4b: BAF Reading

T4b.1 Implement BAF FFI wrapper.

- Dynamically load `libbaf2sql_c`.
- Wrap cache path generation and array store open/read/close.
- Status: implemented.
- Dependencies: local BAF SDK library.

T4b.2 Implement BAF SQLite cache reader.

- Read `Spectra`, `AcquisitionKeys`, and `Properties`.
- Report spectrum counts, polarity, MS levels, cache path, and calibration mode.
- Status: implemented.
- Dependencies: T4b.1 for generated caches, synthetic cache for host tests.

T4b.3 Implement BAF spectrum extraction.

- Read line arrays by default.
- Support profile preference and profile-missing policy.
- Support raw fallback when calibrated access fails.
- Status: implemented.
- Dependencies: T4b.1 and T4b.2.

T4b.4 Add BAF MS2/precursor fixtures.

- Current local BAF fixtures are MS1-only.
- Add MS2 fixtures before claiming precursor metadata support.
- Dependencies: fixture availability.

## Track 4: TSF Reading

T4.1 Evaluate `timsrust-tsf` API.

- Spike against local TSF examples.
- Determine whether it reads all required arrays/metadata.
- Dependencies: T2.1 useful but not required.
- Status: initial pure-Rust spectrum preview works on the local positive TSF
  fixture through `timsrust::SpectrumReader`.

T4.2 Implement TSF reader adapter.

- Produce first N spectra.
- Map `Frames` and `FrameMsMsInfo`.
- Include SDK FFI contingency if `timsrust-tsf` API is insufficient.
- Dependencies: T4.1.

T4.3 Add TSF reader tests.

- Gated by private fixtures.
- Assert counts and representative metadata.
- Dependencies: T4.2.

## Track 5: mzPeak Writer

T5.1 Add mzPeak dependency spike.

- Use HUPO-PSI/mzPeak git/path dependency.
- Pin commit.
- Verify Arrow/Parquet dependency compatibility with `mzdata` and `timsrust`.
- Dependencies: none.

T5.2 Write synthetic stream to mzPeak.

- Minimal spectra and metadata.
- Reopen with mzPeak reader.
- Dependencies: T5.1.

T5.3 Write TSF first-N spectra to mzPeak.

- Dependencies: T4.2 and T5.2.
- Status: implemented for limited TSF smoke conversions; current output passes
  `mzpeak-validate` with zero errors and zero warnings.

T5.4 Write TDF spectra to mzPeak.

- Dependencies: TDF fixture and T5.2.

T5.5 Write BAF spectra to mzPeak.

- Map BAF spectra to `mzdata`/mzPeak spectrum structures.
- Use BAF source-file and native-ID terms.
- Apply BAF `--ms-level`, `--ms2-only`, and spectrum ID range filters.
- Status: implemented and Docker-tested on positive/negative local BAF fixtures.
- Dependencies: T4b.3 and T5.2.

T5.6 Decode UV/PDA and preserve vendor metadata.

- Write `vendor_file_metadata.parquet`.
- Decode validated `.u2` DAD/PDA wavelength spectra into mzPeak wavelength
  spectrum parquet facets when requested.
- Derive method-wavelength absorption chromatograms from decoded `.u2` spectra
  when chromatogram export is enabled.
- Inventory `.u2`, `.unt`, `.hdx`, `.hss`, BAF cache/provenance files, and method
  files without embedding raw detector files in mzPeak output.
- Extract LC method, HDX references, and U2 header facts.
- Status: implemented for conservative metadata, `.u2` wavelength-spectrum
  decoding, and `.u2`-derived absorption chromatograms.
- Dependencies: T5.2.

## Track 6: Validation

T6.1 Add external validator adapter.

- Wire `brfp validate` to `mzpeak-validate` and mzML validator executables.
- Support explicit executable paths and environment variables.
- Support JSON report output for mzPeak.
- Enforce external validator timeout and Unix executable-bit checks.
- Status: implemented for external validator execution.
- Dependencies: T5.2 useful but can start with synthetic bad archives.

T6.2 Add validator-backed integration tests.

- Generate small mzPeak output from private TSF and local BAF fixtures.
- Run `mzpeak-validate` when available.
- Archive JSON report as a test artifact.
- Status: manually verified through `convert --validate` against the local
  `mzpeak-validate`; BAF Docker integration test is implemented.
- Dependencies: T6.1.

T6.3 Integrate conversion validation.

- `convert --validate`.
- Status: implemented for TSF-to-mzPeak conversions.
- Dependencies: T6.1 and T5.2.

T6.4 Define deterministic output policy.

- Stable ordering.
- Stable conversion report fields.
- Timestamp policy.
- Chunk boundary policy.
- Dependencies: T5.2.

## Track 7: mzML

T7.1 Evaluate `mzdata` writer APIs.

- Minimal synthetic mzML write/read.
- Dependencies: none.

T7.2 Add mzML conversion from normalized stream.

- Dependencies: T4/TDF reader adapters.

## Track 8: Query And XIC

T8.1 Query mzPeak output.

- Index/native id/time lookup.
- Dependencies: T5.2.

T8.2 XIC over mzPeak output.

- m/z/tolerance/time filters.
- Dependencies: T8.1.

T8.3 Raw input query/XIC.

- Depends on reader adapter random access support.

## Parallelization Guidance

Can run in parallel now:

- T2.1 Docker helper.
- T2.2 SDK discovery.
- T3.1 inspection hardening.
- T3.2 private fixture tests.
- T4b BAF reader subtasks after the BAF SDK path is available.
- T5.6 UV/PDA metadata forwarding independent of TDF work.
- T3.4 TDF fixture acquisition.
- T3.5 malformed fixture creation.
- T5.1 mzPeak dependency spike as a separate branch.
- T6.1 validation prototype.

Should wait:

- T4.2 until `timsrust-tsf` API is evaluated.
- T5.3 until a TSF stream exists.
- T5.4 until TDF fixture exists.
- Workspace split until at least one conversion path works.
