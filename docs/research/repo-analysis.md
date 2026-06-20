# Repository And Specification Analysis

Analysis date: 2026-06-18.

This document records the upstream behavior BRFP should copy, reuse, or avoid.
Research clones were kept outside the project tree in `/tmp/brfp-research`.

Implementation update: the initial research conclusion to defer BAF was
superseded after reviewing `tdf2mzml` and the local BAF/PDA fixtures. BRFP now
uses a separate `libbaf2sql_c` backend for BAF and keeps TDF-SDK usage scoped to
TDF/TSF.

## Sources Reviewed

- `compomics/ThermoRawFileParser`: mature converter CLI used as the behavioral
  inspiration.
- `mobiusklein/mzdata`: Rust MS data model with a Bruker TDF reader feature.
- `HUPO-PSI/mzPeak-specification`: current mzPeak draft and JSON schemas.
- `HUPO-PSI/mzPeak`: current Rust mzPeak reader/writer implementation and
  converter examples.
- `MannLabs/timsrust`: Bruker timsTOF Rust reader ecosystem, including TDF, TSF,
  and optional Bruker SDK calibration support.
- Bruker TDF-SDK page: current vendor statement about supported formats.
- `MannLabs/alphatims`: independent description of Bruker `.d` directory layout
  and PASEF/DIA-PASEF metadata tables.

## ThermoRawFileParser Lessons

ThermoRawFileParser is useful mainly as a CLI/product reference. Its core shape:

- One binary with a default conversion command and optional `query` and `xic`
  subcommands.
- Single-file mode and input-directory batch mode.
- Output format selection, metadata-only output, stdout mode, gzip/compression
  controls, log levels, MS-level filtering, warnings-as-errors, and cloud output.
- Clear separation between input parsing, raw reader setup, metadata writer, and
  spectrum writer.
- Defensive checks for still-acquiring files, empty files, native API errors,
  symlinks, and directory/file argument conflicts.

BRFP should copy the operational ergonomics, not the internals. The first CLI
should stay smaller: `convert`, `inspect`, and `validate` first; `query` and
`xic` after conversion is reliable.

## Bruker Format And SDK Constraints

Bruker's current TDF-SDK page says TDF-SDK provides programmatic access to raw
data from Bruker timsTOF instruments, supports reading `tdf` and `tsf` files
stored in `.d` directories, and does not support `baf`. It also says the archive
contains Windows and Linux versions, documentation, and C++/Python examples.

Implications:

- MVP target should be current timsTOF TDF `.d` directories.
- TSF should be a planned backend for imaging/MALDI workflows.
- BAF requires a separate backend; the selected backend is `libbaf2sql_c`, not
  TDF-SDK `libtimsdata`.
- Proprietary SDK binaries must not be committed. SDK-backed builds should use
  dynamic location through `TIMSDATA_LIB_DIR`, `PATH`, `LD_LIBRARY_PATH`, or a
  user CLI option.

Local workspace additions:

- `timsdata-5_0_3.zip` contains SDK documentation, schemas, C headers, Python/C++
  examples, `win64/timsdata.dll`, `win64/timsdata.lib`, and
  `linux64/libtimsdata.so`.
- `timsTOF_autoMSMS_Urine_6min_pos.d.zip` and
  `timsTOF_autoMSMS_Urine_6min_neg.d.zip` are TSF examples, not TDF examples.
  Each contains `analysis.tsf`, `analysis.tsf_bin`, chromatography SQLite files,
  `SampleInfo.xml`, and method files.
- The positive TSF example has 4,819 frames, 6,726,191 peaks, all positive
  polarity, and 3,465 `FrameMsMsInfo` rows.
- The negative TSF example has 4,854 frames, 7,701,301 peaks, all negative
  polarity, and 3,486 `FrameMsMsInfo` rows.

## mzdata Findings

`mzdata` is a published Apache-2.0 Rust library for mass spectrometry data. Its
supported formats include mzML/indexed mzML, MGF, mzMLb, Thermo RAW, Bruker TDF,
imzML, and PROXI. The Bruker path is behind the `bruker_tdf` feature.

The cloned `mzdata` TDF reader:

- Opens `.d/analysis.tdf` and reads frame metadata through SQLite.
- Uses `timsrust` for frame reading and coordinate conversion.
- Builds an internal spectrum index from `Frames`, `Precursors`,
  `PasefFrameMsMsInfo`, and `DiaFrameMsMsWindows`.
- Represents Bruker frames as ion-mobility-aware data, then exposes them through
  `MultiLayerSpectrum` for normal spectrum-streaming writers.
