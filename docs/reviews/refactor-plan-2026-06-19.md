# BRFP Refactor Plan — porting patterns from mzML2mzPeak & mzPeak4TRFP

Date: 2026-06-19
Basis: [adversarial-synthesis](adversarial-synthesis-2026-06-19.md) +
[cross-project-comparison](cross-project-comparison-2026-06-19.md).

Goal: port the engineering patterns the sibling converters already prove
(streaming, data-driven dtype, typed errors, centralized CV, MS-level policy,
no-silent-data-loss, conformance tests) into BRFP, **without** attempting the
Bruker-specific TSF calibration rewrite in this pass (that is a separate,
fixture-gated effort and is only scoped, not implemented, here).

Each phase is independently shippable, ends green (`cargo fmt`, `clippy -D
warnings`, `test`), and lists explicit acceptance criteria. Risk is called out so
the riskiest structural change (streaming) lands after the safe correctness wins.

---

## Phase A — Foundations (low risk)

**A1. Centralized schema/CV module** (`src/schema.rs`).
Single source of truth for CV accessions + units, mirroring mzML2mzPeak
`src/schema/cv.rs`. Move the scattered `ControlledVocabulary::MS.param(...)`,
`Unit::*`, and magic accessions (MS:1000294, 1000131, 1000812, UO:269, …) used in
`mzpeak_writer.rs` behind named constants/helpers. Enables the cv_list
conformance test (Phase G) and kills unit drift.

**A2. Typed error taxonomy.** Extend `BrfpError` (pipeline.rs:24) with specific
variants replacing stringly-typed `Reader`/`Writer` for the recurring data
faults, modeled on mzML2mzPeak's `WriteError`:
- `AxisLengthMismatch { kind, a, b }`
- `NonFiniteValue { kind, index, value }`
- `NonPositiveMz { index, value }`
- `EmptySpectrum { id }` (used by Phase F)
Keep `Reader`/`Writer` for genuinely opaque cases. Update call sites in
`mzpeak_writer.rs` (`arrays_from_*`) and `tsf.rs`.

**A3. Fix the bogus `f32::MIN` bound** (mzpeak_writer.rs:843). `f32::MIN` is the
most-negative f32, so the lower-bound check is a no-op. Replace the ad-hoc
per-array numeric validation with shared helpers (`validate_finite_nonneg`,
`validate_axis_lengths`, `validate_strictly_increasing`).

Acceptance: no behavior change for valid data; new unit tests for each validator;
clippy clean.

---

## Phase B — Data-driven intensity dtype (medium)

Problem (H6): intensity is blindly `as f32` regardless of source precision.
mzML2mzPeak rule: m/z never narrows; intensity is written at its **native source
width**.

**CORRECTION (from plan adversarial review):** the mzPeak data-facet schema is
**file-wide** — `sample_array_types_from_spectrum_*` infers ONE intensity width
for the whole file (writer.rs:307). So intensity width is a **per-file**, not
per-spectrum, decision. This is fine because BRFP writes TSF and BAF to separate
files (separate `convert` runs), so each file gets the width native to its
backend:
- TSF intensities are decoded from the binary as `f32` (tsf.rs:176) then widened
  to f64 → the TSF file writes intensity as **Float32** (lossless, native).
