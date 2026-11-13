//! Read-only Git views. Every URL keeps the original filename bytes.
mod fs;
mod metadata;

use crate::render::{display, escape, file_content_with_context, url};
use crate::security::Root;
use crate::types::{AppError, HTML_LIMIT, Page, Params, RAW_LIMIT, Result, TEXT_LIMIT};
use git2::{ObjectType, Oid, Repository, RepositoryOpenFlags, Tree};
#[cfg(test)]
use std::ffi::OsStr;
use std::fmt::Write as _;
#[cfg(test)]
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const PATH_LIMIT: usize = 10_000;
const PAGE_SIZE: usize = 200;
const HISTORY_SIZE: usize = 50;
const SSH_PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

struct Repo {
    git: Repository,
    relative: Vec<u8>,
}

#[cfg(test)]
fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}
fn label(bytes: &[u8]) -> String {
    escape(&display(bytes))
}
fn a(route: &str, args: &[(&str, &[u8])], text: &str) -> String {
    format!(
        "<a href=\"{}\">{}</a>",
        escape(&url(route, args)),
        escape(text)
    )
}
fn hidden(name: &str, value: &[u8]) -> String {
    match std::str::from_utf8(value) {
        Ok(value) if !value.chars().any(char::is_control) => format!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            escape(name),
            escape(value)
        ),
        _ => {
            let value: String = value.iter().map(|byte| format!("{byte:02x}")).collect();
            format!(
                "<input type=\"hidden\" name=\"{}_hex\" value=\"{}\">",
                escape(name),
                value
            )
        }
    }
}
fn finish(title: impl Into<String>, body: String) -> Result<Page> {
    if body.len() > HTML_LIMIT {
        return Err(AppError::limit(
            "This page exceeds the 2 MiB output limit. Narrow the selection or use local Git.",
        ));
    }
    Ok(Page::new(title, body))
}
fn pagination(body: &mut String, route: &str, q: &Params, current: usize, more: bool) {
    if current == 0 && !more {
        return;
    }
    body.push_str("<nav aria-label=\"Pages\">");
    let base: Vec<(&str, &[u8])> =
        q.0.iter()
            .filter(|(k, _)| k.as_str() != "page")
            .map(|(k, v)| (k.as_str(), v.as_slice()))
            .collect();
    if current > 0 {
        let previous = (current - 1).to_string();
        let mut args = base.clone();
        args.push(("page", previous.as_bytes()));
        body.push_str(&a(route, &args, "Previous page"));
    }
    let _ = write!(body, " <span>Page {}</span> ", current + 1);
    if more {
        let next = (current + 1).to_string();
        let mut args = base;
        args.push(("page", next.as_bytes()));
        body.push_str(&a(route, &args, "Next page"));
    }
    body.push_str("</nav>");
}

fn repository(root: &Root, relative: &[u8]) -> Result<Repo> {
    fs::valid(relative)?;
    root.open(relative, true)?;
    let dotgit = fs::join(relative, b".git");
    let gitdir = match fs::stat(root, &dotgit) {
        Ok(st) if fs::is_dir(&st) => dotgit,
        Ok(_) => {
            return Err(AppError::limit(
                "Linked worktrees, submodules, and redirected .git files are not supported. Open an ordinary checkout or bare repository.",
            ));
        }
        Err(e) if e.status == 404 => relative.to_vec(),
        Err(e) => return Err(e),
    };
    for name in [
        b"commondir".as_slice(),
        b"gitdir",
        b"objects/info/alternates",
        b"objects/info/http-alternates",
    ] {
        match fs::stat(root, &fs::join(&gitdir, name)) {
            Ok(_) => {
                return Err(AppError::limit(
                    "Repositories with external Git directories or alternate object stores are not supported.",
                ));
            }
            Err(e) if e.status == 404 => {}
            Err(e) => return Err(e),
        }
    }
    for name in [b"HEAD".as_slice(), b"config", b"packed-refs", b"shallow"] {
        let full = fs::join(&gitdir, name);
        match fs::stat(root, &full) {
            Ok(st) if !fs::is_file(&st) => {
                return Err(AppError::limit(
                    "Git metadata must be regular files inside this repository.",
                ));
            }
            Ok(st) if st.st_size > RAW_LIMIT as i64 => {
                return Err(AppError::limit(
                    "A Git metadata file exceeds the 8 MiB limit. Use local Git for this repository.",
                ));
            }
            Ok(_) => {}
            Err(e) if e.status == 404 => {}
            Err(e) => return Err(e),
        }
    }
    // NO_SEARCH prevents a supplied directory from opening a containing repository.
    let checked = root.checked_path(relative)?;
    let git = Repository::open_ext(
        &checked,
        RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&Path>(),
    )?;
    let expected_gitdir = root.checked_path(&gitdir)?;
    if git.path() != expected_gitdir || git.commondir() != expected_gitdir {
        return Err(AppError::limit(
            "Redirected Git directories are not supported.",
        ));
    }
    if let Some(workdir) = git.workdir()
        && workdir != checked
    {
        return Err(AppError::limit(
            "A configured external working directory is not supported.",
        ));
    }
    let config = git.config()?;
    if config
        .get_string("extensions.objectformat")
        .is_ok_and(|v| v != "sha1")
        || config
            .get_string("extensions.refstorage")
            .is_ok_and(|v| v != "files")
    {
        return Err(AppError::limit(
            "This repository uses an unsupported object or reference format. gitt supports SHA-1 repositories with ordinary loose or packed references.",
        ));
    }
    Ok(Repo {
        git,
        relative: relative.to_vec(),
    })
}

fn object_size(repo: &Repository, id: Oid, limit: usize) -> Result<ObjectType> {
    let (size, kind) = repo.odb()?.read_header(id)?;
    if size > limit {
        return Err(AppError::limit(format!(
            "This Git object exceeds the {} byte limit. Use SSH or local Git to read it.",
            limit
        )));
    }
    Ok(kind)
}
fn blob(repo: &Repository, id: Oid, limit: usize) -> Result<Vec<u8>> {
    if object_size(repo, id, limit)? != ObjectType::Blob {
        return Err(AppError::limit("This entry is not a file."));
    }
    Ok(repo.find_blob(id)?.content().to_vec())
}
fn commit<'a>(repo: &'a Repository, revision: &[u8]) -> Result<git2::Commit<'a>> {
    let revision = if revision.is_empty() {
        b"HEAD"
    } else {
        revision
    };
    if revision.len() > 1024 || revision.contains(&0) {
        return Err(AppError::bad("Invalid revision."));
    }
    let mut id = if revision.len() == 40 && revision.iter().all(u8::is_ascii_hexdigit) {
        Oid::from_str(std::str::from_utf8(revision).expect("hex object ID"))?
    } else if let Ok(name) = std::str::from_utf8(revision) {
        repo.resolve_reference_from_short_name(name)?
            .resolve()?
            .target()
            .ok_or_else(|| AppError::limit("This reference has no object target."))?
    } else {
        // Ref names are bytes. A bounded scan also preserves non-UTF-8 names
        // without replacing a selected branch or tag with its object ID.
        let mut target = None;
        for (index, reference) in repo.references()?.enumerate() {
            if index >= PATH_LIMIT {
                return Err(AppError::limit(
                    "Reference lookup exceeds the 10,000 reference limit.",
                ));
            }
            let reference = reference?;
            if reference.name_bytes() == revision {
                target = reference.resolve()?.target();
                break;
            }
        }
        target.ok_or_else(|| AppError::new(404, "Reference not found."))?
    };
    for _ in 0..16 {
        match object_size(repo, id, RAW_LIMIT)? {
            ObjectType::Tag => id = repo.find_tag(id)?.target_id(),
            ObjectType::Commit => {
                let commit = repo.find_commit(id)?;
                object_size(repo, commit.tree_id(), RAW_LIMIT)?;
                return Ok(commit);
            }
            _ => {
                return Err(AppError::limit(
                    "Choose a branch, tag, or full commit ID that names a commit.",
                ));
            }
        }
    }
    Err(AppError::limit(
        "This tag chain exceeds the 16 object limit.",
    ))
}
fn head_commit(repo: &Repository) -> Result<Option<git2::Commit<'_>>> {
    match repo.head() {
        Ok(reference) => {
            let id = reference
                .target()
                .ok_or_else(|| AppError::limit("HEAD has no commit target."))?;
            Ok(Some(commit(repo, id.to_string().as_bytes())?))
        }
        Err(e) if e.code() == git2::ErrorCode::UnbornBranch => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn tree_entry(
    repo: &Repository,
    tree: &Tree<'_>,
    relative: &[u8],
) -> Result<git2::TreeEntry<'static>> {
    fs::valid(relative)?;
    let mut current = repo.find_tree(tree.id())?;
    let parts: Vec<&[u8]> = relative.split(|b| *b == b'/').collect();
    for (i, name) in parts.iter().enumerate() {
        let entry = current
            .get_name_bytes(name)
            .ok_or_else(|| AppError::new(404, "This path does not exist in this snapshot."))?
            .to_owned();
        if i + 1 == parts.len() {
            return Ok(entry);
        }
        if entry.kind() != Some(ObjectType::Tree) {
            return Err(AppError::new(404, "This path is not a directory."));
        }
        object_size(repo, entry.id(), RAW_LIMIT)?;
        current = repo.find_tree(entry.id())?;
    }
    Err(AppError::new(404, "File not found."))
}

