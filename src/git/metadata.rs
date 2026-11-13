//! Bounded, shared path-history lookup for the entries visible on one page.
//!
//! Each path follows Git's simplified history: an unchanged merge follows the
//! first TREESAME parent, and a change against every parent belongs to the merge.
//! Object identity includes the Git mode; a directory's identity is its complete
//! subtree. Renames are additions at the new path, without rename detection.
//! Limits and missing objects leave metadata unavailable, never invent a date.

use super::{fs, object_size};
use crate::render::display;
use crate::types::Result;
use git2::{ObjectType, Oid, Repository, Tree};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::rc::Rc;
use std::time::{Duration, Instant};

const PATHS: usize = 200;
const PARENTS: usize = 64;
const TREE_CACHE_BYTES: usize = 2 * 1024 * 1024;
const TREE_CACHE_ENTRIES: usize = 64;
const SHALLOW_BYTES: usize = 256 * 1024;
const SHALLOW_COMMITS: usize = 4096;

#[derive(Clone, Debug)]
pub(super) struct LastChange {
    pub(super) commit: Oid,
    /// Committer time, consistent with gitt's history view; never filesystem time.
    pub(super) timestamp: i64,
    /// Display-safe text, not HTML. The caller must still HTML-escape it.
    pub(super) author: String,
    /// The first message line, bounded and display-safe; still needs HTML escaping.
    pub(super) message: String,
}

#[derive(Default, Debug)]
pub(super) struct HistoryMetadata {
    pub(super) entries: BTreeMap<Vec<u8>, LastChange>,
    pub(super) incomplete: bool,
}

#[derive(Clone, Copy)]
struct Limits {
    duration: Duration,
    commits: usize,
    objects: usize,
    steps: usize,
    object_bytes: usize,
    total_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            duration: Duration::from_millis(75),
            commits: 512,
            objects: 4096,
            steps: 32_768,
            object_bytes: 1024 * 1024,
            total_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
enum Stop {
    Budget,
    Unavailable,
}

type WalkResult<T> = std::result::Result<T, Stop>;

impl From<git2::Error> for Stop {
    fn from(_: git2::Error) -> Self {
        Self::Unavailable
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct State {
    id: Oid,
    mode: i32,
}

struct Commit {
    tree: Oid,
    parents: Vec<Oid>,
    /// Shallow/grafted parents are not sufficient evidence for an initial change.
    boundary: bool,
    change: LastChange,
}

struct Walker<'repo> {
    repo: &'repo Repository,
    limits: Limits,
    started: Instant,
    steps: usize,
    objects: usize,
    bytes: usize,
    commits: BTreeMap<Oid, Rc<Commit>>,
    shallow: BTreeSet<Oid>,
    trees: BTreeMap<Oid, Tree<'repo>>,
    tree_bytes: usize,
}

impl<'repo> Walker<'repo> {
    fn new(repo: &'repo Repository, limits: Limits) -> Self {
        Self {
            repo,
            limits,
            started: Instant::now(),
            steps: 0,
            objects: 0,
            bytes: 0,
            commits: BTreeMap::new(),
            shallow: BTreeSet::new(),
            trees: BTreeMap::new(),
            tree_bytes: 0,
        }
    }

    fn read_shallow(&mut self) -> WalkResult<()> {
        self.check_time()?;
        // find_commit() need not load libgit2's shallow grafts; a revwalk does.
        // Read the boundary IDs explicitly, without following a metadata symlink.
        // O_NONBLOCK also prevents a raced-in FIFO from blocking before fstat.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.repo.path().join("shallow"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(Stop::Unavailable),
        };
        let metadata = file.metadata().map_err(|_| Stop::Unavailable)?;
        if !metadata.is_file() || metadata.len() > SHALLOW_BYTES as u64 {
            return Err(Stop::Unavailable);
        }
        let mut content = Vec::new();
        file.take(SHALLOW_BYTES as u64 + 1)
            .read_to_end(&mut content)
            .map_err(|_| Stop::Unavailable)?;
        if content.len() > SHALLOW_BYTES {
            return Err(Stop::Unavailable);
        }
        self.bytes = content.len();
        if self.bytes > self.limits.total_bytes {
            return Err(Stop::Budget);
        }
        for line in content
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
        {
            self.step()?;
            if self.shallow.len() >= SHALLOW_COMMITS || line.len() != 40 {
                return Err(Stop::Unavailable);
            }
            let id = std::str::from_utf8(line).map_err(|_| Stop::Unavailable)?;
            self.shallow.insert(Oid::from_str(id)?);
        }
        self.check_time()
    }

