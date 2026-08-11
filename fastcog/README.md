# fastcog

Fast **GeoTIFF / JP2 → COG** converter. Part of the [gdal-alternate](../) monorepo.

Single static binary. No Python, no `libgdal` at runtime.

On Sentinel-2 L2A (10980×10980 RGB, DEFLATE 6, 512px blocks, nearest overviews), **~1.8× faster than `gdal_translate`** with pixel-identical output.

## Build

```bash
cd fastcog
cargo build --release
```

Binary: `target/release/fastcog`

```bash
cargo install --path .
```

## Usage

```bash
fastcog [OPTIONS] <INPUT> <OUTPUT>
```

### Sentinel-2 JP2 → COG

```bash
fastcog scene.jp2 output_cog.tif -b 512 -c deflate -j 8 -r nearest
```

GDAL equivalent:

```bash
GDAL_NUM_THREADS=8 gdal_translate scene.jp2 output_cog.tif \
  -of COG -co BLOCKSIZE=512 -co COMPRESS=DEFLATE \
  -co ZLEVEL=6 -co OVERVIEW_RESAMPLING=NEAREST
```

### GeoTIFF → COG

```bash
fastcog input.tif output_cog.tif
```

## CLI options

| Flag | Default | Description |
|------|---------|-------------|
| `-b, --blocksize` | `512` | Tile size (multiple of 16) |
| `-c, --compress` | `deflate` | `none`, `lzw`, `deflate`, `zstd`, `jpeg` |
| `--deflate-level` | `6` | DEFLATE level 0–9 |
| `-r, --resampling` | `average` | `nearest`, `average` |
| `-o, --overviews` | auto | e.g. `-o 2 4 8` |
| `--no-overviews` | | Skip pyramids |
| `--mmap` | | Memory-map GeoTIFF input |
| `-j, --jobs` | all CPUs | Worker threads |
| `-q, --quiet` | | Hide progress bar |

## Supported inputs

**GeoTIFF** — any size/dtype (8–64 bit int/float), multi-band, chunky/planar, full georef + nodata preservation, auto BigTIFF.

**JP2** — 3-band RGB 8-bit (e.g. Sentinel-2), GML georeferencing.

## Performance

| Tool | Time (S2 L2A) |
|------|---------------|
| **fastcog** | **~3.3 s** |
| gdal_translate | ~5.7 s |

Settings: `-b 512 -c deflate -j 8 -r nearest`. Pixel-identical output.

## Architecture

```
Input → format detect → parallel tile decode → COG write → output
```

- GeoTIFF: windowed reads → `CogTileWriter`
- JP2: streaming 1024² decode → planar pack → pipelined overviews

Uses a patched [`geotiff-writer`](vendor/geotiff-writer) for parallel DEFLATE.

## License

MIT — see [LICENSE](../LICENSE) in the repository root.
