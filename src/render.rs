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
use sha2::{Digest, Sha256};
use syntect::{
    html::{ClassStyle, line_tokens_to_classed_spans},
    parsing::{ParseState, ScopeStack, SyntaxSet},
    util::LinesWithEndings,
};

use crate::types::{AppError, HTML_LIMIT, Page, Result, TEXT_LIMIT};

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
    active: bool,
}

#[derive(Template)]
#[template(path = "layout.html")]
struct Layout<'a> {
    title: &'a str,
    heading: String,
    show_heading: bool,
    show_create: bool,
    breadcrumbs: String,
    stylesheet: &'static str,
    tabs: Vec<Tab>,
    search_url: String,
    actions: &'a str,
    context: &'a str,
    body: &'a str,
}

pub fn revision_text(revision: &[u8]) -> String {
    if revision.len() == 40 && revision.iter().all(u8::is_ascii_hexdigit) {
        return display(&revision[..7]);
    }
    display(
        revision
            .strip_prefix(b"refs/heads/")
            .or_else(|| revision.strip_prefix(b"refs/tags/"))
            .or_else(|| revision.strip_prefix(b"refs/remotes/"))
            .unwrap_or(if revision.is_empty() {
                b"HEAD"
            } else {
                revision
            }),
    )
}

pub fn revision_context(revision: &[u8], shallow: bool) -> String {
    let name = escape(&revision_text(revision));
    let mut title = display(revision);
    if shallow {
        title.push_str(" · shallow repository");
    }
    let text = if revision.len() == 40 && revision.iter().all(u8::is_ascii_hexdigit) {
        format!("<code>{name}</code>")
    } else {
        name
    };
    format!("<span title=\"{}\">{text}</span>", escape(&title))
}

/// Keep the current location visible. Ancestors remain accessible in a native
/// disclosure, with byte-preserving links and no extra mobile header row.
pub fn breadcrumbs(
    repo: &[u8],
    path: &[u8],
    revision: Option<&[u8]>,
    current_suffix: Option<&str>,
) -> String {
    let repo_label = if repo.is_empty() {
        ".".to_owned()
    } else {
        display(repo)
    };
    let mut full_path = repo_label.clone();
    if !path.is_empty() {
        full_path.push('/');
        full_path.push_str(&display(path));
    }
    if let Some(suffix) = current_suffix {
        full_path.push_str(" / ");
        full_path.push_str(suffix);
    }
    let mut items = vec![(repo_label, Vec::new())];
    let mut cumulative = Vec::new();
    for segment in path
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
    {
        if !cumulative.is_empty() {
            cumulative.push(b'/');
        }
        cumulative.extend_from_slice(segment);
        items.push((display(segment), cumulative.clone()));
    }
    let current = current_suffix
        .map(str::to_owned)
        .unwrap_or_else(|| items.pop().expect("repository breadcrumb").0);
    let home = if let Some(revision) = revision.filter(|value| !value.is_empty()) {
        url("/", &[("repo", repo), ("rev", revision)])
    } else {
        "/".to_owned()
    };
    let mut html = format!("<a href=\"{}\">gitt</a>", escape(&home));
    let separator = "<b aria-hidden=\"true\">/</b>";
    let link = |name: &str, path: &[u8]| {
        let mut args = vec![("repo", repo)];
        if !path.is_empty() {
            args.push(("path", path));
        }
        if let Some(revision) = revision.filter(|value| !value.is_empty()) {
            args.push(("rev", revision));
        }
        format!(
            "<a href=\"{}\">{}</a>",
            escape(&url("/files", &args)),
            escape(name)
        )
    };
    if !items.is_empty() {
        html.push_str(separator);
        html.push_str("<details><summary aria-label=\"Parent folders\" title=\"Parent folders\">…</summary><nav aria-label=\"Parent folders\">");
        for (name, path) in &items {
            html.push_str(&link(name, path));
        }
        html.push_str("</nav></details>");
    }
    html.push_str(separator);
    let _ = write!(
        html,
        "<strong title=\"{}\">{}</strong>",
        escape(&full_path),
        escape(&current)
    );
    html
}

/// `body` must already be trusted application HTML, with repository text escaped.
pub fn layout(title: &str, repo: Option<&[u8]>, body: &str) -> Result<String> {
    layout_with_revision(title, repo, None, body)
}

/// Keep the selected reference or commit when moving between repository views.
pub fn layout_with_revision(
    title: &str,
    repo: Option<&[u8]>,
    revision: Option<&[u8]>,
    body: &str,
) -> Result<String> {
    layout_page(&Page::new(title, body), repo, revision)
}

