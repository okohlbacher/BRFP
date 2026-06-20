# Adversarial Review Synthesis

Date: 2026-06-19

Reviewers (independent, read-only, run in parallel):

- Claude (code-reviewer agent, Opus 4.8)
- OpenAI Codex CLI (`codex exec`, read-only sandbox)
- Moonshot Kimi CLI
- Mistral Vibe CLI (read-only plan mode)

Scope: Rust source under `src/`, `tests/mzpeak_e2e.rs`, `Cargo.toml`,
`.github/workflows/ci.yml`, and `docs/`. Focus on scientific-data correctness,
FFI safety, error handling, mzPeak schema compliance, resource use, supply
chain, and test coverage.

Raw per-reviewer reports are archived under `tmp/adversarial-review/`
(`codex.out.md`, `kimi.full.md`, `vibe.full.md`, and the Claude transcript).

---

## Consensus map

| # | Finding | Severity (synth) | Claude | Codex | Kimi | Vibe |
|---|---------|------------------|:------:|:-----:|:----:|:----:|
| C1 | TSF m/z conversion is an unverified `sqrt`-linear approximation, not Bruker calibration | CRITICAL | ✅ | ✅ | ✅ | ◑ |
| C2 | TSF writes every frame as MS1 centroid; `MsMsType` never read | CRITICAL | ✅ | ✅ | ✅ | ✅ |
| C3 | TSF peak binary decoded as `f64` indices — likely wrong on-disk layout | CRITICAL | ✅ | – | – | – |
| H1 | BAF `auto` calibration silently falls back to RAW m/z; only a warning | HIGH | ✅ | ✅ | ◑ | – |
| H2 | Whole run materialized in memory; "streaming" is cosmetic | HIGH | ✅ | ✅ | ✅ | ✅ |
| H3 | BAF FFI array read has no length guard (`array_read_double`); SDK string buffers unbounded | HIGH | ✅ | ✅ | – | ✅ |
| H4 | No data-correctness test runs in CI; all real decode is env-gated | HIGH | ✅ | ✅ | ✅ | ✅ |
| H5 | UV absorbance (mAU) written with `Unit::DetectorCounts` | HIGH | ✅ | ✅ | ✅ | – |
| H6 | Intensity silently down-cast f64→f32 without the documented precision test; bogus `f32::MIN` bound | HIGH | ✅ | – | ✅ | – |
| H7 | BAF spectra with missing/negative array IDs silently become empty (data loss) | HIGH | – | ✅ | ✅ | – |
| M1 | Stale BAF SQLite cache allowed by default (warning only) | MEDIUM | – | ✅ | ◑ | – |
| M2 | BAF filters (`--ms-level`, ranges) applied *after* reading every array | MEDIUM | – | ✅ | – | – |
| M3 | BAF profile/centroid continuity inferred from array choice, not descriptors | MEDIUM | ✅ | – | – | – |
| M4 | UV wavelength axis fabricated by even spacing | MEDIUM | ✅ | – | – | – |
| M5 | U2 intensity delta-decode can integer-overflow on malformed input | MEDIUM | – | – | – | ✅ |
| M6 | U2 header parser returns `Ok(Some)` with `None` fields on truncation → later panic | MEDIUM | – | – | – | ✅ |
| M7 | Retention time / TOF index unvalidated (NaN/inf/out-of-range propagate) | MEDIUM | – | – | – | ✅ |
| M8 | Zero-peak TSF frames still read a header at byte 0 | MEDIUM | – | ✅ | – | ✅ |
| M9 | No negative/edge tests (truncated files, bad magic, length mismatch) | MEDIUM | ✅ | ✅ | – | ✅ |
| S1 | Pinned git dependency on `mzpeak_prototyping` prototype | MEDIUM | ✅ | ✅ | ✅ | ✅ |
| S2 | Proprietary Bruker SDK binaries + raw/generated artifacts live in the working tree | MEDIUM | ✅ | ✅ | ✅ | – |
| S3 | `Cargo.lock` untracked for a binary crate | LOW | ✅ | – | – | – |
| S4 | README "Implemented" overstates reality (MS levels, TIC/BPC chromatograms) | MEDIUM | ✅ | ✅ | ✅ | ✅ |
| S5 | `ClosedProperly != 1` (truncated acquisition) is a warning, not a failure | LOW | ✅ | – | – | – |

