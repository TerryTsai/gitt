# gitt reference

The [README](../README.md) covers installation and ordinary use. This guide
describes configuration, repository behavior, resource limits, and development.

## Configuration

```sh
gitt /srv/git \
  --bind 0.0.0.0:8080 \
  --origin http://gitbox.local:8080 \
  --ssh-prefix ssh://git@gitbox.local/srv/git/
```

The example assumes `gitt` is on `PATH`, `/srv/git` exists, and the browser can
resolve `gitbox.local` to the server.

| Argument | Default / behavior |
| --- | --- |
| `<repository-root>` | Required existing directory; limits filesystem visibility |
| `--bind <IP:port>` | `127.0.0.1:8080`; numeric IPv4 or IPv6 address |
| `--origin <http(s)://host[:port]>` | Bind URL; required for a wildcard bind or a different browser hostname |
| `--ssh-prefix <ssh://…/>` | Displays clone URLs; include the trailing slash |
| `--initial-branch <name>` | `main`; the creation form can override it |
| `--help`, `--version` | Show help or version |

Configuration comes from command-line arguments. There is no configuration
database. `--ssh-prefix` displays an address; it does not install or configure
Git transport. An SSH server, server-side Git tools, filesystem permissions, and
SSH keys handle clone, fetch, and push. Use a bare repository as a shared push
destination. Ordinary checkouts retain Git's restrictions on pushes to their
checked-out branch.

## Deployment

Run gitt as an unprivileged user. It needs read access to the repositories and
write access to the destination's parent directory to create repositories. New
repositories use shared-group permissions; existing ownership and permissions
are preserved.

[OpenRC](../deploy/gitt.openrc), [OpenRC configuration](../deploy/gitt.openrc.conf),
and [systemd](../deploy/gitt.service) examples are included. Adjust the binary
path, repository root, user, group, and origin before installing a service.

On Alpine, install the release to the service's binary location:

```sh
gitt_installer=$(mktemp)
curl -fsSL https://raw.githubusercontent.com/TerryTsai/gitt/main/install.sh -o "$gitt_installer"
doas env GITT_INSTALL_DIR=/usr/local/bin sh "$gitt_installer"
rm -f "$gitt_installer"
```

Save the OpenRC script as `/etc/init.d/gitt` (mode `755`) and its configuration
as `/etc/conf.d/gitt`. Set `gitt_user` to an existing unprivileged repository
owner, or create the default `gitt` user and `git` group.
Set `gitt_root`, `gitt_bind`, and `gitt_args` for the deployment, then enable it:

```sh
doas rc-update add gitt default
doas rc-service gitt start
```

OpenRC supervises the process and sends its logs to syslog with the tag `gitt`.
For updates, rerun the installer with `GITT_INSTALL_DIR=/usr/local/bin`, then
run `doas rc-service gitt restart`.

Anyone who can reach the configured origin can read and create repositories.
For a reverse proxy, set `--origin` to the external URL and preserve its `Host`
header. Forwarded headers are not used. Configure TLS and authentication at the
proxy or restrict access at the network boundary.

The repository root determines which committed files and history readers can
access, including paths that now match ignore rules. gitt does not run repository
hooks or executable filters, fetch remotes, or follow symlink targets. Landlock
confinement is installed before worker threads start; startup fails when it
cannot be enforced.

## Repository browsing

Discovery searches subdirectories, stops at repositories, and skips symlink
directories. Bare repositories and ordinary checkouts both expose committed
content. Empty repositories show no files until their first commit.

New views start on the HEAD branch. A selected branch or tag keeps its name as
you navigate and resolves when each page loads. An explicitly selected commit
stays fixed. Revision links reload the current page, changing the revision while
preserving its route, file path, filters, and pagination. Tabs, breadcrumbs,
Markdown links, and file-search results retain the selection.

Repository rows show the selected revision, latest commit, author, and commit
age. File rows show the last commit affecting each path, author, size, age, and
mode. Directory history includes descendants. Age comes from Git commit time,
not filesystem timestamps. Missing or bounded history leaves metadata as a dash,
with a notice when lookup is incomplete.

Rows remain on one line and scroll horizontally when necessary. Permissions
appear as `rw- r-- r--`; directories show `-`. Symlinks and submodules identify
their kind. The breadcrumb's **…** menu exposes parent folders.

Markdown is rendered with restricted HTML; embedded images are represented by
their alt text. Source highlighting covers Rust, Python, C, C++, JavaScript,
JSON, HTML, CSS, shell, YAML, Markdown, and SQL. Unknown languages and inputs
that exceed highlighting limits use plain text.

## Resource limits