- BAF intensities come from `array_read_double` (f64) → the BAF file writes
  intensity as **Float64** (lossless; today's blind `as f32` is the lossy bug).

**B1.** Pass an `IntensityEncoding { Float32, Float64 }` into
`arrays_from_mz_and_intensity` chosen by the **caller/backend** (TSF→Float32,
BAF→Float64). It sets the `DataArray`'s `BinaryDataArrayType`; the writer's
sampler then propagates that one width into the file schema. m/z stays Float64.
Drop the lossy `> f32::MAX` rejection (only meaningful when forcing f32) in favor
of width-correct encoding.

**B2.** Port a `profile_intensity_dtype`-style test: a synthetic f64-intensity
spectrum round-trips as Parquet DOUBLE; an f32-native one as FLOAT; values
identical, widths differ.

Acceptance: BAF intensities no longer silently truncated; test pins both widths.

---

## Phase C — UV unit honesty (medium; writer-constrained)

Problem (H5): UV absorbance (mAU) written with `Unit::DetectorCounts`
(mzpeak_writer.rs:800,867). Note MS intensity = `DetectorCounts` is **correct**
and stays; only the UV/absorbance path is wrong.

**C1.** Use a proper unit for absorbance arrays (investigate
`Unit::AbsorbanceUnit` / UO:0000269 in mzdata) or leave the array unit unset
while keeping the absorbance CV param, *whichever the mzPeak writer round-trips*.
Verify by reading back in the existing `writes_decoded_uv_wavelength_spectra`
test and asserting the unit. **Risk:** the writer comment claims the primary
intensity column only accepts schema-known units — if a correct unit is dropped
on read-back, fall back to "unit unset + documented param" rather than asserting
a false unit. Decision recorded in the test.

Acceptance: no array asserts `DetectorCounts` for absorbance; read-back test
proves whichever representation we keep.

---

## Phase D — TSF MS level & continuity (medium, correctness)

Problem (C2): every TSF frame written as MS1 centroid; `MsMsType` never read.

**D1.** Add `MsMsType` to the `read_frames` query (tsf.rs:311), carry `ms_level`
on `TsfFrame`/`TsfSpectrum`, and set it in `tsf_spectrum_to_mzdata`
(mzpeak_writer.rs:365). **Explicit mapping (from the v5 schema comments):**
`0 = MS → 1`; `2 = MS/MS → 2`; `3 = MSn → 3`; `8 = PASEF / 9 = DIA / 10 = PRM →
2` (these are fragmentation acquisitions); anything else → 2 with a warning. Set
the matching spectrum-type CV term (MS1 `MS:1000579` / MSn `MS:1000580`),
mirroring mzML2mzPeak `ms_level_or_ms1()` and mzPeak4TRFP's MSOrder handling.
Precursor/isolation metadata (FrameMsMsInfo) is **out of scope here** — recorded
as a follow-up, because it needs the fragmentation tables and reference
validation.

Acceptance: TSF MS2 frames carry ms_level=2 + MSn term; unit test on a synthetic
Frames table; README "implemented" note corrected accordingly.

---

## Phase E — Streaming write path (higher risk, flagship port)

Problem (H2): `write_tsf_to_mzpeak`/`write_baf_to_mzpeak` collect **all** spectra
into a `Vec`, and `write_spectra_to_mzpeak` additionally clones them into a
sampling stream (mzpeak_writer.rs:194) — double residency.

**E1.** Refactor `write_spectra_to_mzpeak` to take `impl Iterator<Item =
BrfpResult<BrfpSpectrum>>` plus a sampled first spectrum for dtype inference
(mzML2mzPeak pattern: buffer ONE, build writer, write it, then stream the rest).
Convert TSF/BAF callers to stream from their index-based readers without
materializing. Keep UV/chromatogram handling (already bounded-ish) but only build
those after the MS stream when detector data is requested.

**Risk/мitigation:** the writer builder needs array-type sampling before writing;
we sample from the first decoded spectrum only. UV chromatogram type-sampling
currently consumes a Vec — keep that path buffered (UV data is small) and gate it
behind `include_detector_data`. Land E behind the Phase B/D changes so dtype +
ms_level are already correct when streaming.

Acceptance: peak memory bounded (one MS spectrum live); existing read-back tests
still pass; a `streaming`-style test asserts a multi-spectrum file converts
without collecting.

---

## Phase F — No silent data loss (correctness)

Problem (H1/H7, shared with mzPeak4TRFP A5): BAF spectra with missing/negative
array IDs silently become empty; `write_baf_to_mzpeak` also errors the whole run
if nothing matches, but individually-empty spectra vanish.

**F1.** Emit a zero-point spectrum carrying metadata (RT, ms_level, polarity) for
genuinely empty acquisitions, or fail per an explicit policy; warn (not silently
drop) when an array ID is negative/missing. Surface counts in the report.

Acceptance: empty BAF spectrum produces a metadata row, not a dropped scan;
warning recorded.

---

## Phase G — Conformance tests + CI (process)

**G1.** Port adapted `cv_list` (declared == referenced CV set) and `sorting_rank`
(m/z ascending on write) tests over BRFP output.
**G2.** CI: commit one tiny synthetic TSF fixture and assert decoded m/z + MS
level in CI (closes H4's "nothing real runs"); mark SDK/private tests `#[ignore]`
so skips are visible; add an optional `mzpeak-validate` job.

---

## Phase H — Supply chain hygiene (low risk)

**H1.** Track `Cargo.lock`; evaluate aligning `mzpeak_prototyping` to the
reference rev `29e59b24` with exact `=` pins (separate, tested PR — schema may
shift); add a release guard script that fails on vendored SDK/raw/tmp artifacts.

---

## Implementation status

**Increment 1 (A + B + D) — DONE, verified green** (`cargo fmt --check`, `clippy
--all-targets --all-features -D warnings`, `cargo test --all-targets`: 41 unit +
4 e2e passing):
- A1: new `src/schema.rs` — centralized CV terms, units, `IntensityEncoding`,
  `ms_level_from_msms_type`, `spectrum_type_for_ms_level` (+ unit tests).
- A2: `BrfpError` gained typed `AxisLengthMismatch` / `NonFiniteValue` /
  `NonPositiveMz`; used in `arrays_from_mz_and_intensity` (existing asserted
  error strings untouched).
- A3: fixed the no-op `f32::MIN` bound in the UV chromatogram path (now finite +
  within-f32-range; negative absorbance allowed).
- B1/B2: `arrays_from_mz_and_intensity` takes `IntensityEncoding`; TSF writes
  Float32 (lossless native), BAF writes Float64 (no more silent truncation);
  m/z always Float64. Tests pin both widths and an f32-unrepresentable value
  surviving via Float64.
- D1: TSF `read_frames` now reads `MsMsType`; `ms_level` carried on
  `TsfFrame`/`TsfSpectrum`; `tsf_spectrum_to_mzdata` sets per-frame ms_level +
  MS1/MSn spectrum-type term. In-memory-SQLite test pins the mapping.

**Increment 2 (C + F + E + G + H) — DONE, verified green** (42 unit + 4 e2e
passing; release-guard exits 0):
- C: UV absorbance arrays now use `Unit::AbsorbanceUnit` (UO:0000269) instead of
  `DetectorCounts`; a read-back assertion proves the unit survives the mzPeak
  writer (the old "writer rejects non-standard units" caveat was unfounded).
- F: BAF `sql_array_id` now warns (with spectrum id + field) on present-but-zero
  or negative array ids — a stale/damaged cache can no longer masquerade as a
  legitimately-empty scan; genuine `NULL` still maps silently to empty.
- E: `write_spectra_to_mzpeak` is now generic over
  `Iterator<Item = BrfpResult<BrfpSpectrum>>`; it buffers a bounded
  `ARRAY_SAMPLE_LIMIT` sample to infer the schema, then streams the rest one at a
  time. TSF converts straight from the reader (no full-run `Vec`); BAF keeps its
  selection/limit/"no match" semantics but drops the extra sampling clone. A test
  streams 3×+ the sample buffer and round-trips a tail spectrum.
- G: added a streaming conformance test and a CI `release-guard` step.
- H (parallel agent): release-guard script, `.gitignore`/Cargo.lock audit
  (already correct), and a factual README status update for TSF MS levels.

All planned phases for this refactor are implemented. Remaining items are the
explicitly-out-of-scope, Bruker-specific efforts below.

## Execution order & first increment

Order: **A → B → D → C → F → E → G → H**. (Safe correctness first; streaming
after dtype/ms-level are correct; hygiene last.)

**This session implements increment 1 = A + B + D**, verified green, because they
are contained, high-value, and directly port sibling patterns. C, E, F, G, H stay
planned and adversarially reviewed for follow-up increments.

## Plan adversarial-review outcome (2026-06-19)

Reviewed by a Claude Explore agent + Codex against the real code. Verdict for
increment 1 (A+B+D): **GO-WITH-CHANGES**. Must-fix items, all folded in above:
1. Phase B reframed as **file-wide** intensity width per backend (TSF Float32 /
   BAF Float64) — per-spectrum width is impossible (file-wide schema).
2. Phase D mapping made explicit for MsMsType 2/3/8/9/10, not "≠0 ⇒ 2".
3. Phase A2: preserve `#[error(...)]` message text verbatim so existing
   `to_string().contains(...)` assertions (validation.rs, pipeline tests) keep
   passing.
4. Phase A3: the `f32::MIN` lower bound (mzpeak_writer.rs:843) is a confirmed
   no-op; replace with a real `>= 0.0` check.

Confirmed-true assumptions: `MsMsType` exists in tsf-schema_v5;
`Unit::AbsorbanceUnit` (UO:0000269) exists in mzdata 0.64.1;
`sample_array_types_from_spectrum_source` exists on the pinned writer rev (streaming
is feasible later but needs a `RandomAccessSpectrumSource` impl — Phase E stays
deferred).

## Increment 3 — deferred Bruker-specific items (DONE, verified green)

Investigating the deferred TSF-calibration work against the authoritative
pure-Rust reference (`timsrust-tsf` 0.1.4) **corrected two of the original
review's CRITICAL findings** (see the correction appended to
[adversarial-synthesis](adversarial-synthesis-2026-06-19.md)):

- **C3 was wrong.** BRFP's TSF binary decode (8-byte header, zstd, `n*8` bytes of
  `f64` TOF indices + `n*4` bytes of `f32` intensities) is **byte-for-byte
  identical** to the reference `TsfBlobReader`. No change needed.
