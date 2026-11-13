#!/usr/bin/env python3
"""Measure gitt with stdlib Python. SSH targets need only a POSIX shell and /proc.

The HTTP URL can be an SSH forward. Latencies then include the tunnel and client;
this harness deliberately does not subtract transport time. It never drops caches.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
import datetime
import json
import math
from pathlib import Path
import platform
import shlex
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request


ENDPOINTS = {
    "repositories": ("/", {}),
    "checkout_directory": ("/files", {"repo": "demo"}),
    "committed_directory": ("/files", {"repo": "archive.git", "path": "src"}),
    "source_3564_bytes": ("/files", {"repo": "archive.git", "path": "src/main.rs"}),
    "markdown": ("/files", {"repo": "archive.git", "path": "README.md"}),
    "history_50": ("/history", {"repo": "archive.git"}),
    "working_changes": ("/changes", {"repo": "demo"}),
    "snapshot_diff": ("/compare", {"repo": "archive.git", "from": "v0.1", "to": "feature/example"}),
}


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def summarize(values):
    return {"count": len(values), "p50_ms": round(statistics.median(values), 3), "p95_ms": round(percentile(values, .95), 3), "max_ms": round(max(values), 3)}


class Target:
    def __init__(self, ssh_host):
        self.ssh_host = ssh_host

    def run(self, command, *, check=True):
        args = ["ssh", "-oBatchMode=yes", self.ssh_host, command] if self.ssh_host else ["sh", "-c", command]
        return subprocess.run(args, check=check, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def read(self, path):
        if self.ssh_host:
            return self.run("cat " + shlex.quote(str(path))).stdout
        return Path(path).read_text()

    def host_information(self):
        script = """uname -a
printf '\\n--os-release--\\n'
cat /etc/os-release
printf '\\n--cpuinfo--\\n'
head -n 30 /proc/cpuinfo
printf '\\n--meminfo--\\n'
head -n 5 /proc/meminfo
printf '\\n--loadavg--\\n'
cat /proc/loadavg
printf '\\n--filesystems--\\n'
df -T 2>/dev/null || df
"""
        return self.run(script).stdout

    def sample(self, pid):
        script = "cat /proc/{0}/stat; printf '\\n--status--\\n'; cat /proc/{0}/status; printf '\\n--smaps--\\n'; cat /proc/{0}/smaps_rollup 2>/dev/null || true".format(pid)
        return parse_sample(self.run(script).stdout)


def parse_sample(text):
    raw_stat, rest = text.split("\n--status--\n", 1)
    status, smaps = rest.split("\n--smaps--\n", 1)
    # The command in parentheses may contain spaces; fields 14/15 are utime/stime.
    fields = raw_stat[raw_stat.rfind(")") + 2:].split()
    values = {}
    for line in status.splitlines():
        key, _, value = line.partition(":")
        if key in {"VmRSS", "VmHWM", "VmSize", "Threads"}:
            values[key] = int(value.split()[0])
    for line in smaps.splitlines():
        key, _, value = line.partition(":")
        if key in {"Rss", "Pss", "Private_Clean", "Private_Dirty", "Shared_Clean", "Shared_Dirty"}:
            values["smaps_" + key] = int(value.split()[0])
    values.update(cpu_ticks=int(fields[11]) + int(fields[12]), start_ticks=int(fields[19]), observed_at=time.monotonic())
    return values


class Process:
    def __init__(self, target, binary, root, bind, origin):
        self.target = target
        self.arguments = [binary, root, "--bind", bind, "--origin", origin]
        self.process = None
        self.pid = None
        self.start_ticks = None
        self.log = tempfile.TemporaryFile()

    def start(self):
        self.launched_at = time.monotonic()
        if self.target.ssh_host:
            script = 'printf "%s\\n" "$$"; exec "$@"'
            command = "sh -c " + shlex.quote(script) + " sh " + shlex.join(self.arguments)
            self.process = subprocess.Popen(["ssh", "-oBatchMode=yes", self.target.ssh_host, command], stdout=subprocess.PIPE, stderr=self.log)
            first = self.process.stdout.readline()
            if not first.strip().isdigit():
                raise RuntimeError("Remote process failed to return its PID")
            self.pid = int(first)
        else:
            self.process = subprocess.Popen(self.arguments, stdout=self.log, stderr=self.log)
            self.pid = self.process.pid

    def pin_identity(self):
        self.start_ticks = self.target.sample(self.pid)["start_ticks"]

    def close(self):
        if self.process is None or self.log.closed:
            return
        if self.process.poll() is None:
            if self.target.ssh_host and self.pid:
                # Only stop the exact process launched by this object.
                try:
                    current = self.target.sample(self.pid)
                    if self.start_ticks is None or current["start_ticks"] == self.start_ticks:
                        self.target.run(f"kill -TERM {self.pid}", check=False)
                except (subprocess.CalledProcessError, FileNotFoundError):
                    pass
            else:
                self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.log.seek(0)
        self.log_text = self.log.read().decode(errors="replace")
        self.log.close()


class Sampler:
    def __init__(self, target, pid, interval=.05):
        self.target = target
        self.pid = pid
        self.interval = interval
        self.peak_rss_kib = 0
        self.samples = 0
        self.stop_event = threading.Event()
        self.errors = []
        self.worker = None
        self.remote = None

    def start(self):
        self.worker = threading.Thread(target=self._run, daemon=True)
        self.worker.start()

    def _record(self, text):
        sample = parse_sample(text)
        self.peak_rss_kib = max(self.peak_rss_kib, sample.get("smaps_Rss", sample.get("VmRSS", 0)))
        self.samples += 1

    def _run(self):
        try:
            if self.target.ssh_host:
                # One persistent SSH channel; no new SSH handshake for each sample.
                script = f"""set -e
