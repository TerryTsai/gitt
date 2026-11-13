# Product requirements

gitt is a standalone Git browser and empty-repository creator. The primary workflow is reading committed code and history created with existing tools.

## Scope

- Read existing self-contained bare repositories and ordinary working checkouts without migration. Discover them recursively under one watched directory, stopping at repositories and skipping symlink directories.
- Browse committed files, branches, tags, and history. Exclude working files, staged edits, untracked paths, comparison views, and commit detail pages. Keep the user’s selected branch, tag, or commit unchanged through navigation; display it as plain text. Every revision link reloads its current page, replacing only the revision selection while preserving the route, file path, filters, and pagination. Refresh on navigation; no polling.
- Use one fixed Search button at the top right to open a unified search page. Search repository names or committed file paths within a selected repository; preselect the current repository when opened from it. Inputs use the same 13 px font as list rows, including on phones. Render safe Markdown, offer source views and line links, and bounded server-side syntax highlighting.
- Create empty bare repositories or checkouts, including grouped paths. Use a checked Bare toggle and select an initial branch. Never overwrite a destination or create inside another repository.
- Semantic, barely styled HTML; mobile first, including 320 CSS pixel screens. No JavaScript. No classes except syntax tokens; IDs only for line links. No external assets.
- Consistent compact breadcrumbs, keeping the current location visible and parent folders accessible without wasting mobile header space. Files, History, Branches, and Tags navigation, with the selected branch, tag, or commit as plain text at the right of the tab bar. Remove the information bar between tabs and content. Create button beside Search on home, with the same styling; no links to raw files. No Menu, Refresh control, duplicate page titles, routine introductory text, working-tree disclosure, or per-file status labels in listings.
- Compact file metadata: size, mode, commit age, last message, and author. Every list item stays on one line with distinct columns at all viewport widths; overflow scrolls horizontally within the list, without wrapping, clipping, or hiding columns. Repository rows also show the selected revision (HEAD by default), latest commit, author, and commit age. Omit the kind column. Use one shared list style for row spacing, column alignment, links, and hairline separators on phones and desktops, with consistent row typography except for commit hashes. Display file modes as grouped readable permissions, such as `rw- r-- r--`, show `-` for directory mode, and name symlink and submodule modes. Smaller source text and minimal line-number space; the interface uses 11 px/16 px source and 9 px line numbers.
- Show configured SSH clone URLs. Existing SSH/Git tools handle transport. No app accounts; reachable users can read and create. Localhost by default, configurable bind address/port.
- One executable with embedded assets. No runtime Git, database, Node, CGI, polling worker, or companion service. Linux x86-64 and ARM64 releases.
- Document actual features and measured size, RSS, startup, CPU, and navigation performance. Use captures of the actual interface; illustrative repository data must be identified. No invented measurements.

## Explicit exclusions

Editing, staging, committing, renaming, deleting, remote imports, generated initial files, HTTP Git transport, REST/JSON API, full-text/cross-repository search, split diffs, merge-base comparisons, accounts, issues, pull requests, an in-app CI service, agent integrations, linked worktrees, submodule browsing, and LFS fetching.

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

## Implementation

[Implementation decisions](decisions.md) record the architecture and tradeoffs.
[GitHub Actions](../.github/workflows/build.yml) builds and packages the project;
CI is not an application feature.
