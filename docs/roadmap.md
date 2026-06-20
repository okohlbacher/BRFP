# Roadmap

The project should move in narrow, testable milestones. Each phase should end
with a working binary or a concrete validation artifact.

## Phase 0: Planning And Repository Baseline

Status: started.

Deliverables:

- Repository scaffold with MIT license.
- CLI skeleton for `convert`, `inspect`, and `validate`.
- Upstream repo/spec analysis.
- Architecture, roadmap, and testing strategy.
- Decision log for reader/writer choices.

Acceptance:

- `cargo test` passes.
- `cargo run -- --help` displays the planned CLI.
- Initial documentation is sufficient to open implementation issues.

## Phase 1: TSF And BAF mzPeak Smoke Paths, TDF Spike

Goal: prove the mzPeak writer path with the local TSF examples, then convert
the local BAF examples through `libbaf2sql_c`, then convert one small Bruker TDF
`.d` directory to mzPeak.

Implementation tasks:

- Extract local SDK and example archives only into ignored `vendor/` and
  `fixtures/private/` directories.
- Implement `.d` detection for TDF vs TSF.
- Implement `inspect` for TDF and TSF SQLite metadata summaries.
- Implement `inspect` for BAF through generated/existing `analysis.sqlite`
  cache summaries.
- Add a pinned git dependency on HUPO-PSI/mzPeak.
- Keep `rusqlite`, `mzdata`, `timsrust`, Arrow, and Parquet dependency versions
  unified enough to avoid native link conflicts.
- Implement a narrow TSF line-spectrum reader for the provided TSF fixtures.
- Write TSF centroid-like spectra to mzPeak and verify with the mzPeak reader.
- Write BAF line/profile spectra to mzPeak through `libbaf2sql_c`.
- Decode BAF UV/PDA detector sidecars into mzPeak wavelength-spectrum and
  chromatogram facets when requested.
- Forward vendor metadata only; do not embed raw detector or vendor payload
  files in mzPeak archives.
- Extract UV/PDA method/header metadata into the mzPeak vendor metadata facet.
- Enable the TDF reader path through `mzdata` for the next conversion step.
- Implement input detection for `.d/analysis.tdf`.
- Implement `inspect` for TDF counts, acquisition mode, and metadata summary.
- Wire `convert` through `mzdata::MZReaderType` and mzPeak writer builder.
- Copy upstream metadata and append BRFP data-processing metadata.
- Write TIC/BPC chromatograms.
- Write a conversion report.

Acceptance:

- The provided positive and negative TSF examples can be inspected.
- A limited TSF conversion with `--limit-spectra` writes mzPeak output that can
  be opened by the mzPeak Rust reader.
- The provided positive and negative BAF/PDA examples can be inspected, converted
  in Docker/Linux, opened by the mzPeak Rust reader, and validated by
  `mzpeak-validate`.
- One public or locally supplied TDF fixture converts to `.mzpeak`.
- The output can be opened by the mzPeak Rust reader.
- Spectrum count, chromatogram count, and basic metadata match the input reader.
- The converter fails cleanly on missing `.d` components and on BAF conversion
  attempts without an available BAF SDK library/cache.

Note: the two local urine examples are TSF (`analysis.tsf`). They now validate
the writer dependency, Docker build path, and TSF line-spectrum conversion, but
they still do not satisfy the TDF conversion fixture requirement.

## Phase 2: TDF Production Hardening

Goal: make TDF conversion reliable across representative timsTOF acquisitions.

Implementation tasks:

- Add DDA-PASEF, DIA-PASEF, and edge-case fixtures.
- Verify precursor, selected ion, scan window, ion mobility, polarity, MS level,
  and collision energy mapping.
- Add MS-level filtering.
- Add compression/layout/type options backed by precision tests.
- Add progress logging and stable warning/report semantics.
- Add Windows and Linux CI.
- Add optional SDK-backed calibration tests gated by environment variables.

Acceptance:

- Representative DDA-PASEF and DIA-PASEF runs round-trip through mzPeak reader.
- Metadata invariants pass semantic validation through
  `okohlbacher/mzPeakValidator`.
- Conversion memory stays bounded on large inputs.
- Linux and Windows CI pass without proprietary SDK files.

## Phase 3: Secondary Outputs And Query Tools

Goal: expand utility after mzPeak conversion is stable.

Implementation tasks:

- Add mzML output through `mzdata` writer APIs or a dedicated writer adapter.
- Add `query` for spectra by index, native id, and time.
- Add `xic` for m/z/tolerance/time filters.
- Add directory batch mode.
- Add stdout modes only where output is stream-safe.

Acceptance:

- mzML output passes HUPO-PSI mzML validation and opens in standard readers for
  selected fixtures.
- `query` returns deterministic JSON for known spectra.
- `xic` results are validated against a reference implementation or golden data.
- Batch mode handles partial failures and produces a summary report.

## Phase 4: TSF And Imaging Hardening

Goal: move beyond the Phase 1 TSF line-spectrum smoke path to robust TSF and
imaging support.

Implementation tasks:

- Revisit direct `timsrust-tsf` reader integration after dependency conflicts
  with the current mzPeak/mzdata stack are resolved.
- Map TSF spectra and MALDI/imaging metadata to mzPeak or imzML-compatible
  structures.
- Decide how non-mass spectra or wavelength traces map under the evolving mzPeak
  spec.
- Add imaging-specific validation fixtures.

Acceptance:

- One TSF fixture is inspected and converted.
- Coordinate metadata and imaging dimensions are represented explicitly.
- Unsupported TSF variants fail with actionable diagnostics.

## Phase 5: Release Engineering

Goal: make BRFP usable as open-source software.

Implementation tasks:

- Create GitHub repository under `gh/okohlbacher`.
- Add CI for formatting, clippy, tests, and release builds.
- Add reproducible release packaging for Linux and Windows.
- Add installation documentation.
- Add SDK setup documentation without redistributing SDK artifacts.
- Publish signed release assets.

Acceptance:

- GitHub Actions produce release binaries.
- Users can run conversion on Linux and Windows with documented prerequisites.
- Dependency licenses are documented.

## Phase 6: Conformance And Interoperability

Goal: make outputs trustworthy for community adoption.

Implementation tasks:

- Track mzPeak spec changes and update schemas.
- Integrate the reference validator when available.
- Build a conformance corpus with public data and metadata expectations.
- Round-trip BRFP output through mzPeak Rust/Python readers.
- Compare mzML output against established tools where possible.

Acceptance:

- Every release publishes conformance results.
- mzPeak output is consumable by independent mzPeak implementations.
- Known limitations are documented per release.
