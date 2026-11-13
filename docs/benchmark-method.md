# Reproducing measurements

Performance numbers describe the exact binary, fixtures, host, and command recorded
with each result. The targets are a binary no larger than 20 MiB, idle RSS no larger
than 16 MiB, browsing RSS no larger than 64 MiB with three readers, startup within
one second, warm endpoint p95 within 200 ms, and idle CPU below 0.1% of one core.
Targets are not measurements.

The scripts use Python's standard library on the development machine. Fixture
generation needs installed Git. The runtime and an SSH benchmark target do not
need either Python or Git. SSH transport for actual clones still needs server-side
Git independently of gitt.

## Fixtures

Choose a new destination on a filesystem with at least 3 GiB available:

```sh
python3 scripts/fixtures.py /tmp/gitt-bench-001 --repositories 300 --large-mib 1024
```

The command refuses an existing destination. It creates `repositories/`, an
outside-root `private/` sentinel, and `manifest.json`. It never removes a fixture
directory. The large repository contains sixteen deterministic, incompressible
64 MiB blobs plus a small README. Git packing temporarily requires a second copy
of the object data. Ordinary repositories have independent object files rather
than shared hardlinks. Most are copies of a small example history; this measures a
repeatable baseline rather than diversity of real-world repository structures.

The ordinary fixture has 62 reachable commits, a branch containing a slash, a tag,
a merge and root commit, Rust source, Markdown, binary data, non-UTF-8 and spaced
filenames, and symlinks. Its live checkout has three staged paths, four unstaged
paths, and one nonignored untracked path. Ignored untracked content is present but
must stay hidden. A shallow clone and empty bare/checkout repositories are included.
The manifest records commit IDs and layout. The large repository counts toward the
requested 300 repositories.

Security cases are separate from the ordinary performance workload:

```sh
python3 scripts/fixtures.py /tmp/gitt-smoke-001 --security
```

The optional cases include unsupported linked metadata and object alternates,
unresolved conflicts, and executable filter/hook configuration. Copy the complete
fixture directory to the test host so relative escape sentinels retain their
meaning. Serve only its `repositories/` child. Use an isolated destination you own;
never substitute personal repositories for the test data.

## Integration checks

Start gitt against a disposable fixture directory. The smoke suite creates empty
repositories under a unique `created-by-smoke-*` directory and retains them. It
checks existing index/config/HEAD/packed-ref hashes before and after browsing and
creation, preserves existing ownership/permissions, and verifies new metadata is
writable by the shared group. On the same host:

```sh
python3 tests/smoke.py --base-url http://127.0.0.1:18080 \
  --fixtures /tmp/gitt-smoke-001
```

For a host without Python, use an SSH forward and inspect metadata through SSH:

```sh
ssh -N -L 18080:127.0.0.1:18080 gitbox
python3 tests/smoke.py --base-url http://127.0.0.1:18080 \
  --origin http://127.0.0.1:18080 --fixtures /tmp/gitt-smoke-001 \
  --ssh-host gitbox --remote-root /tmp/gitt-smoke-001/repositories
```

The running server's configured origin must match `--origin`; the tunnel endpoint
can have a different local port. Smoke checks cover ordinary HTML flows, path and
symlink boundaries, ignored files, escaped content, byte-safe addressing, root and
merge diffs, endpoint comparison, shallow history, create-only behavior, CSRF,
host checks, concurrent name collisions, and metadata preservation. Browser tests
at 360 CSS pixels and keyboard checks remain necessary; HTML parsing alone cannot
prove that a page fits a phone.

An isolated VM without filesystem access can use `--http-only`. That mode visibly
omits metadata-preservation, ownership/permission, and hook-marker checks; verify
those separately inside the guest. Add `--native-delta` when the targeted
`delta-bomb.git` fixture was included in that guest. It is not a substitute for the
full host integration run.

When the SSH service disables forwarding, use a directly reachable LAN URL instead,
such as `--base-url http://gitbox:18080 --origin http://gitbox:18080`, and bind gitt to its
LAN address explicitly. The `--ssh-host` option is still used for metadata checks.

## Timing and memory