✅ = raised directly · ◑ = touched/partial · – = not raised

Bonus cross-checks discovered during review:

- **Codex** read the vendored SDK header `vendor/timsdata/include/c/tsfdata.h`
  and confirmed `tsf_index_to_mz(handle, frame_id, in, out, cnt)` exists and is
  **frame-dependent** — i.e., the hand-rolled global model in `tsf.rs` cannot be
  correct for recalibrated/per-frame runs.
- **Kimi** confirmed `timsrust v0.4.1` is **already in the dependency tree** via
  `mzdata`, so a validated TSF/TDF reader is available without adding a new
  dependency.

---

## The big picture

Three of four reviewers independently rank the **TSF data path** as the most
dangerous area: m/z is approximated rather than calibrated (C1), the on-disk
peak layout may be misinterpreted as `f64` (C3), and every spectrum is emitted
as MS1 (C2). Because **no decode-correctness test runs in CI** (H4), all of
these can ship while looking green. The unifying theme is *silent scientific
corruption presented as calibrated output* — the worst failure mode for a
converter whose entire value proposition is faithful conversion.

The second cluster is **robustness against hostile/old vendor data**: unbounded
FFI buffers (H3), silent empty spectra (H7), stale caches (M1), overflow in U2
decode (M5/M6), and unvalidated numerics (M7). These don't corrupt good data but
turn malformed input into crashes or quiet data loss.

The third cluster is **process hygiene**: env-gated tests (H4), README
overclaims (S4), the prototype git dependency (S1), and proprietary artifacts in
the tree (S2). Low individual severity, high aggregate risk for a project headed
toward release.

---

## Implementation roadmap

Ordered by risk-reduction per unit effort. Each phase ends in a state you can
ship and defend.

### Phase 0 — Stop the bleeding / make defects visible (days)

Goal: ensure the critical bugs below cannot hide, and the repo is releasable.

1. **CI decode gate (H4).** Commit one tiny, redistributable TSF fixture (or a
   reference dump checked into the repo) and assert decoded m/z + MS level
   against a known-good oracle in CI. Mark all SDK-gated tests `#[ignore]` so
   their non-execution is visible rather than silent.
2. **Honest README (S4).** Move "TSF MS levels", "TIC/BPC chromatograms", and
   "calibrated m/z" out of *Implemented* until they actually hold. Cheap, and it
   reframes everything below as known gaps rather than regressions.
3. **Release/supply-chain guard (S1, S2, S3).** Commit `Cargo.lock`; add a
   pre-release script that fails if `vendor/`, `data/`, `tmp/`, or `*.zip`
   artifacts are present in the tree; pin/track `mzpeak_prototyping` to a tag and
   add a CI check that the rev still resolves.

### Phase 1 — TSF scientific correctness (1–2 weeks)

Goal: TSF output is trustworthy or refuses to claim it is.

4. **Route TSF m/z + MS level through `timsrust`/SDK (C1, C2, C3).** Replace the
   hand-rolled `Tof2MzConverter` and the `f64`-index decode with the already-
   present `timsrust` reader (or `tsf_index_to_mz` via the SDK path BRFP already
   loads). Read `MsMsType`/fragmentation tables, set per-frame MS level, attach
   precursor/isolation metadata. Validate against the SDK on the committed urine
   fixtures. *If* a pure-Rust path is kept, gate uncalibrated output behind a
   loud, provenance-recorded warning rather than presenting it as calibrated.
5. **TSF edge handling (M8, M7).** Return empty spectra immediately for
   `NumPeaks == 0`; validate TOF index range and retention-time finiteness.

### Phase 2 — BAF integrity & FFI hardening (1–2 weeks)

Goal: BAF never emits uncalibrated or partial data as if it were complete, and
the SDK boundary is crash-safe.