fn selection(repo: &Repository, revision: &[u8]) -> Result<Vec<u8>> {
    if !revision.is_empty() {
        return Ok(revision.to_vec());
    }
    let head = repo.find_reference("HEAD")?;
    if let Some(name) = head.symbolic_target_bytes() {
        Ok(name.to_vec())
    } else {
        head.target()
            .map(|id| id.to_string().into_bytes())
            .ok_or_else(|| AppError::limit("HEAD has no commit target."))
    }
}

fn selected_commit<'a>(repo: &'a Repository, revision: &[u8]) -> Result<Option<git2::Commit<'a>>> {
    match commit(repo, revision) {
        Ok(commit) => Ok(Some(commit)),
        Err(_)
            if repo
                .head()
                .is_err_and(|e| e.code() == git2::ErrorCode::UnbornBranch)
                && (revision == b"HEAD" || revision == selection(repo, b"")?) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn revision_link(route: &str, q: &Params, revision: &[u8], text: &str) -> String {
    let mut selected = q.clone();
    selected.0.insert("rev".into(), revision.to_vec());
    let args: Vec<_> = selected
        .0
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_slice()))
        .collect();
    a(route, &args, text)
}

#[derive(Clone)]
struct Found {
    relative: Vec<u8>,
}

fn discover(root: &Root) -> Result<(Vec<Found>, bool)> {
    let mut found = Vec::new();
    let mut pending = vec![Vec::new()];
    let mut scanned = 0;
    let mut incomplete = false;
    while let Some(directory) = pending.pop() {
        scanned += 1;
        if scanned > PATH_LIMIT || found.len() >= 1000 {
            incomplete = true;
            break;
        }
        if directory.split(|b| *b == b'/').count() > 64 {
            incomplete = true;
            continue;
        }
        let dotgit = fs::join(&directory, b".git");
        if fs::stat(root, &dotgit).is_ok() {
            found.push(Found {
                relative: directory,
            });
            continue;
        }
        if fs::stat(root, &fs::join(&directory, b"HEAD")).is_ok()
            && fs::stat(root, &fs::join(&directory, b"objects")).is_ok_and(|st| fs::is_dir(&st))
        {
            found.push(Found {
                relative: directory,
            });
            continue;
        }
        let handle = match root.open(&directory, true) {
            Ok(f) => f,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        let listing = fs::entries(&handle, PATH_LIMIT)?;
        incomplete |= listing.incomplete;
        for name in listing.names.into_iter().rev() {
            if name == b".git" || name.starts_with(b".gitt-create-") {
                continue;
            }
            let full = fs::join(&directory, &name);
            if fs::stat(root, &full).is_ok_and(|st| fs::is_dir(&st)) {
                pending.push(full);
            }
            if pending.len() > PATH_LIMIT {
                incomplete = true;
                break;
            }
        }
    }
    found.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok((found, incomplete))
}

fn repository_metadata(
    root: &Root,
    relative: &[u8],
    revision: &[u8],
) -> Result<(Vec<u8>, Option<metadata::LastChange>)> {
    let repo = repository(root, relative)?;
    let selected = selection(&repo.git, revision)?;
    let Some(snapshot) = selected_commit(&repo.git, &selected)? else {
        return Ok((selected, None));
    };
    let id = snapshot.id();
    // Only inspect the visible repository's selected commit, without walking history
    // or scanning its worktree. Keep oversized/missing metadata local to its row.
    object_size(&repo.git, id, TEXT_LIMIT)?;
    let commit = repo.git.find_commit(id)?;
    let change = metadata::LastChange {
        commit: id,
        timestamp: commit.time().seconds(),
        author: metadata::short_text(commit.author().name_bytes(), 128),
        message: metadata::short_text(
            commit
                .message_bytes()
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or(b""),
            512,
        ),
    };
    Ok((selected, Some(change)))
}

fn repositories(root: &Root, q: &Params) -> Result<Page> {
    let (found, incomplete) = discover(root)?;
    repository_list(root, q, &found, incomplete, "/")
}

fn repository_list(
    root: &Root,
    q: &Params,
    found: &[Found],
    incomplete: bool,
    route: &str,
) -> Result<Page> {
    let filter = q.text("q")?.to_lowercase();
    let found: Vec<_> = found
        .iter()
        .filter(|r| display(&r.relative).to_lowercase().contains(&filter))
        .collect();
    let current = q.page()?;
    let mut body = String::new();
    if incomplete {
        body.push_str("<p><strong>Incomplete scan.</strong> Discovery stops after 10,000 directories, 1,000 repositories, or an unreadable directory. Narrow the watched root to see everything.</p>");
    }
    if found.is_empty() {
        body.push_str("<p>No repositories found.</p>");
    } else {
        body.push_str("<div role=\"region\" aria-label=\"Repository list\" tabindex=\"0\"><table aria-label=\"Projects\"><thead><tr><th scope=\"col\">Name</th><th scope=\"col\">Last commit</th><th scope=\"col\">Author</th><th scope=\"col\">Age</th><th scope=\"col\">Revision</th></tr></thead><tbody>");
        for item in found.iter().skip(current * PAGE_SIZE).take(PAGE_SIZE) {
            let name = if item.relative.is_empty() {
                ".".to_owned()
            } else {
                display(&item.relative)
            };
            let selected = q.0.contains_key("repo") && q.bytes("repo") == item.relative;
            let revision = if selected { q.bytes("rev") } else { b"" };
            let mut args = vec![("repo", item.relative.as_slice())];
            if !revision.is_empty() {
                args.push(("rev", revision));
            }
            let mut row = q.clone();
            row.0.insert("repo".into(), item.relative.clone());
            let _ = write!(body, "<tr><td>{}</td>", a("/files", &args, &name));
            match repository_metadata(root, &item.relative, revision) {
                Ok((selected, change)) => {
                    if let Some(change) = change {
                        let stamp = date(change.timestamp);
                        let iso = stamp.trim_end_matches(" UTC").replacen(' ', "T", 1) + "Z";
                        let _ = write!(
                            body,
                            "<td>{}</td><td>{}</td><td><time datetime=\"{}\" title=\"{}\">{}</time></td>",
                            revision_link(
                                route,
                                &row,
                                change.commit.to_string().as_bytes(),
                                &change.message
                            ),
                            escape(&change.author),
                            escape(&iso),
                            escape(&stamp),
                            age(change.timestamp)
                        );
                    } else {
                        body.push_str("<td>—</td><td>—</td><td>—</td>");
                    }
                    let _ = write!(
                        body,
                        "<td>{}</td>",
                        revision_link(
                            route,
                            &row,
                            &selected,
                            &crate::render::revision_text(&selected)
                        )
                    );
                }
                Err(_) => body.push_str("<td>—</td><td>—</td><td>—</td><td>—</td>"),
            }
            body.push_str("</tr>");
        }
        body.push_str("</tbody></table></div>");
    }
    pagination(
        &mut body,
        route,
        q,
        current,
        found.len() > (current + 1) * PAGE_SIZE,
    );
    let mut page = finish("Repositories", body)?;
    page.show_heading = false;
    page.show_create = true;
    Ok(page)
}

fn search_scope(repo: &[u8]) -> String {
    let mut scope = String::from("repo:");
    for byte in repo {
        let _ = write!(scope, "{byte:02x}");
    }
    scope
}

// Normalize native search form submissions once, so every result link can
// change only rev without carrying stale form state for a previous repository.
pub fn search_params(q: &Params) -> Result<Params> {
    let mut result = q.clone();
    if q.0.contains_key("scope") {
        if q.bytes("scope") == b"repositories" {
            if !q.0.contains_key("repo") {
                result.0.remove("scope");
                result.0.remove("rev");
            }
        } else {
            let repo = search_repository(q)?.expect("repository scope");
            if q.bytes("snapshot_scope") != search_scope(&repo).as_bytes() {
                result.0.remove("rev");
            }
            result.0.insert("repo".into(), repo);
            result.0.remove("scope");
        }
        result.0.remove("snapshot_scope");
    }
    Ok(result)
}

fn search_repository(q: &Params) -> Result<Option<Vec<u8>>> {
    let selected = if q.0.contains_key("scope") {
        match q.text("scope")? {
            "repositories" => None,
            scope => {
                let encoded = scope
                    .strip_prefix("repo:")
                    .ok_or_else(|| AppError::bad("Invalid search scope."))?;
                if !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(AppError::bad("Invalid search scope."));
                }
                Some(
                    Params::parse(&format!("repo_hex={encoded}"))?
                        .bytes("repo")
                        .to_vec(),
                )
            }
        }
    } else {
        q.0.get("repo").cloned()
    };
    Ok(selected)
}