    fn check_time(&self) -> WalkResult<()> {
        if self.started.elapsed() >= self.limits.duration {
            Err(Stop::Budget)
        } else {
            Ok(())
        }
    }

    fn step(&mut self) -> WalkResult<()> {
        self.check_time()?;
        if self.steps >= self.limits.steps {
            return Err(Stop::Budget);
        }
        self.steps += 1;
        Ok(())
    }

    fn object(&mut self, id: Oid, expected: ObjectType) -> WalkResult<usize> {
        self.step()?;
        if self.objects >= self.limits.objects {
            return Err(Stop::Budget);
        }
        self.objects += 1;
        // Use the same pre-decompression size check as the other Git readers.
        // A second, cached header read accounts for exact cumulative bytes.
        let kind =
            object_size(self.repo, id, self.limits.object_bytes).map_err(|_| Stop::Unavailable)?;
        if kind != expected {
            return Err(Stop::Unavailable);
        }
        let (size, _) = self.repo.odb()?.read_header(id)?;
        self.bytes = self.bytes.checked_add(size).ok_or(Stop::Budget)?;
        if self.bytes > self.limits.total_bytes {
            return Err(Stop::Budget);
        }
        self.check_time()?;
        Ok(size)
    }

    fn commit(&mut self, id: Oid) -> WalkResult<Rc<Commit>> {
        self.check_time()?;
        if let Some(commit) = self.commits.get(&id) {
            return Ok(Rc::clone(commit));
        }
        if self.commits.len() >= self.limits.commits {
            return Err(Stop::Budget);
        }
        self.object(id, ObjectType::Commit)?;
        let commit = self.repo.find_commit(id)?;
        if commit.parent_count() > PARENTS {
            return Err(Stop::Unavailable);
        }
        let parents: Vec<_> = commit.parent_ids().collect();
        // libgit2 applies shallow/graft boundaries to parent_ids(), so a shallow
        // endpoint can look like a root even while its raw header has parents.
        let original: Vec<_> = commit
            .raw_header_bytes()
            .split(|b| *b == b'\n')
            .filter_map(|line| line.strip_prefix(b"parent "))
            .take(PARENTS + 1)
            .map(|id| {
                let id = std::str::from_utf8(id).map_err(|_| Stop::Unavailable)?;
                Oid::from_str(id).map_err(Stop::from)
            })
            .collect::<WalkResult<_>>()?;
        let summary = commit
            .message_bytes()
            .split(|b| *b == b'\n')
            .next()
            .unwrap_or_default();
        let author = commit.author();
        let data = Rc::new(Commit {
            tree: commit.tree_id(),
            boundary: self.shallow.contains(&id) || original != parents,
            parents,
            change: LastChange {
                commit: id,
                timestamp: commit.time().seconds(),
                author: short_text(author.name_bytes(), 128),
                message: short_text(summary, 256),
            },
        });
        self.check_time()?;
        self.commits.insert(id, Rc::clone(&data));
        Ok(data)
    }

