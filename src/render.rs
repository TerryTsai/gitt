//! HTML rendering. Repository bytes become text, never executable markup.

use std::{
    cell::Cell,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    io,
    sync::OnceLock,
    time::{Duration, Instant},
};

use askama::Template;
use percent_encoding::{NON_ALPHANUMERIC, percent_encode};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use syntect::{
    html::{ClassStyle, line_tokens_to_classed_spans},
    parsing::{ParseState, ScopeStack, SyntaxSet},
    util::LinesWithEndings,
};

use crate::types::{AppError, HTML_LIMIT, Result, TEXT_LIMIT};

pub const CSS: &str = include_str!("../static/style.css");
const LINE_LIMIT: usize = 20_000;
const HIGHLIGHT_LIMIT: usize = 256 * 1024;
const HIGHLIGHT_LINE_LIMIT: usize = 8192;
const FRAGMENT_LIMIT: usize = HTML_LIMIT - 64 * 1024;
const HIGHLIGHT_BUDGET: Duration = Duration::from_millis(100);

/// Install the native regex limits before serving requests. Every parser entry
/// point also calls this, so unit tests and concurrent first requests cannot
/// race a write to Oniguruma's process-global defaults.
pub fn initialize() -> Result<()> {
    static INITIALIZATION: OnceLock<std::result::Result<(), (&'static str, i32)>> = OnceLock::new();
    let result = *INITIALIZATION.get_or_init(|| {
        // SAFETY: this OnceLock runs the setters once, before any parser in this
        // module. No other runtime module uses Oniguruma. MatchParam::default
        // subsequently copies these globals without changing them. The stack
        // limit counts entries, not bytes; retry limits count backtracking.
        let limits = unsafe {
            [
                (
                    "match stack",
                    onig_sys::onig_set_match_stack_limit_size(16_384),
                ),
                (
                    "match retries",
                    onig_sys::onig_set_retry_limit_in_match(1_000_000),
                ),
                (
                    "search retries",
                    onig_sys::onig_set_retry_limit_in_search(1_000_000),
                ),
            ]
        };
        for (name, status) in limits {
            if status != 0 {
                return Err((name, status));
            }
        }
        Ok(())
    });
    result.map_err(|(name, status)| {
        AppError::new(
            500,
            format!("The syntax highlighting {name} limit could not be installed (Oniguruma error {status})."),
        )
    })
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Preserve invalid UTF-8 and make control characters visible in filenames.
/// HTML escaping is deliberately separate from byte-preserving display.
pub fn display(mut bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    while !bytes.is_empty() {
        let (valid, invalid) = match std::str::from_utf8(bytes) {
            Ok(text) => (text, 0),
            Err(err) => (
                std::str::from_utf8(&bytes[..err.valid_up_to()]).unwrap_or(""),
                err.error_len().unwrap_or(bytes.len() - err.valid_up_to()),
            ),
        };
        for ch in valid.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control()
                    || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') =>
                {
                    let _ = write!(out, "\\u{{{:x}}}", c as u32);
                }
                _ => out.push(ch),
            }
        }
        bytes = &bytes[valid.len()..];
        for byte in &bytes[..invalid] {
            let _ = write!(out, "\\x{byte:02X}");
        }
        bytes = &bytes[invalid..];
    }
    out
}

/// Build a raw URL. Escape the result when placing it in an HTML attribute.
pub fn url(route: &str, args: &[(&str, &[u8])]) -> String {
    let mut result = route.to_owned();
    for (i, (key, value)) in args.iter().enumerate() {
        result.push(if i == 0 { '?' } else { '&' });
        result.push_str(&percent_encode(key.as_bytes(), NON_ALPHANUMERIC).to_string());
        result.push('=');
        result.push_str(&percent_encode(value, NON_ALPHANUMERIC).to_string());
    }
    result
}

struct Tab {
    label: &'static str,
    href: String,
}

