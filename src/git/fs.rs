//! Descriptor-relative repository discovery and Git metadata checks.
use crate::security::Root;
use crate::types::{AppError, Result};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::fd::AsRawFd;

pub fn join(parent: &[u8], child: &[u8]) -> Vec<u8> {
    let mut path = parent.to_vec();
    if !path.is_empty() && !child.is_empty() {
        path.push(b'/');
    }
    path.extend_from_slice(child);
    path
}

pub fn valid(path: &[u8]) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    if path.len() > 4096
        || path
            .split(|b| *b == b'/')
            .any(|p| p.is_empty() || p == b"." || p == b".." || p == b".git" || p.contains(&0))
    {
        return Err(AppError::bad("Invalid repository or file path."));
    }
    Ok(())
}

pub fn parent_name(path: &[u8]) -> (&[u8], &[u8]) {
    path.iter()
        .rposition(|b| *b == b'/')
        .map(|i| (&path[..i], &path[i + 1..]))
        .unwrap_or((b"", path))
}

pub fn stat(root: &Root, path: &[u8]) -> Result<libc::stat> {
    let (parent, name) = parent_name(path);
    let dir = root.open(parent, true)?;
    let name = CString::new(if name.is_empty() { b"." } else { name })
        .map_err(|_| AppError::bad("Invalid path."))?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: name is NUL terminated, dir lives through the call, and fstatat initializes st on success.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { st.assume_init() })
}

pub fn is_dir(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFDIR
}
pub fn is_file(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

pub struct Entries {
    pub names: Vec<Vec<u8>>,
    pub incomplete: bool,
}

pub fn entries(file: &File, limit: usize) -> Result<Entries> {
    // fdopendir takes ownership of this duplicate; the original anchored descriptor stays open.
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err(std::io::Error::last_os_error().into());
    }
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let dir = Dir(dir);
    let mut names = Vec::new();
    let mut incomplete = false;
    loop {
        // readdir's storage is copied before another call; this DIR is local to this thread.
        unsafe {
            *libc::__errno_location() = 0;
        }
        let ent = unsafe { libc::readdir(dir.0) };
        if ent.is_null() {
            if unsafe { *libc::__errno_location() } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() == limit {
            incomplete = true;
            break;
        }
        names.push(name.to_vec());
    }
    names.sort();
    Ok(Entries { names, incomplete })
}