fn search(root: &Root, q: &Params) -> Result<Page> {
    let selected = search_repository(q)?;
    let (found, incomplete) = discover(root)?;
    let scope = selected
        .as_deref()
        .map(search_scope)
        .unwrap_or_else(|| "repositories".into());
    let query = q.text("q")?;
    let mut params = q.clone();
    // Keep a selected snapshot across searches, but use HEAD when the user
    // switches repositories in the same form. A commit belongs to its scope.
    let mut revision =
        if !q.0.contains_key("scope") || q.bytes("snapshot_scope") == scope.as_bytes() {
            q.bytes("rev").to_vec()
        } else {
            Vec::new()
        };
    let mut results = String::new();
    if let Some(relative) = &selected {
        let repo = repository(root, relative)?;
        revision = selection(&repo.git, &revision)?;
        if q.0.contains_key("q") {
            params.0.insert("repo".into(), relative.clone());
            params.0.insert("rev".into(), revision.clone());
            let page = files(&repo, &params, None, "/search")?;
            revision = page.revision.unwrap_or_default();
            results = page.body;
        }
    } else if q.0.contains_key("q") {
        params.0.insert("scope".into(), b"repositories".to_vec());
        results = repository_list(root, &params, &found, incomplete, "/search")?.body;
    }
    let mut body = String::from(
        "<form role=\"search\" method=\"get\" action=\"/search\"><label>Search in<select aria-label=\"Search in\" name=\"scope\">",
    );
    let _ = write!(
        body,
        "<option value=\"repositories\"{}>Repository names</option>",
        if selected.is_none() { " selected" } else { "" }
    );
    let option = |relative: &[u8]| {
        format!(
            "<option value=\"{}\"{}>{}</option>",
            search_scope(relative),
            if selected.as_deref() == Some(relative) {
                " selected"
            } else {
                ""
            },
            if relative.is_empty() {
                ".".into()
            } else {
                escape(&display(relative))
            }
        )
    };
    for item in &found {
        body.push_str(&option(&item.relative));
    }
    if let Some(relative) = &selected
        && !found.iter().any(|item| item.relative == *relative)
    {
        body.push_str(&option(relative));
    }
    body.push_str("</select></label>");
    if selected.is_some() && !revision.is_empty() {
        body.push_str(&hidden("rev", &revision));
        body.push_str(&hidden("snapshot_scope", scope.as_bytes()));
    }
    let _ = write!(
        body,
        "<input aria-label=\"Search query\" placeholder=\"Name or file path\" type=\"search\" name=\"q\" value=\"{}\" autofocus><button type=\"submit\">Search</button></form>",
        escape(query)
    );
    if incomplete && (selected.is_some() || !q.0.contains_key("q")) {
        body.push_str("<p>The repository list is incomplete. Narrow the watched root to see all repositories.</p>");
    }
    body.push_str(&results);
    let mut page = finish("Search", body)?;
    page.show_heading = false;
    Ok(page)
}

fn clone_url(repo: &Repo, ssh: Option<&str>) -> String {
    let Some(prefix) = ssh else {
        return String::new();
    };
    let suffix = percent_encoding::percent_encode(&repo.relative, SSH_PATH).to_string();
    format!(
        "<p aria-label=\"SSH clone URL\"><code>{}{}</code></p>",
        escape(prefix),
        escape(&suffix)
    )
}

struct FileEntry {
    path: Vec<u8>,
    directory: bool,
    mode: u32,
    object: Option<Oid>,
}

fn mode_label(mode: u32) -> String {
    match mode {
        0 => "absent".into(),
        0o040000 => "-".into(),
        0o120000 => "symlink".into(),
        0o160000 => "submodule".into(),
        _ => [6, 3, 0]
            .map(|shift| {
                [(4, 'r'), (2, 'w'), (1, 'x')]
                    .map(|(bit, letter)| {
                        if mode >> shift & bit != 0 {
                            letter
                        } else {
                            '-'
                        }
                    })
                    .iter()
                    .collect::<String>()
            })
            .join(" "),
    }
}

fn size_label(size: Option<u64>) -> String {
    match size {
        None => "—".into(),
        Some(n) if n < 1024 => format!("{n} B"),
        Some(n) if n < 10 * 1024 => format!("{:.1}K", n as f64 / 1024.0),
        Some(n) if n < 1024 * 1024 => format!("{:.0}K", n as f64 / 1024.0),
        Some(n) if n < 1024 * 1024 * 1024 => format!("{:.1}M", n as f64 / (1024.0 * 1024.0)),
        Some(n) => format!("{:.1}G", n as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}

fn age(seconds: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let elapsed = now.saturating_sub(seconds).max(0);
    if elapsed < 3600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3600)
    } else if elapsed < 14 * 86_400 {
        format!("{}d", elapsed / 86_400)
    } else if elapsed < 60 * 86_400 {
        format!("{}w", elapsed / (7 * 86_400))
    } else if elapsed < 730 * 86_400 {
        format!("{}mo", elapsed / (30 * 86_400))
    } else {
        format!("{}y", elapsed / (365 * 86_400))
    }
}

