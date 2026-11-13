//! Filesystem confinement is installed before the runtime creates threads.
use crate::types::{AppError, Result};
use std::{
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
};

pub struct Root {
    path: PathBuf,
    dir: File,
    staging: std::sync::Mutex<Option<File>>,
}

pub fn validate_relative(path: &[u8], allow_empty: bool) -> Result<()> {
    if path.is_empty() && allow_empty {
        return Ok(());
    }
    if path.is_empty()
        || path.len() > 4096
        || path.contains(&0)
        || path.starts_with(b"/")
        || path
            .split(|x| *x == b'/')
            .any(|s| s.is_empty() || s == b"." || s == b"..")
    {
        return Err(AppError::bad(
            "Use a relative path without empty, dot, or parent components.",
        ));
    }
    Ok(())
}

impl Root {
    pub fn new(path: &Path) -> Result<Self> {
        let path = path.canonicalize()?;
        if path == Path::new("/") {
            return Err(AppError::bad(
                "The filesystem root cannot be the repository directory.",
            ));
        }
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&path)?;
        let root = Self {
            path,
            dir,
            staging: std::sync::Mutex::new(None),
        };
        root.open(b"", true)?; // Verify that openat2 is supported, without a weaker fallback.
        Ok(root)
    }
    pub fn open(&self, relative: &[u8], directory: bool) -> Result<File> {
        validate_relative(relative, true)?;
        let name = CString::new(if relative.is_empty() {
            b".".as_slice()
        } else {
            relative
        })
        .map_err(|_| AppError::bad("Invalid path."))?;
        open_beneath(self.dir.as_raw_fd(), &name, directory)
    }
    pub fn checked_path(&self, relative: &[u8]) -> Result<PathBuf> {
        let _checked = self.open(relative, false)?;
        Ok(self.path.join(OsStr::from_bytes(relative)))
    }

    /// Create only new directories, then atomically expose a fully initialized repository.
    pub fn create(&self, relative: &[u8], bare: bool, branch: &str) -> Result<()> {
        validate_relative(relative, false)?;
        let name = std::str::from_utf8(relative)
            .map_err(|_| AppError::bad("New repository names must be UTF-8."))?;
        if name.chars().any(char::is_control)
            || relative
                .split(|c| *c == b'/')
                .any(|s| s.eq_ignore_ascii_case(b".git") || s.starts_with(b".gitt-"))
        {
            return Err(AppError::bad(
                "Repository names cannot contain control characters, .git, or reserved .gitt- names.",
            ));
        }
        if branch.is_empty() || !git2::Reference::is_valid_name(&format!("refs/heads/{branch}")) {
            return Err(AppError::bad("Choose a valid Git branch name."));
        }
        let parts: Vec<&[u8]> = relative.split(|b| *b == b'/').collect();
        let mut parent = self.dir.try_clone()?;
        for part in &parts[..parts.len() - 1] {
            reject_repository_parent(&parent)?;
            let c = CString::new(*part).unwrap();
            let made = unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o2770) };
            if made != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(std::io::Error::last_os_error().into());
            }
            parent = open_beneath(parent.as_raw_fd(), &c, true)?;
        }
        reject_repository_parent(&parent)?;
        let final_name = CString::new(*parts.last().unwrap()).unwrap();
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                final_name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Err(AppError::new(
                409,
                "That destination already exists. Nothing was changed.",
            ));
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
            return Err(std::io::Error::last_os_error().into());
        }
        let private = self.private_staging()?;
        let staging = CString::new(format!("repo-{}", random_hex(16)?)).unwrap();
        if unsafe { libc::mkdirat(private.as_raw_fd(), staging.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let stage = open_beneath(private.as_raw_fd(), &staging, true)?;
        let stage_path = PathBuf::from(format!(
            "/proc/self/fd/{}/{}",
            private.as_raw_fd(),
            staging.to_str().unwrap()
        ));
        let result = (|| {
            // libgit2 canonicalizes paths. A short-lived thread receives an additional
            // write boundary pinned to the staging inode. Even renamed ancestors cannot
            // redirect its writes into existing repositories. The private parent protects
            // the source name used for atomic publication and cleanup.
            std::thread::scope(|scope| -> Result<()> {
                std::thread::Builder::new()
                    .name("gitt-create".into())
                    .stack_size(256 * 1024)
                    .spawn_scoped(scope, || -> Result<()> {
                        confine_creation(&stage)?;
                        let mut options = git2::RepositoryInitOptions::new();
                        options
                            .bare(bare)
                            .external_template(false)
                            .initial_head(branch)
                            .mkdir(false)
                            .mkpath(false)
                            .no_reinit(true)
                            .mode(git2::RepositoryInitMode::SHARED_UMASK);
                        let repo = git2::Repository::init_opts(
                            format!("/proc/self/fd/{}", stage.as_raw_fd()),
                            &options,
                        )?;
                        // Probe mode bits only through an owned file descriptor; libgit2's
                        // path-based chmod probe is disabled by the thread's seccomp filter.
                        let config_name =
                            CString::new(if bare { "config" } else { ".git/config" }).unwrap();
                        let config_file = open_beneath(stage.as_raw_fd(), &config_name, false)?;
                        let original = config_file.metadata()?.mode() & 0o777;
                        config_file
                            .set_permissions(std::fs::Permissions::from_mode(original ^ 0o100))?;
                        let filemode = config_file.metadata()?.mode() & 0o777 != original;
                        config_file.set_permissions(std::fs::Permissions::from_mode(0o660))?;
                        let mut config = repo.config()?;
                        config.set_bool("core.filemode", filemode)?;
                        config.set_i32("core.sharedrepository", 1)?;
                        config.set_bool("receive.denyNonFastforwards", true)?;
                        Ok(())
                    })?
                    .join()
                    .map_err(|_| AppError::new(500, "Repository initialization failed."))?
            })?;
            share_new_repository(&stage)?;
            reject_repository_parent(&parent)?;
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    private.as_raw_fd(),
                    staging.as_ptr(),
                    parent.as_raw_fd(),
                    final_name.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                return Err(if e.raw_os_error() == Some(libc::EEXIST) {
                    AppError::new(
                        409,
                        "That destination was created by another operation. Nothing was overwritten.",
                    )
                } else {
                    e.into()
                });
            }
            Ok(())
        })();
        if result.is_err() {
            // This child is inside a service-owned 0700 parent held open by FD.
            // Other group members cannot substitute it. Never sweep older stages.
            let _ = std::fs::remove_dir_all(&stage_path);
        }
        result
    }

    fn private_staging(&self) -> Result<File> {
        use std::io::{Read, Write};
        let mut cached = self
            .staging
            .lock()
            .map_err(|_| AppError::new(500, "Creation staging is unavailable."))?;
        if let Some(file) = cached.as_ref() {
            return Ok(file.try_clone()?);
        }
        let name = CString::new(format!(".gitt-create-private-{}", unsafe {
            libc::geteuid()
        }))
        .unwrap();
        let made = unsafe { libc::mkdirat(self.dir.as_raw_fd(), name.as_ptr(), 0o700) };
        if made != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
            return Err(std::io::Error::last_os_error().into());
        }
        let directory = open_beneath(self.dir.as_raw_fd(), &name, true)?;
        let meta = directory.metadata()?;
        if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o777 != 0o700 {
            return Err(AppError::new(
                409,
                "The reserved creation directory must be owned by the service user with mode 0700.",
            ));
        }
        let marker = CString::new("owner").unwrap();
        const CONTENT: &[u8] = b"gitt private creation staging v1\n";
        if made == 0 {
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    marker.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            unsafe { File::from_raw_fd(fd) }.write_all(CONTENT)?;
        } else {
            let marker = open_beneath(directory.as_raw_fd(), &marker, false)?;
            let mut bytes = Vec::new();
            marker.take(128).read_to_end(&mut bytes)?;
            if bytes != CONTENT {
                return Err(AppError::new(
                    409,
                    "The reserved creation directory is not owned by gitt. Nothing was changed.",
                ));
            }
        }
        *cached = Some(directory.try_clone()?);
        Ok(directory)
    }

    pub fn confine(&self) -> Result<i32> {
        // ABI 3 adds TRUNCATE, needed for a complete write boundary.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<u8>(),
                0,
                1,
            )
        } as i32;
        if abi < 3 {
            return Err(AppError::new(
                500,
                format!(
                    "Linux Landlock ABI 3 or newer must be enabled (kernel reports {abi}). No insecure fallback is available."
                ),
            ));
        }
        #[repr(C)]
        struct Ruleset {
            fs: u64,
            net: u64,
        }
        #[repr(C, packed)]
        struct Beneath {
            allowed: u64,
            parent: i32,
        }
        let handled_fs = (1u64 << 15) - 1; // ABI 3: EXECUTE through TRUNCATE.
        // ABI >=4 also prevents outbound TCP. Git's transport code is not compiled in.
        let attr = Ruleset {
            fs: handled_fs,
            net: if abi >= 4 { 2 } else { 0 },
        };
        let size = if abi >= 4 {
            std::mem::size_of::<Ruleset>()
        } else {
            8
        };
        let rules = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, size, 0) };
        if rules < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let rules = unsafe { File::from_raw_fd(rules as i32) };
        // Permit read + ordinary file/directory creation inside the watched directory.
        // No executable files, devices, FIFOs, or sockets. Symlinks are needed for Git initialization probes; application reads never follow them.
        let allowed = (1 << 1)
            | (1 << 2)
            | (1 << 3)
            | (1 << 4)
            | (1 << 5)
            | (1 << 7)
            | (1 << 8)
            | (1 << 12)
            | (1 << 13)
            | (1 << 14);
        let path = Beneath {
            allowed,
            parent: self.dir.as_raw_fd(),
        };
        if unsafe { libc::syscall(libc::SYS_landlock_add_rule, rules.as_raw_fd(), 1, &path, 0) } < 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rules.as_raw_fd(), 0) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(abi)
    }
}

