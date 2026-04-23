#!/usr/bin/env python3
"""delayMirror benchmark tool.

Tests npm / pip / gomod package resolution speed:
- Direct: connecting to mirror directly
- Proxy:  through delayMirror proxy

Supports two modes:
  1. Remote mode (default): Tests against a deployed proxy (e.g., CF Workers)
  2. Local mode (--local): Tests against a locally running delay-mirror-server

Usage:
  python benchmark.py                    # Remote mode (CF Workers)
  python benchmark.py --local            # Local mode (http://localhost:8080)
  python benchmark.py --local --port 9090
  python benchmark.py --npm-only         # Only test npm
  python benchmark.py --pip-only         # Only test pip
  python benchmark.py --gomod-only       # Only test gomod
  python benchmark.py --quick            # Quick mode: 1 package per manager
  python benchmark.py --json             # Output results as JSON only
  python benchmark.py --compare          # Compare with previous results
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path

try:
    from rich.console import Console
    from rich.table import Table
    from rich.panel import Panel
    from rich import print as rprint
except ImportError:
    print("rich not installed, run: uv sync", file=sys.stderr)
    sys.exit(1)

console = Console()

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

DEFAULT_REMOTE_PROXY = "https://delay-mirror.fz-sec.workers.dev"
DEFAULT_LOCAL_PROXY = "http://localhost:8080"

NPM_PACKAGES_FULL = ["lodash", "express", "axios"]
PIP_PACKAGES_FULL = ["numpy", "requests", "flask"]
GOMOD_MODULES_FULL = [
    "github.com/gin-gonic/gin",
    "github.com/go-chi/chi/v5",
    "github.com/golang/mock",
]

NPM_PACKAGES_QUICK = ["lodash"]
PIP_PACKAGES_QUICK = ["requests"]
GOMOD_MODULES_QUICK = ["github.com/gin-gonic/gin"]

NPM_DIRECT = "https://registry.npmmirror.com"
PIP_DIRECT = "https://mirrors.aliyun.com/pypi/simple"
GOMOD_DIRECT = "https://goproxy.cn,direct"


# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------

@dataclass
class PackageResult:
    name: str
    direct_sec: float = 0.0
    proxy_sec: float = 0.0
    direct_status: str = "ok"
    proxy_status: str = "ok"


@dataclass
class ManagerResult:
    name: str
    direct_total: float = 0.0
    proxy_total: float = 0.0
    overhead_sec: float = 0.0
    overhead_pct: float = 0.0
    packages: list = field(default_factory=list)


@dataclass
class BenchmarkSummary:
    timestamp: str = ""
    target: str = ""
    mode: str = "remote"
    platform: str = ""
    results: dict = field(default_factory=dict)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def log_step(msg: str) -> None:
    console.print(f"  [dim]→[/dim] {msg}", style="cyan")


def log_ok(msg: str) -> None:
    console.print(f"  [green]✓[/green] {msg}")


def log_fail(msg: str) -> None:
    console.print(f"  [red]✗[/red] {msg}")


def log_warn(msg: str) -> None:
    console.print(f"  [yellow]⚠[/yellow] {msg}")


def timed_run(cmd: list[str], timeout: int = 120, **kwargs) -> tuple[float, str]:
    """Run command and return (elapsed_seconds, status)."""
    start = time.perf_counter()
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
            **kwargs,
        )
        elapsed = time.perf_counter() - start
        status = "ok" if result.returncode == 0 else f"exit_{result.returncode}"
        return elapsed, status
    except subprocess.TimeoutExpired:
        elapsed = time.perf_counter() - start
        return elapsed, "timeout"
    except Exception as e:
        elapsed = time.perf_counter() - start
        return elapsed, f"error: {e}"


def load_previous_results(path: Path) -> dict | None:
    """Load previous benchmark results for comparison."""
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text())
        return data.get("results", {})
    except (json.JSONDecodeError, KeyError):
        return None


# ---------------------------------------------------------------------------
# NPM benchmark
# ---------------------------------------------------------------------------

def bench_npm(packages: list[str], proxy_url: str) -> ManagerResult:
    console.print("\n[bold blue][1/3][/bold blue] npm benchmark")
    result = ManagerResult(name="npm")

    for pkg in packages:
        log_step(f"Testing [yellow]{pkg}[/yellow] (direct)...")

        with tempfile.TemporaryDirectory(prefix="npm-d-") as d:
            subprocess.run(["npm", "init", "-y"], cwd=d, capture_output=True)
            d_time, d_status = timed_run(
                ["npm", "install", pkg,
                 "--registry", NPM_DIRECT,
                 "--prefer-offline=false",
                 "--no-save", "--no-audit", "--no-fund"],
                cwd=d,
            )

        log_step(f"Testing [yellow]{pkg}[/yellow] (proxy)...")
        with tempfile.TemporaryDirectory(prefix="npm-p-") as p:
            subprocess.run(["npm", "init", "-y"], cwd=p, capture_output=True)
            p_time, p_status = timed_run(
                ["npm", "install", pkg,
                 "--registry", f"{proxy_url}/npm/",
                 "--prefer-offline=False",
                 "--no-save", "--no-audit", "--no-fund"],
                cwd=p,
            )

        pr = PackageResult(
            name=pkg,
            direct_sec=round(d_time, 3),
            proxy_sec=round(p_time, 3),
            direct_status=d_status,
            proxy_status=p_status,
        )
        result.packages.append(pr)

        if d_status == "ok" and p_status == "ok" and d_time > 0:
            ratio = p_time / d_time * 100 - 100
            tag = "green" if abs(ratio) < 20 else "yellow" if abs(ratio) < 50 else "red"
            log_ok(f"{pkg}: direct={d_time:.2f}s  proxy={p_time:.2f}s  [{tag}]{ratio:+.0f}%[/]")
        elif p_status != "ok":
            log_fail(f"{pkg}: proxy failed ({p_status})")
        elif d_status != "ok":
            log_warn(f"{pkg}: direct failed ({d_status}), cannot compare")

    result.direct_total = round(sum(p.direct_sec for p in result.packages), 3)
    result.proxy_total = round(sum(p.proxy_sec for p in result.packages), 3)
    if result.direct_total > 0:
        result.overhead_sec = round(result.proxy_total - result.direct_total, 3)
        result.overhead_pct = round(result.overhead_sec / result.direct_total * 100, 1)

    return result


# ---------------------------------------------------------------------------
# pip benchmark
# ---------------------------------------------------------------------------

def bench_pip(packages: list[str], proxy_url: str) -> ManagerResult:
    console.print("\n[bold blue][2/3][/bold blue] pip benchmark")
    result = ManagerResult(name="pip")

    for pkg in packages:
        log_step(f"Testing [yellow]{pkg}[/yellow] (direct)...")
        with tempfile.TemporaryDirectory(prefix="pip-d-") as d:
            venv_d = Path(d) / "venv"
            subprocess.run([sys.executable, "-m", "venv", str(venv_d)], capture_output=True)
            pip_d = venv_d / "bin" / "pip"
            subprocess.run([str(pip_d), "install", "--upgrade", "pip", "-q"],
                           capture_output=True)
            d_time, d_status = timed_run(
                [str(pip_d), "install",
                 "--index-url", PIP_DIRECT,
                 "--trusted-host", "mirrors.aliyun.com",
                 pkg, "-q"],
            )

        log_step(f"Testing [yellow]{pkg}[/yellow] (proxy)...")
        with tempfile.TemporaryDirectory(prefix="pip-p-") as p:
            venv_p = Path(p) / "venv"
            subprocess.run([sys.executable, "-m", "venv", str(venv_p)], capture_output=True)
            pip_p = venv_p / "bin" / "pip"
            subprocess.run([str(pip_p), "install", "--upgrade", "pip", "-q"],
                           capture_output=True)
            p_time, p_status = timed_run(
                [str(pip_p), "install",
                 "--index-url", f"{proxy_url}/pypi/simple/",
                 "--trusted-host", _extract_host(proxy_url),
                 pkg, "-q"],
            )

        pr = PackageResult(
            name=pkg,
            direct_sec=round(d_time, 3),
            proxy_sec=round(p_time, 3),
            direct_status=d_status,
            proxy_status=p_status,
        )
        result.packages.append(pr)

        if d_status == "ok" and p_status == "ok" and d_time > 0:
            ratio = p_time / d_time * 100 - 100
            tag = "green" if abs(ratio) < 20 else "yellow" if abs(ratio) < 50 else "red"
            log_ok(f"{pkg}: direct={d_time:.2f}s  proxy={p_time:.2f}s  [{tag}]{ratio:+.0f}%[/]")
        elif p_status != "ok":
            log_fail(f"{pkg}: proxy failed ({p_status})")
        elif d_status != "ok":
            log_warn(f"{pkg}: direct failed ({d_status}), cannot compare")

    result.direct_total = round(sum(p.direct_sec for p in result.packages), 3)
    result.proxy_total = round(sum(p.proxy_sec for p in result.packages), 3)
    if result.direct_total > 0:
        result.overhead_sec = round(result.proxy_total - result.direct_total, 3)
        result.overhead_pct = round(result.overhead_sec / result.direct_total * 100, 1)

    return result


def _extract_host(url: str) -> str:
    """Extract hostname from URL for --trusted-host."""
    from urllib.parse import urlparse
    parsed = urlparse(url)
    return parsed.hostname or ""


# ---------------------------------------------------------------------------
# gomod benchmark
# ---------------------------------------------------------------------------

def bench_gomod(modules: list[str], proxy_url: str) -> ManagerResult:
    console.print("\n[bold blue][3/3][/bold blue] gomod benchmark")
    result = ManagerResult(name="gomod")

    go_bin = shutil.which("go")
    if not go_bin:
        log_fail("go binary not found, skipping gomod benchmark")
        result.direct_status = "skipped"
        result.proxy_status = "skipped"
        return result

    for mod in modules:
        log_step(f"Testing [yellow]{mod}[/yellow] (direct)...")
        with tempfile.TemporaryDirectory(prefix="go-d-") as d:
            (Path(d) / "go.mod").write_text(
                f'module benchtmp\ngo 1.21\n'
            )
            d_time, d_status = timed_run(
                [go_bin, "mod", "download", mod],
                cwd=d,
                env={**os.environ, "GOPROXY": GOMOD_DIRECT},
            )

        log_step(f"Testing [yellow]{mod}[/yellow] (proxy)...")
        with tempfile.TemporaryDirectory(prefix="go-p-") as p:
            (Path(p) / "go.mod").write_text(
                f'module benchtmp\ngo 1.21\n'
            )
            p_time, p_status = timed_run(
                [go_bin, "mod", "download", mod],
                cwd=p,
                env={**os.environ, "GOPROXY": f"{proxy_url}/gomod/"},
            )

        pr = PackageResult(
            name=mod,
            direct_sec=round(d_time, 3),
            proxy_sec=round(p_time, 3),
            direct_status=d_status,
            proxy_status=p_status,
        )
        result.packages.append(pr)

        if d_status == "ok" and p_status == "ok" and d_time > 0:
            ratio = p_time / d_time * 100 - 100
            tag = "green" if abs(ratio) < 20 else "yellow" if abs(ratio) < 50 else "red"
            log_ok(f"{mod}: direct={d_time:.2f}s  proxy={p_time:.2f}s  [{tag}]{ratio:+.0f}%[/]")
        elif p_status != "ok":
            log_fail(f"{mod}: proxy failed ({p_status})")
        elif d_status != "ok":
            log_warn(f"{mod}: direct failed ({d_status}), cannot compare")

    result.direct_total = round(sum(p.direct_sec for p in result.packages), 3)
    result.proxy_total = round(sum(p.proxy_sec for p in result.packages), 3)
    if result.direct_total > 0:
        result.overhead_sec = round(result.proxy_total - result.direct_total, 3)
        result.overhead_pct = round(result.overhead_sec / result.direct_total * 100, 1)

    return result


# ---------------------------------------------------------------------------
# Results display
# ---------------------------------------------------------------------------

def print_results(
    results: list[ManagerResult],
    proxy_url: str,
    mode: str,
    json_only: bool = False,
    compare_prev: dict | None = None,
) -> BenchmarkSummary:
    summary = BenchmarkSummary(
        timestamp=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        target=proxy_url,
        mode=mode,
        platform="local-server" if mode == "local" else "cloudflare-workers",
    )

    all_results = {}
    for res in results:
        all_results[res.name] = asdict(res)

    summary.results = all_results

    if json_only:
        console.print_json(json.dumps(asdict(summary), indent=2))
        return summary

    # Summary table
    table = Table(title="delayMirror Benchmark Results", show_lines=True)
    table.add_column("Package Manager", style="bold")
    table.add_column("Direct (s)", justify="right")
    table.add_column("Proxy (s)", justify="right")
    table.add_column("Overhead (s)", justify="right")
    table.add_column("Overhead %", justify="right")
    table.add_column("Status", justify="center")

    for res in results:
        if res.direct_status == "skipped":
            table.add_row(res.name, "-", "-", "-", "-", "SKIPPED")
            continue

        tag = "green" if abs(res.overhead_pct) < 20 else "yellow" if abs(res.overhead_pct) < 50 else "red"
        status = "OK" if abs(res.overhead_pct) < 50 else "SLOW"
        if res.direct_total == 0:
            status = "NO_BASELINE"
            tag = "yellow"

        table.add_row(
            res.name,
            f"{res.direct_total:.2f}",
            f"{res.proxy_total:.2f}",
            f"{res.overhead_sec:+.2f}",
            f"[{tag}]{res.overhead_pct:+.1f}%[/]",
            status,
        )

    console.print()
    console.print(table)

    # Per-package detail table
    for res in results:
        if not res.packages:
            continue
        pkg_table = Table(title=f"  {res.name} — per-package detail", show_lines=True)
        pkg_table.add_column("Package", style="bold")
        pkg_table.add_column("Direct (s)", justify="right")
        pkg_table.add_column("Proxy (s)", justify="right")
        pkg_table.add_column("Direct Status", justify="center")
        pkg_table.add_column("Proxy Status", justify="center")

        for pkg in res.packages:
            d_tag = "green" if pkg.direct_status == "ok" else "red"
            p_tag = "green" if pkg.proxy_status == "ok" else "red"
            pkg_table.add_row(
                pkg.name,
                f"{pkg.direct_sec:.2f}",
                f"{pkg.proxy_sec:.2f}",
                f"[{d_tag}]{pkg.direct_status}[/]",
                f"[{p_tag}]{pkg.proxy_status}[/]",
            )

        console.print()
        console.print(pkg_table)

    # Comparison with previous results
    if compare_prev:
        console.print()
        comp_table = Table(title="Comparison with Previous Run", show_lines=True)
        comp_table.add_column("Manager", style="bold")
        comp_table.add_column("Prev Overhead %", justify="right")
        comp_table.add_column("Curr Overhead %", justify="right")
        comp_table.add_column("Change", justify="right")

        for res in results:
            if res.name in compare_prev:
                prev = compare_prev[res.name]
                prev_pct = prev.get("overhead_pct", 0)
                curr_pct = res.overhead_pct
                diff = curr_pct - prev_pct
                tag = "green" if diff < -5 else "red" if diff > 5 else "yellow"
                comp_table.add_row(
                    res.name,
                    f"{prev_pct:+.1f}%",
                    f"{curr_pct:+.1f}%",
                    f"[{tag}]{diff:+.1f}%[/]",
                )

        console.print(comp_table)

    return summary


# ---------------------------------------------------------------------------
# Health check
# ---------------------------------------------------------------------------

def check_proxy_health(proxy_url: str) -> bool:
    """Quick health check to see if proxy is reachable."""
    log_step(f"Checking proxy health: {proxy_url}")
    try:
        import urllib.request
        import urllib.error
        req = urllib.request.Request(proxy_url, method="GET")
        req.add_header("User-Agent", "delay-mirror-bench/1.0")
        with urllib.request.urlopen(req, timeout=10) as resp:
            status = resp.status
            if 200 <= status < 400:
                log_ok(f"Proxy is reachable (HTTP {status})")
                return True
            else:
                log_warn(f"Proxy returned HTTP {status}")
                return False
    except Exception as e:
        log_fail(f"Proxy unreachable: {e}")
        return False


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="delayMirror benchmark tool",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--local", action="store_true",
                        help="Use local server instead of remote CF Workers")
    parser.add_argument("--port", type=int, default=8080,
                        help="Local server port (default: 8080)")
    parser.add_argument("--proxy", type=str, default=None,
                        help="Custom proxy URL (overrides --local/--port)")
    parser.add_argument("--npm-only", action="store_true", help="Only run npm benchmarks")
    parser.add_argument("--pip-only", action="store_true", help="Only run pip benchmarks")
    parser.add_argument("--gomod-only", action="store_true", help="Only run gomod benchmarks")
    parser.add_argument("--quick", action="store_true",
                        help="Quick mode: 1 package per manager")
    parser.add_argument("--json", action="store_true",
                        help="Output results as JSON only (no tables)")
    parser.add_argument("--compare", action="store_true",
                        help="Compare results with previous run")
    parser.add_argument("--no-health-check", action="store_true",
                        help="Skip proxy health check")
    parser.add_argument("--timeout", type=int, default=120,
                        help="Per-package timeout in seconds (default: 120)")

    args = parser.parse_args()

    # Determine proxy URL
    if args.proxy:
        proxy_url = args.proxy
        mode = "custom"
    elif args.local:
        proxy_url = f"http://localhost:{args.port}"
        mode = "local"
    else:
        proxy_url = DEFAULT_REMOTE_PROXY
        mode = "remote"

    # Select packages
    if args.quick:
        npm_pkgs = NPM_PACKAGES_QUICK
        pip_pkgs = PIP_PACKAGES_QUICK
        gomod_mods = GOMOD_MODULES_QUICK
    else:
        npm_pkgs = NPM_PACKAGES_FULL
        pip_pkgs = PIP_PACKAGES_FULL
        gomod_mods = GOMOD_MODULES_FULL

    # Determine which benchmarks to run
    run_npm = not args.pip_only and not args.gomod_only
    run_pip = not args.npm_only and not args.gomod_only
    run_gomod = not args.npm_only and not args.pip_only

    # Health check
    if not args.no_health_check:
        if not check_proxy_health(proxy_url):
            console.print("\n[yellow]Proxy health check failed. "
                          "Results may be unreliable. Use --no-health-check to skip.[/]\n")

    console.print(Panel.fit(
        f"[bold cyan]delayMirror Benchmark[/bold cyan]\n"
        f"[dim]Mode:[/dim] {mode}\n"
        f"[dim]Target:[/dim] {proxy_url}\n"
        f"[dim]Timeout:[/dim] {args.timeout}s per package",
        border_style="blue",
    ))

    t0 = time.perf_counter()

    results: list[ManagerResult] = []

    if run_npm:
        results.append(bench_npm(npm_pkgs, proxy_url))
    if run_pip:
        results.append(bench_pip(pip_pkgs, proxy_url))
    if run_gomod:
        results.append(bench_gomod(gomod_mods, proxy_url))

    elapsed = time.perf_counter() - t0

    # Load previous results for comparison
    compare_prev = None
    if args.compare:
        results_path = Path(__file__).parent / "results.json"
        compare_prev = load_previous_results(results_path)
        if compare_prev is None:
            log_warn("No previous results found for comparison")

    summary = print_results(results, proxy_url, mode, args.json, compare_prev)

    # Save results
    output_path = Path(__file__).parent / "results.json"
    output_path.write_text(json.dumps(asdict(summary), indent=2) + "\n")
    console.print(f"\n[dim]Results saved to: {output_path}[/dim]")
    console.print(f"[dim]Total benchmark time: {elapsed:.1f}s[/dim]")

    # Exit code: 0 if all proxy tests passed, 1 otherwise
    all_ok = all(
        pkg.proxy_status == "ok"
        for res in results
        for pkg in res.packages
    )
    sys.exit(0 if all_ok else 1)


if __name__ == "__main__":
    main()