- **C1 was overblown.** The `sqrt`-linear two-point model *is* the canonical TSF
  conversion (the reference uses exactly it; per-frame `MzCalibration`/`T1`/`T2`
  are TDF-only). The one genuine gap was the **otofControl ±5 Th boundary
  correction**, now implemented in `Tof2MzConverter::from_metadata` with a unit
  test and validated against the real urine fixture (which is `timsTOF`-acquired,
  so the correction correctly does not apply there).
- **H3 (FFI hardening):** SDK string buffers are now bounded
  (`MAX_BAF_PATH_BUFFER_BYTES`, `MAX_BAF_ERROR_BUFFER_BYTES`) so a buggy/hostile
  library cannot trigger a huge allocation. (Array reads were already capped.)
- **BAF calibration provenance (H1):** the calibration mode actually used is now
  recorded as a `data_processing` param in the mzPeak output, so a raw/uncalibrated
  fallback is durable provenance, not just a transient warning.

Still genuinely out of scope (needs the proprietary SDK on a Linux/Windows runtime
or new fixtures): SDK-backed BAF e2e in CI, TDF spectrum conversion, mzML output.

## Phase I — Vendor-metadata layer parity & conformance (ingested from the mzPeak4TRFR handoff)

Source: [mzpeak4trfr-metadata-mapping-handoff](../research/mzpeak4trfr-metadata-mapping-handoff.md).
That doc is the design origin of BRFP's vendor layer; this phase distils its
transferable lessons into Bruker/Rust actions. The Thermo-only facets (Trailer
Extra, status/error log) have no exact Bruker analog and are mapped to their
closest equivalents (BAF `Properties`, per-spectrum variables).

