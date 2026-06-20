# Cross-Project Comparison: BRFP vs mzML2mzPeak vs mzPeak4TRFP

Date: 2026-06-19

Compares BRFP's adversarial-review findings
([adversarial-synthesis-2026-06-19.md](adversarial-synthesis-2026-06-19.md))
against two sibling converters that also emit HUPO-PSI mzPeak:

- **mzML2mzPeak** (`~/Claude/mzML2mzPeak`) — mature Rust reference converter
  (mzML/imzML → mzPeak). Cited as "the Rust reference converter" by mzPeak4TRFP.
- **mzPeak4TRFP** (`~/Claude/mzPeak4TRFP`) — C#/.NET fork of ThermoRawFileParser
  adding a `--format mzpeak` writer (Thermo .raw → mzPeak). BRFP mimics TRFP's
  CLI shape.

Key framing: both siblings read from **already-calibrated, already-MS-leveled**
sources (mzML, Thermo RAW). So BRFP's calibration/MS-level bugs have no analog
there — but both siblings *write* mzPeak and have independently solved the
engineering concerns BRFP fails (streaming, dtype policy, units, schema
conformance, precursor linkage). **BRFP has largely regressed relative to its own
reference implementation.**

---

## Difference matrix

| BRFP finding | BRFP today | mzML2mzPeak | mzPeak4TRFP | Takeaway |
|---|---|---|---|---|
| **m/z calibration** (C1) | Hand-rolled `sqrt`-linear approximation | N/A — m/z arrives calibrated in mzML | N/A — calibrated by Thermo API | **Bruker-specific; no precedent to borrow.** Must use timsrust/SDK. |
| **MS level** (C2) | Hardcoded MS1; `MsMsType` never read | Single-source policy `ms_level_or_ms1()`, explicit spectrum-type CV term (`spectrum.rs:89`) | Reads Thermo `MSOrder`, sets MS1/MSn CV term (`ScanStager.cs:31,81`) | **Both siblings do it right.** BRFP is the only one hardcoding. |
| **Peak binary layout** (C3) | Decoded as `f64` indices (suspect) | N/A (mzdata decodes) | N/A (Thermo API) | Bruker-specific; validate vs timsrust. |
| **Streaming / memory** (H2) | Collects ALL spectra into a `Vec` | One spectrum live at a time; "NO buffering/sorting" + bounded-memory test on 34,840-spectrum file (`convert.rs:190`, `streaming_reader.rs`) | Streams to temp Parquet, dual row-group cap (1 M rows / 64 MB) (`PointFacetStream.cs:38`) | **Both stream; BRFP materializes.** Direct pattern to copy. |
| **Intensity dtype** (H6) | Blind `as f32`, broken `f32::MIN` bound, no test | Data-driven: narrows f64→f32 **only if source f64**, never narrows m/z; 3-direction pinning test (`profile_intensity_dtype.rs`) | Fixed f64 m/z + f32 intensity, configurable point/chunk/numpress, correct array_index transforms | BRFP should adopt mzML2mzPeak's tested, source-driven policy. |
| **FFI buffer safety** (H3) | No length guard on `array_read_double`; unbounded SDK string buffers | N/A (no FFI) | N/A (managed .NET) | Bruker-specific; bound all SDK buffers. |
| **BAF raw-calib fallback / data loss** (H1, H7) | Silent raw fallback + empty spectra | Typed errors: `NonFiniteMz`, `AxisLengthMismatch`, `NonPositiveCoordinate` (`spectrum.rs:120-151`) | A5 self-noted: empty spectra still silently dropped (`ScanStager` returns null) | mzML2mzPeak's **typed error taxonomy** is the model. mzPeak4TRFP shares the empty-spectrum bug. |
| **UV unit = DetectorCounts** (H5) | Absorbance mislabeled `Unit::DetectorCounts` | No UV; optical handled as `role="optical"` metadata, not a fake-unit array | Centralized `MzPeakCv` constants; all units correct (`MzPeakCv.cs:9`) | Both avoid unit lies via **centralized CV constants**. BRFP should define a proper UO term or leave unset. |
| **Schema conformance** (M3, schema risk) | No sorting_rank / cv_list / page-index handling or tests | Explicit `cv_list()` single source + tests `sorting_rank.rs`, `cv_list.rs`, `conformance_l2.rs` (L1/L2) | array_index transforms per layout; pyarrow+validator differential tests | mzML2mzPeak's conformance test suite is the gold standard to port. |
| **CI runs real conversions** (H4) | All decode env-gated; nothing real in CI | `msconvert-nonthermo.yml` downloads vendor RAW → mzML → mzPeak end-to-end + uploads artifact; committed-fixture streaming tests run unconditionally | `buildandtest.yml` builds+tests; e2e validator harness exists but **not in CI** | mzML2mzPeak has true e2e CI. **Both BRFP and TRFP lack validator-in-CI.** |
| **Streaming verify / preflight** (new) | None | `preflight` binary (UUID+checksum gate, spawned-process test); streaming `verify` (L1/L2) on real data | e2e `compare_mzpeak.py` differential | Adopt mzML2mzPeak's preflight + verify gates. |
| **mzpeak_prototyping pin** (S1) | rev `b63302b9`, arrow `57.3.1` (loose) | rev `29e59b24` (newer), arrow/parquet/mzdata pinned **exactly** `=`, with a vendored 512MB→16MB row-group patch | (C#, uses Parquet.Net) | **BRFP is on an older rev with looser pins.** Align to `29e59b24` + exact pins, or document why not. |
| **Supply chain / artifacts** (S2, S3) | SDK binaries + zips in tree; `Cargo.lock` untracked | `Cargo.lock` tracked; CLAUDE.md stack discipline | gitignored refs; vendored TRFP under license | Track `Cargo.lock`; add release guard. |
| **README overclaim** (S4) | Claims TSF MS levels + TIC/BPC implemented | Claims match tests | Self-audit in `.planning/review/SYNTHESIS.md` with A1–A6 tracked | Adopt a tracked review log like TRFP's SYNTHESIS.md. |

---

## What this tells us

1. **The CRITICALs are genuinely Bruker-specific.** TSF m/z calibration (C1),
   peak layout (C3), and FFI safety (H3) have no analog in the siblings because
   they read pre-calibrated sources with managed/native readers. There is no
   shortcut here — the fix is `timsrust`/SDK, as both Codex and Kimi noted
   (`timsrust` is already a transitive dep).

2. **Everything else BRFP was dinged for is already solved next door.** MS level,
   streaming, dtype policy, units, schema conformance, typed errors, precursor
   linkage, real CI — mzML2mzPeak (Rust, same writer crate) implements all of
   them with tests. BRFP can port patterns almost verbatim, not invent them.

3. **mzPeak4TRFP confirms the operational target** (MS-level from source,
   precursor resolution, configurable point/chunk/numpress, nullable leaf
   semantics) — and shares two of BRFP's bugs: **empty spectra silently dropped**
   and **no validator in CI**. Those two are cross-project and worth fixing in
   both.

4. **BRFP is on an older mzPeak prototype rev with looser pins** than the
   reference. Worth aligning before schema-conformance work, so BRFP targets the
   same schema mzML2mzPeak validates against.

---

## Concrete cross-pollination list (priority order)

1. **Port mzML2mzPeak's streaming write loop** (`src/write/mzml.rs`,
   `convert.rs`) — replaces BRFP H2 materialization with a proven bounded-memory
   pattern.
2. **Port the dtype-narrowing policy + `profile_intensity_dtype.rs` test** —
   fixes H6 with an established, tested rule.
3. **Port the conformance test trio** (`sorting_rank.rs`, `cv_list.rs`,
   `conformance_l2.rs`) and the centralized `schema/cv.rs` — closes the schema
   risk and the UV unit lie (H5) via a single CV source of truth.
4. **Adopt the typed error taxonomy** (`NonFiniteMz`, `AxisLengthMismatch`, …)
   and stop silently emitting empty spectra (H7 / TRFP A5).
5. **Copy `msconvert-nonthermo.yml`'s shape** for a real BRFP e2e CI job, and add
   `mzpeak-validate` to CI (gap shared with TRFP).
6. **Align `mzpeak_prototyping` to rev `29e59b24` with exact `=` pins** (S1);
   evaluate the 16 MB row-group patch for BRFP's profile spectra.
7. **Adopt a tracked self-audit log** like mzPeak4TRFP's `.planning/review/
   SYNTHESIS.md` so README claims and known gaps stay honest (S4).
