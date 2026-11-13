//! Read-only Git views. Every URL keeps the original filename bytes.
mod fs;

use crate::render::{display, escape, file_content_with_context, url};
use crate::security::Root;
use crate::types::{AppError, HTML_LIMIT, Page, Params, RAW_LIMIT, Result, TEXT_LIMIT};
use git2::{Index, ObjectType, Oid, Repository, RepositoryOpenFlags, Tree};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const PATH_LIMIT: usize = 10_000;
const PAGE_SIZE: usize = 200;
const HISTORY_SIZE: usize = 50;
const LINE_LIMIT: usize = 20_000;
const SSH_PATH: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub enum Raw {
    Bytes(Vec<u8>),
    File(File),
}

struct Repo {
    git: Repository,
    relative: Vec<u8>,
    gitdir: Vec<u8>,
}

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
    for name in [
        b"HEAD".as_slice(),
        b"config",
        b"packed-refs",
        b"index",
        b"shallow",
    ] {
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
    if config.get_bool("core.sparsecheckout").unwrap_or(false)
        || config.get_bool("core.splitindex").unwrap_or(false)
    {
        return Err(AppError::limit(
            "Sparse checkouts and split indexes are not supported.",
        ));
    }
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
        gitdir,
    })
}

fn index(repo: &Repo) -> Result<Index> {
    let index = repo.git.index().map_err(|_| AppError::limit("The Git index could not be read. Split or sparse indexes and corrupt index files are not supported; inspect this checkout with local Git."))?;
    if index.len() > 100_000 {
        return Err(AppError::limit(
            "This index exceeds the 100,000 entry limit.",
        ));
    }
    if index
        .iter()
        .any(|e| e.mode == 0o040000 || e.flags_extended & 0x4000 != 0)
    {
        return Err(AppError::limit(
            "Sparse indexes and skip-worktree entries are not supported.",
        ));
    }
    Ok(index)
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
        "HEAD"
    } else {
        std::str::from_utf8(revision)
            .map_err(|_| AppError::bad("Use an object ID for non-UTF-8 references."))?
    };
    if revision.len() > 1024 || revision.as_bytes().contains(&0) {
        return Err(AppError::bad("Invalid revision."));
    }
    let mut id = if revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        Oid::from_str(revision)?
    } else {
        repo.resolve_reference_from_short_name(revision)?
            .resolve()?
            .target()
            .ok_or_else(|| AppError::limit("This reference has no object target."))?
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

fn header(repo: &Repo, q: &Params, ssh_prefix: Option<&str>) -> String {
    let r = repo.relative.as_slice();
    let mut body = String::from("<p><small>");
    body.push_str(if repo.git.is_bare() {
        "Bare repository"
    } else {
        "Working checkout"
    });
    if let Ok(head) = repo.git.head() {
        body.push_str(" · HEAD: ");
        body.push_str(&label(head.shorthand_bytes()));
        if repo.git.head_detached().unwrap_or(false) {
            body.push_str(" (detached HEAD)");
        }
    } else {
        body.push_str(" · No commits yet");
    }
    if repo.git.is_shallow() {
        body.push_str(" · Shallow history: only locally available commits are shown");
    }
    body.push_str("</small></p>");
    if let Some(prefix) = ssh_prefix {
        // SSH URI paths preserve arbitrary bytes by percent encoding; scp-like prefixes are displayed literally.
        let suffix = if prefix.starts_with("ssh://") {
            percent_encoding::percent_encode(r, SSH_PATH).to_string()
        } else {
            display(r)
        };
        let _ = write!(
            body,
            "<details><summary>SSH clone URL</summary><pre><code>{}{}</code></pre></details>",
            escape(prefix),
            escape(&suffix)
        );
    }
    let _ = q;
    body
}

#[derive(Clone)]
struct Found {
    relative: Vec<u8>,
    kind: &'static str,
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
        if let Ok(st) = fs::stat(root, &dotgit) {
            found.push(Found {
                relative: directory,
                kind: if fs::is_dir(&st) {
                    "checkout"
                } else {
                    "unsupported linked checkout"
                },
            });
            continue;
        }
        if fs::stat(root, &fs::join(&directory, b"HEAD")).is_ok()
            && fs::stat(root, &fs::join(&directory, b"objects")).is_ok_and(|st| fs::is_dir(&st))
        {
            found.push(Found {
                relative: directory,
                kind: "bare",
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

fn repositories(root: &Root, q: &Params) -> Result<Page> {
    let (found, incomplete) = discover(root)?;
    let filter = q.text("q")?.to_lowercase();
    let found: Vec<_> = found
        .iter()
        .filter(|r| display(&r.relative).to_lowercase().contains(&filter))
        .collect();
    let current = q.page()?;
    let mut body = format!(
        "<p>Browse code and create empty repositories.</p><form method=\"get\" action=\"/\"><label>Repository name <input type=\"search\" name=\"q\" value=\"{}\"></label><button type=\"submit\">Find</button></form><p>{} · {}</p>",
        label(q.bytes("q")),
        a("/new", &[], "Create repository"),
        a("/", &[("q", q.bytes("q"))], "Refresh")
    );
    if incomplete {
        body.push_str("<p><strong>Incomplete scan.</strong> Discovery stops after 10,000 directories, 1,000 repositories, or an unreadable directory. Narrow the watched root to see everything.</p>");
    }
    if found.is_empty() {
        body.push_str("<p>No repositories found.</p>");
    } else {
        body.push_str("<ul>");
        for item in found.iter().skip(current * PAGE_SIZE).take(PAGE_SIZE) {
            let name = if item.relative.is_empty() {
                ".".to_owned()
            } else {
                display(&item.relative)
            };
            let _ = write!(
                body,
                "<li>{} <small>{}</small></li>",
                a("/files", &[("repo", &item.relative)], &name),
                item.kind
            );
        }
        body.push_str("</ul>");
    }
    pagination(
        &mut body,
        "/",
        q,
        current,
        found.len() > (current + 1) * PAGE_SIZE,
    );
    finish("Repositories", body)
}

fn tracked(index: &Index, relative: &[u8]) -> bool {
    if (0..=3).any(|stage| index.get_path(path(relative), stage).is_some()) {
        return true;
    }
    let mut directory = relative.to_vec();
    directory.push(b'/');
    index.find_prefix(directory).is_ok()
}
fn working_path(root: &Root, repo: &Repo, index: &Index, relative: &[u8]) -> Result<Vec<u8>> {
    fs::valid(relative)?;
    if !relative.is_empty()
        && !tracked(index, relative)
        && repo.git.is_path_ignored(path(relative))?
    {
        return Err(AppError::new(404, "Ignored files are not exposed by gitt."));
    }
    let mut ancestor = Vec::new();
    for segment in relative.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
        ancestor = fs::join(&ancestor, segment);
        if index
            .get_path(path(&ancestor), 0)
            .is_some_and(|e| e.mode == 0o160000)
        {
            return Err(AppError::limit(
                "Submodule contents are not browsed. Use the recorded commit with local Git.",
            ));
        }
        if fs::stat(
            root,
            &fs::join(&fs::join(&repo.relative, &ancestor), b".git"),
        )
        .is_ok()
        {
            return Err(AppError::limit(
                "Nested repositories and linked worktrees are not browsed through a parent checkout.",
            ));
        }
    }
    Ok(fs::join(&repo.relative, relative))
}

fn breadcrumbs(body: &mut String, repo: &[u8], relative: &[u8], revision: &[u8]) {
    body.push_str("<nav aria-label=\"Path\">");
    body.push_str(&a("/files", &[("repo", repo), ("rev", revision)], "Root"));
    let mut prefix = Vec::new();
    for segment in relative.split(|b| *b == b'/').filter(|s| !s.is_empty()) {
        prefix = fs::join(&prefix, segment);
        body.push_str(" / ");
        body.push_str(&a(
            "/files",
            &[("repo", repo), ("path", &prefix), ("rev", revision)],
            &display(segment),
        ));
    }
    body.push_str("</nav>");
}

struct FileEntry {
    path: Vec<u8>,
    directory: bool,
    note: String,
}

fn list_working(
    root: &Root,
    repo: &Repo,
    index: &Index,
    head: Option<&Tree<'_>>,
    relative: &[u8],
    query: &str,
) -> Result<(Vec<FileEntry>, bool)> {
    let mut result = Vec::new();
    let mut pending = vec![relative.to_vec()];
    let mut scanned = 0;
    let mut incomplete = false;
    while let Some(directory) = pending.pop() {
        let full = working_path(root, repo, index, &directory)?;
        let listing = fs::entries(&root.open(&full, true)?, PATH_LIMIT)?;
        incomplete |= listing.incomplete;
        for name in listing.names {
            if name == b".git" {
                continue;
            }
            scanned += 1;
            if scanned > PATH_LIMIT {
                incomplete = true;
                break;
            }
            let relative = fs::join(&directory, &name);
            if !tracked(index, &relative) && repo.git.is_path_ignored(path(&relative))? {
                continue;
            }
            let st = match fs::stat(root, &fs::join(&repo.relative, &relative)) {
                Ok(s) => s,
                Err(_) => {
                    incomplete = true;
                    continue;
                }
            };
            let directory = fs::is_dir(&st);
            let submodule = index
                .get_path(path(&relative), 0)
                .is_some_and(|e| e.mode == 0o160000);
            let nested = directory
                && fs::stat(
                    root,
                    &fs::join(&fs::join(&repo.relative, &relative), b".git"),
                )
                .is_ok();
            if !query.is_empty() && directory && !submodule && !nested {
                pending.push(relative.clone());
            }
            if !query.is_empty() && !display(&relative).to_lowercase().contains(query) {
                continue;
            }
            let note = if submodule {
                "submodule (contents unsupported)".into()
            } else if nested {
                "nested repository (contents unsupported)".into()
            } else if directory {
                "directory".into()
            } else if fs::is_link(&st) {
                format!(
                    "symbolic link · {}",
                    file_status(root, repo, index, head, &relative, &st)?
                )
            } else if !fs::is_file(&st) {
                "special file (not readable)".into()
            } else {
                file_status(root, repo, index, head, &relative, &st)?
            };
            result.push(FileEntry {
                path: relative,
                directory,
                note,
            });
        }
        if scanned > PATH_LIMIT {
            break;
        }
    }
    result.sort_by(|a, b| b.directory.cmp(&a.directory).then(a.path.cmp(&b.path)));
    Ok((result, incomplete))
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
            let note = match entry.filemode() {
                0o040000 => "directory",
                0o120000 => "symbolic link",
                0o160000 => "submodule (contents unsupported)",
                0o100755 => "executable",
                _ => "file",
            };
            result.push(FileEntry {
                path: full,
                directory: is_directory,
                note: note.to_owned(),
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
    q: &Params,
    revision: &[u8],
    entries: Vec<FileEntry>,
    incomplete: bool,
) -> Result<()> {
    let relative = q.bytes("path");
    let _ = write!(
        body,
        "<form method=\"get\" action=\"/files\">{}{}{}<label>Find a path <input type=\"search\" name=\"q\" value=\"{}\"></label><button type=\"submit\">Find</button></form>",
        hidden("repo", &repo.relative),
        hidden("path", relative),
        hidden("rev", revision),
        escape(q.text("q")?)
    );
    if incomplete {
        body.push_str("<p><strong>Incomplete listing:</strong> the 10,000 path limit was reached or a path became unreadable.</p>");
    }
    if entries.is_empty() {
        body.push_str("<p>No matching files.</p>");
    }
    let current = q.page()?;
    body.push_str("<ul>");
    for entry in entries.iter().skip(current * PAGE_SIZE).take(PAGE_SIZE) {
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
        let _ = write!(
            body,
            "<li>{} <small>{}</small></li>",
            a(
                "/files",
                &[
                    ("repo", &repo.relative),
                    ("path", &entry.path),
                    ("rev", revision)
                ],
                &text
            ),
            escape(&entry.note)
        );
    }
    body.push_str("</ul>");
    pagination(
        body,
        "/files",
        q,
        current,
        entries.len() > (current + 1) * PAGE_SIZE,
    );
    Ok(())
}

fn files(root: &Root, repo: &Repo, q: &Params, ssh: Option<&str>) -> Result<Page> {
    let relative = q.bytes("path");
    fs::valid(relative)?;
    let mut body = header(repo, q, ssh);
    let mut revision = q.bytes("rev").to_vec();
    let working = revision.is_empty() && !repo.git.is_bare();
    let query = q.text("q")?.to_lowercase();
    let mut content = None;
    let mut working_state = None;
    if working {
        body.push_str(
            "<p>Working tree: current files on disk. Ignored untracked files are hidden.</p>",
        );
        let before = fs::stat(root, &fs::join(&repo.gitdir, b"index")).ok();
        let index = index(repo)?;
        let head = head_commit(&repo.git)?;
        working_state = Some((before, head.as_ref().map(|c| c.id())));
        let tree = head.as_ref().map(|c| c.tree()).transpose()?;
        let full = working_path(root, repo, &index, relative)?;
        let st = fs::stat(root, &full)?;
        breadcrumbs(&mut body, &repo.relative, relative, b"");
        if fs::is_dir(&st) {
            let (entries, incomplete) =
                list_working(root, repo, &index, tree.as_ref(), relative, &query)?;
            directory_html(&mut body, repo, q, b"", entries, incomplete)?;
        } else if fs::is_link(&st) {
            let _ = write!(
                body,
                "<p>Symbolic link (never followed): <code>{}</code></p>",
                label(&fs::link(root, &full)?)
            );
            let _ = write!(
                body,
                "<p>{}</p>",
                escape(&file_status(
                    root,
                    repo,
                    &index,
                    tree.as_ref(),
                    relative,
                    &st
                )?)
            );
        } else if fs::is_file(&st) {
            body.push_str(&format!(
                "<p>{}</p>",
                escape(&file_status(
                    root,
                    repo,
                    &index,
                    tree.as_ref(),
                    relative,
                    &st
                )?)
            ));
            content = Some(fs::read(root, &full, TEXT_LIMIT));
        } else {
            return Err(AppError::limit(
                "Special files, devices, and sockets are not readable.",
            ));
        }
    } else {
        let snapshot = match commit(&repo.git, &revision) {
            Ok(c) => c,
            Err(_) if revision.is_empty() && repo.git.head().is_err() => {
                body.push_str("<p>This repository has no commits yet. Push a branch using SSH to begin browsing.</p>");
                return finish(display(&repo.relative), body);
            }
            Err(e) => return Err(e),
        };
        revision = snapshot.id().to_string().into_bytes();
        let _ = write!(body, "<p>Snapshot <code>{}</code></p>", label(&revision));
        if !repo.git.is_bare() {
            body.push_str(&format!(
                "<p>{}</p>",
                a(
                    "/files",
                    &[("repo", &repo.relative)],
                    "Browse current working tree"
                )
            ));
        }
        breadcrumbs(&mut body, &repo.relative, relative, &revision);
        let tree = snapshot.tree()?;
        if relative.is_empty() {
            let (entries, incomplete) = list_tree(&repo.git, &tree, b"", &query)?;
            directory_html(&mut body, repo, q, &revision, entries, incomplete)?;
        } else {
            let entry = tree_entry(&repo.git, &tree, relative)?;
            match entry.filemode() {
                0o040000 => {
                    object_size(&repo.git, entry.id(), RAW_LIMIT)?;
                    let child = repo.git.find_tree(entry.id())?;
                    let (entries, incomplete) = list_tree(&repo.git, &child, relative, &query)?;
                    directory_html(&mut body, repo, q, &revision, entries, incomplete)?;
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
                _ => content = Some(blob(&repo.git, entry.id(), TEXT_LIMIT)),
            }
        }
    }
    if let Some(bytes) = content {
        let args = [
            ("repo", repo.relative.as_slice()),
            ("path", relative),
            ("rev", revision.as_slice()),
        ];
        body.push_str("<p>");
        body.push_str(&a("/raw", &args, "Download raw"));
        let markdown = is_markdown(relative);
        if markdown {
            let mut source = args.to_vec();
            source.push(("source", if q.flag("source") { b"0" } else { b"1" }));
            body.push_str(" · ");
            body.push_str(&a(
                "/files",
                &source,
                if q.flag("source") {
                    "Rendered Markdown"
                } else {
                    "View source"
                },
            ));
        }
        body.push_str("</p>");
        match bytes {
            Ok(bytes) => body.push_str(&file_content_with_context(
                relative,
                &bytes,
                markdown && !q.flag("source"),
                &repo.relative,
                if working { None } else { Some(&revision) },
            )?),
            Err(e) => return Err(e),
        }
    }
    if let Some((before, head)) = working_state {
        state_notice(&mut body, root, repo, before, head);
    }
    finish(
        if relative.is_empty() {
            display(&repo.relative)
        } else {
            format!("{} — {}", display(relative), display(&repo.relative))
        },
        body,
    )
}
fn is_markdown(path: &[u8]) -> bool {
    let text = display(path).to_ascii_lowercase();
    text.ends_with(".md") || text.ends_with(".markdown") || text.ends_with(".mdown")
}

pub fn raw(root: &Root, q: &Params) -> Result<Raw> {
    let repo = repository(root, q.bytes("repo"))?;
    let relative = q.bytes("path");
    fs::valid(relative)?;
    if relative.is_empty() {
        return Err(AppError::bad("A file path is required."));
    }
    if q.bytes("rev").is_empty() && !repo.git.is_bare() {
        let index = index(&repo)?;
        let full = working_path(root, &repo, &index, relative)?;
        let file = root.open(&full, false)?;
        if !file.metadata()?.is_file() {
            return Err(AppError::limit("Only regular files can be downloaded."));
        }
        return Ok(Raw::File(file));
    }
    let snapshot = commit(&repo.git, q.bytes("rev"))?;
    let entry = tree_entry(&repo.git, &snapshot.tree()?, relative)?;
    if entry.filemode() == 0o120000 || entry.filemode() == 0o160000 {
        return Err(AppError::limit(
            "Symbolic links and submodules cannot be downloaded as files.",
        ));
    }
    Ok(Raw::Bytes(blob(&repo.git, entry.id(), RAW_LIMIT)?))
}

fn conversion(repo: &Repository, relative: &[u8]) -> Result<bool> {
    let config = repo.config()?;
    if config
        .get_string("core.autocrlf")
        .is_ok_and(|s| s != "false" && s != "0")
    {
        return Ok(true);
    }
    for attribute in ["filter", "text", "eol", "working-tree-encoding", "ident"] {
        let value = repo.get_attr_bytes(
            path(relative),
            attribute,
            git2::AttrCheckFlags::FILE_THEN_INDEX | git2::AttrCheckFlags::NO_SYSTEM,
        )?;
        if !matches!(
            git2::AttrValue::from_bytes(value),
            git2::AttrValue::False | git2::AttrValue::Unspecified
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn working_bytes(root: &Root, repo: &Repo, relative: &[u8], st: &libc::stat) -> Result<Vec<u8>> {
    let full = fs::join(&repo.relative, relative);
    if fs::is_link(st) {
        return fs::link(root, &full);
    }
    if !fs::is_file(st) {
        return Err(AppError::limit(
            "Special files and submodule directories are not compared.",
        ));
    }
    fs::read(root, &full, TEXT_LIMIT)
}

fn file_status(
    root: &Root,
    repo: &Repo,
    index: &Index,
    head: Option<&Tree<'_>>,
    relative: &[u8],
    st: &libc::stat,
) -> Result<String> {
    let Some(entry) = index.get_path(path(relative), 0) else {
        if (1..=3).any(|stage| index.get_path(path(relative), stage).is_some()) {
            return Ok("conflicted".into());
        }
        return Ok("untracked".into());
    };
    let mut statuses = Vec::new();
    let head_entry = head.map(|t| tree_entry(&repo.git, t, relative)).transpose();
    let head_entry = match head_entry {
        Ok(entry) => entry,
        Err(e) if e.status == 404 => None,
        Err(e) => return Err(e),
    };
    if head_entry
        .as_ref()
        .is_none_or(|e| e.id() != entry.id || e.filemode() as u32 != entry.mode)
    {
        statuses.push("staged");
    }
    if conversion(&repo.git, relative)? {
        statuses.push("conversion configured: working bytes not compared");
    } else if st.st_size > TEXT_LIMIT as i64 {
        statuses.push("working comparison exceeds 1 MiB limit");
    } else {
        let bytes = working_bytes(root, repo, relative, st)?;
        let mode = if fs::is_link(st) {
            0o120000
        } else if st.st_mode & 0o111 != 0
            && repo.git.config()?.get_bool("core.filemode").unwrap_or(true)
        {
            0o100755
        } else {
            0o100644
        };
        let check_mode = entry.mode == 0o120000
            || mode == 0o120000
            || repo.git.config()?.get_bool("core.filemode").unwrap_or(true);
        if Oid::hash_object(ObjectType::Blob, &bytes)? != entry.id
            || (check_mode && mode != entry.mode)
        {
            statuses.push("unstaged");
        }
    }
    if statuses.is_empty() {
        Ok("unchanged".into())
    } else {
        Ok(statuses.join(" · "))
    }
}

fn refs(repo: &Repo, q: &Params, ssh: Option<&str>) -> Result<Page> {
    let mut body = header(repo, q, ssh);
    let mut items = Vec::new();
    let mut incomplete = false;
    for reference in repo.git.references()? {
        let reference = reference?;
        if !reference.is_branch() && !reference.is_tag() && !reference.is_remote() {
            continue;
        }
        if items.len() == PATH_LIMIT {
            incomplete = true;
            break;
        }
        let name = reference.name_bytes().to_vec();
        let id = reference.resolve()?.target();
        let kind = if reference.is_tag() {
            "tag"
        } else if reference.is_remote() {
            "remote branch"
        } else {
            "branch"
        };
        items.push((name, id, kind));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    if incomplete {
        body.push_str("<p><strong>Incomplete reference list:</strong> the 10,000 reference limit was reached.</p>");
    }
    if items.is_empty() {
        body.push_str("<p>No branches or tags yet.</p>");
    }
    body.push_str("<ul>");
    let current = q.page()?;
    for (name, id, kind) in items.iter().skip(current * PAGE_SIZE).take(PAGE_SIZE) {
        let shown = name
            .strip_prefix(b"refs/heads/")
            .or_else(|| name.strip_prefix(b"refs/tags/"))
            .or_else(|| name.strip_prefix(b"refs/remotes/"))
            .unwrap_or(name);
        let revision = id.map(|v| v.to_string()).unwrap_or_default();
        let _ = write!(
            body,
            "<li>{} <small>{}</small> · {}</li>",
            a(
                "/files",
                &[("repo", &repo.relative), ("rev", revision.as_bytes())],
                &display(shown)
            ),
            kind,
            a(
                "/history",
                &[("repo", &repo.relative), ("rev", revision.as_bytes())],
                "history"
            )
        );
    }
    body.push_str("</ul>");
    pagination(
        &mut body,
        "/refs",
        q,
        current,
        items.len() > (current + 1) * PAGE_SIZE,
    );
    finish(
        format!("Branches & tags — {}", display(&repo.relative)),
        body,
    )
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

fn history(repo: &Repo, q: &Params, ssh: Option<&str>) -> Result<Page> {
    let mut body = header(repo, q, ssh);
    let start = match commit(&repo.git, q.bytes("rev")) {
        Ok(c) => c,
        Err(_) if q.bytes("rev").is_empty() && repo.git.head().is_err() => {
            body.push_str("<p>No commits yet.</p>");
            return finish(format!("History — {}", display(&repo.relative)), body);
        }
        Err(e) => return Err(e),
    };
    let current = q.page()?;
    if current >= PATH_LIMIT / HISTORY_SIZE {
        return Err(AppError::limit(
            "History browsing stops after 10,000 commits. Select an older full commit ID to continue.",
        ));
    }
    let pinned = start.id().to_string();
    let mut stable = q.clone();
    stable.0.insert("rev".into(), pinned.as_bytes().to_vec());
    let _ = write!(
        body,
        "<p>History from <code>{}</code>. Most recent commits first.</p>",
        escape(&pinned)
    );
    let mut walk = repo.git.revwalk()?;
    walk.set_sorting(git2::Sort::TIME)?;
    walk.push(start.id())?;
    let mut more = false;
    body.push_str("<ol>");
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
            "<li>{}<br><small><code>{}</code> · {} · {}</small></li>",
            a(
                "/commit",
                &[("repo", &repo.relative), ("id", id.to_string().as_bytes())],
                &display(summary)
            ),
            &id.to_string()[..10],
            label(author.name_bytes()),
            escape(&date(c.time().seconds()))
        );
        if body.len() > HTML_LIMIT - 65536 {
            return Err(AppError::limit(
                "Commit metadata exceeds the page output limit.",
            ));
        }
    }
    body.push_str("</ol>");
    pagination(&mut body, "/history", &stable, current, more);
    finish(format!("History — {}", display(&repo.relative)), body)
}

#[derive(Clone)]
struct Delta {
    path: Vec<u8>,
    old: Option<Oid>,
    new: Option<Oid>,
    old_mode: u32,
    new_mode: u32,
    kind: &'static str,
}

fn diff_options() -> git2::DiffOptions {
    let mut options = git2::DiffOptions::new();
    options
        .update_index(false)
        .include_typechange(true)
        .ignore_submodules(false)
        .skip_binary_check(true)
        .max_size(TEXT_LIMIT as i64)
        .context_lines(3);
    options
}

fn delta_list(diff: &git2::Diff<'_>) -> Result<Vec<Delta>> {
    if diff.deltas().len() > PATH_LIMIT {
        return Err(AppError::limit(
            "This comparison exceeds 10,000 changed paths. Use local Git.",
        ));
    }
    let mut out = Vec::new();
    for d in diff.deltas() {
        let old = d.old_file();
        let new = d.new_file();
        let relative = new
            .path_bytes()
            .or_else(|| old.path_bytes())
            .unwrap_or(b"")
            .to_vec();
        let kind = match d.status() {
            git2::Delta::Added => "added",
            git2::Delta::Deleted => "deleted",
            git2::Delta::Typechange => "type changed",
            git2::Delta::Conflicted => "conflicted",
            _ => "modified",
        };
        out.push(Delta {
            path: relative,
            old: if old.id().is_zero() {
                None
            } else {
                Some(old.id())
            },
            new: if new.id().is_zero() {
                None
            } else {
                Some(new.id())
            },
            old_mode: u32::from(old.mode()),
            new_mode: u32::from(new.mode()),
            kind,
        });
    }
    Ok(out)
}

fn render_patch(body: &mut String, relative: &[u8], old: &[u8], new: &[u8]) -> Result<()> {
    let lines = |bytes: &[u8]| {
        bytes.iter().filter(|b| **b == b'\n').count()
            + usize::from(!bytes.is_empty() && bytes.last() != Some(&b'\n'))
    };
    if old.len() > TEXT_LIMIT
        || new.len() > TEXT_LIMIT
        || lines(old) > LINE_LIMIT
        || lines(new) > LINE_LIMIT
    {
        body.push_str("<p>Diff omitted: a side exceeds 1 MiB or 20,000 lines. Use local Git.</p>");
        return Ok(());
    }
    if old.contains(&0) || new.contains(&0) {
        body.push_str("<p>Binary contents differ. Download a snapshot or use local Git.</p>");
        return Ok(());
    }
    let mut options = diff_options();
    let patch = git2::Patch::from_buffers(
        old,
        Some(path(relative)),
        new,
        Some(path(relative)),
        Some(&mut options),
    )?;
    if patch.num_hunks() == 0 {
        body.push_str("<p>No text changes.</p>");
        return Ok(());
    }
    body.push_str("<pre tabindex=\"0\" aria-label=\"Unified diff\"><code>");
    for h in 0..patch.num_hunks() {
        let (hunk, lines) = patch.hunk(h)?;
        body.push_str(&escape(&String::from_utf8_lossy(hunk.header())));
        for n in 0..lines {
            let line = patch.line_in_hunk(h, n)?;
            let old = line.old_lineno().map(|v| v.to_string()).unwrap_or_default();
            let new = line.new_lineno().map(|v| v.to_string()).unwrap_or_default();
            let content = escape(&String::from_utf8_lossy(line.content()));
            match line.origin() {
                '+' => {
                    let _ = write!(body, "<ins>{old:>5} {new:>5} +{content}</ins>");
                }
                '-' => {
                    let _ = write!(body, "<del>{old:>5} {new:>5} -{content}</del>");
                }
                ' ' => {
                    let _ = write!(body, "{old:>5} {new:>5}  {content}");
                }
                _ => {
                    let _ = write!(body, "              {content}");
                }
            }
            if body.len() > HTML_LIMIT - 65536 {
                body.push_str("</code></pre><p><strong>Diff truncated at the 2 MiB page output limit.</strong> Use local Git for the complete comparison.</p>");
                return Ok(());
            }
        }
    }
    body.push_str("</code></pre>");
    Ok(())
}

fn render_deltas(body: &mut String, repo: &Repo, deltas: &[Delta], revision: &[u8]) -> Result<()> {
    if deltas.is_empty() {
        body.push_str("<p>No changes.</p>");
        return Ok(());
    }
    for delta in deltas {
        if body.len() > HTML_LIMIT - 131072 {
            body.push_str(
                "<p><strong>Remaining files omitted at the 2 MiB page output limit.</strong></p>",
            );
            break;
        }
        let _ = write!(
            body,
            "<section><h3>{} <small>{}</small></h3>",
            label(&delta.path),
            delta.kind
        );
        if delta.old_mode != delta.new_mode {
            let _ = write!(
                body,
                "<p>Mode <code>{:06o}</code> → <code>{:06o}</code></p>",
                delta.old_mode, delta.new_mode
            );
        }
        if delta.old_mode == 0o160000 || delta.new_mode == 0o160000 {
            let _ = write!(
                body,
                "<p>Submodule commit <code>{}</code> → <code>{}</code>. Submodule contents are not browsed.</p>",
                delta
                    .old
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "absent".into()),
                delta
                    .new
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "absent".into())
            );
        } else {
            let old = delta
                .old
                .map(|id| blob(&repo.git, id, TEXT_LIMIT))
                .transpose();
            let new = delta
                .new
                .map(|id| blob(&repo.git, id, TEXT_LIMIT))
                .transpose();
            match (old, new) {
                (Ok(old), Ok(new)) => render_patch(
                    body,
                    &delta.path,
                    old.as_deref().unwrap_or(b""),
                    new.as_deref().unwrap_or(b""),
                )?,
                (Err(e), _) | (_, Err(e)) => {
                    let _ = write!(body, "<p>Diff unavailable: {}</p>", escape(&e.message));
                }
            }
            if delta.new.is_some() {
                body.push_str(&format!(
                    "<p>{}</p>",
                    a(
                        "/files",
                        &[
                            ("repo", &repo.relative),
                            ("path", &delta.path),
                            ("rev", revision)
                        ],
                        if revision.is_empty() {
                            "View working file"
                        } else {
                            "View file"
                        }
                    )
                ));
            }
        }
        body.push_str("</section>");
    }
    Ok(())
}

fn commit_page(repo: &Repo, q: &Params, ssh: Option<&str>) -> Result<Page> {
    let c = commit(&repo.git, q.bytes("id"))?;
    let mut body = header(repo, q, ssh);
    let id = c.id().to_string();
    let author = c.author();
    let committer = c.committer();
    let _ = write!(
        body,
        "<p><code>{id}</code></p><dl><dt>Author</dt><dd>{} &lt;{}&gt;</dd><dt>Author date</dt><dd>{}</dd><dt>Committer</dt><dd>{} &lt;{}&gt;</dd><dt>Commit date</dt><dd>{}</dd></dl><pre tabindex=\"0\" aria-label=\"Commit message\">{}</pre><p>{}</p>",
        label(author.name_bytes()),
        label(author.email_bytes()),
        escape(&date(author.when().seconds())),
        label(committer.name_bytes()),
        label(committer.email_bytes()),
        escape(&date(committer.when().seconds())),
        escape(&String::from_utf8_lossy(c.message_bytes())),
        a(
            "/files",
            &[("repo", &repo.relative), ("rev", id.as_bytes())],
            "Browse this snapshot"
        )
    );
    if c.parent_count() > 0 {
        body.push_str("<p>Parents: ");
        for parent in c.parent_ids() {
            body.push_str(&a(
                "/commit",
                &[
                    ("repo", &repo.relative),
                    ("id", parent.to_string().as_bytes()),
                ],
                &parent.to_string(),
            ));
            body.push(' ');
        }
        body.push_str("</p>");
    }
    body.push_str("<h2>Changes</h2>");
    let old = if c.parent_count() > 0 {
        let parent_id = c.parent_id(0)?;
        match commit(&repo.git, parent_id.to_string().as_bytes()) {
            Ok(parent) => Some(parent.tree()?),
            Err(_) => {
                body.push_str("<p>The first parent is unavailable in this shallow or incomplete repository. No parent diff can be shown.</p>");
                return finish(
                    format!("Commit {} — {}", &id[..10], display(&repo.relative)),
                    body,
                );
            }
        }
    } else {
        None
    };
    if c.parent_count() > 1 {
        body.push_str("<p>Merge commit. Showing the diff against its first parent.</p>");
    } else if old.is_none() {
        body.push_str("<p>Root commit. Comparing against an empty tree.</p>");
    }
    let new = c.tree()?;
    let mut options = diff_options();
    let diff = repo
        .git
        .diff_tree_to_tree(old.as_ref(), Some(&new), Some(&mut options))?;
    render_deltas(&mut body, repo, &delta_list(&diff)?, id.as_bytes())?;
    finish(
        format!("Commit {} — {}", &id[..10], display(&repo.relative)),
        body,
    )
}

fn compare(repo: &Repo, q: &Params, ssh: Option<&str>) -> Result<Page> {
    let mut body = header(repo, q, ssh);
    let _ = write!(
        body,
        "<p>Compare two endpoint snapshots. Enter branch names, tags, or full commit IDs.</p><form method=\"get\" action=\"/compare\">{}<label>From <input name=\"from\" value=\"{}\" required></label><label>To <input name=\"to\" value=\"{}\" required></label><button type=\"submit\">Compare</button></form>",
        hidden("repo", &repo.relative),
        escape(q.text("from")?),
        escape(q.text("to")?)
    );
    if q.bytes("from").is_empty() && q.bytes("to").is_empty() {
        return finish(format!("Compare — {}", display(&repo.relative)), body);
    }
    if q.bytes("from").is_empty() || q.bytes("to").is_empty() {
        return Err(AppError::bad("Choose both snapshots."));
    }
    let old = commit(&repo.git, q.bytes("from"))?;
    let new = commit(&repo.git, q.bytes("to"))?;
    let _ = write!(
        body,
        "<p><code>{}</code> → <code>{}</code></p><h2>Changes</h2>",
        old.id(),
        new.id()
    );
    let mut options = diff_options();
    let diff =
        repo.git
            .diff_tree_to_tree(Some(&old.tree()?), Some(&new.tree()?), Some(&mut options))?;
    render_deltas(
        &mut body,
        repo,
        &delta_list(&diff)?,
        new.id().to_string().as_bytes(),
    )?;
    finish(format!("Compare — {}", display(&repo.relative)), body)
}

fn changes(root: &Root, repo: &Repo, q: &Params, ssh: Option<&str>) -> Result<Page> {
    if repo.git.is_bare() {
        return Err(AppError::limit(
            "Bare repositories have no working tree or index changes.",
        ));
    }
    let mut body = header(repo, q, ssh);
    body.push_str("<p>Staged compares HEAD with the index. Unstaged compares the index with files on disk. Working files can change during a request; refresh for current content.</p>");
    let before = fs::stat(root, &fs::join(&repo.gitdir, b"index")).ok();
    let index = index(repo)?;
    let head = head_commit(&repo.git)?;
    let head_id = head.as_ref().map(|c| c.id());
    let tree = head.as_ref().map(|c| c.tree()).transpose()?;
    let mut options = diff_options();
    let diff = repo
        .git
        .diff_tree_to_index(tree.as_ref(), Some(&index), Some(&mut options))?;
    body.push_str("<h2>Staged</h2>");
    render_deltas(&mut body, repo, &delta_list(&diff)?, b"")?;
    let mut conflicts = BTreeSet::new();
    let mut staged_paths = BTreeMap::new();
    for entry in index.iter() {
        if entry.flags & 0x3000 != 0 {
            conflicts.insert(entry.path);
        } else {
            staged_paths.insert(entry.path.clone(), entry);
        }
    }
    if !conflicts.is_empty() {
        body.push_str("<h2>Conflicts</h2><p>Resolve these paths using local Git. gitt does not choose a side.</p><ul>");
        for relative in &conflicts {
            body.push_str(&format!(
                "<li>{}</li>",
                a(
                    "/files",
                    &[("repo", &repo.relative), ("path", relative)],
                    &display(relative)
                )
            ));
        }
        body.push_str("</ul>");
    }
    body.push_str("<h2>Unstaged</h2>");
    let mut changed = 0;
    for (relative, entry) in &staged_paths {
        if changed >= PATH_LIMIT || body.len() > HTML_LIMIT - 131072 {
            body.push_str(
                "<p><strong>Remaining changes omitted at a path or page output limit.</strong></p>",
            );
            break;
        }
        if conflicts.contains(relative) {
            continue;
        }
        if entry.mode == 0o160000 {
            body.push_str(&format!(
                "<p><code>{}</code>: submodule working state is not inspected.</p>",
                label(relative)
            ));
            continue;
        }
        fs::valid(relative)?;
        let full = fs::join(&repo.relative, relative);
        let st = match fs::stat(root, &full) {
            Ok(st) => Some(st),
            Err(e) if e.status == 404 => None,
            Err(e) => {
                body.push_str(&format!(
                    "<p><code>{}</code>: {}</p>",
                    label(relative),
                    escape(&e.message)
                ));
                continue;
            }
        };
        if st.is_some() && conversion(&repo.git, relative)? {
            body.push_str(&format!("<p><code>{}</code>: working comparison unavailable because text conversion or a filter is configured. No filter is executed.</p>",label(relative)));
            changed += 1;
            continue;
        }
        let old = blob(&repo.git, entry.id, TEXT_LIMIT);
        let new = st
            .as_ref()
            .map(|st| working_bytes(root, repo, relative, st))
            .transpose();
        match (old, new) {
            (Ok(old), Ok(new)) => {
                let new_bytes = new.as_deref().unwrap_or(b"");
                let mode = st
                    .as_ref()
                    .map(|st| {
                        if fs::is_link(st) {
                            0o120000
                        } else if st.st_mode & 0o111 != 0 {
                            0o100755
                        } else {
                            0o100644
                        }
                    })
                    .unwrap_or(0);
                let mode_changed = mode != entry.mode
                    && (mode == 0o120000
                        || entry.mode == 0o120000
                        || repo.git.config()?.get_bool("core.filemode").unwrap_or(true));
                if new.is_some() && old == new_bytes && !mode_changed {
                    continue;
                }
                changed += 1;
                let _ = write!(
                    body,
                    "<section><h3>{} <small>{}</small></h3>",
                    label(relative),
                    if new.is_none() { "deleted" } else { "modified" }
                );
                if mode_changed {
                    let _ = write!(
                        body,
                        "<p>Mode <code>{:06o}</code> → <code>{mode:06o}</code></p>",
                        entry.mode
                    );
                }
                render_patch(&mut body, relative, &old, new_bytes)?;
                if new.is_some() {
                    body.push_str(&format!(
                        "<p>{}</p>",
                        a(
                            "/files",
                            &[("repo", &repo.relative), ("path", relative)],
                            "View working file"
                        )
                    ));
                }
                body.push_str("</section>");
            }
            (Err(e), _) | (_, Err(e)) => {
                changed += 1;
                let _ = write!(
                    body,
                    "<p><code>{}</code>: comparison unavailable: {}</p>",
                    label(relative),
                    escape(&e.message)
                );
            }
        }
    }
    if changed == 0 {
        body.push_str("<p>No unstaged changes.</p>");
    }
    body.push_str("<h2>Untracked</h2>");
    let mut pending = vec![Vec::new()];
    let mut count = 0;
    let mut untracked = 0;
    let mut incomplete = false;
    body.push_str("<ul>");
    while let Some(directory) = pending.pop() {
        let full = fs::join(&repo.relative, &directory);
        let entries = fs::entries(&root.open(&full, true)?, PATH_LIMIT)?;
        incomplete |= entries.incomplete;
        for name in entries.names {
            if name == b".git" {
                continue;
            }
            count += 1;
            if count > PATH_LIMIT || body.len() > HTML_LIMIT - 65536 {
                incomplete = true;
                break;
            }
            let relative = fs::join(&directory, &name);
            if staged_paths.contains_key(&relative) || conflicts.contains(&relative) {
                continue;
            }
            if repo.git.is_path_ignored(path(&relative))? {
                continue;
            }
            let st = fs::stat(root, &fs::join(&repo.relative, &relative))?;
            if fs::is_dir(&st) {
                if fs::stat(
                    root,
                    &fs::join(&fs::join(&repo.relative, &relative), b".git"),
                )
                .is_ok()
                {
                    body.push_str(&format!(
                        "<li><code>{}/</code> (nested repository; contents not browsed)</li>",
                        label(&relative)
                    ));
                    untracked += 1;
                } else {
                    pending.push(relative);
                }
            } else {
                body.push_str(&format!(
                    "<li>{}</li>",
                    a(
                        "/files",
                        &[("repo", &repo.relative), ("path", &relative)],
                        &display(&relative)
                    )
                ));
                untracked += 1;
            }
        }
        if count > PATH_LIMIT || body.len() > HTML_LIMIT - 65536 {
            break;
        }
    }
    body.push_str("</ul>");
    if untracked == 0 {
        body.push_str("<p>No untracked files.</p>");
    }
    if incomplete {
        body.push_str("<p><strong>Incomplete untracked listing:</strong> a scan or output limit was reached.</p>");
    }
    state_notice(&mut body, root, repo, before, head_id);
    finish(format!("Changes — {}", display(&repo.relative)), body)
}

fn state_notice(
    body: &mut String,
    root: &Root,
    repo: &Repo,
    before: Option<libc::stat>,
    head_id: Option<Oid>,
) {
    let after = fs::stat(root, &fs::join(&repo.gitdir, b"index")).ok();
    let index_changed = match (before, after) {
        (Some(a), Some(b)) => {
            a.st_mtime != b.st_mtime
                || a.st_mtime_nsec != b.st_mtime_nsec
                || a.st_size != b.st_size
                || a.st_ino != b.st_ino
                || a.st_ctime != b.st_ctime
                || a.st_ctime_nsec != b.st_ctime_nsec
        }
        (None, None) => false,
        _ => true,
    };
    if index_changed || repo.git.head().ok().and_then(|h| h.target()) != head_id {
        body.push_str("<p><strong>HEAD or the index changed during this request.</strong> Refresh before relying on this comparison.</p>");
    }
}

pub fn page(root: &Root, route: &str, q: &Params, ssh_prefix: Option<&str>) -> Result<Page> {
    if route == "/" {
        return repositories(root, q);
    }
    let repo = repository(root, q.bytes("repo"))?;
    match route {
        "/files" => files(root, &repo, q, ssh_prefix),
        "/refs" => refs(&repo, q, ssh_prefix),
        "/history" => history(&repo, q, ssh_prefix),
        "/commit" => commit_page(&repo, q, ssh_prefix),
        "/compare" => compare(&repo, q, ssh_prefix),
        "/changes" => changes(root, &repo, q, ssh_prefix),
        _ => Err(AppError::new(404, "Page not found.")),
    }
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
    fn working_changes_are_separate_and_read_only() {
        let f = Fixture::new();
        f.put(b"file.txt", b"original\n");
        f.stage(b"file.txt");
        f.commit("Initial");
        f.put(b"file.txt", b"staged\n");
        f.stage(b"file.txt");
        f.put(b"file.txt", b"working\n");
        f.put(b"new.txt", b"new\n");
        f.put(b".gitignore", b"secret.txt\n");
        f.put(b"secret.txt", b"private\n");
        let index = f.directory.join("demo/.git/index");
        let before = std::fs::read(&index).unwrap();
        let page = page(&f.root, "/changes", &f.params(""), None).unwrap();
        assert!(page.body.contains("<h2>Staged</h2>"));
        assert!(page.body.contains("+staged\n"));
        assert!(page.body.contains("+working\n"));
        assert!(page.body.contains("new.txt"));
        assert!(!page.body.contains("secret.txt"));
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert_eq!(
            raw(&f.root, &f.params("path=secret.txt"))
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
        match raw(&f.root, &q).unwrap() {
            Raw::Bytes(bytes) => assert_eq!(bytes, b"<script>alert(1)</script>\n"),
            _ => panic!("expected stored bytes"),
        }
        let listing = page(&f.root, "/files", &f.params(&format!("rev={first}")), None).unwrap();
        assert!(listing.body.contains("odd%FF%2Etxt"));
        let c = page(&f.root, "/commit", &f.params(&format!("id={first}")), None).unwrap();
        assert!(c.body.contains("Root commit"));
        assert!(!c.body.contains("<script>"));
        assert!(c.body.contains("+&lt;script&gt;"));
    }

    #[test]
    fn symlink_targets_are_visible_but_not_read() {
        let f = Fixture::new();
        std::os::unix::fs::symlink("/etc/passwd", f.directory.join("demo/link")).unwrap();
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
    fn configured_filters_are_reported_and_never_executed() {
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
        let p = page(&f.root, "/changes", &f.params(""), None).unwrap();
        assert!(p.body.contains("No filter is executed"));
        assert!(!marker.exists());
    }

    #[test]
    fn compare_uses_endpoint_trees() {
        let f = Fixture::new();
        f.put(b"file.txt", b"one\n");
        f.stage(b"file.txt");
        let first = f.commit("First");
        f.put(b"file.txt", b"two\n");
        f.stage(b"file.txt");
        let second = f.commit("Second");
        let p = page(
            &f.root,
            "/compare",
            &f.params(&format!("from={first}&to={second}")),
            None,
        )
        .unwrap();
        assert!(p.body.contains("-one\n"));
        assert!(p.body.contains("+two\n"));
        assert!(p.body.contains(&second.to_string()));
    }

    #[test]
    fn working_status_uses_one_pinned_head_and_reports_a_concurrent_move() {
        let f = Fixture::new();
        f.put(b"file.txt", b"original\n");
        f.stage(b"file.txt");
        let first = f.commit("First");
        let repo = repository(&f.root, b"demo").unwrap();
        let index = index(&repo).unwrap();
        let tree = repo.git.find_commit(first).unwrap().tree().unwrap();
        let before = fs::stat(&f.root, b"demo/.git/index").ok();

        // Move only HEAD, as another process can do while this request retains
        // its original index and HEAD tree. The disk file remains unchanged.
        let other_blob = f.git.blob(b"another snapshot\n").unwrap();
        let mut builder = f.git.treebuilder(Some(&tree)).unwrap();
        builder.insert("file.txt", other_blob, 0o100644).unwrap();
        let other_tree = f.git.find_tree(builder.write().unwrap()).unwrap();
        let parent = f.git.find_commit(first).unwrap();
        let sig = git2::Signature::now("Terry", "test@example.invalid").unwrap();
        f.git
            .commit(Some("HEAD"), &sig, &sig, "Moved", &other_tree, &[&parent])
            .unwrap();

        let st = fs::stat(&f.root, b"demo/file.txt").unwrap();
        assert_eq!(
            file_status(&f.root, &repo, &index, Some(&tree), b"file.txt", &st).unwrap(),
            "unchanged"
        );
        let mut notice = String::new();
        state_notice(&mut notice, &f.root, &repo, before, Some(first));
        assert!(notice.contains("HEAD or the index changed"));
    }

    #[test]
    fn date_is_utc_and_form_values_preserve_backslashes() {
        assert_eq!(date(0), "1970-01-01 00:00 UTC");
        assert_eq!(date(-60), "1969-12-31 23:59 UTC");
        assert!(hidden("path", b"a\\b").contains("value=\"a\\b\""));
        assert!(hidden("path", b"a\xff\n").contains("name=\"path_hex\" value=\"61ff0a\""));
    }
}
