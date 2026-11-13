# Implementation decisions

The user approved the requirements and technical direction, then authorized implementation and remaining routine decisions on 2026-11-13. This record distinguishes product requirements from implementation choices.

| Choice | Reason and consequence |
| --- | --- |
| git2 0.21 / bundled libgit2 1.9.7, network features disabled | Cohesive local Git APIs and a standalone executable. Installed Git is used only to generate and verify development fixtures and by separately configured SSH transport. |
| Evaluate gitoxide, retain git2 for v0.1 | Current gitoxide can handle staged status; it was not rejected for lacking that feature. A second backend would add unproven maintenance cost. |
| Axum / Hyper / one-thread Tokio runtime | Explicit connection and blocking-work limits. Two active Git/render operations, four waiting operations, 16 connections. No separate worker service. |
| Askama + embedded CSS/templates | Autoescaped outer document, audited escaped fragments, zero browser JavaScript. |
| pulldown-cmark + Ammonia | Restricted Markdown; repository HTML is displayed as text. Images become alt text. Relative links stay within the selected repository snapshot. |
| syntect, per-language compiled packs with Oniguruma | The preferred pure-Rust fancy-regex backend retained 23–28 MiB with concurrent source requests, exceeding the unchanged 16 MiB idle target. Curated packs alone did not fix it. Bundled Oniguruma brought the measured source-only retained RSS below 8 MiB, with complete highlighting of the 64 KiB fixture. YAML grammar compilation runs at build time; each request loads only its language and dependencies. Plain text remains available. |
| Linux Landlock ABI ≥3 and openat2 required | Kernel confinement applies to library reads too. Startup fails if enforcement is unavailable. The development host has ABI 2, so runtime acceptance testing occurs on the target host. |
| Disable libgit2 owner validation in this process only | The explicitly configured root is the shared-group boundary. Kernel confinement is installed before worker threads. No existing safe.directory or repository config is changed. |
| Exclude system/global Git configuration and clear GIT_* environment | Avoid implicit redirects, helpers, and machine-specific behavior. No shell or executable repository hooks/filters are called. |
| Single exact browser origin, explicit on wildcard bind | Host validation and same-origin creation prevent DNS rebinding and cross-origin writes. Forwarded headers are not trusted. |
| Signed expiring form token and SameSite=Strict cookie | Creation is an explicit protected POST; no state-changing GET. Origin and token checks do not authenticate users. |
| Descriptor-relative private staging + rename without replacement | A service-owned 0700 reserved directory protects stage names from other group members. A temporary initialization thread adds a pinned write-only Landlock policy and blocks path-based chmod/chown probes; libgit2 uses umask mode. Only newly staged content receives shared modes before atomic no-replace publication. Existing repositories are not chmodded. |
| SHA-1 / ordinary refs / self-contained storage | SHA-256, reftable, redirected Git directories, alternates, sparse/split index and linked worktrees are explicitly unsupported. No automatic conversion. |
| Raw-byte worktree comparison with explicit conversion warnings | Never run clean/smudge/textconv helpers or write index stat caches. Files needing configured conversions are reported instead of falsely labeled clean. |
| MIT project license, Cargo.lock committed | Small reusable utility with reproducible dependency selection. Third-party licenses accompany release artifacts. |
| No implicit service installation or exposure of existing personal repositories | Validate using isolated synthetic fixtures; ship OpenRC and systemd examples. Default listener remains localhost. |

Limits are observable product behavior, not claims of hard per-request preemption. A response timeout cannot interrupt a running C function; permits remain held until it exits. Working-tree views are observations, not filesystem transactions. File and index changes detected during reads are reported. An administrator or machine user who can place data inside the watched root controls what is visible there.

The README and performance report record measured outcomes, including any target that was not met. The target numbers are not silently raised.

Creation retains one reserved `.gitt-create-private-UID` directory with a marker. It reuses that private directory and never sweeps interrupted stages. Grouping directories created for failed operations may remain empty. Each active creation can use one short-lived 256 KiB-stack initialization thread while its normal work permit remains held. This is needed because Landlock restrictions cannot be removed from a reusable worker.

A 24 MiB shared native allocation budget additionally bounds libgit2 malloc/realloc, including packed delta bases. This is not a whole-process RSS guarantee. A verified fixture producing five bytes from a 64 MiB delta base returns 422 and the server remains available. Each HTTP connection has a 120-second lifetime, so stalled downloads cannot hold all 16 slots indefinitely. A timed-out creation explicitly warns that its operation may still finish.

All browser form contexts preserve non-UTF-8 filename bytes using internal hexadecimal hidden fields; navigation URLs use percent-encoded original bytes. Markdown is limited to 40,000 events and 128 nested elements.

Oniguruma is initialized before threads with 16,384 match-stack entries and one million match/search retries, in addition to the per-file size, line-length, and cooperative time limits. The C regex backend is an explicit measured tradeoff against the initial pure-Rust preference; it adds no runtime library dependency. Full release workload measurements are recorded separately in performance.md.