#[derive(Template)]
#[template(path = "layout.html")]
struct Layout<'a> {
    title: &'a str,
    repo_name: String,
    tabs: Vec<Tab>,
    body: &'a str,
}

/// `body` must already be trusted application HTML, with repository text escaped.
pub fn layout(title: &str, repo: Option<&[u8]>, body: &str) -> Result<String> {
    layout_with_revision(title, repo, None, body)
}

/// Keep an explicitly selected snapshot when moving between Files and History.
pub fn layout_with_revision(
    title: &str,
    repo: Option<&[u8]>,
    revision: Option<&[u8]>,
    body: &str,
) -> Result<String> {
    if body.len() > HTML_LIMIT {
        return Err(output_limit());
    }
    let tabs = repo
        .map(|repo| {
            [
                ("Files", "/files"),
                ("Changes", "/changes"),
                ("History", "/history"),
                ("Branches & tags", "/refs"),
                ("Compare", "/compare"),
            ]
            .into_iter()
            .map(|(label, route)| {
                let mut args = vec![("repo", repo)];
                if matches!(route, "/files" | "/history")
                    && let Some(revision) = revision.filter(|revision| !revision.is_empty())
                {
                    args.push(("rev", revision));
                }
                Tab {
                    label,
                    href: url(route, &args),
                }
            })
            .collect()
        })
        .unwrap_or_default();
    let page = Layout {
        title: if title.is_empty() {
            "Repository"
        } else {
            title
        },
        repo_name: repo
            .map(|repo| {
                if repo.is_empty() {
                    ".".to_owned()
                } else {
                    display(repo)
                }
            })
            .unwrap_or_default(),
        tabs,
        body,
    }
    .render()
    .map_err(|_| AppError::new(500, "The page could not be rendered."))?;
    if page.len() > HTML_LIMIT {
        return Err(output_limit());
    }
    Ok(page)
}

#[cfg(test)]
pub fn file_content(path: &[u8], data: &[u8], markdown: bool) -> Result<String> {
    file_content_inner(path, data, markdown, None)
}

/// The revision is absent for a working tree, or the pinned snapshot's revision.
pub fn file_content_with_context(
    path: &[u8],
    data: &[u8],
    markdown: bool,
    repo: &[u8],
    revision: Option<&[u8]>,
) -> Result<String> {
    file_content_inner(path, data, markdown, Some((repo, revision)))
}

fn file_content_inner(
    path: &[u8],
    data: &[u8],
    markdown: bool,
    context: Option<(&[u8], Option<&[u8]>)>,
) -> Result<String> {
    if data.len() > TEXT_LIMIT {
        return Err(AppError::limit(
            "Text previews are limited to 1 MiB. Download the raw file or use local Git.",
        ));
    }
    let text = match std::str::from_utf8(data) {
        Ok(text) if !data.contains(&0) => text,
        _ => return Ok("<p>This file contains binary data or text that is not UTF-8. Download the raw file to inspect it.</p>".to_owned()),
    };
    if text.lines().count() > LINE_LIMIT {
        return Err(AppError::limit(
            "Text previews are limited to 20,000 lines. Download the raw file or use local Git.",
        ));
    }
    if text.is_empty() {
        return Ok("<p>Empty file.</p>".to_owned());
    }
    if markdown {
        return markdown_content(path, text, context);
    }
    if data.len() <= HIGHLIGHT_LIMIT
        && text.lines().all(|line| line.len() <= HIGHLIGHT_LINE_LIMIT)
        && let Some(highlighted) = highlighted_source(path, text)
    {
        return Ok(highlighted);
    }
    plain_source(text)
}