/// Extra policy only on the temporary initialization thread. Read access still
/// inherits the process root policy; every path-based mutation stays in stage.
fn confine_creation(stage: &File) -> Result<()> {
    #[repr(C, packed)]
    struct Beneath {
        allowed: u64,
        parent: i32,
    }
    let writes: u64 = ((1 << 15) - 1) & !((1 << 0) | (1 << 2) | (1 << 3));
    let rules = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &writes, 8, 0) };
    if rules < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let rules = unsafe { File::from_raw_fd(rules as i32) };
    let beneath = Beneath {
        allowed: writes,
        parent: stage.as_raw_fd(),
    };
    if unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            rules.as_raw_fd(),
            1,
            &beneath,
            0,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, rules.as_raw_fd(), 0) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // Landlock does not restrict chmod/chown. Block the path-based variants
    // used by libgit2 probes. fchmod remains available for our pinned new-file FD.
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xc000003e;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xc00000b7;
    let instruction = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
    let mut filter = vec![
        instruction(0x20, 0, 0, 4),
        instruction(0x15, 1, 0, ARCH),
        instruction(0x06, 0, 0, 0x80000000),
        instruction(0x20, 0, 0, 0),
    ];
    let mut denied = vec![
        libc::SYS_fchmodat,
        452,
        libc::SYS_fchownat,
        libc::SYS_fchown,
    ];
    #[cfg(target_arch = "x86_64")]
    denied.extend([libc::SYS_chmod, libc::SYS_chown, libc::SYS_lchown]);
    for syscall in denied {
        filter.push(instruction(0x15, 0, 1, syscall as u32));
        filter.push(instruction(0x06, 0, 0, 0x00050000 | libc::EPERM as u32));
    }
    filter.push(instruction(0x06, 0, 0, 0x7fff0000));
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &program, 0, 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// The entire tree is new and still beneath a private 0700 parent. Work through
/// descriptors and refuse links; no permissions of existing repositories change.
fn share_new_repository(directory: &File) -> Result<()> {
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        let entry = entry?;
        let name = CString::new(entry.file_name().as_bytes())
            .map_err(|_| AppError::bad("Invalid generated path."))?;
        let file = open_beneath(directory.as_raw_fd(), &name, false)?;
        if file.metadata()?.is_dir() {
            share_new_repository(&file)?;
        } else {
            file.set_permissions(std::fs::Permissions::from_mode(0o660))?;
        }
    }
    directory.set_permissions(std::fs::Permissions::from_mode(0o2770))?;
    Ok(())
}