**Already shared with the handoff (confirmed, no work):** the two-layer design
(standard CV via `mzpeak_prototyping` + verbatim vendor facets); the
`vendor_file_metadata.parquet` schema `category, entry_index, label, value,
value_float` matches TRFP exactly; `--vendor-metadata[=tall|wide|both]` +
`--vendor-metadata-json`; `DataKind::Proprietary` injection seam; lossless
`Float64` m/z with chunked default; emitting spectrum type as the concrete child
`MS:1000294`.

Actions, by value:

- **I1 — `number_of_data_points` vs `number_of_peaks` conformance** (HIGH). The
  handoff's sharpest gotcha: in **point layout** the validator
  (`per_spectrum_data_points`) fails unless data-point count comes from the
  written data facet and peak count only from the separate peaks facet; chunk
  layout (BRFP's default) masks it. Action: add a point-layout round-trip test and
  confirm `mzpeak_prototyping` populates both counts correctly; if not, coalesce as
  TRFP does. **Run `mzpeak-validate` on BRFP output** to confirm 0/0 like TRFP.
- **I2 — Delete-on-failure** (MEDIUM, small). BRFP `File::create`s the output then
  can error mid-write (now more reachable after the streaming refactor), leaving a
  truncated `.mzpeak`. Adopt TRFP's "no corrupt output": remove the partial output
  on any error before the final `finish`. *(Implemented in this pass — see status.)*
- **I3 — Best-effort vendor reads** (MEDIUM). Ensure `VendorMetadataBundle::collect`
  degrades per-facet with a `warn!` rather than aborting the conversion (TRFP:
  "never abort a multi-GB run for an optional facet"). Audit current error
  propagation in `vendor_metadata.rs`.
- **I4 — Wide vendor layout** (MEDIUM). `--vendor-metadata=wide` currently warns and
  falls back to tall (`vendor_metadata.rs:149`). Either implement the per-label
  typed-column pivot for the file-metadata facet (+ a `vendor_trailer_schema`-style
  label→column map) or make the CLI/help state it's tall-only, so the flag isn't
  silently misleading.
- **I5 — Per-spectrum vendor facet** (MEDIUM/LARGE). TRFP streams a tall
  per-scan trailer facet keyed by `ordinal` + native id, with typed `value_float`,
  in bounded row groups. BRFP's analog: a `vendor_scan_metadata.parquet` from BAF
  `Properties`/per-spectrum variables (and TSF per-frame fields), streamed during
  the now-streaming write loop. Adopt the **ordinal (0..N-1) + verbatim native id**
  keying and `value/value_float` typing convention so facets join the `spectra_*`
  tables the same way.
- **I6 — Richer run-level metadata** (MEDIUM). The handoff's run-level block is
  fuller than BRFP's `configure_mzpeak_metadata` (which writes a placeholder
  instrument model and minimal software/sample/processing). Enrich from Bruker
  `GlobalMetadata`/`Properties`: real instrument model/serial, acquisition
  software, sample name — keeping unmappable values in the vendor facet.

Sequencing: I1 (conformance, do with G's CI fixture) → I3 → I4 → I6 → I5.

## Explicitly NOT in this refactor
- TSF m/z calibration via timsrust/SDK (C1) and TSF binary-layout validation (C3)
  — Bruker-specific, fixture+SDK-gated; tracked separately.
- BAF FFI buffer hardening (H3) and calibration-fallback provenance (BAF H1) —
  belong with the SDK-boundary work, not the cross-project port.
- mzML output, TDF conversion (already roadmap post-MVP).