pub fn layout_page(page: &Page, repo: Option<&[u8]>, revision: Option<&[u8]>) -> Result<String> {
    let revision = page.revision.as_deref().or(revision);
    if page
        .body
        .len()
        .saturating_add(page.actions.len())
        .saturating_add(page.context.len())
        > HTML_LIMIT
    {
        return Err(output_limit());
    }
    let tabs = repo
        .map(|repo| {
            [
                ("Files", "/files"),
                ("History", "/history"),
                ("Branches", "/branches"),
                ("Tags", "/tags"),
            ]
            .into_iter()
            .map(|(label, route)| {
                let mut args = vec![("repo", repo)];
                if let Some(revision) = revision.filter(|revision| !revision.is_empty()) {
                    args.push(("rev", revision));
                }
                Tab {
                    label,
                    href: url(route, &args),
                    active: page.active_tab == Some(label),
                }
            })
            .collect()
        })
        .unwrap_or_default();
    let title = if page.title.is_empty() {
        "Repository"
    } else {
        &page.title
    };
    // A new embedded stylesheet must not reuse an earlier build's cached URL.
    static STYLESHEET: OnceLock<String> = OnceLock::new();
    let layout = Layout {
        title,
        heading: page.heading.clone().unwrap_or_else(|| escape(title)),
        show_heading: page.show_heading,
        show_create: page.show_create && repo.is_none(),
        breadcrumbs: page.breadcrumbs.clone().unwrap_or_else(|| match repo {
            Some(repo) => breadcrumbs(repo, b"", revision, None),
            None if page.show_create => "<a href=\"/\">gitt</a>".to_owned(),
            None => format!(
                "<a href=\"/\">gitt</a><b aria-hidden=\"true\">/</b><strong>{}</strong>",
                escape(title)
            ),
        }),
        stylesheet: STYLESHEET
            .get_or_init(|| format!("/style.css?v={:x}", Sha256::digest(CSS.as_bytes()))),
        tabs,
        search_url: match repo {
            Some(repo) => {
                let mut args = vec![("repo", repo)];
                if let Some(revision) = revision.filter(|value| !value.is_empty()) {
                    args.push(("rev", revision));
                }
                url("/search", &args)
            }
            None => "/search".into(),
        },
        actions: &page.actions,
        context: &page.context,
        body: &page.body,
    };
    // Askama's static size hint excludes the source body. Without reserving
    // room for it, appending the closing tags can double a multi-megabyte
    // buffer. Include the dynamic header and a small allowance for markup.
    let mut document = String::with_capacity(
        (layout.body.len()
            + layout.search_url.len() * 6
            + layout.actions.len()
            + layout.context.len()
            + layout.breadcrumbs.len()
            + layout.heading.len()
            + layout.title.len() * 12
            + layout
                .tabs
                .iter()
                .map(|tab| tab.href.len() * 6)
                .sum::<usize>()
            + 4096)
            .min(HTML_LIMIT),
    );
    layout
        .render_into(&mut document)
        .map_err(|_| AppError::new(500, "The page could not be rendered."))?;
    if document.len() > HTML_LIMIT {
        return Err(output_limit());
    }
    Ok(document)
}

#[cfg(test)]
pub fn file_content(path: &[u8], data: &[u8], markdown: bool) -> Result<String> {
    file_content_inner(path, data, markdown, None)
}