    fn lookup(&mut self, id: Oid, name: &[u8]) -> WalkResult<Option<State>> {
        self.step()?;
        if !self.trees.contains_key(&id) {
            let size = self.object(id, ObjectType::Tree)?;
            // This is a request-local cache. Clear it before loading another tree
            // so old parsed trees do not accumulate across a long history.
            if self.tree_bytes + size > TREE_CACHE_BYTES || self.trees.len() >= TREE_CACHE_ENTRIES {
                self.trees.clear();
                self.tree_bytes = 0;
            }
            let tree = self.repo.find_tree(id)?;
            self.check_time()?;
            self.trees.insert(id, tree);
            self.tree_bytes += size;
        }
        Ok(self.trees[&id].get_name_bytes(name).map(|entry| State {
            id: entry.id(),
            mode: entry.filemode(),
        }))
    }

    fn state(&mut self, tree: Oid, path: &[u8]) -> WalkResult<Option<State>> {
        if path.is_empty() {
            self.step()?;
            return Ok(Some(State {
                id: tree,
                mode: 0o040000,
            }));
        }
        let mut current = tree;
        let mut parts = path.split(|b| *b == b'/').peekable();
        while let Some(name) = parts.next() {
            let Some(entry) = self.lookup(current, name)? else {
                return Ok(None);
            };
            if parts.peek().is_none() {
                return Ok(Some(entry));
            }
            if entry.mode != 0o040000 {
                return Ok(None);
            }
            current = entry.id;
        }
        Ok(None)
    }

