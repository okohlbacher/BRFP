# ims-compact + consolidate-ms2 — format & decoder handoff

Audience: implementers of a **decoder** (e.g. mzPeakViewer in TypeScript) for the
opt-in `--ims-compact` timsTOF (TDF) representation produced by BRFP.

> Status: **PoC**. The ims-compact output is a *signal-only* Parquet facet plus a
> tiny JSON calibration sidecar — it is **not** a standard mzPeak archive. It
> carries peaks only; per-spectrum metadata (MS level, retention time, precursor,
> polarity) is **not** stored here (see [Limitations](#limitations)).

## 1. Where the encoder lives

All encoding is in one file:

```
/Users/kohlbach/Claude/mzPeak/BRFP/src/mzpeak_writer.rs
```

- `read_ims_calibration` — line **557** — reads the global TOF→m/z model from the
  TDF SQLite (`analysis.tdf` → `GlobalMetadata`).
- `write_tdf_to_ims_compact` — line **588** — the writer (schema, encodings,
  per-spectrum ordering, optional MS2 consolidation, sidecar).
- `write_ims_batch` — line **709** — flushes a RecordBatch.
- The calibration is the **same model timsrust uses** —
  `~/.cargo/registry/src/index.crates.io-*/timsrust-0.4.1/src/domain_converters/tof_to_mz.rs`
  (`Tof2MzConverter::convert` = `(intercept + slope·tof)²`).

CLI: `brfp convert <dir.d> -o out.parquet -f mzpeak --ims-compact [--consolidate-ms2]`.

## 2. What gets written

Two files:

| file | content |
|---|---|
| `out.parquet` (the `-o` path verbatim) | the peaks, as a flat Parquet table |
| `out.parquet.ims.json` | calibration sidecar (see §3) |

### Parquet schema (one row = one peak)

| column | Arrow type | Parquet encoding | meaning |
|---|---|---|---|
| `spectrum_index` | `UInt32` | `DELTA_BINARY_PACKED`, no dict | which spectrum this peak belongs to (0-based, reader order) |
| `tof` | `UInt32` | `DELTA_BINARY_PACKED`, no dict | **time-of-flight bin index** (0…`DigitizerNumSamples`); m/z is derived (§4) |
| `intensity` | `UInt32` | RLE-dictionary | raw integer detector counts |
| `mobility` | `Float64` | RLE-dictionary | mean inverse reduced ion mobility (1/K0, unit `MS:1002814`); see §5 |

Page compression: **zstd level 9** on every column.
(`write_tdf_to_ims_compact`, lines 602–622.)

### Row ordering (important for the decoder — it is *not* m/z-sorted)

- Rows are grouped by `spectrum_index` in ascending order (spectra are emitted in
  reader order `0..N`).
- **Within each spectrum, rows are sorted mobility-major**: primary key
  `mobility` ascending, secondary key `tof` ascending
  (`write_tdf_to_ims_compact` line 662). This is what makes the `mobility` column
  RLE-compress to near-nothing (the "implicit mobility" trick).
- A decoder that wants the conventional **m/z-ascending** peak list must re-sort
  each spectrum's peaks by `tof` (equivalently by reconstructed m/z) itself.

## 3. The calibration sidecar (`*.ims.json`)

```json
{"codec":"ims-compact","mz_from_tof":"(a+b*tof)^2","a":9.9996966453988,"b":0.0000778611663727645}
```

`a` and `b` are the two parameters of the global sqrt model. They are computed
(encoder lines 573–577) from three `GlobalMetadata` values in `analysis.tdf`:

```
a = sqrt(MzAcqRangeLower)
b = (sqrt(MzAcqRangeUpper) - a) / DigitizerNumSamples
```

There is exactly **one** `(a, b)` per run (no per-frame recalibration), so the
model is constant for every peak in the file.

## 4. Decoding m/z (lossless)

For each row:

```
mz = (a + b * tof)^2          // f64; bit-exact inverse of the encoder
```

This is exact: the encoder obtained `tof` by inverting this same function
(`tof = round((sqrt(mz) - a) / b)`), and round-trips were verified bit-exact
(0.00000 ppb error) at full scale. `intensity` is the value as-is (integer
counts). `mobility` is the value as-is (1/K0), **except** under MS2 consolidation
(§5).

## 5. `--consolidate-ms2` (collapsing the MS2 ion-mobility dimension)

When `--consolidate-ms2` is set, **MS2 spectra only** are flattened
(`write_tdf_to_ims_compact` lines 650–660):

- peaks are grouped by exact `tof` (= exact m/z), their `intensity` **summed**,
- and **`mobility` is set to `0.0`** (the fragment co-migrates with the precursor;
  per-peak mobility is dropped).

So in a `--consolidate-ms2` file:

- `mobility == 0.0` ⇒ a **consolidated MS2** peak (mobility was dropped).
- `mobility != 0.0` (real 1/K0, ~0.6–1.6) ⇒ an **MS1** peak (untouched).

This `mobility == 0.0` sentinel is the **only** in-file signal distinguishing MS1
from MS2 — and it exists *only* when consolidation was applied. Without
`--consolidate-ms2`, MS1 and MS2 peaks both carry real mobility and are **not**
distinguishable from the Parquet alone (no MS-level column). Total ion current is
preserved exactly (intensities summed); m/z stays grid-exact; MS1 is byte-for-byte
unchanged.

## 6. Decoder recipe (TypeScript / mzPeakViewer)

```ts
// 1. read the sidecar
const { a, b } = JSON.parse(await readText("out.parquet.ims.json"));

// 2. read the Parquet (e.g. hyparquet, parquet-wasm, or apache-arrow + parquet)
//    columns: spectrum_index:Uint32, tof:Uint32, intensity:Uint32, mobility:Float64
const { spectrum_index, tof, intensity, mobility } = await readParquet("out.parquet");

// 3. per-peak decode
function mzOf(tofBin: number): number { const r = a + b * tofBin; return r * r; }

// 4. group into spectra (rows are already grouped by spectrum_index, ascending)
type Peak = { mz: number; intensity: number; mobility: number | null };
const spectra = new Map<number, Peak[]>();
for (let i = 0; i < tof.length; i++) {
  const si = spectrum_index[i];
  const mob = mobility[i];
  (spectra.get(si) ?? spectra.set(si, []).get(si)!).push({
    mz: mzOf(tof[i]),
    intensity: intensity[i],
    mobility: mob === 0 ? null : mob,      // 0 = consolidated-MS2 (mobility dropped)
  });
}

// 5. for a conventional view, sort each spectrum's peaks by mz (= by tof)
for (const peaks of spectra.values()) peaks.sort((x, y) => x.mz - y.mz);
```

Notes for the TS side:
- `tof` and `spectrum_index` use Parquet `DELTA_BINARY_PACKED` + zstd; `intensity`
  and `mobility` use RLE-dictionary + zstd. Any compliant Parquet reader handles
  these; you do **not** need a custom codec — only the `(a+b·tof)²` reconstruction.
- Use `Float64` for the m/z reconstruction (matches the encoder); `Float32` would
  lose the bit-exactness.

## Limitations (read before integrating)

1. **Signal only.** No MS level, retention time, scan/frame id, precursor m/z,
   isolation window, polarity, or instrument metadata is in this file. To render
   real spectra a viewer must obtain that separately — either run a standard
   `brfp convert … -f mzpeak` (whose `spectra_metadata.parquet` is keyed by the
   same `spectrum_index`) and join, or extend the format to bundle metadata.
2. **MS1/MS2** are only distinguishable in-file via the `mobility==0` sentinel,
   and only when `--consolidate-ms2` was used. Otherwise pair with the metadata
   facet (`MS_1000511_ms_level`).
3. **Not a conformant mzPeak archive** (single Parquet + sidecar, no
   `mzpeak_index.json`, no registered transform CURIE). Productionizing would wrap
   the `tof` column as a chunked-layout array with a registered TOF→m/z transform
   so standard readers reconstruct m/z without bespoke code.
4. **`mobility == 0.0` as a sentinel** assumes no real 1/K0 is exactly 0 (true for
   timsTOF). If a future instrument violates that, switch to a nullable column.

## Reference numbers (SBA415, full run)

`a = 9.9996966453988`, `b = 0.0000778611663727645`, `DigitizerNumSamples = 401116`.
A row with `tof = 200000` decodes to `mz = (9.9996966 + 7.78612e-5·200000)² ≈
653.06`. Full-file sizes: standard mzPeak 4.64 GB · `--ims-compact` 3.05 GB ·
`--ims-compact --consolidate-ms2` 2.65 GB · raw `.d` 2.21 GB.
