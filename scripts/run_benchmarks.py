#!/usr/bin/env python3
"""Benchmark gdal-alternate tools vs GDAL on all rasters in a test-data folder."""

from __future__ import annotations

import os
import re
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "release"
DEFAULT_DATA = Path.home() / "Downloads" / "test_data"
OUT_ROOT = DEFAULT_DATA / "benchmarks"
OUT_ROOT.mkdir(parents=True, exist_ok=True)

GDAL_COG = [
    "gdal_translate",
    "-of",
    "COG",
    "-co",
    "COMPRESS=DEFLATE",
    "-co",
    "BLOCKSIZE=512",
]

RASTER_SUFFIXES = {".tif", ".tiff", ".jp2", ".j2k", ".j2c"}

ENV = os.environ.copy()
ENV["GDAL_NUM_THREADS"] = "ALL_CPUS"
ENV["OMP_NUM_THREADS"] = str(os.cpu_count() or 1)


@dataclass
class BenchResult:
    dataset: str
    tool: str
    task: str
    fast_s: float | None = None
    gdal_s: float | None = None
    skipped: str | None = None
    fast_out: str | None = None
    gdal_out: str | None = None
    validated: str | None = None
    fast_ok: bool = True
    gdal_ok: bool = True

    @property
    def winner(self) -> str:
        if self.skipped:
            return "skip"
        if self.gdal_s is None:
            return "fast"
        if not self.fast_ok:
            return "gdal"
        if not self.gdal_ok:
            return "fast"
        if self.fast_s is None:
            return "gdal"
        return "fast" if self.fast_s <= self.gdal_s else "gdal"

    @property
    def speedup(self) -> str:
        if self.skipped or self.gdal_s is None or self.fast_s is None or self.fast_s == 0:
            return "—"
        if not self.fast_ok:
            return "fast failed"
        if not self.gdal_ok:
            return "gdal failed"
        ratio = self.gdal_s / self.fast_s
        if self.winner == "fast":
            return f"{ratio:.1f}×"
        return f"{1 / ratio:.1f}× slower"


def slugify(path: Path) -> str:
    stem = path.stem.lower()
    stem = re.sub(r"[^a-z0-9]+", "_", stem)
    return stem.strip("_")[:80] or "dataset"


def run_cmd(cmd: list[str], quiet: bool = True, check: bool = True) -> int:
    r = subprocess.run(
        cmd,
        check=False,
        env=ENV,
        stdout=subprocess.DEVNULL if quiet else None,
        stderr=subprocess.DEVNULL if quiet else None,
    )
    if check and r.returncode != 0:
        raise subprocess.CalledProcessError(r.returncode, cmd)
    return r.returncode


def bench(cmd: list[str], runs: int = 3, repeat: int = 1, check: bool = True) -> float:
    times: list[float] = []
    for _ in range(runs):
        if repeat > 1:
            start = time.perf_counter()
            for __ in range(repeat):
                run_cmd(cmd, check=check)
            times.append((time.perf_counter() - start) / repeat)
        else:
            start = time.perf_counter()
            run_cmd(cmd, check=check)
            times.append(time.perf_counter() - start)
    return statistics.mean(times)


def fmt_time(t: float | None) -> str:
    if t is None:
        return "—"
    if t < 0.01:
        return f"{t * 1000:.1f}ms"
    return f"{t:.3f}s"


def validate(path: Path) -> str:
    if not path.exists() or path.stat().st_size == 0:
        return "MISSING"
    code = run_cmd([str(BIN / "fastvalidate"), str(path)], check=False)
    return "PASSED" if code == 0 else "FAILED"


def bench_output(cmd: list[str], out_path: Path, runs: int = 3) -> tuple[float, bool]:
    times: list[float] = []
    ok = False
    for _ in range(runs):
        if out_path.exists():
            out_path.unlink()
        start = time.perf_counter()
        code = run_cmd(cmd, check=False)
        elapsed = time.perf_counter() - start
        times.append(elapsed)
        ok = code == 0 and out_path.exists() and out_path.stat().st_size > 0
    return statistics.mean(times), ok