/// File navigation retains the selected branch, tag, or commit.
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
            "Text previews are limited to 1 MiB. Use local Git to inspect this file.",
        ));
    }
    let text = match std::str::from_utf8(data) {
        Ok(text) if !data.contains(&0) => text,
        _ => return Ok("<p>This file contains binary data or text that is not UTF-8. Use local Git to inspect it.</p>".to_owned()),
    };
    if text.lines().count() > LINE_LIMIT {
        return Err(AppError::limit(
            "Text previews are limited to 20,000 lines. Use local Git to inspect this file.",
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
    html.push_str("<pre tabindex=\"0\" aria-label=\"File contents\"><code>");
    for (index, line) in LinesWithEndings::from(text).enumerate() {
        source_row(&mut html, index + 1);
        for ch in source_line(line).chars() {
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
        html.push_str("</span></span>");
        if html.len() > FRAGMENT_LIMIT {
            return Err(output_limit());
        }
    }
    html.push_str("</code></pre>");
    Ok(html)
}

fn source_row(html: &mut String, line: usize) {
    let _ = write!(
        html,
        "<span><a id=\"L{line}\" href=\"#L{line}\" aria-label=\"Line {line}\">{line}</a><span>"
    );
}

fn source_line(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .unwrap_or(line)
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
    let mut html = String::with_capacity(text.len().saturating_mul(2).min(FRAGMENT_LIMIT));
    html.push_str("<pre tabindex=\"0\" aria-label=\"File contents\"><code>");
    for (index, line) in LinesWithEndings::from(text).enumerate() {
        if started.elapsed() > HIGHLIGHT_BUDGET {
            return None;
        }
        let mut operations = parser.parse_line(line, &syntaxes).ok()?;
        let visible = source_line(line);
        // The parser still sees line endings, but table rows provide the visual
        // line break. End-of-line scope changes belong at the final text offset.
        for (offset, _) in &mut operations {
            *offset = (*offset).min(visible.len());
        }
        source_row(&mut html, index + 1);
        // Syntect scopes can cross physical lines. Reopen them inside this row
        // and close them at its end so no token span can contain another row.
        if scopes.len() > 1024 {
            return None;
        }
        for scope in scopes.as_slice() {
            let classes = scope.to_string().replace('.', " ");
            let _ = write!(html, "<span class=\"{}\">", escape(&classes));
        }
        let (fragment, delta) =
            line_tokens_to_classed_spans(visible, &operations, ClassStyle::Spaced, &mut scopes)
                .ok()?;
        if !(-1024..=1024).contains(&delta)
            || scopes.len() > 1024
            || html.len() + fragment.len() + scopes.len() * 7 + 32 > FRAGMENT_LIMIT
        {
            return None;
        }
        html.push_str(&fragment);
        for _ in 0..scopes.len() {
            html.push_str("</span>");
        }
        html.push_str("</span></span>");
    }
    if started.elapsed() > HIGHLIGHT_BUDGET {
        return None;
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
            "This Markdown document exceeds the rendering complexity limit. Open its source or use local Git.",
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
        "This preview exceeds the 2 MiB page limit. Open a smaller file or use local Git.",
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
        assert!(html.contains("<strong title=\".\">.</strong>"));
    }

    #[test]
    fn snapshot_tabs_preserve_the_selected_revision() {
        let html = layout_with_revision("Files", Some(b"demo"), Some(b"deadbeef"), "").unwrap();
        assert_eq!(html.matches("rev=deadbeef").count(), 6);
    }

    #[test]
    fn breadcrumb_menu_preserves_bytes_snapshot_and_parent_links() {
        let html = breadcrumbs(
            b"team/repo&\xff",
            b"src/deep/<parent>/a\"\xff.rs",
            Some(b"topic/a&b"),
            None,
        );
        assert!(!html.contains("<parent>"));
        assert!(html.contains("&lt;parent&gt;"));
        assert!(html.contains("a&quot;\\xFF.rs"));
        assert!(html.contains("path=src%2Fdeep%2F%3Cparent%3E&amp;rev=topic%2Fa%26b"));
        let menu_end = html.find("</nav></details>").unwrap();
        let parent = html.rfind("&lt;parent&gt;</a>").unwrap();
        assert!(parent < menu_end);
        assert!(html[menu_end..].contains("<strong"));
        for anchor in html.split("href=\"").skip(2) {
            let href = anchor.split('"').next().unwrap().replace("&amp;", "&");
            let params = crate::types::Params::parse(href.split_once('?').unwrap().1).unwrap();
            assert_eq!(params.bytes("repo"), b"team/repo&\xff");
            assert_eq!(params.bytes("rev"), b"topic/a&b");
        }
    }

    #[test]
    fn page_navigation_reflects_repository_context() {
        let mut page = Page::new("Files", "<p>Contents</p>");
        page.show_heading = false;
        page.active_tab = Some("Files");
        page.show_create = true;
        let html = layout_page(&page, Some(b"demo.git"), None).unwrap();
        assert!(!html.contains("<h1>"));
        assert!(!html.contains("/changes?"));
        assert!(!html.contains("href=\"/new\""));
        assert!(html.contains("/branches?repo=demo%2Egit"));
        assert!(html.contains("/tags?repo=demo%2Egit"));
        assert!(html.contains("aria-current=\"page\">Files</a>"));
        let html = layout_page(&page, None, None).unwrap();
        assert!(html.contains("href=\"/new\">Create</a>"));
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
    fn multiline_highlighting_keeps_line_rows_independent() {
        let html = highlighted_source(b"main.rs", "/* first\r\nsecond */\r\nfn main() {}\n")
            .expect("small Rust source should be highlighted");
        let mut stack = Vec::new();
        let mut anchors = 0;
        for part in html.split('<').skip(1) {
            let tag = part.split('>').next().unwrap();
            if let Some(closing) = tag.strip_prefix('/') {
                assert_eq!(stack.pop(), Some(closing));
            } else {
                let name = tag.split_whitespace().next().unwrap();
                if tag.starts_with("a id=") {
                    assert_eq!(stack, ["pre", "code", "span"]);
                    anchors += 1;
                }
                stack.push(name);
            }
        }
        assert!(stack.is_empty());
        assert_eq!(anchors, 3);
        let second = html
            .split("id=\"L2\"")
            .nth(1)
            .unwrap()
            .split("id=\"L3\"")
            .next()
            .unwrap();
        assert!(second.contains("class=\"comment block"));
        let text: String = second
            .split('>')
            .skip(1)
            .filter_map(|part| part.split('<').next())
            .collect();
        assert!(text.contains("second */"));
        assert!(!html.contains('\r'));
        assert!(!html.contains('\n'));
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