fn plain_source(text: &str) -> Result<String> {
    let mut html = String::with_capacity(text.len().min(FRAGMENT_LIMIT) + 128);
    html.push_str("<p><small>Plain text preview. Highlighting was skipped because of the file type, size, or time limit.</small></p><pre tabindex=\"0\" aria-label=\"File contents\"><code>");
    for (index, line) in LinesWithEndings::from(text).enumerate() {
        line_anchor(&mut html, index + 1);
        for ch in line.chars() {
            match ch {
                '&' => html.push_str("&amp;"),
                '<' => html.push_str("&lt;"),
                '>' => html.push_str("&gt;"),
                '"' => html.push_str("&quot;"),
                '\'' => html.push_str("&#39;"),
                _ => html.push(ch),
            }
            if html.len() > FRAGMENT_LIMIT {
                return Err(output_limit());
            }
        }
    }
    html.push_str("</code></pre>");
    Ok(html)
}

fn line_anchor(html: &mut String, line: usize) {
    let _ = write!(
        html,
        "<a id=\"L{line}\" href=\"#L{line}\" aria-label=\"Line {line}\">{line}</a> "
    );
}

fn highlighted_source(path: &[u8], text: &str) -> Option<String> {
    initialize().ok()?;
    let started = Instant::now();
    let filename = std::str::from_utf8(path).ok()?.rsplit('/').next()?;
    let (dump, name) = syntax_pack(filename)?;
    // Only this language and its declared dependencies are loaded. A full
    // default syntax set caused the musl allocator to retain >20 MiB after
    // browsing. Request-local packs also drop their compiled regex caches.
    let syntaxes: SyntaxSet = syntect::dumps::from_uncompressed_data(dump).ok()?;
    let syntax = syntaxes.find_syntax_by_name(name)?;
    let mut parser = ParseState::new(syntax);
    let mut scopes = ScopeStack::new();
    let mut open_spans = 0isize;
    let mut html = String::with_capacity(text.len().saturating_mul(2).min(FRAGMENT_LIMIT));
    html.push_str("<pre tabindex=\"0\" aria-label=\"File contents\"><code>");
    for (index, line) in LinesWithEndings::from(text).enumerate() {
        if started.elapsed() > HIGHLIGHT_BUDGET {
            return None;
        }
        let operations = parser.parse_line(line, &syntaxes).ok()?;
        let (fragment, delta) =
            line_tokens_to_classed_spans(line, &operations, ClassStyle::Spaced, &mut scopes)
                .ok()?;
        open_spans += delta;
        if !(0..=1024).contains(&open_spans) || html.len() + fragment.len() + 128 > FRAGMENT_LIMIT {
            return None;
        }
        line_anchor(&mut html, index + 1);
        html.push_str(&fragment);
    }
    if started.elapsed() > HIGHLIGHT_BUDGET {
        return None;
    }
    for _ in 0..open_spans {
        html.push_str("</span>");
    }
    html.push_str("</code></pre>");
    if html.len() > FRAGMENT_LIMIT {
        return None;
    }
    Some(html)
}

fn syntax_pack(filename: &str) -> Option<(&'static [u8], &'static str)> {
    macro_rules! pack {
        ($file:literal, $name:literal) => {
            Some((
                include_bytes!(concat!(env!("OUT_DIR"), "/", $file, ".packdump")).as_slice(),
                $name,
            ))
        };
    }
    let filename = filename.to_ascii_lowercase();
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or(&filename);
    match extension {
        "rs" => pack!("rust", "Rust"),
        "py" | "pyw" => pack!("python", "Python"),
        "c" | "h" => pack!("c", "C"),
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => pack!("cpp", "C++"),
        "js" | "jsx" | "mjs" | "cjs" => pack!("javascript", "JavaScript"),
        "json" | "jsonl" => pack!("json", "JSON"),
        "css" => pack!("css", "CSS"),
        "html" | "htm" | "xhtml" => pack!("html", "HTML"),
        "sh" | "bash" | "bashrc" | "bash_profile" | "profile" => {
            pack!("shell", "Bourne Again Shell (bash)")
        }
        "yml" | "yaml" => pack!("yaml", "YAML"),
        "md" | "markdown" | "mdown" => pack!("markdown", "Markdown"),
        "sql" => pack!("sql", "SQL"),
        _ => None,
    }
}

