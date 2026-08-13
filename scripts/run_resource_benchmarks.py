#!/usr/bin/env python3
"""Benchmark gdal-alternate vs GDAL: time, peak memory, CPU, and disk I/O."""

from __future__ import annotations

import os
import re
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

import psutil

ROOT = Path(__file__).resolve().parents[1]
BIN = ROOT / "target" / "release"
DEFAULT_DATA = Path.home() / "Downloads" / "test_data"
OUT_ROOT = DEFAULT_DATA / "benchmarks" / "resource"
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


def fmt_bytes(n: int) -> str:
    if n < 1024:
        return f"{n} B"
    if n < 1024**2:
        return f"{n / 1024:.1f} KB"
    if n < 1024**3:
        return f"{n / 1024**2:.1f} MB"
    return f"{n / 1024**3:.2f} GB"


def fmt_time(t: float) -> str:
    if t < 0.01:
        return f"{t * 1000:.1f}ms"
    return f"{t:.3f}s"


@dataclass
class ResourceMetrics:
    wall_s: float = 0.0
    user_cpu_s: float = 0.0
    sys_cpu_s: float = 0.0
    max_rss_mb: float = 0.0
    read_bytes: int = 0
    write_bytes: int = 0
    ok: bool = False

    @property
    def total_cpu_s(self) -> float:
        return self.user_cpu_s + self.sys_cpu_s

    @property
    def cpu_pct(self) -> float:
        if self.wall_s <= 0:
            return 0.0
        return 100.0 * self.total_cpu_s / self.wall_s


@dataclass
class PairResult:
    dataset: str
    tool: str
    task: str
    fast: ResourceMetrics = field(default_factory=ResourceMetrics)
    gdal: ResourceMetrics = field(default_factory=ResourceMetrics)
    validated: str = "—"

    @property
    def time_winner(self) -> str:
        if not self.fast.ok:
            return "gdal"
        if not self.gdal.ok:
            return "fast"
        return "fast" if self.fast.wall_s <= self.gdal.wall_s else "gdal"

    @property
    def mem_winner(self) -> str:
        if not (self.fast.ok and self.gdal.ok):
            return "—"
        return "fast" if self.fast.max_rss_mb <= self.gdal.max_rss_mb else "gdal"

    @property
    def io_winner(self) -> str:
        if not (self.fast.ok and self.gdal.ok):
            return "—"
        f = self.fast.read_bytes + self.fast.write_bytes
        g = self.gdal.read_bytes + self.gdal.write_bytes
        return "fast" if f <= g else "gdal"


def _tree_procs(root: psutil.Process) -> list[psutil.Process]:
    try:
        return [root, *root.children(recursive=True)]
    except psutil.Error:
        return [root]


