# Ingested handoff: Thermo RAW → mzPeak metadata mapping (mzPeak4TRFR)

> **Provenance.** Ingested verbatim from the sibling project's handoff at
> `~/Claude/mzPeak4TRFR/METADATA-MAPPING.md` on 2026-06-19. It documents how
> ThermoRawFileParser (TRFP) maps Thermo RAW metadata into mzPeak. BRFP's
> vendor-metadata layer (`src/vendor_metadata.rs`, `--vendor-metadata`,
> `--vendor-metadata-json`) is modeled on this design. The BRFP-specific actions
> derived from it live in **Phase I** of
> [refactor-plan-2026-06-19.md](../reviews/refactor-plan-2026-06-19.md). Kept here
> as the design reference; it describes the Thermo/.NET implementation, not BRFP.

---

## 1. Big picture

There are **two metadata mappings**, by design:

| Layer | What it captures | Who owns it | Where it lands |
|---|---|---|---|
| **Standard / CV** | Everything that has a PSI-MS controlled-vocabulary term (MS level, polarity, base peak, TIC, scan windows, precursor/isolation/activation, instrument config, software, sample, TIC chromatogram) | **Vendored mzPeak.NET** | The standard mzPeak facets: `spectra_metadata`, `chromatograms_metadata`, run-level metadata in `mzpeak_index.json` |
| **Verbatim / vendor** | Everything Thermo exposes that mzML's CV **cannot** represent — per-scan Trailer Extra bag, tune data, run header, instrument methods, status log, error log | **TRFP** (`Writer/MzPeak/Vendor*.cs`) | Proprietary non-CV Parquet entries (`vendor_*.parquet`), opt-in via `--vendor-metadata` |

Guiding principle: **never lose vendor information**. mzML forces everything
through CV terms and drops what doesn't fit; mzPeak keeps the standard mapping
*and* attaches the raw vendor metadata verbatim.

## 2. Per-scan flow (read-then-commit)

For each scan: read filter → MS level (guarded) → cheap `--msLevel` skip →
**READ PHASE** (ScanStatistics, SegmentedScan, CentroidStream, precursor+trailer)
→ **COMMIT PHASE** (AddSpectrumData → `spectra_data`; AddSpectrumPeakData →
`spectra_peaks`; AddSpectrum/AddScan/AddPrecursor/AddSelectedIon →
`spectra_metadata`; stream tall vendor trailers). Then TIC chromatogram, optional
vendor facets, close (writes facets + `mzpeak_index.json`), flush, set
`committed`, optional JSON sidecar.

## 3. Standard CV mapping (per spectrum/scan/precursor)

- spectrum: `MS:1000511` ms level, `MS:1000465` polarity, `MS:1000525`
  representation (centroid `MS:1000127` / profile `MS:1000128`), spectrum type as
  concrete child `MS:1000294`, `MS:1000504/1000505` base peak m/z/intensity,
  `MS:1000285` TIC, `MS:1003060` number of data points, `MS:1003059` number of
  peaks (coalescing rule §7).
- scan: start time, filter string, ion injection time, mass resolution, scan
  window lower/upper, FAIMS CV when present.
- precursor/selected_ion: isolation target + lower/upper offsets, activation
  (dissociation method + collision energy), charge, selected-ion m/z; master scan
  resolved from Trailer "Master Scan" with fallback to most-recent lower-MS-level.
- run-level: FileDescription (nativeID format, `MS:1000524` content + MS1/MSn
  children), instrument config, software, sample, data processing.

## 4. Verbatim vendor facets (opt-in `--vendor-metadata[=tall|wide|both]`)

| Facet | Layout | Schema | Source |
|---|---|---|---|
| `vendor_scan_trailers.parquet` | tall (scan×label) | `ordinal:u64, scan_number:i32, label, value, value_float` | per-scan Trailer Extra |
| `vendor_scan_trailers_wide.parquet` | wide (per scan) | `ordinal, scan_number, +typed column per label` | same, pivoted |
| `vendor_trailer_schema.parquet` | — | `ordinal, label, data_type, column_name, value_kind` | trailer header |
| `vendor_file_metadata.parquet` | — | `category, entry_index, label, value, value_float` | instrument/sample/run header/tune/status header/methods |
| `vendor_status_log.parquet` | — | `position, rt, label, value, value_float` | full status log |
| `vendor_error_log.parquet` | — | `index, rt, message` | run error log |

- **Keying:** every per-scan row carries dense `ordinal` (0..N-1, joins
  `spectra_*`) **and** verbatim `scan_number`.
- **Typed values:** `value` = exact verbatim string; `value_float` = typed numeric
  when datatype is numeric (avoids culture-dependent re-parse).
- **tall** = canonical/schema-stable; **wide** = convenience pivot; **both** = both.
- All vendor reads are **best-effort**: a failing API logs `Warn` and degrades
  that facet, never aborting a multi-GB conversion.

## 5. Injection seam

`StartProprietaryEntry` / `StartProprietaryParquetEntry` flush standard CV content
first, then append proprietary entries marked `EntityType(Other,"proprietary")` +
`DataKind.Proprietary`.

## 6/7. CLI & non-obvious decisions

- m/z stored lossless `Float64` (no Numpress); `--point` for point layout vs
  default chunk.
- **`number_of_data_points` vs `number_of_peaks` coalescing:** whatever
  `AddSpectrumData` writes lands in `spectra_data`, so its row count is always
  `number_of_data_points` regardless of profile/centroid. The library reports that
  as `PeakCount` for centroid scans, so TRFP coalesces `DataPointCount ?? PeakCount`
  into `number_of_data_points` and sets `number_of_peaks` only from the separate
  `spectra_peaks` facet. Without this the point-layout validator
  (`per_spectrum_data_points`) fails; chunk layout masks it.
- **Delete-on-failure:** any failure before commit deletes the partial `.mzpeak`;
  `committed` is set only after the final flush.
- **Read-then-commit phasing:** each scan fully read in a guarded block (skips +
  counts bad scans) before any write, so a bad scan can't half-write a spectrum.
- `CHAR` trailers are strings; wide columns joined by position (fixed per-file
  trailer schema).

## 8. Validation status

`mzpeak-validate` (profile `mzpeak-0.9`): 0 errors / 0 warnings across CLI modes,
five Thermo instrument classes, and large files (21 GB / 744k spectra). Standard
mapping matches the reference converter exactly. Six upstream mzPeak.NET
conformance bugs were patched in the vendored copy.