| Operation | Limit |
| --- | --- |
| Source preview | 1 MiB and 20,000 lines |
| Syntax highlighting | 256 KiB; lines up to 8 KiB; bounded work with plain-text fallback |
| Committed raw file | 8 MiB; larger objects require local Git or SSH |
| Rendered page | 2 MiB |
| Discovery | 10,000 directories, 1,000 repositories, 64 levels |
| List / history page | 200 entries / 50 commits |
| File-list history metadata | 75 ms cooperative lookup budget; up to 512 commits for the visible page |
| Concurrent work | 2 active operations, 4 waiting, 16 HTTP connections |
| Response wait | Up to 2 seconds queued, then 10 seconds for browsing or 30 seconds for creation |

A timeout cannot interrupt an already-running native Git operation; its work
slot remains occupied until it finishes. A timed-out creation may still finish.
Check the repository list before retrying it. The 24 MiB native Git allocation
budget is separate from total process memory usage.

Operations taking at least 500 ms, queue exhaustion, and timeouts are logged
with a request number, route, and timing. Logs omit query parameters, search
text, repository paths, and form tokens.

## HTTP interface

The interface serves HTML and committed raw files. It provides no REST/JSON API
or HTTP Git transport. `GET /new` displays a form; its protected `POST` creates
an empty repository and redirects to it. Existing destinations are never
overwritten. Other application routes are read-only.

Raw-file links are absent from the UI, but the `/raw` route remains available.
Form tokens and query parameters are internal interfaces, not a versioned
automation API.

## Installation

The [installer](../install.sh) downloads the matching Linux release, verifies its
SHA-256 checksum, and installs to `~/.local/bin`. It needs `curl` or `wget`, `tar`,
and `sha256sum`. It does not use sudo or change shell configuration.

To use `gitt` without its full path, add the install directory to your shell's
`PATH`:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

| Environment variable | Default / behavior |
| --- | --- |
| `GITT_INSTALL_DIR` | `$HOME/.local/bin`; override the installation directory |
| `GITT_VERSION` | `latest`; set a release tag such as `v0.2.0` to pin a version |

For example:

```sh
curl -fsSL https://raw.githubusercontent.com/TerryTsai/gitt/main/install.sh | GITT_VERSION=v0.2.0 sh
```

Rerun the installer to update. A failed download or checksum check leaves an
existing installation untouched.

The [GitHub releases](https://github.com/TerryTsai/gitt/releases) also provide
archives for manual installation:

| Platform | Latest archive |
| --- | --- |
| Linux x86-64 | [gitt-linux-x86_64.tar.gz](https://github.com/TerryTsai/gitt/releases/latest/download/gitt-linux-x86_64.tar.gz) |
| Linux ARM64 | [gitt-linux-aarch64.tar.gz](https://github.com/TerryTsai/gitt/releases/latest/download/gitt-linux-aarch64.tar.gz) |

Each release includes `SHA256SUMS`, the executable, and license notices. The musl
executables include templates, styles, syntax grammars, Git support, and
compression support, with no dynamic interpreter or shared-library dependencies.

## Limitations

- Search matches file paths, not file contents.
- Repositories must use SHA-1 objects, loose or packed references, and
  self-contained storage. Linked worktrees, redirected Git directories,
  alternates, SHA-256, and reftable are unsupported.
- Submodule commit IDs and LFS pointer files are displayed; their contents are
  not fetched.
- gitt does not edit, rename, or delete repositories.

## Standalone builds

The source checkout pins Rust 1.95.0. Building needs a C compiler and `make` for
bundled native dependencies. A normal `cargo build --locked --release` produces
a binary for the host target; it does not guarantee static linking.

For a standalone Linux x86-64 executable, install the Rust musl target and a
musl C toolchain, then run:

```sh
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc \
  sh scripts/build-release.sh x86_64-unknown-linux-musl
```

The result is `target/x86_64-unknown-linux-musl/release/gitt`. The script uses
`readelf` to reject executables with a dynamic interpreter or shared-library
dependencies. ARM64 uses `aarch64-unknown-linux-musl`, with `musl-gcc` on an ARM64
host or a matching cross compiler on another architecture.

## Releases

[GitHub Actions](https://github.com/TerryTsai/gitt/actions/workflows/build.yml)
builds Linux x86-64 and ARM64 archives on pushes to `main`, pull requests, and
manual workflow runs. Each native runner checks standalone linking, server
startup, an HTTP response, and installation and update behavior. Build artifacts
are retained for 14 days.

To publish a release, update the package version in `Cargo.toml` and `Cargo.lock`,
then push a matching `vX.Y.Z` tag. The tag workflow publishes both archives and
`SHA256SUMS` after both architecture jobs pass. The installer selects the latest
published release unless `GITT_VERSION` is set.

## Development checks

```sh
cargo test --locked
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
```

Git and Python are required by the fixture and integration-test scripts. Browser
checks additionally use Playwright and Chromium. The [benchmark guide](benchmark-method.md)
includes fixture creation, HTTP checks, and resource measurements;
[browser checks](browser-checks.md) covers viewport and navigation checks.
The [verification record](verification.md) distinguishes recorded results from
checks that have not been repeated for later changes.
