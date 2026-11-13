# Runtime notices

- `rust-1.95-standard-library.html`: the standard-library copyright inventory shipped with Rust 1.95.0 (`share/doc/rust/COPYRIGHT-library.html`).
- `musl-COPYRIGHT`: the musl copyright notice. The historical v0.1.0 builds used the copy shipped with Zig 0.17.1; GitHub release packages include the build host's musl notice.
- `zig-runtime-LICENSE`: Zig 0.17.1's license inventory, retained for the historical builds that used `zig cc`. GitHub Actions builds use `musl-gcc`.

These files accompany the linked runtime components. The Rust and Zig compiler executables are not distributed with gitt. Cargo dependencies and bundled syntax grammars are listed in `THIRD_PARTY_NOTICES.md` at the project root.