def run_measured(cmd: list[str], *, out: Path | None = None) -> ResourceMetrics:
    if out is not None and out.exists():
        out.unlink()

    metrics = ResourceMetrics()
    stop = threading.Event()
    peak_rss = 0.0
    io_read = 0
    io_write = 0
    cpu_user = 0.0
    cpu_sys = 0.0

    def monitor(pid: int) -> None:
        nonlocal peak_rss, io_read, io_write, cpu_user, cpu_sys
        try:
            root = psutil.Process(pid)
        except psutil.Error:
            return
        while not stop.is_set():
            try:
                if not root.is_running() and not root.children(recursive=True):
                    break
            except psutil.Error:
                break
            for p in _tree_procs(root):
                try:
                    peak_rss = max(peak_rss, p.memory_info().rss / (1024 * 1024))
                    ct = p.cpu_times()
                    cpu_user = max(cpu_user, ct.user)
                    cpu_sys = max(cpu_sys, ct.system)
                    io = p.io_counters()
                    io_read = max(io_read, io.read_bytes)
                    io_write = max(io_write, io.write_bytes)
                except (psutil.Error, AttributeError):
                    pass
            time.sleep(0.02)

    t0 = time.perf_counter()
    proc = subprocess.Popen(cmd, env=ENV, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    mon = threading.Thread(target=monitor, args=(proc.pid,), daemon=True)
    mon.start()
    code = proc.wait()
    stop.set()
    mon.join(timeout=2.0)
    metrics.wall_s = time.perf_counter() - t0
    metrics.user_cpu_s = cpu_user
    metrics.sys_cpu_s = cpu_sys
    metrics.max_rss_mb = peak_rss
    metrics.read_bytes = io_read
    metrics.write_bytes = io_write
    metrics.ok = code == 0
    if out is not None:
        metrics.ok = metrics.ok and out.exists() and out.stat().st_size > 0
    return metrics


def bench_cmd(cmd: list[str], out: Path | None = None, runs: int = 1) -> ResourceMetrics:
    samples = [run_measured(cmd, out=out) for _ in range(runs)]
    if not all(s.ok for s in samples):
        return next(s for s in samples if not s.ok)

    agg = ResourceMetrics()
    agg.wall_s = statistics.mean(s.wall_s for s in samples)
    agg.user_cpu_s = statistics.mean(s.user_cpu_s for s in samples)
    agg.sys_cpu_s = statistics.mean(s.sys_cpu_s for s in samples)
    agg.max_rss_mb = max(s.max_rss_mb for s in samples)
    agg.read_bytes = int(statistics.mean(s.read_bytes for s in samples))
    agg.write_bytes = int(statistics.mean(s.write_bytes for s in samples))
    agg.ok = True
    return agg


def validate(path: Path) -> str:
    if not path.exists() or path.stat().st_size == 0:
        return "MISSING"
    code = subprocess.run(
        [str(BIN / "fastvalidate"), str(path)],
        env=ENV,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ).returncode
    return "PASSED" if code == 0 else "FAILED"


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


def slugify(path: Path) -> str:
    stem = re.sub(r"[^a-z0-9]+", "_", path.stem.lower()).strip("_")
    return stem[:60] or "dataset"


def discover(data_dir: Path) -> list[Path]:
    return sorted(
        p for p in data_dir.iterdir() if p.is_file() and p.suffix.lower() in RASTER_SUFFIXES
    )


def pair_bench(
    results: list[PairResult],
    dataset: str,
    tool: str,
    task: str,
    fast_cmd: list[str],
    gdal_cmd: list[str],
    fast_out: Path | None,
    gdal_out: Path | None,
    *,
    runs: int = 1,
) -> None:
    if fast_out is not None:
        fast_out.parent.mkdir(parents=True, exist_ok=True)
    if gdal_out is not None:
        gdal_out.parent.mkdir(parents=True, exist_ok=True)

    fast_m = bench_cmd(fast_cmd, fast_out, runs=runs)
    gdal_m = bench_cmd(gdal_cmd, gdal_out, runs=runs)

    if fast_out and fast_m.ok:
        v_fast = validate(fast_out)
    else:
        v_fast = "FAILED" if fast_out else "n/a"
    if gdal_out and gdal_m.ok:
        v_gdal = validate(gdal_out)
    else:
        v_gdal = "FAILED" if gdal_out else "n/a"

    results.append(
        PairResult(
            dataset=dataset,
            tool=tool,
            task=task,
            fast=fast_m,
            gdal=gdal_m,
            validated=f"fast:{v_fast}, gdal:{v_gdal}",
        )
    )


def write_report(results: list[PairResult], data_dir: Path) -> Path:
    gdal_ver = (
        subprocess.check_output(["gdaltranslate" if False else "gdal_translate", "--version"], text=True, env=ENV)
        .strip()
        .split("\n")[0]
    )
    lines = [
        "# Resource Benchmark: gdal-alternate vs GDAL",
        "",
        f"- **Data:** `{data_dir}`",
        f"- **GDAL:** {gdal_ver}",
        f"- **CPUs:** {os.cpu_count()}",
        f"- **Build:** release (`{ROOT}`)",
        "",
        "Metrics per run (averaged when multiple runs): **wall time**, **peak RSS**, "
        "**CPU user+sys**, **disk read/write** (process tree via `/proc`).",
        "",
    ]

    datasets: list[str] = []
    for r in results:
        if r.dataset not in datasets:
            datasets.append(r.dataset)

    time_wins = mem_wins = io_wins = cmp_n = 0

    for ds in datasets:
        rows = [r for r in results if r.dataset == ds]
        lines.append(f"## {ds}")
        lines.append("")
        lines.append(
            "| Tool | Task | Impl | Time | Peak RAM | CPU | CPU% | Read | Write | Valid |"
        )
        lines.append("|------|------|------|------|----------|-----|------|------|-------|-------|")
        for r in rows:
            for impl, m in (("fast", r.fast), ("gdal", r.gdal)):
                ok = "ok" if m.ok else "FAIL"
                lines.append(
                    f"| {r.tool} | {r.task} | **{impl}** | {fmt_time(m.wall_s)} "
                    f"| {m.max_rss_mb:.0f} MB | {fmt_time(m.total_cpu_s)} | {m.cpu_pct:.0f}% "
                    f"| {fmt_bytes(m.read_bytes)} | {fmt_bytes(m.write_bytes)} | {ok} |"
                )
            if r.fast.ok and r.gdal.ok:
                cmp_n += 1
                if r.time_winner == "fast":
                    time_wins += 1
                if r.mem_winner == "fast":
                    mem_wins += 1
                if r.io_winner == "fast":
                    io_wins += 1
                ratio = r.gdal.wall_s / r.fast.wall_s if r.fast.wall_s > 0 else 0
                lines.append(
                    f"| | | winner | time:**{r.time_winner}** ({ratio:.2f}×) "
                    f"mem:**{r.mem_winner}** io:**{r.io_winner}** | | | | | | {r.validated} |"
                )
            else:
                lines.append(f"| | | winner | — | | | | | | {r.validated} |")
        lines.append("")

    lines.extend(
        [
            "## Overall",
            "",
            f"| Metric | Fast wins |",
            f"|--------|-----------|",
            f"| Wall time | **{time_wins}/{cmp_n}** |",
            f"| Peak memory | **{mem_wins}/{cmp_n}** |",
            f"| Disk I/O | **{io_wins}/{cmp_n}** |",
            "",
        ]
    )

    report = OUT_ROOT / "RESOURCE_RESULTS.md"
    report.write_text("\n".join(lines))
    return report


def main() -> int:
    data_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_DATA
    if not data_dir.is_dir():
        print(f"error: {data_dir} not found", file=sys.stderr)
        return 1

    for binary in ("fastcog", "fastcrop", "fastband", "fastinfo", "fastvalidate", "fasttranslate"):
        if not (BIN / binary).exists():
            print("error: run cargo build --release first", file=sys.stderr)
            return 1

    files = discover(data_dir)
    if not files:
        print(f"error: no rasters in {data_dir}", file=sys.stderr)
        return 1

    results: list[PairResult] = []
    print(f"Resource benchmark: {len(files)} datasets -> {OUT_ROOT}\n")

    for path in files:
        slug = slugify(path)
        w, h, bands = probe(path)
        heavy = w * h > 150_000_000 or path.stat().st_size > 400_000_000
        runs = 1 if heavy else 2
        ds_dir = OUT_ROOT / slug
        ds_dir.mkdir(parents=True, exist_ok=True)

        print(f"[{slug}] {w}x{h}, {bands}b, heavy={heavy}")

        pair_bench(
            results,
            slug,
            "fastinfo",
            "metadata",
            [str(BIN / "fastinfo"), str(path)],
            ["gdalinfo", str(path)],
            None,
            None,
            runs=20,
        )

        cog_fast = ds_dir / "to_cog_fast.tif"
        cog_gdal = ds_dir / "to_cog_gdal.tif"
        pair_bench(
            results,
            slug,
            "fastcog",
            "encode → COG",
            [str(BIN / "fastcog"), str(path), str(cog_fast), "-q"],
            [*GDAL_COG, str(path), str(cog_gdal)],
            cog_fast,
            cog_gdal,
            runs=runs,
        )

        cog_source = (
            cog_gdal
            if cog_gdal.exists() and validate(cog_gdal) == "PASSED"
            else cog_fast
        )
        if validate(cog_source) != "PASSED":
            print("  skip derivatives (no valid COG)")
            continue

        pair_bench(
            results,
            slug,
            "fastcog",
            "COG → COG remux",
            [str(BIN / "fastcog"), str(cog_source), str(ds_dir / "copy_fast.tif"), "-q"],
            [*GDAL_COG, str(cog_source), str(ds_dir / "copy_gdal.tif")],
            ds_dir / "copy_fast.tif",
            ds_dir / "copy_gdal.tif",
            runs=runs,
        )

        aw = max(512, min(8192, (w // 512) * 512))
        ah = max(512, min(8192, (h // 512) * 512))
        win = ["0", "0", str(aw), str(ah)]
        pair_bench(
            results,
            slug,
            "fastcrop",
            f"crop {aw}×{ah}",
            [
                str(BIN / "fastcrop"),
                str(cog_source),
                str(ds_dir / "crop_fast.tif"),
                "--srcwin",
                *win,
                "-q",
            ],
            [*GDAL_COG, "-srcwin", *win, str(cog_source), str(ds_dir / "crop_gdal.tif")],
            ds_dir / "crop_fast.tif",
            ds_dir / "crop_gdal.tif",
            runs=runs,
        )

        if bands >= 3:
            pair_bench(
                results,
                slug,
                "fastband",
                "bands 1,2,3",
                [
                    str(BIN / "fastband"),
                    str(cog_source),
                    str(ds_dir / "bands_fast.tif"),
                    "-b",
                    "1",
                    "-b",
                    "2",
                    "-b",
                    "3",
                    "-q",
                ],
                [
                    *GDAL_COG,
                    "-b",
                    "1",
                    "-b",
                    "2",
                    "-b",
                    "3",
                    str(cog_source),
                    str(ds_dir / "bands_gdal.tif"),
                ],
                ds_dir / "bands_fast.tif",
                ds_dir / "bands_gdal.tif",
                runs=runs,
            )

    report = write_report(results, data_dir)
    print(f"\nWrote {report}")
    print(report.read_text())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
