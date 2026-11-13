# Performance: v0.1.0

Measured 2026-11-13 on an Intel Core i5-4570T (2 cores / 4 logical CPUs), 8 GB RAM,
PNY CS900 500GB SATA SSD, ext4, Alpine Linux 3.23.4, and Linux 6.18.33-0-lts.
The host's initial load average was 0.00 / 0.00 / 0.00. These are release-binary
measurements, not estimates. All agreed targets passed for the workload below.

| Measurement | Observed | Target |
| --- | --- | --- |
| x86-64 executable, assets included | 3,602,224 bytes / 3.44 MiB | ≤20 MiB |
| Idle RSS before browsing | 2,792 KiB / 2.73 MiB | ≤16 MiB |
| Idle RSS after browsing | 13,056 KiB / 12.75 MiB | ≤16 MiB |
| Peak browsing RSS, process high-water mark | 16,968 KiB / 16.57 MiB | ≤64 MiB |
| Fresh process to complete repository list, worst of 10 | 272.04 ms | ≤1,000 ms |
| Slowest endpoint p95, three readers | 91.43 ms | ≤200 ms |
| Idle CPU, before and after | 0 ticks in each ≥60-second window | <0.1% of one core |

One CPU tick corresponds to approximately 0.0166% of one core over each observation.
Zero observed ticks means no CPU time measurable at that resolution. The idle
windows lasted 60.271 and 60.225 seconds; the process returned to one thread.

## Workload

300 repositories, predominantly copies of a small mixed bare/checkout history with
independent object files, plus one repository containing 1 GiB of deterministic,
incompressible blob data. Ordinary populated repositories each have one pack,
62 reachable commits, 21 tracked paths, and Rust, Markdown, and plain-text content.
The working fixture has three staged paths, four unstaged paths, and one nonignored
untracked path. Its Rust samples are 3,564 and 65,536 bytes.

The large repository contains sixteen 64 MiB blobs and a small README in one pack.
The ordinary benchmark reads its README, not its oversized blobs. Oversized raw
objects and a small-result delta with a 64 MiB base have separate rejection checks.

After three warmup requests per endpoint, three client threads made 50 requests
per endpoint: 500 measured requests over 2.558 seconds. Both source sizes were
fully syntax-highlighted on all 50 requests, with no plain-text fallbacks. There
were no request or memory-sampling errors.

## HTTP latency

Milliseconds, including LAN transport and client overhead. The aggregate p50 was
6.70 ms, p95 76.87 ms, and maximum 101.12 ms. Per-endpoint results are more useful
than the aggregate when comparing operations.

| Endpoint | p50 | p95 | Maximum |
| --- | ---: | ---: | ---: |
| Repository list (first 200 of 300) | 9.56 | 11.56 | 13.50 |
| Working directory | 6.77 | 9.22 | 17.51 |
| Committed directory | 3.58 | 5.61 | 12.95 |
| Rust source, 3,564 bytes | 8.45 | 10.84 | 12.35 |
| Rendered Markdown | 6.85 | 9.90 | 10.80 |
| History, 50 commits | 3.46 | 5.98 | 7.35 |
| Working changes | 6.13 | 8.70 | 8.86 |
| Two-snapshot diff | 3.81 | 6.41 | 7.95 |
| Rust source, 65,536 bytes | 76.88 | 91.43 | 101.11 |
| README in the 1 GiB repository | 16.84 | 39.07 | 45.28 |

The 64 KiB syntax-heavy sample produced about 1.48 MiB of HTML. The size is included
in these timings; there is no response compression in this release.

## Conditions and reproduction

Startup measures the launch request until both pages of the 300-repository list
have arrived. It includes SSH process-launch overhead and LAN HTTP latency.
Median startup was 257.62 ms. Every trial uses a fresh server process, while
filesystem caches remain warm. No host cache dropping or reboot was performed.

RSS includes mapped pages. A persistent SSH stream sampled `smaps_rollup` at about
50 ms; the reported peak uses the higher process `VmHWM` value to capture activity
between samples. Sampling shares the machine with the server. Results describe
this bounded fixture workload; large histories, different languages, concurrent
writers, or pathological Git objects may behave differently.

See [the benchmark guide](benchmark-method.md) for commands and limitations, and
[the machine-readable measurements](performance.json) for all per-endpoint values,
startup trials, CPU resolution, source-rendering counts, and target checks.

## Builds

Built with Rust 1.95.0 and Zig cc 0.17.1 targeting musl, using release optimization
for size, LTO, one codegen unit, stripped symbols, and abort-on-panic. Both executables
have no ELF interpreter and no shared-library dependency.

| Artifact | Bytes | SHA-256 |
| --- | ---: | --- |
| Linux x86-64 | 3,602,224 | `8c470e3ec702a64ec95f635543fe44abd2035762fa2703b6968410bc90f60843` |
| Linux ARM64 | 3,447,984 | `c5ecae67b8b38435f47943936a6d174be59c469dc58294677147f14a252b5ca3` |

ARM compatibility was tested in a complete QEMU ARM64 Linux guest: Alpine 3.23.4,
Linux 6.18.22, Landlock ABI 7, an unprivileged user, and no installed Git executable.
All 16 HTTP integration checks and guest filesystem/permission checks passed.
Emulation does not establish ARM hardware performance; none is claimed here.