The following command starts and stops only its own benchmark processes. The port
must be available; an existing server should use a different port. Keep an SSH
forward running for remote tests:

```sh
python3 scripts/benchmark.py --ssh-host gitbox \
  --binary /tmp/gitt-bench-001/gitt \
  --root /tmp/gitt-bench-001/repositories \
  --base-url http://127.0.0.1:18080 --bind 127.0.0.1:18080 \
  --origin http://127.0.0.1:18080 \
  --manifest /tmp/gitt-bench-001/manifest.json \
  --startup-runs 10 --readers 3 --rounds 50 --idle-seconds 60 \
  --output /tmp/gitt-benchmark-001.json
```

Use `--pid PID` to measure an already-running process without stopping it; startup
is then unmeasured. Output paths must be new. Shorter idle periods are useful when
debugging, but the report explicitly marks them insufficient for acceptance.
The command exits 1 for request/sampling errors or 2 for an unmet resource target
or incomplete reference workload. The JSON is retained in either case. A complete
reference run has 300 repositories including the 1 GiB fixture, both source sizes,
three readers, at least 50 requests per endpoint, and 60-second idle observations.
For direct LAN measurements, use the host's HTTP URL and LAN address in `--bind`;
SSH then controls processes and reads `/proc` only. Keep a separate immutable copy
of the release binary in the fixture directory so an ongoing build cannot replace
it during measurements.

Startup is a fresh process reaching usable responses for every expected repository
listing page. Remote launch measurements include SSH setup and HTTP tunnel time,
so they are conservative observations rather than isolated server timings. The
script reports each run and the maximum. Filesystem caches stay warm; it never
drops host caches or reboots a machine. Do not label these numbers "cold storage."

After three warmups per endpoint, three client threads repeatedly request the
repository list, checkout and committed directories, 3,564-byte and 65,536-byte source files,
Markdown, a 50-commit history page, working changes, and a small snapshot diff. A
large-repository README is included when present. Each endpoint gets 50 measured
requests by default. The report includes endpoint and aggregate p50/p95/max,
response sizes, source responses with highlighting versus plain fallback, failures,
workload duration, binary bytes and SHA-256, host details,
and the full fixture manifest. HTTP latency includes client and transport time;
the harness does not subtract network overhead or discard failed requests.

RSS includes mapped pages: a persistent SSH stream samples `/proc/PID/smaps_rollup`
and `/proc/PID/status` at approximately 50 ms; `VmHWM` records the process high-water
mark between samples. Sampling overhead and SSH traffic share the host with the
application and are part of this test setup. The report uses the larger observed
RSS/high-water value for the browsing target. Native allocations are included.

Idle CPU comes from differences in `/proc/PID/stat` user and system ticks across
60-second windows before and after browsing. Linux x86-64 and ARM64 use a userspace
clock tick of 100 Hz. The harness reads `getconf CLK_TCK` on the target, falls back to
100 Hz if unavailable, and records the source. `--clock-ticks` overrides detection.
Report the resulting resolution (about 0.0167% for one tick over 60 seconds).
Zero observed ticks means no CPU time measurable at that resolution, not proof of
zero work. Retained RSS after browsing is reported separately from the initial idle
RSS. All resource targets remain observations for the defined workload, not hard
memory or scheduling guarantees for arbitrary malicious Git object graphs.

## Service examples

`deploy/gitt.openrc` and `deploy/gitt.openrc.conf` are examples for Alpine/OpenRC;
install them as `/etc/init.d/gitt` and `/etc/conf.d/gitt`. Make the init script
executable. `deploy/gitt.service` is an optional systemd example. Create a dedicated
`gitt` user and shared `git` group first, and grant that group access to the watched
root. A setgid root/grouping-directory policy helps new repositories inherit the
shared group. Review paths and origin before enabling either service. Installation
does not imply configuring SSH, a reverse proxy, or authentication.

These examples never change ownership or permissions of existing repositories.
The application requires Landlock ABI 3 or newer; service-manager restrictions add
to its own confinement rather than replacing it. The systemd example hides home
directories, so a watched root under `/home` needs an operator-adjusted unit.
