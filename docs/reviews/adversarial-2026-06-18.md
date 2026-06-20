# Adversarial Review Findings

Date: 2026-06-18

Reviewers:

- Kimi CLI
- Vibe CLI

Scope:

- `docs/project-plan.md`
- `docs/work-breakdown.md`
- `docs/docker-sdk.md`

## Accepted High-Priority Findings

1. Missing TDF fixture ownership blocks TDF acceptance.
2. Conversion must fail, not warn, when the binary payload is missing.
3. Bruker SDK redistribution and SDK-linked binary policy must be explicit
   before release packaging.
4. `convert` and `validate` need stable exit/error contracts.
5. mzPeak and mzdata dependency pins need a clear policy.
6. Bruker SQLite schema/version compatibility must be handled explicitly.
7. `SchemaType` must be verified against detected format.
8. `--warnings-are-errors` must be implemented.
9. SDK-backed tests need an automated Docker path and should not be allowed to
   bit-rot.
10. TSF conversion should move earlier because the local real fixtures are TSF.
11. Deterministic output policy is required for conformance reports.
12. Profile vs centroid placement needs explicit per-spectrum behavior.
13. Dependency version conflicts among mzPeak, mzdata, timsrust, Arrow, and
   Parquet need an explicit spike.
14. Negative/malformed fixture tests need concrete tasks.
15. Progress, cancellation, and partial-output cleanup need design before large
   conversion claims.
16. SDK integrity/version checks need a task.

## Rejected Or Already Addressed Findings

- "No project license": rejected. `LICENSE` exists and the manifest declares
  MIT.
- "No MSRV definition": rejected. `Cargo.toml` currently sets
  `rust-version = "1.85"`.
- "Input detection does not handle both TDF and TSF": rejected for current code.
  `input.rs` errors when both are present.
- "No platform path handling at all": partially rejected. The code uses `Path`
  and `PathBuf`; Windows-specific release testing is still required.

## Immediate Actions Taken

- Add schema-type mismatch warnings.
- Add inspect-level `--warnings-are-errors`.
- Add conversion preflight that fails when binary payload is missing.
- Add SDK discovery module.
- Add Docker SDK test helper.
- Add private fixture integration tests gated by `BRFP_TEST_PRIVATE_DATA`.
- Pin mzPeak from HUPO-PSI/mzPeak at
  `b63302b927704c347157ed30d466d78e22c22848`.
- Resolve the active SQLite dependency conflict by aligning direct SQLite usage
  with mzPeak/mzdata on `rusqlite 0.31`.
- Avoid the conflicting `timsrust-tsf` stack for now by implementing a narrow
  local TSF line-spectrum reader over `analysis.tsf` and `analysis.tsf_bin`.
- Add a TSF-to-mzPeak smoke conversion path with mzPeak reader round-trip tests.
- Update Docker verification to install `cmake`, required by the current
  zlib-ng build path.

## Backlog Updates

The work breakdown now owns:

- TDF fixture acquisition.
- SDK redistribution policy, now documented as "do not redistribute; users
  download proprietary SDK/runtime components from Bruker".
- Dependency pin/conflict spike.
- Deterministic output policy.
- Malformed fixture creation.
- SDK integrity/version checks.