fn open_beneath(fd: i32, name: &CString, directory: bool) -> Result<File> {
    #[repr(C)]
    struct How {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    let how = How {
        flags: flags as u64,
        mode: 0,
        resolve: 0x08 | 0x04 | 0x02,
    };
    let opened = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            fd,
            name.as_ptr(),
            &how,
            std::mem::size_of::<How>(),
        )
    };
    if opened < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(opened as i32) };
    let ty = file.metadata()?.file_type();
    if !ty.is_dir() && !ty.is_file() {
        return Err(AppError::limit("Special files cannot be opened."));
    }
    Ok(file)
}

fn reject_repository_parent(parent: &File) -> Result<()> {
    let path = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    if path.join(".git").symlink_metadata().is_ok()
        || (path.join("HEAD").symlink_metadata().is_ok()
            && path.join("objects").symlink_metadata().is_ok())
    {
        return Err(AppError::bad(
            "Repositories cannot be created inside an existing repository.",
        ));
    }
    Ok(())
}

pub fn random_hex(len: usize) -> Result<String> {
    let mut bytes = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = unsafe { libc::getrandom(bytes[done..].as_mut_ptr().cast(), len - done, 0) };
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        done += n as usize;
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn initialize_git() -> Result<()> {
    // Only called by main before creating any threads or opening repositories.
    unsafe {
        libc::umask(0o007);
        for (key, _) in std::env::vars_os().filter(|(k, _)| k.as_bytes().starts_with(b"GIT_")) {
            std::env::remove_var(key);
        }
        for level in [
            git2::ConfigLevel::System,
            git2::ConfigLevel::Global,
            git2::ConfigLevel::XDG,
            git2::ConfigLevel::ProgramData,
        ] {
            git2::opts::set_search_path(level, "/__gitt_no_global_configuration__")?;
        }
        git2::opts::set_cache_max_size(2 * 1024 * 1024)?;
        git2::opts::set_cache_object_limit(git2::ObjectType::Blob, 0)?;
        git2::opts::set_mwindow_size(1024 * 1024)?;
        git2::opts::set_mwindow_mapped_limit(4 * 1024 * 1024)?;
        git2::opts::set_mwindow_file_limit(16)?;
        // Shared Unix-group ownership is intentional. The explicit root + kernel policy is the boundary.
        git2::opts::set_verify_owner_validation(false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_are_relative() {
        for p in [b"/etc".as_slice(), b"a/../b", b"a//b", b"a/./b", b"a\0b"] {
            assert!(validate_relative(p, false).is_err());
        }
        assert!(validate_relative(b"team/spaces here", false).is_ok());
    }
    #[test]
    fn symlink_escape_is_refused() {
        let dir = std::env::temp_dir().join(format!("gitt-test-{}", random_hex(8).unwrap()));
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("outside")).unwrap();
        let root = Root::new(&dir).unwrap();
        assert!(root.open(b"outside/passwd", false).is_err());
        std::fs::remove_file(dir.join("outside")).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
