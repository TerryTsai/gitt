# gitt v0.1 requirements

Approved scope: a small, standalone Rust Git browser and empty-repository creator for a homelab. The primary workflow is reading code and comparing changes made with existing tools. Nothing is agent-specific.

## Confirmed requirements

- Read existing self-contained bare repositories and ordinary working checkouts without migration. Discover them recursively under one watched directory, stopping at repositories and skipping symlink directories.
- Browse working files, committed snapshots, branches, tags, history, commits, and unified diffs. Separate staged, unstaged, and nonignored untracked changes. Compare two endpoint snapshots. Refresh on navigation; no polling.
- Search repository names and file paths. Render safe Markdown, offer source and raw downloads, line links, and bounded server-side syntax highlighting.
- Create empty bare repositories or checkouts, including grouped paths. Select an initial branch. Never overwrite a destination or create inside another repository.
- Semantic, barely styled HTML; mobile first at 360 CSS pixels. No JavaScript. No classes except syntax tokens; IDs only for line links. No external assets.
- Show configured SSH clone URLs. Existing SSH/Git tools handle transport. No app accounts; reachable users can read and create. Localhost by default, configurable bind address/port.
- One executable with embedded assets. No runtime Git, database, Node, CGI, polling worker, or companion service. Linux x86-64 and ARM64 releases.
- Document actual features and measured size, RSS, startup, CPU, and navigation performance. Use real screenshots. No invented results.

## Explicit exclusions

Editing, staging, committing, renaming, deleting, remote imports, generated initial files, HTTP Git transport, REST/JSON API, full-text/cross-repository search, split diffs, merge-base comparisons, accounts, issues, pull requests, CI, agent integrations, linked worktrees, submodule browsing, and LFS fetching.

## Acceptance targets

| Measurement | Target |
| --- | --- |
| Stripped executable including assets | ≤20 MiB |
| Idle RSS, including after browsing | ≤16 MiB |
| Ordinary browsing peak RSS, three readers | ≤64 MiB |
| Fresh process to usable repository list | ≤1 second |
| Warm ordinary request p95 | ≤200 ms |
| Idle CPU | <0.1% of one core |

Workload: 300 mostly small repositories including one approximately 1 GiB repository, on an SSD. Target host: i5-4570T, approximately 8 GB RAM, Alpine Linux. These are targets; actual results and exceptions belong in performance.md.

Acceptance also includes Git-oracle fixtures, unchanged index/config after reads, safe path/symlink handling, hostile content escaping, protected creation, collision/concurrent-creation checks, static-link inspection, and all primary flows at phone and desktop widths.

## Decisions delegated during implementation

The user approved autonomous completion on 2026-11-13 and authorized remaining decisions to be made and recorded. Implementation choices are recorded in decisions.md. No unresolved requirement blocks implementation. Recommendations and measured outcomes must not be presented as prior user requirements.
