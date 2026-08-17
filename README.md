# gdal-alternate

Pure-Rust tools that replace common GDAL workflows — faster, fewer dependencies, single binaries.

This repository is a **Cargo workspace** monorepo. Shared raster logic lives in `crates/gdal-alt-core`.

## Tools

| Directory | Binary | GDAL equivalent | Description |
|-----------|--------|-----------------|-------------|
| [`fastcog/`](fastcog/) | `fastcog` | `gdal_translate -of COG` | GeoTIFF or JP2 → COG (~1.8× faster than GDAL on Sentinel-2 JP2) |
| [`fastinfo/`](fastinfo/) | `fastinfo` | `gdalinfo` | Metadata only — size, CRS, bbox, nodata, compression, overviews. No pixel decode |
| [`fastvalidate/`](fastvalidate/) | `fastvalidate` | `rio cogeo validate` | COG layout checks — tiled, overviews, compression, georef |
| [`fastcrop/`](fastcrop/) | `fastcrop` | `gdal_translate -srcwin / -projwin` | Extract a pixel or geographic window to COG |
| [`fastband/`](fastband/) | `fastband` | `gdal_translate -b` | Subset or reorder bands into a new COG |
| [`fasttranslate/`](fasttranslate/) | `fasttranslate` | `gdal_translate` + batch | **All tools in one binary** — subcommands below |

### `fasttranslate` subcommands

| Subcommand | Equivalent | Description |
|------------|------------|-------------|
| `cog` (alias `translate`) | `fastcog` | GeoTIFF or JP2 → COG |
| `info` | `fastinfo` | Metadata only |
| `validate` | `fastvalidate` | COG layout checks |
| `crop` | `fastcrop` | Pixel or geographic window → COG |
| `band` | `fastband` | Band subset/reorder → COG |
| `batch` | — | Convert every raster in a directory to COG (parallel) |

`fastcrop` and `fastband` accept the same compression and mask flags as `fastcog` (`--jpeg-quality`, `--lerc-max-z-error`, `--lerc-additional-compression`, `--no-mask-from-alpha`, `--black-rgb-transparent`).

## Requirements

- [Rust](https://rustup.rs) 1.77+
- Linux or macOS (Windows may work; primary development is on Linux)

## Build

```bash
git clone https://github.com/YOUR_USERNAME/gdal-alternate.git
cd gdal-alternate
cargo build --release
```

Binaries land in `target/release/`:

```bash
./target/release/fastinfo scene.tif
./target/release/fastvalidate output_cog.tif
./target/release/fastcrop input.tif crop.tif --srcwin 0 0 1024 1024
./target/release/fastband input.tif rgb.tif -b 1 -b 2 -b 3
./target/release/fastcog scene.jp2 output_cog.tif -b 512 -c deflate -j 8
./target/release/fasttranslate cog scene.jp2 output_cog.tif -c deflate -j 8
./target/release/fasttranslate batch ./scenes ./cogs --skip-existing -j 4
```

Install one tool globally:

```bash
cargo install --path fastinfo
```

## Examples

```bash
# Metadata (no pixel I/O)
fastinfo scene.tif

# Validate COG after conversion
fastvalidate output_cog.tif

# Crop by pixel window (col row width height)
fastcrop big.tif window.tif --srcwin 1000 2000 512 512 -c deflate

# Crop by geographic bounds (ulx uly lrx lry)
fastcrop georef.tif subset.tif --projwin 500000 6000000 510000 5990000

# RGB from 4-band (drop alpha, keep alpha as mask IFD by default)
fastband rgba.tif rgb.tif -b 1 -b 2 -b 3

# Crop with LERC output and no alpha-derived mask
fastcrop big.tif window.tif --srcwin 1000 2000 512 512 -c lerc --lerc-max-z-error 0.01 --no-mask-from-alpha

# Batch-convert a folder (skip files already in output dir)
fasttranslate batch ./inputs ./outputs --skip-existing -j 4 -c deflate

# Full COG conversion
fastcog scene.jp2 output_cog.tif -b 512 -c deflate -j 8 -r nearest
```

## Repository layout

```
gdal-alternate/
├── Cargo.toml              ← workspace root
├── crates/gdal-alt-core/   ← shared I/O, COG builder, info, validate
├── fastcog/
├── fastinfo/
├── fastvalidate/
├── fastcrop/
├── fastband/
└── fasttranslate/
```

## Development

```bash
cargo fmt --all
cargo clippy --release --workspace -- -D warnings
cargo test
```

## Why not GDAL?

GDAL is the industry standard, but it pulls in a large C stack and is often slower for focused tasks like COG creation or metadata reads. These tools target **one job done well** with static Rust binaries and no runtime GDAL dependency.

## License

MIT — see [LICENSE](LICENSE).