while test -r /proc/{self.pid}/stat; do
cat /proc/{self.pid}/stat
printf '\\n--status--\\n'
cat /proc/{self.pid}/status
printf '\\n--smaps--\\n'
cat /proc/{self.pid}/smaps_rollup 2>/dev/null || true
printf '\\n--sample-end--\\n'
sleep {self.interval}
done
"""
                self.remote = subprocess.Popen(["ssh", "-oBatchMode=yes", self.target.ssh_host, script], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
                block = []
                for line in self.remote.stdout:
                    if self.stop_event.is_set():
                        break
                    if line.strip() == "--sample-end--":
                        self._record("".join(block))
                        block = []
                    else:
                        block.append(line)
            else:
                while not self.stop_event.is_set():
                    sample = self.target.sample(self.pid)
                    self.peak_rss_kib = max(self.peak_rss_kib, sample.get("smaps_Rss", sample.get("VmRSS", 0)))
                    self.samples += 1
                    self.stop_event.wait(self.interval)
        except Exception as error:
            if not self.stop_event.is_set():
                self.errors.append(str(error))
        finally:
            if self.remote and self.remote.poll() is None:
                self.remote.terminate()
                self.remote.wait()

    def close(self):
        self.stop_event.set()
        if self.remote and self.remote.poll() is None:
            self.remote.terminate()
        if self.worker:
            self.worker.join(timeout=5)


def request(base_url, origin, path, params=None, render_stats=False):
    url = base_url.rstrip("/") + path
    if params:
        url += "?" + urllib.parse.urlencode(params)
    req = urllib.request.Request(url, headers={"Host": urllib.parse.urlsplit(origin).netloc})
    began = time.monotonic()
    with urllib.request.urlopen(req, timeout=30) as response:
        body = response.read()
        if response.status != 200:
            raise RuntimeError(f"{path}: HTTP {response.status}")
    elapsed = (time.monotonic() - began) * 1000
    if render_stats:
        return elapsed, len(body), body.count(b"<span class=")
    return elapsed, len(body)


def await_ready(process, base_url, origin, repository_count):
    deadline = time.monotonic() + 15
    last_error = None
    while time.monotonic() < deadline:
        if process.process.poll() is not None:
            process.close()
            raise RuntimeError("gitt exited during startup: " + process.log_text)
        try:
            # Fetch every expected listing page, including discovery time.
            for page in range(max(1, math.ceil(repository_count / 200))):
                request(base_url, origin, "/", {"page": page})
            return (time.monotonic() - process.launched_at) * 1000
        except (OSError, urllib.error.URLError) as error:
            last_error = str(error)
            time.sleep(.01)
    raise RuntimeError("No usable repository listing within 15 seconds: " + str(last_error))


def idle(target, pid, seconds, clock_ticks):
    before = target.sample(pid)
    began = time.monotonic()
    deadline = began + seconds
    while time.monotonic() < deadline:
        time.sleep(max(0, min(1, deadline - time.monotonic())))
    after = target.sample(pid)
    elapsed = time.monotonic() - began
    ticks = after["cpu_ticks"] - before["cpu_ticks"]
    return {
        "seconds": round(elapsed, 3), "cpu_ticks": ticks,
        "cpu_percent_of_one_core": round(ticks / clock_ticks / elapsed * 100, 5),
        "cpu_resolution_percent": round(100 / clock_ticks / elapsed, 5),
        "rss_kib": after.get("smaps_Rss", after.get("VmRSS")),
        "status_vm_rss_kib": after.get("VmRSS"), "status_vm_hwm_kib": after.get("VmHWM"),
        "threads": after.get("Threads"),
    }


def benchmark(args):
    manifest = json.loads(args.manifest.read_text())
    target = Target(args.ssh_host)
    origin = args.origin or args.base_url
    detected_ticks = target.run("getconf CLK_TCK", check=False)
    if args.clock_ticks:
        clock_ticks, ticks_source = args.clock_ticks, "Explicit --clock-ticks"
    elif detected_ticks.returncode == 0 and detected_ticks.stdout.strip().isdigit():
        clock_ticks, ticks_source = int(detected_ticks.stdout.strip()), "getconf CLK_TCK on target"
    else:
        clock_ticks, ticks_source = 100, "Linux x86-64/ARM64 USER_HZ fallback; override with --clock-ticks"
    result = {
        "schema": 1, "measured_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "host": target.host_information(), "client": platform.platform(),
        "transport": ("SSH-forwarded HTTP including client/tunnel latency" if args.ssh_host and urllib.parse.urlsplit(args.base_url).hostname in {"localhost", "127.0.0.1", "::1"} else "HTTP from benchmark client; remote process control and /proc sampling over SSH" if args.ssh_host else "HTTP from benchmark client"),
        "cache_conditions": "Fresh processes for startup; filesystem caches are not dropped. Latency follows per-endpoint warmup.",
        "fixtures": manifest, "readers": args.readers,
        "clock_ticks_per_second": clock_ticks,
        "clock_ticks_source": ticks_source,
        "binary": None, "startup": None,
    }
    if args.binary:
        info = target.run("wc -c < " + shlex.quote(args.binary) + "; sha256sum " + shlex.quote(args.binary)).stdout.splitlines()
        result["binary"] = {"bytes": int(info[0]), "sha256": info[1].split()[0], "path": args.binary}
    process = None
    sampler = None
    try:
        if not args.pid:
            times = []
            for number in range(args.startup_runs):
                current = Process(target, args.binary, args.root, args.bind, origin)
                try:
                    current.start()
                    times.append(await_ready(current, args.base_url, origin, manifest["ordinary_repositories"]))
                    current.pin_identity()
                finally:
                    current.close()
                print(f"startup {number + 1}/{args.startup_runs}: {times[-1]:.1f} ms", file=sys.stderr, flush=True)
            result["startup"] = dict(summarize(times), individual_ms=[round(x, 3) for x in times], definition="Launch request to all repository listing pages received. Remote runs include SSH process-launch overhead; filesystem caches remain warm.")
            process = Process(target, args.binary, args.root, args.bind, origin)
            process.start()
            await_ready(process, args.base_url, origin, manifest["ordinary_repositories"])
            process.pin_identity()
            pid = process.pid
        else:
            pid = args.pid
        result["pid"] = pid
        print(f"idle before browsing: {args.idle_seconds:g} seconds", file=sys.stderr, flush=True)
        result["idle_before"] = idle(target, pid, args.idle_seconds, clock_ticks)
        endpoints = dict(ENDPOINTS)
        if manifest["fixtures"].get("source_large_bytes"):
            endpoints["source_65536_bytes"] = ("/files", {"repo": "archive.git", "path": "src/sample.rs"})
        if manifest.get("large"):
            endpoints["large_repository_readme"] = ("/files", {"repo": "large.git", "path": "README.md"})
        # Warmup is included in peak accounting; it is not included in latency percentiles.
        sampler = Sampler(target, pid)
        sampler.start()
        for path, params in endpoints.values():
            for _ in range(3):
                request(args.base_url, origin, path, params)
        jobs = [(name, path, params) for _ in range(args.rounds) for name, (path, params) in endpoints.items()]
        samples = {name: [] for name in endpoints}
        response_sizes = {name: [] for name in endpoints}
        highlighted_spans = {name: [] for name in endpoints if name.startswith("source_")}
        errors = []
        def run(job):
            name, path, params = job
            try:
                measured = request(args.base_url, origin, path, params, render_stats=name in highlighted_spans)
                return name, measured[0], measured[1], measured[2] if len(measured) > 2 else None, None
            except Exception as error:
                return name, None, None, None, str(error)
        print(f"{len(jobs)} requests with {args.readers} concurrent readers", file=sys.stderr, flush=True)
        began = time.monotonic()
        with ThreadPoolExecutor(max_workers=args.readers) as pool:
            for name, latency, size, spans, error in pool.map(run, jobs):
                if error:
                    errors.append({"endpoint": name, "error": error})
                else:
                    samples[name].append(latency)
                    response_sizes[name].append(size)
                    if spans is not None:
                        highlighted_spans[name].append(spans)
        result["workload_seconds"] = round(time.monotonic() - began, 3)
        final = target.sample(pid)
        sampler.close()
        result["latency"] = {name: dict(summarize(values), min_response_bytes=min(response_sizes[name]), max_response_bytes=max(response_sizes[name])) for name, values in samples.items() if values}
        result["source_rendering"] = {name: {"responses_with_highlighting": sum(count > 0 for count in counts), "plain_fallback_responses": counts.count(0), "min_highlighted_spans": min(counts), "max_highlighted_spans": max(counts)} for name, counts in highlighted_spans.items() if counts}
        all_values = [value for values in samples.values() for value in values]
        result["aggregate_latency"] = summarize(all_values) if all_values else None
        result["errors"] = errors
        result["memory"] = {
            "sampled_peak_rss_kib": sampler.peak_rss_kib,
            "process_vm_hwm_kib": final.get("VmHWM"),
            "samples": sampler.samples, "sampling_interval_seconds": sampler.interval,
            "sampling_errors": sampler.errors,
            "method": "smaps_rollup RSS sampled at ~50 ms; VmHWM captures process high-water RSS including mappings. SSH sampling uses one persistent shell stream.",
        }
        print(f"idle after browsing: {args.idle_seconds:g} seconds", file=sys.stderr, flush=True)
        result["idle_after"] = idle(target, pid, args.idle_seconds, clock_ticks)
        result["target_checks"] = {
            "reference_workload_complete": manifest["ordinary_repositories"] == 300 and args.readers == 3 and args.rounds >= 50 and bool(manifest.get("large")) and manifest["large"]["blob_bytes"] >= 1024 * 1024 * 1024 and manifest["fixtures"].get("source_large_bytes") == 65536,
            "binary_at_most_20_mib": result["binary"]["bytes"] <= 20 * 1024 * 1024 if result["binary"] else None,
            "idle_rss_at_most_16_mib": max(result["idle_before"]["rss_kib"], result["idle_after"]["rss_kib"]) <= 16 * 1024,
            "browsing_rss_at_most_64_mib": max(sampler.peak_rss_kib, final.get("VmHWM", 0)) <= 64 * 1024,
            "every_endpoint_p95_at_most_200_ms": bool(all_values) and not errors and all(entry["p95_ms"] <= 200 for entry in result["latency"].values()),
            "startup_at_most_1_second": result["startup"]["max_ms"] <= 1000 if result["startup"] else None,
            "idle_cpu_below_0_1_percent": all(result[phase]["cpu_percent_of_one_core"] < .1 for phase in ["idle_before", "idle_after"]),
            "idle_observation_at_least_60_seconds": args.idle_seconds >= 60,
        }
        return result
    finally:
        if sampler:
            sampler.close()
        if process:
            process.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--origin", help="Canonical origin when base-url is an SSH forward")
    parser.add_argument("--manifest", required=True, type=Path)
    parser.add_argument("--ssh-host")
    parser.add_argument("--pid", type=int, help="Measure an existing process; never starts/stops it or measures startup")
    parser.add_argument("--binary", help="Binary path on the target; used to launch owned benchmark processes unless --pid is given")
    parser.add_argument("--root", help="Fixture repositories directory on target")
    parser.add_argument("--bind", default="127.0.0.1:18080")
    parser.add_argument("--startup-runs", type=int, default=10)
    parser.add_argument("--idle-seconds", type=float, default=60)
    parser.add_argument("--readers", type=int, default=3)
    parser.add_argument("--rounds", type=int, default=50, help="Measured requests per endpoint")
    parser.add_argument("--clock-ticks", type=int, help="Override getconf CLK_TCK detection (Linux fallback: 100)")
    parser.add_argument("--output", type=Path, required=True, help="New JSON file; existing output is refused")
    args = parser.parse_args()
    if not args.pid and not (args.binary and args.root):
        parser.error("Supply --pid, or both --binary and --root")
    if min(args.startup_runs, args.idle_seconds, args.readers, args.rounds) <= 0 or (args.clock_ticks is not None and args.clock_ticks <= 0):
        parser.error("Counts and durations must be positive")
    if args.output.exists():
        parser.error("Refusing to overwrite existing output")
    result = benchmark(args)
    with args.output.open("x") as output:
        json.dump(result, output, indent=2)
        output.write("\n")
    print(json.dumps({"output": str(args.output.resolve()), "target_checks": result["target_checks"]}, indent=2))
    if result["errors"] or result["memory"]["sampling_errors"]:
        sys.exit(1)
    if any(value is False for value in result["target_checks"].values()):
        sys.exit(2)


if __name__ == "__main__":
    main()