6. **Calibration honesty (H1, M1).** In `auto` mode, make raw fallback and
   stale-cache use either a hard error by default (opt-in via
   `--calibration-mode raw` / `--allow-stale-cache`) or tag every affected
   array with a `calibration=raw` CV param recorded in mzPeak provenance and the
   conversion report — not a transient log line.
7. **FFI boundary (H3, H7).** Bound all SDK-returned buffer sizes; re-query
   element count immediately before `array_read_double` and treat any mismatch
   as an error; reject negative non-null array IDs with spectrum context;
   add a post-load SDK sanity/version check before issuing data calls.
8. **Filter before read (M2).** Apply `--ms-level`/range filters to BAF row
   metadata first, then read arrays only for selected spectra.

### Phase 3 — Detector/UV correctness & schema fidelity (1 week)

9. **UV units & axis (H5, M4).** Stop asserting `DetectorCounts` for mAU
   absorbance — use the correct unit or leave it unset; read the real wavelength
   axis (or document and test the even-spacing assumption against the MTBLS18
   NetCDF reference, which must run somewhere automated).
10. **Intensity precision (H6).** Keep intensity as f64 until a documented
    precision test justifies f32; unify UV/spectrum/chromatogram intensity types;
    fix the no-op `f32::MIN` lower-bound check.
11. **U2 robustness (M5, M6).** Bound delta accumulation; validate minimum file
    size before header parsing so truncated files error instead of panicking.

### Phase 4 — Streaming & scale (1–2 weeks)

12. **Bounded-memory pipeline (H2).** Replace `Vec`-materialization with a
    reader→writer stream over a bounded channel (the architecture's own model);
    chunk-read or mmap `analysis.tsf_bin` instead of `fs::read`; stream BAF rows
    via SQLite `LIMIT/OFFSET` rather than loading all metadata.

### Phase 5 — Test depth (ongoing, start in Phase 0)

13. **Negative & edge corpus (M9).** Truncated files, bad magic, length
    mismatches, missing arrays, NaN numerics, zero-peak frames. Run in CI without
    proprietary deps.

---

## Suggested first PRs

- **PR1 (Phase 0):** README correction + `Cargo.lock` + release guard script +
  `#[ignore]` on SDK tests. Pure hygiene, no behavior change, unblocks honesty.
- **PR2 (Phase 1, highest value):** TSF via `timsrust` for m/z + MS level, with a
  CI fixture asserting both. Closes C1, C2, C3, and H4 in one stroke.
- **PR3 (Phase 2):** Calibration-honesty + FFI bounds. Closes H1, H3, H7, M1.

---

## Correction (2026-06-19, after implementation against the reference reader)

Implementing the fixes surfaced evidence that **revises two CRITICAL findings**.
Cross-checked against the authoritative pure-Rust reference `timsrust-tsf` 0.1.4
(`src/mz.rs`, `src/blobs.rs`):

- **C3 (TSF peak binary decoded as f64) — RETRACTED.** TSF genuinely stores TOF
  indices as little-endian `f64` and intensities as `f32` (`n*8 + n*4` bytes per
  `n` peaks, 8-byte chunk header, zstd). BRFP's decoder is byte-for-byte
  identical to the reference. Not a bug.
- **C1 (TSF m/z is a wrong approximation) — DOWNGRADED.** The two-point
  `sqrt`-linear model is the *canonical* TSF conversion; the reference uses
  exactly it. The per-frame `MzCalibration`/`T1`/`T2` columns are TDF-only. The
  only real defect was the missing **"Bruker otofControl" ±5 Th boundary
  correction**, which has been implemented (`Tof2MzConverter::from_metadata`).
  C2 (MS levels) remains valid and was fixed separately.

Lesson: the reviewers reasoned from general timsTOF/TDF calibration knowledge and
over-applied it to TSF. The fix was much smaller and safer than "route through the
SDK/timsrust"; the headline risk was real but narrower than stated. H1, H3, H7,
M1, and the streaming/dtype/units items stand as originally assessed and are
implemented.
