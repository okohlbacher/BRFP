# Decision 0001: Initial Reader And Writer Strategy

Date: 2026-06-18

## Status

Accepted for the first implementation spike.

## Context

BRFP must read Bruker `.d` directories and write mzPeak by default. There are two
usable Rust ecosystems:

- `mzdata`, which already exposes Bruker TDF data through a mass-spectrometry data
  model that the mzPeak Rust writer consumes.
- `timsrust`, which has broader Bruker-specific coverage including TDF, TSF, and
  optional Bruker SDK calibration support.

The mzPeak Rust implementation is active and directly uses `mzdata` types, but
the prototype package is not currently published to crates.io.

## Decision

Use `mzdata` as the first TDF reader path and HUPO-PSI/mzPeak as the first writer
path. Depend on mzPeak by git or local path during the spike. Keep a BRFP reader
trait boundary so direct `timsrust` TSF or SDK-backed paths can be added later.

## Consequences

Benefits:

- Fastest path to a working TDF-to-mzPeak converter.
- Reuses existing Bruker TDF indexing, metadata construction, and mzPeak writer
  code.
- Reduces risk of inventing a subtly incompatible data model.

Costs:

- mzPeak writer API may change while the spec is still a draft.
- TSF support probably needs a second backend.
- BRFP must pin upstream revisions carefully for reproducible releases.

Follow-up:

- Revisit once mzPeak publishes a stable Rust crate.
- Revisit if `mzdata` and current `timsrust` diverge in calibration or metadata
  behavior that materially affects output correctness.