fn list_tree(
    repo: &Repository,
    tree: &Tree<'_>,
    relative: &[u8],
    query: &str,
) -> Result<(Vec<FileEntry>, bool)> {
    let mut result = Vec::new();
    let mut pending = vec![(relative.to_vec(), tree.id())];
    let mut scanned = 0;
    let mut incomplete = false;
    while let Some((directory, id)) = pending.pop() {
        object_size(repo, id, RAW_LIMIT)?;
        let tree = repo.find_tree(id)?;
        for entry in tree.iter() {
            scanned += 1;
            if scanned > PATH_LIMIT {
                incomplete = true;
                break;
            }
            let full = fs::join(&directory, entry.name_bytes());
            let is_directory = entry.kind() == Some(ObjectType::Tree);
            if !query.is_empty() && is_directory {
                pending.push((full.clone(), entry.id()));
            }
            if !query.is_empty() && !display(&full).to_lowercase().contains(query) {
                continue;
            }
            result.push(FileEntry {
                path: full,
                directory: is_directory,
                mode: entry.filemode() as u32,
                object: if entry.kind() == Some(ObjectType::Blob) {
                    Some(entry.id())
                } else {
                    None
                },
            });
        }
        if scanned > PATH_LIMIT {
            break;
        }
    }
    result.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.path.cmp(&b.path)));
    Ok((result, incomplete))
}

fn directory_html(
    body: &mut String,
    repo: &Repo,
    navigation: (&str, &Params),
    revision: &[u8],
    entries: Vec<FileEntry>,
    incomplete: bool,
    tip: Oid,
) -> Result<()> {
    let (route, q) = navigation;
    let relative = q.bytes("path");
    if incomplete {
        body.push_str("<p><strong>Incomplete listing:</strong> the 10,000 path limit was reached or a path became unreadable.</p>");
    }
    if entries.is_empty() {
        body.push_str("<p>No matching files.</p>");
    }
    let current = q.page()?;
    let visible: Vec<_> = entries
        .iter()
        .skip(current * PAGE_SIZE)
        .take(PAGE_SIZE)
        .collect();
    let paths: Vec<_> = visible.iter().map(|entry| entry.path.clone()).collect();
    let changes = metadata::last_changes(&repo.git, tip, &paths)?;
    let mut size_unavailable = false;
    body.push_str("<div role=\"region\" aria-label=\"File list\" tabindex=\"0\"><table aria-label=\"Files\"><thead><tr><th scope=\"col\">Name</th><th scope=\"col\">Last commit</th><th scope=\"col\">Author</th><th scope=\"col\">Size</th><th scope=\"col\">Age</th><th scope=\"col\">Mode</th></tr></thead><tbody>");
    for entry in visible {
        let name = entry
            .path
            .strip_prefix(relative)
            .unwrap_or(&entry.path)
            .strip_prefix(b"/")
            .unwrap_or(entry.path.strip_prefix(relative).unwrap_or(&entry.path));
        let mut text = display(name);
        if entry.directory {
            text.push('/');
        }
        let href = url(
            "/files",
            &[
                ("repo", &repo.relative),
                ("path", &entry.path),
                ("rev", revision),
            ],
        );
        let change = changes.entries.get(&entry.path);
        let mut size = None;
        if let Some(id) = entry.object {
            match repo.git.odb()?.read_header(id) {
                Ok((n, ObjectType::Blob)) => size = Some(n as u64),
                _ => size_unavailable = true,
            }
        }
        let _ = write!(
            body,
            "<tr><td><a href=\"{}\"><span>{}</span></a></td>",
            escape(&href),
            escape(&text)
        );
        if let Some(m) = change {
            let stamp = date(m.timestamp);
            let iso = stamp.trim_end_matches(" UTC").replacen(' ', "T", 1) + "Z";
            let _ = write!(
                body,
                "<td>{}</td><td>{}</td><td>{}</td><td><time datetime=\"{}\" title=\"{}\">{}</time></td>",
                revision_link(route, q, m.commit.to_string().as_bytes(), &m.message),
                escape(&m.author),
                size_label(size),
                escape(&iso),
                escape(&stamp),
                age(m.timestamp)
            );
        } else {
            let _ = write!(
                body,
                "<td>—</td><td>—</td><td>{}</td><td>—</td>",
                size_label(size)
            );
        }
        let _ = write!(body, "<td>{}</td></tr>", mode_label(entry.mode));
    }
    body.push_str("</tbody></table></div>");
    if changes.incomplete {
        body.push_str("<p><small>Some last-change metadata is unavailable within the history limit. Open History to continue.</small></p>");
    }
    if size_unavailable {
        body.push_str("<p><small>Some file sizes could not be read.</small></p>");
    }
    let mut stable = q.clone();
    if !revision.is_empty() {
        stable.0.insert("rev".into(), revision.to_vec());
    }
    pagination(
        body,
        route,
        &stable,
        current,
        entries.len() > (current + 1) * PAGE_SIZE,
    );
    Ok(())
}

fn files(repo: &Repo, q: &Params, ssh: Option<&str>, list_route: &str) -> Result<Page> {
    let relative = q.bytes("path");
    fs::valid(relative)?;
    let mut body = String::new();
    let revision = selection(&repo.git, q.bytes("rev"))?;
    let snapshot = selected_commit(&repo.git, &revision)?;
    let Some(snapshot) = snapshot else {
        if !relative.is_empty() {
            return Err(AppError::new(
                404,
                "This repository has no committed files yet.",
            ));
        }
        body.push_str("<p>No commits yet.</p>");
        body.push_str(&clone_url(repo, ssh));
        let mut page = finish(display(&repo.relative), body)?;
        page.revision = Some(revision);
        return Ok(page);
    };
    let tree = snapshot.tree()?;
    let query = q.text("q")?.to_lowercase();
    let mut content = None;
    if relative.is_empty() {
        let (entries, incomplete) = list_tree(&repo.git, &tree, b"", &query)?;
        directory_html(
            &mut body,
            repo,
            (list_route, q),
            &revision,
            entries,
            incomplete,
            snapshot.id(),
        )?;
    } else {
        let entry = tree_entry(&repo.git, &tree, relative)?;
        match entry.filemode() {
            0o040000 => {
                object_size(&repo.git, entry.id(), RAW_LIMIT)?;
                let child = repo.git.find_tree(entry.id())?;
                let (entries, incomplete) = list_tree(&repo.git, &child, relative, &query)?;
                directory_html(
                    &mut body,
                    repo,
                    (list_route, q),
                    &revision,
                    entries,
                    incomplete,
                    snapshot.id(),
                )?;
            }
            0o160000 => {
                let _ = write!(
                    body,
                    "<p>Submodule at commit <code>{}</code>. Submodule contents are not browsed.</p>",
                    entry.id()
                );
            }
            0o120000 => {
                let _ = write!(
                    body,
                    "<p>Symbolic link (never followed): <code>{}</code></p>",
                    label(&blob(&repo.git, entry.id(), TEXT_LIMIT)?)
                );
            }
            _ => {
                content = Some(blob(&repo.git, entry.id(), TEXT_LIMIT)?);
            }
        }
    }
    let mut actions = String::new();
    if let Some(bytes) = content {
        let args = [
            ("repo", repo.relative.as_slice()),
            ("path", relative),
            ("rev", revision.as_slice()),
        ];
        let markdown = is_markdown(relative);
        if markdown {
            let mut source = args.to_vec();
            source.push(("source", if q.flag("source") { b"0" } else { b"1" }));
            actions.push_str(&a(
                "/files",
                &source,
                if q.flag("source") {
                    "Markdown"
                } else {
                    "View source"
                },
            ));
        }
        body.push_str(&file_content_with_context(
            relative,
            &bytes,
            markdown && !q.flag("source"),
            &repo.relative,
            Some(&revision),
        )?);
    }
    if relative.is_empty() {
        body.push_str(&clone_url(repo, ssh));
    }
    let mut page = finish(
        if relative.is_empty() {
            display(&repo.relative)
        } else {
            format!("{} — {}", display(relative), display(&repo.relative))
        },
        body,
    )?;
    page.breadcrumbs = Some(crate::render::breadcrumbs(
        &repo.relative,
        relative,
        Some(&revision),
        None,
    ));
    page.show_heading = false;
    page.actions = actions;
    page.revision = Some(revision);
    Ok(page)
}
fn is_markdown(path: &[u8]) -> bool {
    let text = display(path).to_ascii_lowercase();
    text.ends_with(".md") || text.ends_with(".markdown") || text.ends_with(".mdown")
}

