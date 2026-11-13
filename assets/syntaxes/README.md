These syntax definitions come from Sublime Text's Packages repository at
`fa6b8629c95041bf262d4c1dab95c456a0530122`, the source snapshot used by syntect
5.3.0. They retain their upstream licenses in `LICENSE` and `Rust/LICENSE.txt`.

Source: https://github.com/sublimehq/Packages/tree/fa6b8629c95041bf262d4c1dab95c456a0530122

`build.rs` reads the vendored YAML only at build time. It produces a small
embedded pack for each supported language, including that language's grammar
dependencies. A file preview loads and drops only the pack it needs. There is
no runtime syntax-file lookup, download, or permanently growing grammar cache.

Highlighting covers Rust, Python, C, C++, JavaScript, JSON, HTML, CSS, shell,
YAML, Markdown source, and SQL. Other files, including TOML, use plain text.
Large, long-line, or expensive inputs also fall back to plain text. Markdown
rendering is separate from syntax highlighting.