fn markdown_content(
    path: &[u8],
    text: &str,
    context: Option<(&[u8], Option<&[u8]>)>,
) -> Result<String> {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let exceeded = Cell::new(false);
    let mut depth = 0usize;
    let parser = Parser::new_ext(text, options)
        .enumerate()
        .scan((), |_, (index, event)| {
            match &event {
                Event::Start(_) => depth += 1,
                Event::End(_) => depth = depth.saturating_sub(1),
                _ => {}
            }
            if index >= 40_000 || depth > 128 {
                exceeded.set(true);
                return None;
            }
            Some(event)
        })
        .filter_map(|event| match event {
            Event::Html(text) | Event::InlineHtml(text) => Some(Event::Text(text)),
            // Keep alt text, but never request images from the repository or network.
            Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image) => None,
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                let destination =
                    markdown_destination(path, &dest_url, context).unwrap_or_default();
                Some(Event::Start(Tag::Link {
                    link_type,
                    dest_url: destination.into(),
                    title,
                    id,
                }))
            }
            other => Some(other),
        });
    let mut output = LimitedWriter(Vec::new());
    pulldown_cmark::html::write_html_io(&mut output, parser).map_err(|_| output_limit())?;
    if exceeded.get() {
        return Err(AppError::limit(
            "This Markdown document exceeds the rendering complexity limit. Open its source or raw download.",
        ));
    }
    let html = std::str::from_utf8(&output.0)
        .map_err(|_| AppError::new(500, "Markdown could not be rendered."))?;
    let tags: HashSet<_> = [
        "p",
        "br",
        "hr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "blockquote",
        "pre",
        "code",
        "em",
        "strong",
        "del",
        "ul",
        "ol",
        "li",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
        "a",
    ]
    .into_iter()
    .collect();
    let attributes: HashMap<_, HashSet<_>> = [
        ("a", ["href", "title"].into_iter().collect()),
        ("ol", ["start"].into_iter().collect()),
    ]
    .into_iter()
    .collect();
    let cleaned = ammonia::Builder::default()
        .tags(tags)
        .tag_attributes(attributes)
        .generic_attributes(HashSet::new())
        .url_schemes(["http", "https", "mailto"].into_iter().collect())
        .url_relative(ammonia::UrlRelative::PassThrough)
        .link_rel(Some("nofollow noopener noreferrer"))
        .attribute_filter(|element, attribute, value| {
            if element == "a" && attribute == "href" && value.is_empty() {
                None
            } else {
                Some(value.into())
            }
        })
        .clean(html)
        .to_string();
    if cleaned.len() + 32 > FRAGMENT_LIMIT {
        return Err(output_limit());
    }
    Ok(format!("<article>{cleaned}</article>"))
}

