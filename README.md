# gdal-alternate

Pure-Rust tools that replace common GDAL workflows — faster, fewer dependencies, single binaries.

This repository is a **monorepo**. Each tool lives in its own directory and can be built independently.

## Projects

| Directory | Tool | Description |
|-----------|------|-------------|
| [`fastcog/`](fastcog/) | **fastcog** | Convert GeoTIFF or JP2 → Cloud Optimized GeoTIFF (COG). ~1.8× faster than `gdal_translate` on Sentinel-2 JP2. |

More tools will be added here as the project grows.

## Requirements

- [Rust](https://rustup.rs) 1.77+
- Linux or macOS (Windows may work; primary development is on Linux)

## Quick start

```bash
git clone https://github.com/YOUR_USERNAME/gdal-alternate.git
cd gdal-alternate/fastcog
cargo build --release
./target/release/fastcog --help
```

Install a tool globally:

```bash
cargo install --path fastcog
```

## Repository layout

```
gdal-alternate/
├── README.md          ← you are here
├── LICENSE
├── .gitignore
└── fastcog/           ← GeoTIFF/JP2 → COG converter
    ├── Cargo.toml
    ├── README.md      ← fastcog-specific docs
    └── src/
```

## Development

Work inside the project directory you are changing:

```bash
cd fastcog
cargo fmt
cargo clippy --release -- -D warnings
cargo test
```

## Why not GDAL?

GDAL is the industry standard, but it pulls in a large C stack and is often slower for focused tasks like COG creation. These tools target **one job done well** with a static Rust binary and no runtime GDAL dependency.

## License

MIT — see [LICENSE](LICENSE).