pub fn raw(root: &Root, q: &Params) -> Result<Vec<u8>> {
    let repo = repository(root, q.bytes("repo"))?;
    let relative = q.bytes("path");
    fs::valid(relative)?;
    if relative.is_empty() {
        return Err(AppError::bad("A file path is required."));
    }
    let snapshot = if q.bytes("rev").is_empty() {
        head_commit(&repo.git)?
            .ok_or_else(|| AppError::new(404, "This repository has no committed files yet."))?
    } else {
        commit(&repo.git, q.bytes("rev"))?
    };
    let entry = tree_entry(&repo.git, &snapshot.tree()?, relative)?;
    if entry.filemode() == 0o120000 || entry.filemode() == 0o160000 {
        return Err(AppError::limit(
            "Symbolic links and submodules cannot be downloaded as files.",
        ));
    }
    blob(&repo.git, entry.id(), RAW_LIMIT)
}

fn refs(repo: &Repo, q: &Params, route: &str, tags: bool) -> Result<Page> {
    let name = if tags { "Tags" } else { "Branches" };
    let mut body = String::new();
    let mut items = Vec::new();
    let mut incomplete = false;
    let mut scanned = 0;
    for reference in repo.git.references()? {
        let reference = reference?;
        scanned += 1;
        if scanned > PATH_LIMIT {
            incomplete = true;
            break;
        }
        if if tags {
            !reference.is_tag()
        } else {
            !reference.is_branch() && !reference.is_remote()
        } {
            continue;
        }
        let reference_name = reference.name_bytes().to_vec();
        let id = reference.resolve()?.target();
        items.push((reference_name, id));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    if incomplete {
        body.push_str("<p><strong>Incomplete reference list:</strong> the 10,000 reference limit was reached.</p>");
    }
    if items.is_empty() {
        let _ = write!(body, "<p>No {} yet.</p>", name.to_lowercase());
    }
    body.push_str("<div role=\"region\" aria-label=\"Reference list\" tabindex=\"0\"><table aria-label=\"References\"><thead><tr><th scope=\"col\">Name</th><th scope=\"col\">Commit</th><th scope=\"col\">Author</th><th scope=\"col\">Age</th></tr></thead><tbody>");
    let current = q.page()?;
    for (reference_name, id) in items.iter().skip(current * PAGE_SIZE).take(PAGE_SIZE) {
        let shown = reference_name
            .strip_prefix(b"refs/heads/")
            .or_else(|| reference_name.strip_prefix(b"refs/tags/"))
            .or_else(|| reference_name.strip_prefix(b"refs/remotes/"))
            .unwrap_or(reference_name);
        let revision = id.map(|v| v.to_string()).unwrap_or_default();
        let _ = write!(
            body,
            "<tr><td>{}</td>",
            revision_link(route, q, reference_name, &metadata::short_text(shown, 1024))
        );
        if let Ok(c) = commit(&repo.git, revision.as_bytes()) {
            let stamp = date(c.time().seconds());
            let iso = stamp.trim_end_matches(" UTC").replacen(' ', "T", 1) + "Z";
            let _ = write!(
                body,
                "<td><code>{}</code></td><td>{}</td><td><time datetime=\"{}\" title=\"{}\">{}</time></td></tr>",
                revision_link(
                    route,
                    q,
                    c.id().to_string().as_bytes(),
                    &c.id().to_string()[..7]
                ),
                escape(&metadata::short_text(c.author().name_bytes(), 128)),
                escape(&iso),
                escape(&stamp),
                age(c.time().seconds())
            );
        } else {
            body.push_str("<td>—</td><td>—</td><td>—</td></tr>");
        }
        if body.len() > HTML_LIMIT - 65536 {
            return Err(AppError::limit(
                "Reference metadata exceeds the page output limit.",
            ));
        }
    }
    body.push_str("</tbody></table></div>");
    pagination(
        &mut body,
        route,
        q,
        current,
        items.len() > (current + 1) * PAGE_SIZE,
    );
    finish(format!("{name} — {}", display(&repo.relative)), body)
}

fn date(seconds: i64) -> String {
    // Gregorian civil date conversion, independent of host timezone and locale.
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    let time = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        time / 3600,
        (time % 3600) / 60
    )
}

fn history(repo: &Repo, q: &Params, _ssh: Option<&str>) -> Result<Page> {
    let mut body = String::new();
    let revision = selection(&repo.git, q.bytes("rev"))?;
    let Some(start) = selected_commit(&repo.git, &revision)? else {
        body.push_str("<p>No commits yet.</p>");
        return finish(format!("History — {}", display(&repo.relative)), body);
    };
    let current = q.page()?;
    if current >= PATH_LIMIT / HISTORY_SIZE {
        return Err(AppError::limit(
            "History browsing stops after 10,000 commits. Select an older full commit ID to continue.",
        ));
    }
    let mut stable = q.clone();
    stable.0.insert("rev".into(), revision.clone());
    let mut walk = repo.git.revwalk()?;
    walk.set_sorting(git2::Sort::TIME)?;
    walk.push(start.id())?;
    let mut more = false;
    body.push_str("<div role=\"region\" aria-label=\"Commit history\" tabindex=\"0\"><table aria-label=\"History\"><thead><tr><th scope=\"col\">Message</th><th scope=\"col\">Commit</th><th scope=\"col\">Author</th><th scope=\"col\">Date</th></tr></thead><tbody>");
    for (n, id) in walk
        .skip(current * HISTORY_SIZE)
        .take(HISTORY_SIZE + 1)
        .enumerate()
    {
        let id = id?;
        if n == HISTORY_SIZE {
            more = true;
            break;
        }
        object_size(&repo.git, id, RAW_LIMIT)?;
        let c = repo.git.find_commit(id)?;
        let summary = c
            .message_bytes()
            .split(|b| *b == b'\n')
            .next()
            .unwrap_or(b"(no message)");
        let author = c.author();
        let _ = write!(
            body,
            "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
            revision_link("/history", q, id.to_string().as_bytes(), &display(summary)),
            revision_link(
                "/history",
                q,
                id.to_string().as_bytes(),
                &id.to_string()[..10]
            ),
            label(author.name_bytes()),
            escape(&date(c.time().seconds()))
        );
        if body.len() > HTML_LIMIT - 65536 {
            return Err(AppError::limit(
                "Commit metadata exceeds the page output limit.",
            ));
        }
    }
    body.push_str("</tbody></table></div>");
    pagination(&mut body, "/history", &stable, current, more);
    let mut page = finish(format!("History — {}", display(&repo.relative)), body)?;
    page.revision = Some(revision);
    Ok(page)
}