    /// An identical parent is the only edge simplified path history follows.
    fn previous(&mut self, commit: &Commit, path: &[u8], state: State) -> WalkResult<Option<Oid>> {
        self.step()?;
        if commit.boundary {
            return Err(Stop::Unavailable);
        }
        for id in &commit.parents {
            let parent = self.commit(*id)?;
            if self.state(parent.tree, path)? == Some(state) {
                return Ok(Some(*id));
            }
        }
        Ok(None)
    }
}

pub(super) fn short_text(bytes: &[u8], limit: usize) -> String {
    let mut end = bytes.len().min(limit);
    // A byte cap must not turn a valid, cut-off character into a visible \xNN.
    // Already-invalid input still uses display()'s byte-preserving notation.
    if let Err(error) = std::str::from_utf8(&bytes[..end])
        && error.error_len().is_none()
    {
        end = error.valid_up_to();
    }
    let mut text = display(&bytes[..end]);
    if bytes.len() > end {
        text.push('…');
    }
    text
}

/// Find the last committed change for at most 200 visible, byte-preserving paths.
/// An absent-at-tip path has no result (ordinary for an untracked working file).
/// An unborn checkout should skip this lookup; a zero OID also returns no history.
///
/// The 75 ms deadline is cooperative: an individual Git read cannot be interrupted.
/// Existing native allocation limits also apply. No blobs or external Git process
/// are needed, and all caches are dropped at the end of this request.
pub(super) fn last_changes(
    repo: &Repository,
    tip: Oid,
    paths: &[Vec<u8>],
) -> Result<HistoryMetadata> {
    last_changes_with_limits(repo, tip, paths, Limits::default())
}

fn last_changes_with_limits(
    repo: &Repository,
    tip: Oid,
    paths: &[Vec<u8>],
    limits: Limits,
) -> Result<HistoryMetadata> {
    let mut result = HistoryMetadata {
        incomplete: paths.len() > PATHS,
        ..HistoryMetadata::default()
    };
    if tip.is_zero() || paths.is_empty() {
        return Ok(result);
    }
    let paths: BTreeSet<_> = paths.iter().take(PATHS).collect();
    for path in &paths {
        fs::valid(path)?;
    }
    let mut walker = Walker::new(repo, limits);
    if walker.read_shallow().is_err() {
        result.incomplete = true;
        return Ok(result);
    }
    let start = match walker.commit(tip) {
        Ok(start) => start,
        Err(_) => {
            result.incomplete = true;
            return Ok(result);
        }
    };
    let mut pending = VecDeque::new();
    for path in paths {
        match walker.state(start.tree, path) {
            Ok(Some(state)) => pending.push_back((path, tip, state)),
            Ok(None) => {}
            Err(Stop::Unavailable) => result.incomplete = true,
            Err(Stop::Budget) => {
                result.incomplete = true;
                break;
            }
        }
    }
    // Round-robin traversal prevents one ancient path from consuming the budget
    // before a recently changed neighboring entry has had a chance to resolve.
    while let Some((path, id, state)) = pending.pop_front() {
        let step = walker
            .commit(id)
            .and_then(|commit| Ok((walker.previous(&commit, path, state)?, commit)));
        match step {
            Ok((Some(previous), _)) => pending.push_back((path, previous, state)),
            Ok((None, commit)) => {
                result.entries.insert(path.clone(), commit.change.clone());
            }
            Err(Stop::Unavailable) => result.incomplete = true,
            Err(Stop::Budget) => {
                result.incomplete = true;
                break;
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use git2::{Index, IndexEntry, IndexTime, Signature, Time};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Fixture {
        repo: Repository,
        path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "gitt-path-metadata-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&path).unwrap();
            Self {
                repo: Repository::init_bare(&path).unwrap(),
                path,
            }
        }

        fn commit(
            &self,
            parents: &[Oid],
            files: &[(&[u8], &[u8], u32)],
            message: &str,
            timestamp: i64,
        ) -> Oid {
            let mut index = Index::new().unwrap();
            for (name, content, mode) in files {
                index
                    .add(&IndexEntry {
                        ctime: IndexTime::new(0, 0),
                        mtime: IndexTime::new(0, 0),
                        dev: 0,
                        ino: 0,
                        mode: *mode,
                        uid: 0,
                        gid: 0,
                        file_size: content.len() as u32,
                        id: self.repo.blob(content).unwrap(),
                        flags: 0,
                        flags_extended: 0,
                        path: name.to_vec(),
                    })
                    .unwrap();
            }
            let tree = self
                .repo
                .find_tree(index.write_tree_to(&self.repo).unwrap())
                .unwrap();
            let signature = Signature::new(
                "A. Author",
                "private@example.test",
                &Time::new(timestamp, 0),
            )
            .unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|id| self.repo.find_commit(*id).unwrap())
                .collect();
            self.repo
                .commit(
                    None,
                    &signature,
                    &signature,
                    message,
                    &tree,
                    &parents.iter().collect::<Vec<_>>(),
                )
                .unwrap()
        }

        fn metadata(&self, tip: Oid, paths: &[&[u8]]) -> HistoryMetadata {
            last_changes_with_limits(
                &self.repo,
                tip,
                &paths.iter().map(|path| path.to_vec()).collect::<Vec<_>>(),
                Limits {
                    duration: Duration::from_secs(5),
                    ..Limits::default()
                },
            )
            .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // The fixture exclusively owns this newly created, unique directory.
            std::fs::remove_dir_all(&self.path).unwrap();
        }
    }

    #[test]
    fn edits_initial_commit_directory_mode_and_byte_paths() {
        let fixture = Fixture::new();
        let first = fixture.commit(
            &[],
            &[
                (b"stable", b"same", 0o100644),
                (b"dir/file", b"old", 0o100644),
                (b"run", b"same", 0o100644),
                (b"bad-\xff", b"same", 0o100644),
            ],
            "Initial files",
            100,
        );
        let edited = fixture.commit(
            &[first],
            &[
                (b"stable", b"same", 0o100644),
                (b"dir/file", b"new", 0o100644),
                (b"run", b"same", 0o100755),
                (b"bad-\xff", b"same", 0o100644),
            ],
            "Edit and chmod\n\nBody is not the summary",
            200,
        );
        let result = fixture.metadata(
            edited,
            &[
                b"stable",
                b"dir",
                b"dir/file",
                b"run",
                b"bad-\xff",
                b"untracked",
            ],
        );
        assert!(!result.incomplete);
        assert_eq!(result.entries[b"stable".as_slice()].commit, first);
        assert_eq!(result.entries[b"bad-\xff".as_slice()].commit, first);
        for path in [b"dir".as_slice(), b"dir/file", b"run"] {
            let change = &result.entries[path];
            assert_eq!(change.commit, edited);
            assert_eq!(change.timestamp, 200);
            assert_eq!(change.author, "A. Author");
            assert_eq!(change.message, "Edit and chmod");
            assert!(!change.author.contains("private@example.test"));
        }
        assert!(!result.entries.contains_key(b"untracked".as_slice()));
    }

    #[test]
    fn merges_follow_matching_parent_instead_of_newest_side_commit() {
        let fixture = Fixture::new();
        let root = fixture.commit(&[], &[(b"file", b"base", 0o100644)], "Root", 100);
        let left = fixture.commit(
            &[root],
            &[(b"file", b"left", 0o100644)],
            "Left changed",
            900,
        );
        let right = fixture.commit(
            &[root],
            &[(b"file", b"right", 0o100644)],
            "Right changed",
            200,
        );
        let merge = fixture.commit(
            &[left, right],
            &[(b"file", b"right", 0o100644)],
            "Take right",
            300,
        );
        let result = fixture.metadata(merge, &[b"file"]);
        assert!(!result.incomplete);
        assert_eq!(result.entries[b"file".as_slice()].commit, right);
        let resolution = fixture.commit(
            &[left, right],
            &[(b"file", b"resolution", 0o100644)],
            "Resolve both",
            400,
        );
        assert_eq!(
            fixture.metadata(resolution, &[b"file"]).entries[b"file".as_slice()].commit,
            resolution
        );
        let keep_left = fixture.commit(
            &[root, left],
            &[(b"file", b"base", 0o100644)],
            "Ignore side change",
            1000,
        );
        assert_eq!(
            fixture.metadata(keep_left, &[b"file"]).entries[b"file".as_slice()].commit,
            root
        );
    }

    #[test]
    fn rename_is_an_addition_and_deleted_descendants_change_a_directory() {
        let fixture = Fixture::new();
        let root = fixture.commit(
            &[],
            &[
                (b"dir/old", b"same", 0o100644),
                (b"dir/keep", b"keep", 0o100644),
            ],
            "Root",
            100,
        );
        let renamed = fixture.commit(
            &[root],
            &[
                (b"new", b"same", 0o100644),
                (b"dir/keep", b"keep", 0o100644),
            ],
            "Move out of directory",
            200,
        );
        let result = fixture.metadata(renamed, &[b"new", b"dir", b"dir/keep", b"dir/old"]);
        assert!(!result.incomplete);
        assert_eq!(result.entries[b"new".as_slice()].commit, renamed);
        assert_eq!(result.entries[b"dir".as_slice()].commit, renamed);
        assert_eq!(result.entries[b"dir/keep".as_slice()].commit, root);
        assert!(!result.entries.contains_key(b"dir/old".as_slice()));
    }

    #[test]
    fn shallow_boundaries_are_not_reported_as_initial_changes() {
        let fixture = Fixture::new();
        let root = fixture.commit(&[], &[(b"file", b"old", 0o100644)], "Root", 100);
        let shallow = fixture.commit(
            &[root],
            &[(b"file", b"new", 0o100644)],
            "Shallow boundary",
            200,
        );
        std::fs::write(fixture.path.join("shallow"), format!("{shallow}\n")).unwrap();
        let reopened = Repository::open_bare(&fixture.path).unwrap();
        assert!(reopened.is_shallow());
        let result = last_changes(&reopened, shallow, &[b"file".to_vec()]).unwrap();
        assert!(result.incomplete);
        assert!(result.entries.is_empty());
    }

    #[test]
    fn missing_objects_return_partial_metadata_without_aborting_the_listing() {
        let fixture = Fixture::new();
        let root = fixture.commit(&[], &[(b"old", b"same", 0o100644)], "Root", 100);
        let tip = fixture.commit(
            &[root],
            &[(b"old", b"same", 0o100644), (b"new", b"new", 0o100644)],
            "New file",
            200,
        );
        let id = root.to_string();
        std::fs::remove_file(fixture.path.join("objects").join(&id[..2]).join(&id[2..])).unwrap();
        let reopened = Repository::open_bare(&fixture.path).unwrap();
        let result = last_changes(&reopened, tip, &[b"old".to_vec(), b"new".to_vec()]).unwrap();
        assert!(result.incomplete);
        // Both need their missing parent to establish whether a change occurred.
        assert!(result.entries.is_empty());
    }

    #[test]
    fn budgets_are_partial_and_recent_neighbors_are_not_starved() {
        let fixture = Fixture::new();
        let root = fixture.commit(&[], &[(b"ancient", b"same", 0o100644)], "Root", 100);
        let mut tip = root;
        for n in 0..8 {
            tip = fixture.commit(
                &[tip],
                &[(b"ancient", b"same", 0o100644)],
                "Unrelated empty change",
                101 + n,
            );
        }
        tip = fixture.commit(
            &[tip],
            &[
                (b"ancient", b"same", 0o100644),
                (b"recent", b"new", 0o100644),
            ],
            "Recent file",
            200,
        );
        let paths = [b"ancient".to_vec(), b"recent".to_vec()];
        let result = last_changes_with_limits(
            &fixture.repo,
            tip,
            &paths,
            Limits {
                duration: Duration::from_secs(5),
                commits: 3,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(result.incomplete);
        assert!(!result.entries.contains_key(b"ancient".as_slice()));
        assert_eq!(result.entries[b"recent".as_slice()].commit, tip);

        for limit in [
            Limits {
                duration: Duration::ZERO,
                ..Limits::default()
            },
            Limits {
                steps: 0,
                ..Limits::default()
            },
            Limits {
                objects: 0,
                ..Limits::default()
            },
            Limits {
                object_bytes: 1,
                ..Limits::default()
            },
            Limits {
                total_bytes: 1,
                ..Limits::default()
            },
        ] {
            let result = last_changes_with_limits(&fixture.repo, tip, &paths, limit).unwrap();
            assert!(result.incomplete);
            assert!(result.entries.is_empty());
        }
    }

    #[test]
    fn duplicate_paths_unborn_tips_and_invalid_paths() {
        let fixture = Fixture::new();
        let root = fixture.commit(&[], &[(b"file", b"same", 0o100644)], "Root", 100);
        let result = fixture.metadata(root, &[b"file", b"file"]);
        assert_eq!(result.entries.len(), 1);
        assert!(!result.incomplete);
        let empty = last_changes(&fixture.repo, Oid::ZERO_SHA1, &[b"file".to_vec()]).unwrap();
        assert!(empty.entries.is_empty());
        assert!(!empty.incomplete);
        assert!(last_changes(&fixture.repo, root, &[b"../file".to_vec()]).is_err());
        let result = last_changes(&fixture.repo, root, &vec![b"file".to_vec(); PATHS + 1]).unwrap();
        assert!(result.incomplete);
    }

    #[test]
    fn object_cache_is_shared_and_display_text_is_bounded() {
        let fixture = Fixture::new();
        let root = fixture.commit(
            &[],
            &[(b"one", b"one", 0o100644), (b"two", b"two", 0o100644)],
            &"m".repeat(1000),
            100,
        );
        let result = last_changes_with_limits(
            &fixture.repo,
            root,
            &[b"one".to_vec(), b"two".to_vec()],
            Limits {
                duration: Duration::from_secs(5),
                objects: 2, // One commit and one shared tree; no blob reads.
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(!result.incomplete);
        assert_eq!(result.entries.len(), 2);
        assert_eq!(
            result.entries[b"one".as_slice()].message,
            format!("{}…", "m".repeat(256))
        );
        assert_eq!(short_text(b"a\n\t\xff", 128), "a\\n\\t\\xFF");
        assert_eq!(short_text("éé".as_bytes(), 3), "é…");
        assert_eq!(
            short_text(&[0; 1000], 128),
            format!("{}…", "\\u{0}".repeat(128))
        );
        assert_eq!(short_text(b"<script>&", 128), "<script>&"); // HTML escaping is caller-owned.
    }
}