def pair_bench(
    results: list[BenchResult],
    dataset: str,
    tool: str,
    task: str,
    fast_cmd: list[str],
    gdal_cmd: list[str],
    fast_out: Path,
    gdal_out: Path,
    *,
    runs: int = 3,
) -> None:
    fast_out.parent.mkdir(parents=True, exist_ok=True)
    gdal_out.parent.mkdir(parents=True, exist_ok=True)

    ft, fast_ok = bench_output(fast_cmd, fast_out, runs=runs)
    gt, gdal_ok = bench_output(gdal_cmd, gdal_out, runs=runs)

    v_fast = validate(fast_out) if fast_ok else "FAILED"
    v_gdal = validate(gdal_out) if gdal_ok else "FAILED"

    results.append(
        BenchResult(
            dataset=dataset,
            tool=tool,
            task=task,
            fast_s=ft,
            gdal_s=gt,
            fast_out=str(fast_out.relative_to(OUT_ROOT)) if fast_out.exists() else None,
            gdal_out=str(gdal_out.relative_to(OUT_ROOT)) if gdal_out.exists() else None,
            validated=f"fast:{v_fast}, gdal:{v_gdal}",
            fast_ok=fast_ok,
            gdal_ok=gdal_ok,
        )
    )


def fast_only_bench(
    results: list[BenchResult],
    dataset: str,
    tool: str,
    task: str,
    cmd: list[str],
    *,
    repeat: int = 50,
    check: bool = True,
) -> None:
    ft = bench(cmd, repeat=repeat, check=check)
    results.append(
        BenchResult(dataset=dataset, tool=tool, task=task, fast_s=ft, gdal_s=None)
    )


def skip(results: list[BenchResult], dataset: str, tool: str, task: str, reason: str) -> None:
    results.append(BenchResult(dataset=dataset, tool=tool, task=task, skipped=reason))


@dataclass
class DatasetSpec:
    slug: str
    path: Path
    width: int
    height: int
    bands: int
    is_geotiff: bool
    heavy: bool = False


def probe(path: Path) -> tuple[int, int, int]:
    out = subprocess.check_output([str(BIN / "fastinfo"), str(path)], text=True, env=ENV)
    size = None
    bands = 1
    for line in out.splitlines():
        if line.startswith("Size is "):
            w, h = line.replace("Size is ", "").split(", ")
            size = (int(w), int(h))
        if line.startswith("Band count: "):
            bands = int(line.split(":", 1)[1].split()[0])
    if size is None:
        raise RuntimeError(f"could not probe {path}")
    return size[0], size[1], bands