fn markdown_destination(
    path: &[u8],
    destination: &str,
    context: Option<(&[u8], Option<&[u8]>)>,
) -> Option<String> {
    if destination.chars().any(char::is_control) || destination.starts_with("//") {
        return None;
    }
    if let Some((scheme, _)) = destination.split_once(':') {
        if matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "mailto"
        ) {
            return Some(destination.to_owned());
        }
        return None;
    }
    if destination.starts_with('#') {
        return Some(destination.to_owned());
    }
    let (repo, revision) = context?;
    let (location, fragment) = destination
        .split_once('#')
        .map_or((destination, None), |(p, f)| (p, Some(f)));
    // Query strings are web resource parameters, not Git file path components.
    // A literal question mark in a filename remains addressable as %3F.
    let location = location.split_once('?').map_or(location, |(p, _)| p);
    let decoded: Vec<u8> = percent_encoding::percent_decode_str(location).collect();
    if decoded.contains(&0) || decoded.iter().any(|b| b.is_ascii_control()) {
        return None;
    }
    let mut segments: Vec<&[u8]> = if decoded.starts_with(b"/") {
        Vec::new()
    } else {
        path.rsplitn(2, |byte| *byte == b'/')
            .nth(1)
            .map(|parent| parent.split(|byte| *byte == b'/').collect())
            .unwrap_or_default()
    };
    for segment in decoded.split(|byte| *byte == b'/') {
        match segment {
            b"" | b"." => {}
            b".." => {
                segments.pop()?;
            }
            b".git" => return None,
            _ => segments.push(segment),
        }
    }
    let target = segments.join(&b'/');
    let mut args = vec![("repo", repo), ("path", target.as_slice())];
    if let Some(revision) = revision {
        args.push(("rev", revision));
    }
    let mut result = url("/files", &args);
    if let Some(fragment) = fragment {
        result.push('#');
        result.push_str(&percent_encode(fragment.as_bytes(), NON_ALPHANUMERIC).to_string());
    }
    Some(result)
}

struct LimitedWriter(Vec<u8>);
impl io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > FRAGMENT_LIMIT {
            return Err(io::Error::other("Rendered HTML exceeds the output limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn output_limit() -> AppError {
    AppError::limit(
        "This preview exceeds the 2 MiB page limit. Open a smaller file or use the raw download.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_bytes_and_urls_are_not_lossy() {
        assert_eq!(display(b"a\xff\n\\\xc3\xa9"), "a\\xFF\\n\\\\é");
        let link = url("/files", &[("repo", b"team/tool"), ("path", b"a +?&\xff")]);
        assert_eq!(link, "/files?repo=team%2Ftool&path=a%20%2B%3F%26%FF");
        let params = crate::types::Params::parse(link.split_once('?').unwrap().1).unwrap();
        assert_eq!(params.bytes("path"), b"a +?&\xff");
    }

    #[test]
    fn layout_escapes_metadata_but_accepts_trusted_body() {
        let html = layout(
            "<img src=x onerror=alert(1)>",
            Some(b"a\"<script>"),
            "<p>Okay</p>",
        )
        .unwrap();
        assert!(!html.contains("<img"));
        assert!(!html.contains("<script"));
        assert!(html.contains("img src=x onerror=alert(1)"));
        assert!(html.contains("<p>Okay</p>"));
        assert!(!html.contains("class="));
        assert!(!html.contains(" id="));
        assert!(html.contains("width=device-width"));
    }

    #[test]
    fn watched_root_repository_keeps_navigation() {
        let html = layout("", Some(b""), "<p>Files</p>").unwrap();
        assert!(html.contains("<h1>Repository</h1>"));
        assert!(html.contains("/history?repo="));
        assert!(html.contains("<code>.</code>"));
    }

    #[test]
    fn snapshot_tabs_preserve_the_selected_revision() {
        let html = layout_with_revision("Files", Some(b"demo"), Some(b"deadbeef"), "").unwrap();
        assert_eq!(html.matches("rev=deadbeef").count(), 2);
    }

    #[test]
    fn embedded_language_packs_link_and_parse() {
        initialize().unwrap();
        for (filename, source) in [
            ("main.rs", "fn main() { println!(\"hello\"); }\n"),
            ("main.py", "def main(): print('hello')\n"),
            ("main.c", "int main(void) { return 0; }\n"),
            ("main.cpp", "template <typename T> class Example {};\n"),
            ("main.js", "const re = /hello/g;\n"),
            ("data.json", "{\"hello\": true}\n"),
            ("style.css", "body { color: red; }\n"),
            ("index.html", "<script>const n = 1;</script>\n"),
            ("run.sh", "echo \"hello\"\n"),
            ("data.yaml", "hello: true\n"),
            ("README.md", "# Hello **world**\n"),
            ("query.sql", "SELECT count(*) FROM things;\n"),
        ] {
            let (dump, name) = syntax_pack(filename).expect("supported language");
            let syntaxes: SyntaxSet = syntect::dumps::from_uncompressed_data(dump).unwrap();
            let syntax = syntaxes.find_syntax_by_name(name).unwrap();
            let mut parser = ParseState::new(syntax);
            assert!(
                !parser.parse_line(source, &syntaxes).unwrap().is_empty(),
                "{filename}"
            );
        }
        assert!(syntax_pack("Cargo.toml").is_none());
        assert!(syntax_pack("unknown.language").is_none());
    }

    #[test]
    fn source_is_escaped_and_has_line_links() {
        let html = file_content(b"unknown-extension", b"<script>\n\"&'\n", false).unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("id=\"L1\""));
        assert!(html.contains("id=\"L2\""));
        assert!(!html.contains("id=\"L3\""));
    }

    #[test]
    fn markdown_cannot_execute_or_load_resources() {
        let source = b"# Readme\n\n<script>alert(1)</script>\n\n<img src='https://example.invalid/track' onerror='alert(1)'>\n\n![diagram](https://example.invalid/a.svg)\n\n[bad](javascript:alert%281%29)\n\n[good](https://example.com)\n\n```rust\nunsafe {}\n```";
        let html = file_content(b"README.md", source, true).unwrap();
        for forbidden in [
            "<script",
            "<img",
            "<iframe",
            "<style",
            " href=\"javascript:",
            " class=",
            " id=",
            " style=",
        ] {
            assert!(!html.contains(forbidden), "unexpected {forbidden}: {html}");
        }
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("diagram"));
        assert!(html.contains("href=\"https://example.com\""));
    }

