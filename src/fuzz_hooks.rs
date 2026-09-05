//! Test seams for fuzzing and stress testing, active only with the
//! `fuzz-hooks` cargo feature. Without the feature every function here is a
//! constant `None` and compiles away.
//!
//! Environment variables (read with `libc::getenv`, so no allocation happens
//! during heap bootstrap):
//!
//! * `NUMALLOC_FUZZ_NODES` — pretend the machine has this many NUMA nodes
//!   (1..=`MAX_NODES`). `mbind` to a missing node fails silently, so the
//!   extra nodes act as independent virtual regions.
//! * `NUMALLOC_FUZZ_REGION_MB` — per-node region size in MiB; must be a
//!   power of two.

#[cfg(feature = "fuzz-hooks")]
fn getenv_usize(name: &core::ffi::CStr) -> Option<usize> {
    // SAFETY: `name` is a valid NUL-terminated string; getenv returns either
    // null or a pointer to a NUL-terminated string owned by the environment.
    let raw = unsafe { libc::getenv(name.as_ptr()) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: non-null result of getenv is a valid C string.
    let bytes = unsafe { core::ffi::CStr::from_ptr(raw) }.to_bytes();
    if bytes.is_empty() || bytes.len() > 12 {
        return None;
    }
    let mut value = 0usize;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(usize::from(b - b'0'))?;
    }
    Some(value)
}

/// Override for the detected NUMA node count.
#[inline]
pub fn num_nodes_override() -> Option<usize> {
    #[cfg(feature = "fuzz-hooks")]
    {
        getenv_usize(c"NUMALLOC_FUZZ_NODES").filter(|&n| (1..=crate::heap::MAX_NODES).contains(&n))
    }
    #[cfg(not(feature = "fuzz-hooks"))]
    {
        None
    }
}

/// Override for the per-node region size in bytes.
#[inline]
pub fn region_size_override() -> Option<usize> {
    #[cfg(feature = "fuzz-hooks")]
    {
        getenv_usize(c"NUMALLOC_FUZZ_REGION_MB")
            .map(|mb| mb << 20)
            .filter(|&bytes| bytes.is_power_of_two() && bytes >= crate::size_class::SMALL_LIMIT)
    }
    #[cfg(not(feature = "fuzz-hooks"))]
    {
        None
    }
}