def crop_windows(w: int, h: int) -> tuple[list[str], list[str]]:
    aw = min(8192, (w // 512) * 512)
    ah = min(8192, (h // 512) * 512)
    aw = max(512, aw)
    ah = max(512, ah)
    aligned = ["0", "0", str(aw), str(ah)]
    misaligned = [
        "100",
        "200",
        str(min(2048, w - 100)),
        str(min(2048, h - 200)),
    ]
    return aligned, misaligned


def discover_datasets(data_dir: Path) -> list[Path]:
    files: list[Path] = []
    for path in sorted(data_dir.iterdir()):
        if not path.is_file():
            continue
        if path.suffix.lower() in RASTER_SUFFIXES:
            files.append(path)
    return files


def run_dataset(spec: DatasetSpec, results: list[BenchResult]) -> Path | None:
    ds_dir = OUT_ROOT / spec.slug
    ds_dir.mkdir(parents=True, exist_ok=True)
    (ds_dir / "SOURCE.txt").write_text(
        f"{spec.path}\n{spec.width}x{spec.height}, {spec.bands} band(s)\n"
    )

    fast_only_bench(
        results,
        spec.slug,
        "fastinfo",
        "metadata",
        [str(BIN / "fastinfo"), str(spec.path)],
    )
    results[-1].gdal_s = bench(["gdalinfo", str(spec.path)], repeat=50)

    fast_only_bench(
        results,
        spec.slug,
        "fastvalidate",
        "input layout check",
        [str(BIN / "fastvalidate"), str(spec.path)],
        check=False,
    )
    code = run_cmd([str(BIN / "fastvalidate"), str(spec.path)], check=False)
    results[-1].validated = "PASSED" if code == 0 else "FAILED (not COG)"

    cog_fast = ds_dir / "fastcog" / "fast" / "to_cog.tif"
    cog_gdal = ds_dir / "fastcog" / "gdal" / "to_cog.tif"
    encode_runs = 1 if spec.heavy else 3

    if spec.is_geotiff or spec.path.suffix.lower() in {".jp2", ".j2k", ".j2c"}:
        pair_bench(
            results,
            spec.slug,
            "fastcog",
            "→ COG",
            [str(BIN / "fastcog"), str(spec.path), str(cog_fast), "-q"],
            [*GDAL_COG, str(spec.path), str(cog_gdal)],
            cog_fast,
            cog_gdal,
            runs=encode_runs,
        )
    else:
        skip(results, spec.slug, "fastcog", "→ COG", "unsupported input")

    cog_source: Path | None = None
    if cog_gdal.exists() and validate(cog_gdal) == "PASSED":
        cog_source = cog_gdal
    elif cog_fast.exists() and validate(cog_fast) == "PASSED":
        cog_source = cog_fast

    if cog_source is None:
        skip(results, spec.slug, "fastcog", "COG → COG (remux)", "no valid COG produced")
        skip(results, spec.slug, "fastcrop", "tile-aligned crop", "no valid COG source")
        skip(results, spec.slug, "fastcrop", "misaligned crop", "no valid COG source")
        skip(results, spec.slug, "fastband", "band subset", "no valid COG source")
        return None

    copy_fast = ds_dir / "cog_copy" / "fast" / "copy.tif"
    copy_gdal = ds_dir / "cog_copy" / "gdal" / "copy.tif"
    pair_bench(
        results,
        spec.slug,
        "fastcog",
        "COG → COG (remux)",
        [str(BIN / "fastcog"), str(cog_source), str(copy_fast), "-q"],
        [*GDAL_COG, str(cog_source), str(copy_gdal)],
        copy_fast,
        copy_gdal,
    )

    aligned, misaligned = crop_windows(spec.width, spec.height)
    for label, win, sub in (
        ("tile-aligned crop", aligned, "tile_aligned"),
        ("misaligned crop", misaligned, "misaligned"),
    ):
        crop_fast = ds_dir / "fastcrop" / sub / "fast" / "crop.tif"
        crop_gdal = ds_dir / "fastcrop" / sub / "gdal" / "crop.tif"
        pair_bench(
            results,
            spec.slug,
            "fastcrop",
            f"{label} ({' '.join(win)})",
            [
                str(BIN / "fastcrop"),
                str(cog_source),
                str(crop_fast),
                "--srcwin",
                *win,
                "-q",
            ],
            [*GDAL_COG, "-srcwin", *win, str(cog_source), str(crop_gdal)],
            crop_fast,
            crop_gdal,
        )

    if spec.bands >= 3:
        for label, bands, sub in (
            ("same bands 1,2,3", [1, 2, 3], "identity"),
            ("reorder 3,2,1", [3, 2, 1], "reorder"),
        ):
            band_fast = ds_dir / "fastband" / sub / "fast" / "bands.tif"
            band_gdal = ds_dir / "fastband" / sub / "gdal" / "bands.tif"
            fast_cmd = [str(BIN / "fastband"), str(cog_source), str(band_fast), "-q"]
            gdal_cmd = [*GDAL_COG, str(cog_source), str(band_gdal)]
            for b in bands:
                fast_cmd.extend(["-b", str(b)])
                gdal_cmd.extend(["-b", str(b)])
            pair_bench(
                results,
                spec.slug,
                "fastband",
                label,
                fast_cmd,
                gdal_cmd,
                band_fast,
                band_gdal,
            )
    else:
        skip(results, spec.slug, "fastband", "band subset", f"single-band ({spec.bands} band)")

    return cog_source


def write_report(results: list[BenchResult], data_dir: Path) -> Path:
    gdal_ver = (
        subprocess.check_output(["gdal_translate", "--version"], text=True, env=ENV)
        .strip()
        .split("\n")[0]
    )
    lines = [
        "# gdal-alternate Benchmark Results",
        "",
        f"- **Test data:** `{data_dir}`",
        f"- **GDAL:** {gdal_ver}",
        f"- **CPU threads:** {os.cpu_count()}",
        f"- **Build:** release (`{ROOT}`)",
        f"- **Method:** 3-run wall-clock mean (1 run for heavy encodes); metadata averaged over 50 runs",
        f"- **Outputs:** `{OUT_ROOT}`",
        "",
        "## Summary by dataset",
        "",
    ]

    datasets: list[str] = []
    for r in results:
        if r.dataset not in datasets:
            datasets.append(r.dataset)

    total_cmp = 0
    total_wins = 0

    for ds in datasets:
        rows = [r for r in results if r.dataset == ds]
        cmp_rows = [r for r in rows if not r.skipped and r.gdal_s is not None and r.fast_ok]
        wins = sum(1 for r in cmp_rows if r.winner == "fast")
        total_cmp += len(cmp_rows)
        total_wins += wins
        lines.append(f"### `{ds}` — **{wins}/{len(cmp_rows)}** vs GDAL")
        lines.append("")
        lines.append("| Tool | Task | Fast | GDAL | Winner | Speedup | Validation |")
        lines.append("|------|------|------|------|--------|---------|------------|")
        for r in rows:
            if r.skipped:
                lines.append(f"| {r.tool} | {r.task} | — | — | skip | — | {r.skipped} |")
                continue
            winner = f"**{r.winner}**" if r.winner == "fast" else r.winner
            speed = r.speedup
            val = r.validated or "—"
            lines.append(
                f"| {r.tool} | {r.task} | {fmt_time(r.fast_s)} | {fmt_time(r.gdal_s)} "
                f"| {winner} | {speed} | {val} |"
            )
        lines.append("")

    lines.extend(
        [
            f"## Overall: **{total_wins}/{total_cmp}** GDAL-comparable tests won by fast tools",
            "",
            "## Output layout",
            "",
            "```",
            "benchmarks/",
            "  RESULTS.md",
            "  <dataset_slug>/",
            "    SOURCE.txt",
            "    fastcog/fast/to_cog.tif",
            "    fastcog/gdal/to_cog.tif",
            "    cog_copy/fast|gdal/copy.tif",
            "    fastcrop/tile_aligned|misaligned/fast|gdal/crop.tif",
            "    fastband/identity|reorder/fast|gdal/bands.tif",
            "```",
            "",
        ]
    )

    report = OUT_ROOT / "RESULTS.md"
    report.write_text("\n".join(lines))
    return report


def main() -> int:
    data_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_DATA
    if not data_dir.is_dir():
        print(f"error: data directory not found: {data_dir}", file=sys.stderr)
        return 1

    for binary in ("fastcog", "fastcrop", "fastband", "fastinfo", "fastvalidate"):
        if not (BIN / binary).exists():
            print(f"error: build release binaries first: cargo build --release", file=sys.stderr)
            return 1

    OUT_ROOT.mkdir(parents=True, exist_ok=True)
    results: list[BenchResult] = []

    files = discover_datasets(data_dir)
    if not files:
        print(f"error: no raster files found in {data_dir}", file=sys.stderr)
        return 1

    print(f"Benchmarking {len(files)} datasets from {data_dir}")
    print(f"Outputs -> {OUT_ROOT}\n")

    for path in files:
        w, h, bands = probe(path)
        pixels = w * h
        heavy = pixels > 150_000_000 or path.stat().st_size > 500_000_000
        is_geotiff = path.suffix.lower() in {".tif", ".tiff"}
        spec = DatasetSpec(
            slug=slugify(path),
            path=path,
            width=w,
            height=h,
            bands=bands,
            is_geotiff=is_geotiff,
            heavy=heavy,
        )
        print(f"[{spec.slug}] {w}x{h}, {bands} bands, heavy={heavy}")
        run_dataset(spec, results)
        print()

    report = write_report(results, data_dir)
    print(f"Wrote {report}")
    print(report.read_text())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