- Generates TIC and BPC chromatograms from frame summary columns.
- Exposes random access by index, native id, and time.

This makes `mzdata` the best first reader abstraction for TDF conversion because
the mzPeak Rust writer already consumes its data model.

Limitations to track:

- `mzdata` currently advertises Bruker TDF, not TSF. TSF may need direct
  `timsrust-tsf` integration.
- TDF spectra are conceptually ion-mobility frames. Flattening or unstacking must
  preserve the ion mobility coordinate and respect mzPeak's centroid/profile
  placement rules.
- `mzdata` release and git HEAD differ. BRFP should pin intentionally.

## mzPeak Specification Findings

The mzPeak specification is a HUPO-PSI working draft. The current docs describe
version `0.9.0` draft content and say the `docs/` tree is the canonical source.

Conformance requirements that affect BRFP:

- Every archive must include `mzpeak_index.json`.
- Writers must create a conformant archive and write Parquet page indexes for
  index/coordinate columns.
- Writers must declare every controlled vocabulary used in `cv_list` with pinned
  versions.
- Readers and writers must resolve arrays through the array index, not column
  names.
- Signal files must use exactly one layout per data/peak file: point or chunked.
- `spectra_data.parquet` is for profile data; centroid spectra go in
  `spectra_peaks.parquet`.
- The spec explicitly says timsTOF-style data that is centroided in m/z but
  profiled in ion mobility should be treated as centroid for the mass-spectrum
  dimension and stored in `spectra_peaks.parquet`.

BRFP should therefore default TDF centroid-like spectra to `spectra_peaks` and
only write `spectra_data` when true profile signal is available.

## mzPeak Rust Implementation Findings

The Rust mzPeak implementation is active but not currently published to crates.io
under the prototype package name. It uses `mzdata` types and provides:

- `MzPeakWriterBuilder` for choosing point vs chunked layout, compression,
  buffering, type overrides, null-zero behavior, separate peaks/profiles, and
  metadata fields.
- An example converter that opens input through `mzdata::io::MZReaderType`, then
  samples array schemas, copies metadata, streams spectra/chromatograms, and
  writes mzPeak.
- Existing Bruker handling in the converter: when the input is Bruker TDF, it
  disables older peak consolidation behavior so ion mobility is preserved.

BRFP should start by depending on the HUPO-PSI/mzPeak git repository or a local
path during the spike. Once a stable crate is published, switch to crates.io.

## timsrust Findings

`timsrust` is a modular Rust ecosystem from MannLabs. Crates.io shows version
`0.5.5`; the facade crate supports TDF, miniTDF, TSF, and Parquet spectra.

Important pieces:

- `timsrust-tdf`: direct TDF reader for `.d` folders.
- `timsrust-tsf`: direct TSF reader for MALDI/imaging style data.
- `timsrust-sdk`: optional C FFI to Bruker SDK. Its build script expects
  `libtimsdata.so` on Linux or `timsdata.dll`/import library on Windows, with
  `TIMSDATA_LIB_DIR` as an override.
- `timsrust` can use SDK calibration or patched/open calibration backends.

BRFP should use `mzdata` first for fastest mzPeak integration, but keep the
input boundary backend-oriented so direct `timsrust` TSF or SDK-calibrated TDF
paths can be added without rewriting the CLI or writers.

## Initial Technical Decisions

- License: MIT for BRFP. Upstream Rust libraries such as `mzdata` and
  `timsrust` remain under their own licenses.
- First implementation milestones now cover TSF and BAF mzPeak smoke paths,
  because local fixtures exist for both. TDF conversion still needs a TDF
  fixture for acceptance testing.
- Primary writer: HUPO-PSI/mzPeak Rust writer through a git/path dependency.
- Reader abstraction: start with `mzdata::MZReaderType` for TDF, wrap it behind a
  BRFP trait.
- Output default: mzPeak chunked layout with Zstd level 3.
- Optional output: mzML after mzPeak path is stable.
- Validation: mzPeak syntactic/semantic checks plus round-trip reads through the
  mzPeak reference implementation.

## Open Risks

- mzPeak is still a working draft; schema or writer APIs can change.
- Public Bruker test data with permissive redistribution terms must be confirmed
  before fixtures are committed.
- SDK redistribution/license terms need explicit documentation before releases.
- `mzdata` and `timsrust` version compatibility must be pinned and tested.
- DIA-PASEF, DDA-PASEF, and prmPASEF/MRM need separate acceptance data because
  metadata tables and precursor semantics differ.