    #[test]
    fn markdown_relative_links_stay_inside_the_repository() {
        let context = Some((b"team/repo".as_slice(), Some(b"0123".as_slice())));
        assert_eq!(
            markdown_destination(b"docs/README.md", "../src/a%20b.rs#L2", context).unwrap(),
            "/files?repo=team%2Frepo&path=src%2Fa%20b%2Ers&rev=0123#L2"
        );
        assert!(markdown_destination(b"README.md", "../../outside", context).is_none());
        assert!(markdown_destination(b"README.md", "/.git/config", context).is_none());
        assert!(markdown_destination(b"README.md", "//example.com", context).is_none());
        assert!(markdown_destination(b"README.md", "data:text/html,test", context).is_none());
        let html = file_content_with_context(
            b"docs/README.md",
            b"[source](../src/a.rs) [outside](../../other)",
            true,
            b"team/repo",
            Some(b"0123"),
        )
        .unwrap();
        assert!(html.contains("path=src%2Fa%2Ers&amp;rev=0123"));
        assert!(!html.contains("href=\"../../"));
    }

    #[test]
    fn previews_enforce_size_lines_and_binary_behavior() {
        assert_eq!(
            file_content(b"a", &vec![b'a'; TEXT_LIMIT + 1], false)
                .unwrap_err()
                .status,
            422
        );
        assert_eq!(
            file_content(b"a", "a\n".repeat(LINE_LIMIT + 1).as_bytes(), false)
                .unwrap_err()
                .status,
            422
        );
        assert!(
            file_content(b"a", b"a\0b", false)
                .unwrap()
                .contains("binary")
        );
        assert!(
            file_content(b"a", b"\xff", false)
                .unwrap()
                .contains("not UTF-8")
        );
        assert_eq!(
            file_content(b"a", &vec![b'&'; TEXT_LIMIT], false)
                .unwrap_err()
                .status,
            422
        );
    }

    #[test]
    fn deeply_nested_markdown_has_an_explicit_limit() {
        let input = format!("{}nested", "> ".repeat(140));
        let err = file_content(b"README.md", input.as_bytes(), true).unwrap_err();
        assert_eq!(err.status, 422);
        assert!(err.message.contains("complexity"));
    }
}
