//! Bound libgit2's native heap, including decompressed pack delta bases.
//!
//! The cap is shared by concurrent requests. It is not an RSS cap: Rust
//! allocations, thread stacks, allocator overhead and Git's mmap windows are
//! separate. The mapped-window budget is configured in security.rs.

use crate::types::{AppError, Result};
use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub const LIMIT: usize = 24 * 1024 * 1024;
const ALIGNMENT: usize = std::mem::align_of::<libc::max_align_t>();
// malloc returns max_align_t-aligned memory. A whole alignment unit of metadata
// keeps the returned user pointer aligned for every C type libgit2 allocates.
const HEADER: usize = std::mem::size_of::<usize>().div_ceil(ALIGNMENT) * ALIGNMENT;
static ALLOCATED: AtomicUsize = AtomicUsize::new(0);
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct Allocator {
    gmalloc: unsafe extern "C" fn(usize, *const c_char, c_int) -> *mut c_void,
    grealloc: unsafe extern "C" fn(*mut c_void, usize, *const c_char, c_int) -> *mut c_void,
    gfree: unsafe extern "C" fn(*mut c_void),
}

/// Install exactly once, before any git2/libgit2 call and before starting threads.
///
/// # Safety
/// libgit2 must never have been initialized or used. Changing its allocator
/// after objects exist would mix incompatible allocation and free functions.
pub unsafe fn initialize() -> Result<()> {
    if INSTALLED.swap(true, Ordering::AcqRel) {
        return Err(AppError::new(
            500,
            "The native Git allocator was already installed.",
        ));
    }
    let allocator = Allocator {
        gmalloc: allocate,
        grealloc: resize,
        gfree: release,
    };
    // libgit2 1.9's SET_ALLOCATOR copies this three-pointer struct immediately.
    // Its allocator initialization explicitly preserves an allocator installed
    // before git_libgit2_init, so there are no earlier native allocations.
    let result = unsafe {
        libgit2_sys::git_libgit2_opts(
            libgit2_sys::GIT_OPT_SET_ALLOCATOR as c_int,
            &allocator as *const Allocator,
        )
    };
    if result != 0 {
        return Err(AppError::new(
            500,
            "The native Git allocation limit could not be installed.",
        ));
    }
    Ok(())
}

fn reserve(bytes: usize) -> bool {
    let mut current = ALLOCATED.load(Ordering::Relaxed);
    loop {
        let Some(next) = current.checked_add(bytes).filter(|n| *n <= LIMIT) else {
            return false;
        };
        match ALLOCATED.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

unsafe extern "C" fn allocate(size: usize, _file: *const c_char, _line: c_int) -> *mut c_void {
    let Some(total) = size.checked_add(HEADER) else {
        return std::ptr::null_mut();
    };
    if !reserve(total) {
        return std::ptr::null_mut();
    }
    let base = unsafe { libc::malloc(total) }.cast::<u8>();
    if base.is_null() {
        ALLOCATED.fetch_sub(total, Ordering::Relaxed);
        return std::ptr::null_mut();
    }
    unsafe {
        base.cast::<usize>().write(total);
        base.add(HEADER).cast()
    }
}

unsafe extern "C" fn resize(
    pointer: *mut c_void,
    size: usize,
    file: *const c_char,
    line: c_int,
) -> *mut c_void {
    if pointer.is_null() {
        return unsafe { allocate(size, file, line) };
    }
    if size == 0 {
        unsafe {
            release(pointer);
        }
        return std::ptr::null_mut();
    }
    let Some(total) = size.checked_add(HEADER) else {
        return std::ptr::null_mut();
    };
    let base = unsafe { pointer.cast::<u8>().sub(HEADER) };
    let old = unsafe { base.cast::<usize>().read() };
    let growth = total.saturating_sub(old);
    if !reserve(growth) {
        return std::ptr::null_mut();
    }
    let resized = unsafe { libc::realloc(base.cast(), total) }.cast::<u8>();
    if resized.is_null() {
        ALLOCATED.fetch_sub(growth, Ordering::Relaxed);
        return std::ptr::null_mut();
    }
    if total < old {
        ALLOCATED.fetch_sub(old - total, Ordering::Relaxed);
    }
    unsafe {
        resized.cast::<usize>().write(total);
        resized.add(HEADER).cast()
    }
}

unsafe extern "C" fn release(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    let base = unsafe { pointer.cast::<u8>().sub(HEADER) };
    let total = unsafe { base.cast::<usize>().read() };
    unsafe {
        libc::free(base.cast());
    }
    ALLOCATED.fetch_sub(total, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_allocator_preserves_alignment_data_accounting_and_failure_semantics() {
        // Do not install the process-global allocator in the unit-test process:
        // other tests have already used git2 with its normal allocator.
        unsafe {
            let initial = ALLOCATED.load(Ordering::Relaxed);
            let p = allocate(32, std::ptr::null(), 0).cast::<u8>();
            assert!(!p.is_null());
            assert_eq!(p as usize % ALIGNMENT, 0);
            for i in 0..32 {
                p.add(i).write(i as u8);
            }
            assert_eq!(ALLOCATED.load(Ordering::Relaxed), initial + HEADER + 32);
            let p = resize(p.cast(), 128, std::ptr::null(), 0).cast::<u8>();
            assert!(!p.is_null());
            for i in 0..32 {
                assert_eq!(p.add(i).read(), i as u8);
            }
            assert_eq!(ALLOCATED.load(Ordering::Relaxed), initial + HEADER + 128);
            let p = resize(p.cast(), 16, std::ptr::null(), 0).cast::<u8>();
            assert!(!p.is_null());
            for i in 0..16 {
                assert_eq!(p.add(i).read(), i as u8);
            }
            assert_eq!(ALLOCATED.load(Ordering::Relaxed), initial + HEADER + 16);
            assert!(resize(p.cast(), usize::MAX, std::ptr::null(), 0).is_null());
            assert_eq!(p.read(), 0); // Failed realloc keeps the original allocation alive.
            release(p.cast());
            assert_eq!(ALLOCATED.load(Ordering::Relaxed), initial);
            assert!(allocate(usize::MAX, std::ptr::null(), 0).is_null());
            let near_limit = allocate(LIMIT - HEADER - initial, std::ptr::null(), 0);
            assert!(!near_limit.is_null());
            assert!(allocate(1, std::ptr::null(), 0).is_null());
            release(near_limit);
            release(std::ptr::null_mut());
            let p = resize(std::ptr::null_mut(), 1, std::ptr::null(), 0);
            assert!(!p.is_null());
            assert!(resize(p, 0, std::ptr::null(), 0).is_null());
            assert_eq!(ALLOCATED.load(Ordering::Relaxed), initial);
        }
    }
}