pub fn page(root: &Root, route: &str, q: &Params, ssh_prefix: Option<&str>) -> Result<Page> {
    if route == "/" {
        return repositories(root, q);
    }
    if route == "/search" {
        return search(root, q);
    }
    if !matches!(
        route,
        "/files" | "/branches" | "/refs" | "/tags" | "/history"
    ) {
        return Err(AppError::new(404, "Page not found."));
    }
    let repo = repository(root, q.bytes("repo"))?;
    let revision = selection(&repo.git, q.bytes("rev"))?;
    let mut selected = q.clone();
    selected.0.insert("rev".into(), revision.clone());
    let q = &selected;
    let mut page = match route {
        "/files" => files(&repo, q, ssh_prefix, route),
        "/branches" | "/refs" => refs(&repo, q, route, false),
        "/tags" => refs(&repo, q, route, true),
        "/history" => history(&repo, q, ssh_prefix),
        _ => unreachable!(),
    }?;
    page.show_heading = false;
    page.active_tab = Some(match route {
        "/branches" | "/refs" => "Branches",
        "/tags" => "Tags",
        "/history" => "History",
        _ => "Files",
    });
    page.context = crate::render::revision_context(&revision, repo.git.is_shallow());
    if page.breadcrumbs.is_none() {
        page.breadcrumbs = Some(crate::render::breadcrumbs(
            &repo.relative,
            b"",
            Some(&revision),
            None,
        ));
    }
    page.revision = Some(revision);
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture {
        directory: PathBuf,
        root: Root,
        git: Repository,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "gitt-backend-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&directory).unwrap();
            let mut opts = git2::RepositoryInitOptions::new();
            opts.external_template(false).initial_head("main");
            let git = Repository::init_opts(directory.join("demo"), &opts).unwrap();
            let root = Root::new(&directory).unwrap();
            Self {
                directory,
                root,
                git,
            }
        }
        fn put(&self, name: &[u8], content: &[u8]) {
            std::fs::write(self.directory.join("demo").join(path(name)), content).unwrap();
        }
        fn stage(&self, name: &[u8]) {
            let mut index = self.git.index().unwrap();
            index.add_path(path(name)).unwrap();
            index.write().unwrap();
        }
        fn commit(&self, message: &str) -> Oid {
            let mut index = self.git.index().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = self.git.find_tree(tree_id).unwrap();
            let sig = git2::Signature::new(
                "Terry",
                "test@example.invalid",
                &git2::Time::new(1_700_000_000, 0),
            )
            .unwrap();
            let parent = self.git.head().ok().and_then(|h| h.peel_to_commit().ok());
            let parents = parent.as_ref().map(|p| vec![p]).unwrap_or_default();
            self.git
                .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
                .unwrap()
        }
        fn params(&self, extra: &str) -> Params {
            Params::parse(&format!("repo=demo&{extra}")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn repository_metadata_follows_head_and_preserves_empty_branch_names() {
        let f = Fixture::new();
        let (branch, change) = repository_metadata(&f.root, b"demo", b"").unwrap();
        assert_eq!(branch, b"refs/heads/main");
        assert!(change.is_none());

        f.put(b"file.txt", b"first\n");
        f.stage(b"file.txt");
        let first = f.commit("First version\n\nCommit body is not a summary");
        f.put(b"file.txt", b"second\n");
        f.stage(b"file.txt");
        let second = f.commit("Second version");
        f.put(b"file.txt", b"uncommitted\n");
        let (branch, change) = repository_metadata(&f.root, b"demo", b"").unwrap();
        let change = change.unwrap();
        assert_eq!(branch, b"refs/heads/main");
        assert_eq!(change.commit, second);
        assert_eq!(change.message, "Second version");
        assert_eq!(change.author, "Terry");
        assert_eq!(change.timestamp, 1_700_000_000);

        f.git.set_head_detached(first).unwrap();
        let (branch, change) = repository_metadata(&f.root, b"demo", b"").unwrap();
        let change = change.unwrap();
        assert_eq!(branch, first.to_string().as_bytes());
        assert_eq!(change.commit, first);
        assert_eq!(change.message, "First version");
    }

    #[test]
    fn repository_listing_escapes_metadata_and_survives_unavailable_repositories() {
        let f = Fixture::new();
        f.put(b"file.txt", b"one\n");
        f.stage(b"file.txt");
        let id = f.commit("Fix <script> & display\n\nPrivate commit body");
        Repository::init_bare(f.directory.join("empty.git")).unwrap();
        std::fs::create_dir(f.directory.join("unsupported")).unwrap();
        std::fs::write(f.directory.join("unsupported/.git"), "gitdir: /outside\n").unwrap();
        let q = Params::parse("").unwrap();
        let listing = repositories(&f.root, &q).unwrap();
        assert!(listing.body.contains("Fix &lt;script&gt; &amp; display"));
        assert!(!listing.body.contains("Private commit body"));
        assert!(listing.body.contains(&format!("rev={id}")));
        assert!(listing.body.contains(">empty.git</a>"));
        assert!(listing.body.contains(">unsupported</a>"));
        assert!(!listing.body.contains(">Kind</th>"));

        std::fs::write(f.directory.join("demo/.git/HEAD"), "invalid HEAD\n").unwrap();
        let listing = repositories(&f.root, &q).unwrap();
        assert!(listing.body.contains(">demo</a></td><td>—</td>"));
        assert!(listing.body.contains(">empty.git</a>"));
    }

    #[test]
    fn directory_metadata_uses_committed_sizes_and_selected_history() {
        let f = Fixture::new();
        f.put(b"file.txt", b"one\n");
        f.stage(b"file.txt");
        let first = f.commit("First version");
        f.put(b"file.txt", b"second version\n");
        f.stage(b"file.txt");
        f.commit("Second version");
        f.put(b"file.txt", b"uncommitted on disk\n");
        f.put(b"new.txt", b"new\n");
        let disk = page(&f.root, "/files", &f.params(""), None).unwrap();
        assert!(disk.body.contains("Second version"));
        assert!(disk.body.contains("15 B"));
        assert!(!disk.body.contains("new.txt"));
        assert!(!disk.body.contains("unchanged"));
        assert!(!disk.body.contains("untracked"));
        let snapshot = page(&f.root, "/files", &f.params(&format!("rev={first}")), None).unwrap();
        assert!(snapshot.body.contains("First version"));
        assert!(!snapshot.body.contains("Second version"));
        assert!(snapshot.body.contains("4 B"));
        assert!(!snapshot.body.contains("new.txt"));
        assert!(snapshot.breadcrumbs.as_ref().unwrap().contains("gitt"));
    }

    #[test]
    fn branches_and_tags_are_independent_and_preserve_snapshot_links() {
        let f = Fixture::new();
        f.put(b"file.txt", b"one\n");
        f.stage(b"file.txt");
        let id = f.commit("First");
        f.git
            .tag_lightweight("v1", &f.git.find_object(id, None).unwrap(), false)
            .unwrap();
        let branches = page(&f.root, "/branches", &f.params(""), None).unwrap();
        let tags = page(&f.root, "/tags", &f.params(""), None).unwrap();
        assert_eq!(branches.active_tab, Some("Branches"));
        assert_eq!(tags.active_tab, Some("Tags"));
        assert!(branches.body.contains(">main</a>"));
        assert!(!branches.body.contains(">v1</a>"));
        assert!(tags.body.contains(">v1</a>"));
        assert!(tags.body.contains(&format!("rev={id}")));
    }

    #[test]
    fn browsing_search_and_raw_expose_only_committed_content() {
        let f = Fixture::new();
        f.put(b"file.txt", b"original\n");
        f.put(b"deleted.txt", b"still committed\n");
        f.stage(b"file.txt");
        f.stage(b"deleted.txt");
        f.commit("Initial");
        f.put(b"file.txt", b"staged\n");
        f.put(b"staged-only.txt", b"not committed\n");
        f.stage(b"file.txt");
        f.stage(b"staged-only.txt");
        f.put(b"file.txt", b"working\n");
        f.put(b"untracked.txt", b"private\n");
        std::fs::remove_file(f.directory.join("demo/deleted.txt")).unwrap();
        let index = f.directory.join("demo/.git/index");
        let before = std::fs::read(&index).unwrap();
        let listing = page(&f.root, "/files", &f.params(""), None).unwrap();
        assert_eq!(listing.revision, Some(b"refs/heads/main".to_vec()));
        assert!(listing.body.contains("deleted.txt"));
        assert!(!listing.body.contains("staged-only.txt"));
        assert!(!listing.body.contains("untracked.txt"));
        let source = page(&f.root, "/files", &f.params("path=file.txt"), None).unwrap();
        assert!(source.body.contains("original"));
        assert!(!source.body.contains("staged"));
        assert!(!source.body.contains("working"));
        assert_eq!(
            raw(&f.root, &f.params("path=file.txt")).unwrap(),
            b"original\n"
        );
        assert_eq!(
            raw(&f.root, &f.params("path=deleted.txt")).unwrap(),
            b"still committed\n"
        );
        for name in ["untracked.txt", "staged-only.txt"] {
            assert_eq!(
                raw(&f.root, &f.params(&format!("path={name}")))
                    .err()
                    .unwrap()
                    .status,
                404
            );
            assert_eq!(
                page(&f.root, "/files", &f.params(&format!("path={name}")), None)
                    .err()
                    .unwrap()
                    .status,
                404
            );
            let search = page(&f.root, "/search", &f.params(&format!("q={name}")), None).unwrap();
            assert!(!search.body.contains(&format!(">{name}<")));
        }
        assert_eq!(
            page(&f.root, "/changes", &f.params(""), None)
                .err()
                .unwrap()
                .status,
            404
        );
        assert_eq!(std::fs::read(&index).unwrap(), before);
    }

    #[test]
    fn unified_search_preserves_snapshots_and_resets_them_when_switching_repository() {
        let f = Fixture::new();
        f.put(b"file.txt", b"original\n");
        f.stage(b"file.txt");
        let first = f.commit("Initial");
        f.put(b"new.txt", b"next\n");
        f.stage(b"new.txt");
        f.commit("Add file");
        let selected = search_scope(b"demo");
        let search =
            |query: &str| page(&f.root, "/search", &Params::parse(query).unwrap(), None).unwrap();
        let names = search("q=demo");
        assert!(names.body.contains("aria-label=\"Projects\""));
        assert!(!names.body.contains("aria-label=\"Files\""));
        let old = search(&format!(
            "scope={selected}&snapshot_scope={selected}&rev={first}&q=txt"
        ));
        assert!(old.body.contains("file.txt</span>"));
        assert!(!old.body.contains("new.txt</span>"));
        assert!(old.body.contains(&format!("rev={first}")));
        let other = Repository::init_bare(f.directory.join("other.git")).unwrap();
        let tree = other.treebuilder(None).unwrap().write().unwrap();
        let sig = git2::Signature::now("Terry", "test@example.invalid").unwrap();
        other
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "Other",
                &other.find_tree(tree).unwrap(),
                &[],
            )
            .unwrap();
        let changed = search(&format!(
            "scope={}&snapshot_scope={selected}&rev={first}&q=txt",
            search_scope(b"other.git")
        ));
        assert!(!changed.body.contains(&first.to_string()));
        assert!(
            changed.body.contains(
                std::str::from_utf8(
                    other
                        .find_reference("HEAD")
                        .unwrap()
                        .symbolic_target_bytes()
                        .unwrap()
                )
                .unwrap()
            )
        );
        assert!(changed.body.contains("No matching files"));
    }

    #[test]
    fn unified_search_paginates_with_the_selected_repository_and_commit() {
        let f = Fixture::new();
        let mut index = f.git.index().unwrap();
        for number in 0..=PAGE_SIZE {
            let name = format!("file-{number:03}.txt");
            f.put(name.as_bytes(), b"committed\n");
            index.add_path(path(name.as_bytes())).unwrap();
        }
        index.write().unwrap();
        let id = f.commit("Files");
        let first = page(
            &f.root,
            "/search",
            &f.params(&format!("q=file-&rev={id}")),
            None,
        )
        .unwrap();
        let next = first
            .body
            .split("href=\"")
            .skip(1)
            .map(|value| value.split('"').next().unwrap())
            .find(|value| value.starts_with("/search?") && value.contains("page=1"))
            .unwrap()
            .replace("&amp;", "&")
            .replace("&#38;", "&");
        let q = Params::parse(next.split_once('?').unwrap().1).unwrap();
        assert_eq!(q.bytes("repo"), b"demo");
        assert_eq!(q.bytes("rev"), id.to_string().as_bytes());
        let second = page(&f.root, "/search", &q, None).unwrap();
        assert!(second.body.contains("file-200.txt</span>"));
        assert!(!second.body.contains("file-000.txt</span>"));
    }

    #[test]
    fn repositories_without_commits_do_not_show_disk_files() {
        let f = Fixture::new();
        f.put(b"draft.txt", b"private draft\n");
        let listing = page(&f.root, "/files", &f.params(""), None).unwrap();
        assert!(listing.body.contains("No commits yet"));
        assert!(!listing.body.contains("draft.txt"));
        assert_eq!(
            page(&f.root, "/files", &f.params("path=draft.txt"), None)
                .err()
                .unwrap()
                .status,
            404
        );
        assert_eq!(
            raw(&f.root, &f.params("path=draft.txt"))
                .err()
                .unwrap()
                .status,
            404
        );
    }

    #[test]
    fn snapshots_root_commits_and_raw_preserve_bytes() {
        let f = Fixture::new();
        f.put(b"odd\xff.txt", b"<script>alert(1)</script>\n");
        f.stage(b"odd\xff.txt");
        let first = f.commit("<script> commit");
        let q = f.params(&format!("rev={first}&path=odd%FF.txt"));
        assert_eq!(raw(&f.root, &q).unwrap(), b"<script>alert(1)</script>\n");
        let listing = page(&f.root, "/files", &f.params(&format!("rev={first}")), None).unwrap();
        assert!(listing.body.contains("odd%FF%2Etxt"));
        let history = page(
            &f.root,
            "/history",
            &f.params(&format!("rev={first}")),
            None,
        )
        .unwrap();
        assert!(!history.body.contains("<script>"));
        assert!(history.body.contains("&lt;script&gt; commit"));
    }

    #[test]
    fn symlink_targets_are_visible_but_not_read() {
        let f = Fixture::new();
        std::os::unix::fs::symlink("/etc/passwd", f.directory.join("demo/link")).unwrap();
        f.stage(b"link");
        f.commit("Record link");
        let p = page(&f.root, "/files", &f.params("path=link"), None).unwrap();
        assert!(p.body.contains("Symbolic link (never followed)"));
        assert!(p.body.contains("/etc/passwd"));
        assert!(raw(&f.root, &f.params("path=link")).is_err());
        assert!(
            page(
                &f.root,
                "/files",
                &f.params("path=..%2F..%2Fetc%2Fpasswd"),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn committed_reads_ignore_worktree_filters() {
        let f = Fixture::new();
        f.put(b"file.txt", b"first\n");
        f.stage(b"file.txt");
        f.commit("Initial");
        f.put(b".gitattributes", b"*.txt filter=evil\n");
        let marker = f.directory.join("filter-was-executed");
        f.git
            .config()
            .unwrap()
            .set_str("filter.evil.clean", &format!("touch {}", marker.display()))
            .unwrap();
        let p = page(&f.root, "/files", &f.params("path=file.txt"), None).unwrap();
        assert!(p.body.contains("first"));
        assert_eq!(
            raw(&f.root, &f.params("path=file.txt")).unwrap(),
            b"first\n"
        );
        assert!(!marker.exists());
    }

    #[test]
    fn removed_commit_and_compare_views_return_not_found() {
        let f = Fixture::new();
        for route in ["/commit", "/compare"] {
            assert_eq!(
                page(&f.root, route, &f.params(""), None)
                    .err()
                    .unwrap()
                    .status,
                404
            );
        }
    }

    #[test]
    fn snapshot_navigation_stays_pinned_when_head_moves() {
        let f = Fixture::new();
        f.put(b"file.txt", b"original\n");
        f.stage(b"file.txt");
        let first = f.commit("First");
        let listing = page(&f.root, "/files", &f.params(&format!("rev={first}")), None).unwrap();
        f.put(b"file.txt", b"another snapshot\n");
        f.stage(b"file.txt");
        f.commit("Moved");
        assert_eq!(listing.revision, Some(first.to_string().into_bytes()));
        assert!(listing.body.contains(&format!("rev={first}")));
        assert_eq!(
            raw(&f.root, &f.params(&format!("path=file.txt&rev={first}"))).unwrap(),
            b"original\n"
        );
        assert_eq!(
            raw(&f.root, &f.params("path=file.txt")).unwrap(),
            b"another snapshot\n"
        );
    }

    #[test]
    fn named_selections_survive_all_tabs_files_and_markdown() {
        let f = Fixture::new();
        f.put(b"file.txt", b"original version\n");
        f.put(b"README.md", b"[Open file](file.txt)\n");
        f.stage(b"file.txt");
        f.stage(b"README.md");
        let first = f.commit("First");
        f.git
            .tag_lightweight("release", &f.git.find_object(first, None).unwrap(), false)
            .unwrap();
        f.put(b"file.txt", b"new version\n");
        f.stage(b"file.txt");
        let second = f.commit("Second");
        f.git
            .branch("release", &f.git.find_commit(second).unwrap(), false)
            .unwrap();
        for selected in ["refs/tags/release", "refs/heads/release"] {
            for route in ["/files", "/history", "/branches", "/tags"] {
                let page =
                    page(&f.root, route, &f.params(&format!("rev={selected}")), None).unwrap();
                assert_eq!(page.revision.as_deref(), Some(selected.as_bytes()));
                assert!(page.context.ends_with(">release</span>"));
                assert!(!page.context.contains("href="));
                let html = crate::render::layout_page(&page, Some(b"demo"), None).unwrap();
                for href in html.split("href=\"").skip(1).map(|value| {
                    value
                        .split('"')
                        .next()
                        .unwrap()
                        .replace("&amp;", "&")
                        .replace("&#38;", "&")
                }) {
                    assert!(!href.starts_with("/commit?"));
                    assert!(!href.starts_with("/compare?"));
                    if href.starts_with("/search?") {
                        assert_eq!(
                            Params::parse(href.split_once('?').unwrap().1)
                                .unwrap()
                                .bytes("rev"),
                            selected.as_bytes()
                        );
                    }
                }
            }
            let markdown = page(
                &f.root,
                "/files",
                &f.params(&format!("path=README.md&rev={selected}")),
                None,
            )
            .unwrap();
            assert!(markdown.body.contains(&format!(
                    "rev={}",
                    crate::render::url("", &[("rev", selected.as_bytes())])
                        .split_once('=')
                        .unwrap()
                        .1
                )));
        }
        assert_eq!(
            raw(&f.root, &f.params("path=file.txt&rev=refs/tags/release")).unwrap(),
            b"original version\n"
        );
        assert_eq!(
            raw(&f.root, &f.params("path=file.txt&rev=refs/heads/release")).unwrap(),
            b"new version\n"
        );
        let default = page(&f.root, "/files", &f.params(""), None).unwrap();
        assert_eq!(
            default.revision.as_deref(),
            Some(b"refs/heads/main".as_slice())
        );
    }

    #[test]
    fn revision_links_keep_the_route_path_filters_and_page() {
        let f = Fixture::new();
        std::fs::create_dir(f.directory.join("demo/src")).unwrap();
        f.put(b"src/file.txt", b"one\n");
        f.stage(b"src/file.txt");
        let first = f.commit("First");
        f.git
            .tag_lightweight("v1", &f.git.find_object(first, None).unwrap(), false)
            .unwrap();
        let original = f.params("path=src&q=file&page=0&source=1&rev=refs/heads/main&extra=a%26b");
        for (route, text, revision) in [
            ("/files", "First", first.to_string()),
            ("/history", "First", first.to_string()),
            ("/branches", "main", "refs/heads/main".into()),
            ("/tags", "v1", "refs/tags/v1".into()),
            ("/refs", "main", "refs/heads/main".into()),
        ] {
            let rendered = page(&f.root, route, &original, None).unwrap();
            let before_text = rendered.body.split(&format!(">{text}</a>")).next().unwrap();
            let href = before_text
                .rsplit("href=\"")
                .next()
                .unwrap()
                .trim_end_matches('"')
                .replace("&amp;", "&")
                .replace("&#38;", "&");
            let (target, query) = href.split_once('?').unwrap();
            assert_eq!(target, route);
            let selected = Params::parse(query).unwrap();
            let mut expected = original.clone();
            expected.0.insert("rev".into(), revision.into_bytes());
            assert_eq!(selected.0, expected.0);
        }
    }

    #[test]
    fn non_utf8_tag_names_remain_selected_and_readable() {
        let f = Fixture::new();
        f.put(b"file.txt", b"one\n");
        f.stage(b"file.txt");
        let id = f.commit("First");
        let tag = f.directory.join("demo/.git/refs/tags");
        std::fs::create_dir_all(&tag).unwrap();
        std::fs::write(tag.join(path(b"tag\xff")), format!("{id}\n")).unwrap();
        let q = f.params("path=file.txt&rev=refs%2Ftags%2Ftag%FF");
        let selected = page(&f.root, "/files", &q, None).unwrap();
        assert_eq!(
            selected.revision.as_deref(),
            Some(b"refs/tags/tag\xff".as_slice())
        );
        assert!(selected.context.contains("tag\\xFF</span>"));
        assert_eq!(raw(&f.root, &q).unwrap(), b"one\n");
    }

    #[test]
    fn date_is_utc_and_form_values_preserve_backslashes() {
        assert_eq!(date(0), "1970-01-01 00:00 UTC");
        assert_eq!(date(-60), "1969-12-31 23:59 UTC");
        assert!(hidden("path", b"a\\b").contains("value=\"a\\b\""));
        assert!(hidden("path", b"a\xff\n").contains("name=\"path_hex\" value=\"61ff0a\""));
    }
}
