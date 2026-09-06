use std::ptr::NonNull;

/// NUMA topology information.
pub struct NumaTopology {
    pub num_nodes: usize,
}

/// Detect the NUMA topology of the current system.
pub fn detect_topology() -> NumaTopology {
    #[cfg(all(target_os = "linux", not(miri)))]
    {
        let num_nodes = detect_numa_nodes_linux();
        NumaTopology { num_nodes }
    }
    #[cfg(not(all(target_os = "linux", not(miri))))]
    {
        NumaTopology { num_nodes: 1 }
    }
}

/// Length of the NUL-terminated sysfs cpulist path for one node.
#[cfg(all(target_os = "linux", any(test, not(miri))))]
const CPULIST_PATH_LEN: usize = b"/sys/devices/system/node/node0/cpulist\0".len();

/// Build `/sys/devices/system/node/node<N>/cpulist` without allocating.
///
/// `node` must be a single decimal digit (`MAX_NODES` is 8).
#[cfg(all(target_os = "linux", any(test, not(miri))))]
fn node_cpulist_path(node: usize) -> [u8; CPULIST_PATH_LEN] {
    debug_assert!(node < 10);
    let mut path = *b"/sys/devices/system/node/node0/cpulist\0";
    // The digit sits right after the "node" prefix; compute its index from
    // the template rather than hard-coding it.
    const DIGIT: usize = b"/sys/devices/system/node/node".len();
    path[DIGIT] = b'0' + (node % 10) as u8;
    path
}

// Sysfs discovery must not call the global allocator during OnceLock init.
#[cfg(all(target_os = "linux", not(miri)))]
fn read_sysfs(path: &std::ffi::CStr, output: &mut [u8]) -> Option<usize> {
    // SAFETY: path is NUL terminated and output is writable for its length.
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
        if fd < 0 {
            return None;
        }
        let count = libc::read(fd, output.as_mut_ptr().cast(), output.len());
        libc::close(fd);
        usize::try_from(count).ok()
    }
}

#[cfg(all(target_os = "linux", not(miri)))]
fn detect_numa_nodes_linux() -> usize {
    // Probe actual node directories without allocating. node_ids in topology
    // are physical IDs, so sparse online node numbers are preserved.
    let mut highest = 0;
    for node in 0..crate::heap::MAX_NODES {
        let path = node_cpulist_path(node);
        let path = std::ffi::CStr::from_bytes_with_nul(&path).unwrap();
        if read_sysfs(path, &mut [0u8; 1]).is_some() {
            highest = node + 1;
        }
    }
    highest.max(1)
}

/// Allocate anonymous memory via `mmap`.
///
/// # Safety
/// Caller must ensure `size > 0`.
#[cfg(not(miri))]
pub unsafe fn mmap_anonymous(size: usize) -> Option<NonNull<u8>> {
    #[cfg(target_os = "macos")]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    #[cfg(not(target_os = "macos"))]
    let flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            flags,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    NonNull::new(ptr as *mut u8)
}

/// Release memory previously obtained from [`mmap_anonymous`].
///
/// # Safety
/// `ptr` must originate from `mmap_anonymous` and `size` must match.
#[cfg(not(miri))]
pub unsafe fn munmap(ptr: NonNull<u8>, size: usize) {
    unsafe { libc::munmap(ptr.as_ptr() as *mut libc::c_void, size) };
}

/// Under Miri, back "mappings" with the system allocator so that every
/// portable allocator path (freelists, bump regions, large headers, caches)
/// runs with full provenance and aliasing checks.  Miri does not model
/// `mbind`, sysfs, or thread affinity, so those become no-ops.
#[cfg(miri)]
fn miri_layout(size: usize) -> std::alloc::Layout {
    std::alloc::Layout::from_size_align(size, page_size()).expect("miri mapping layout")
}

/// See the non-Miri variant.
///
/// # Safety
/// Caller must ensure `size > 0`.
#[cfg(miri)]
pub unsafe fn mmap_anonymous(size: usize) -> Option<NonNull<u8>> {
    use std::alloc::GlobalAlloc;
    // SAFETY: size is non-zero per the caller contract.
    NonNull::new(unsafe { std::alloc::System.alloc_zeroed(miri_layout(size)) })
}

/// See the non-Miri variant.
///
/// # Safety
/// `ptr` must originate from `mmap_anonymous` and `size` must match.
#[cfg(miri)]
pub unsafe fn munmap(ptr: NonNull<u8>, size: usize) {
    use std::alloc::GlobalAlloc;
    // SAFETY: ptr/size came from mmap_anonymous above.
    unsafe { std::alloc::System.dealloc(ptr.as_ptr(), miri_layout(size)) };
}

/// Bind a memory region to a specific NUMA node via `mbind`.
///
/// # Safety
/// `ptr` and `size` must describe a valid, mmap'd region.
#[cfg(all(target_os = "linux", not(miri)))]
pub unsafe fn bind_to_node(ptr: NonNull<u8>, size: usize, node: usize) {
    debug_assert!(node < 64);
    let nodemask: u64 = 1u64 << (node % 64);
    unsafe {
        libc::syscall(
            libc::SYS_mbind,
            ptr.as_ptr() as *mut libc::c_void,
            size,
            2i32, // MPOL_BIND
            &nodemask as *const u64,
            64u64,
            0u32,
        );
    }
}

#[cfg(not(all(target_os = "linux", not(miri))))]
pub unsafe fn bind_to_node(_ptr: NonNull<u8>, _size: usize, _node: usize) {}

/// Bind the calling thread to all CPUs belonging to `node`.
#[cfg(all(target_os = "linux", not(miri)))]
pub fn bind_thread_to_node(node: usize) {
    if node >= crate::heap::MAX_NODES {
        return;
    }
    let path = node_cpulist_path(node);
    let path = std::ffi::CStr::from_bytes_with_nul(&path).unwrap();
    let mut buffer = [0u8; 8192];
    let Some(count) = read_sysfs(path, &mut buffer) else {
        return;
    };
    // A truncated list may end in the middle of a CPU ID; leave affinity alone.
    if count == buffer.len() {
        return;
    }
    let Ok(cpulist) = std::str::from_utf8(&buffer[..count]) else {
        return;
    };
    // SAFETY: all-zero cpu_set_t is a valid empty CPU mask.
    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    let mut any = false;
    for range in cpulist.trim().split(',') {
        let (start, end) = range.split_once('-').unwrap_or((range, range));
        let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) else {
            return;
        };
        if start > end {
            return;
        }
        for cpu in start..=end.min(libc::CPU_SETSIZE as usize - 1) {
            // SAFETY: cpu is bounded by CPU_SETSIZE and cpuset is initialized.
            unsafe { libc::CPU_SET(cpu, &mut cpuset) };
            any = true;
        }
    }
    // An empty mask would make sched_setaffinity fail with EINVAL anyway.
    if !any {
        return;
    }
    // SAFETY: cpuset is initialized and its size is passed correctly.
    unsafe { libc::sched_setaffinity(0, std::mem::size_of_val(&cpuset), &cpuset) };
}

#[cfg(not(all(target_os = "linux", not(miri))))]
pub fn bind_thread_to_node(_node: usize) {}

/// Return the system page size (cached after first call).
pub fn page_size() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    let val = CACHED.load(Ordering::Relaxed);
    if val != 0 {
        return val;
    }
    // SAFETY: sysconf has no memory-safety preconditions.
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    // A failed query (-1) or a non-power-of-two answer would break the page
    // rounding arithmetic; fall back to the conventional 4 KiB page.
    let ps = usize::try_from(ps)
        .ok()
        .filter(|p| p.is_power_of_two())
        .unwrap_or(4096);
    CACHED.store(ps, Ordering::Relaxed);
    ps
}

/// Advise the kernel that the given memory range is no longer needed.
/// The kernel may reclaim the physical pages; future accesses will
/// zero-fault them back in.
///
/// # Safety
/// `ptr` and `size` must describe a valid mmap'd region.
#[cfg(not(miri))]
pub unsafe fn madvise_dontneed(ptr: NonNull<u8>, size: usize) {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::madvise(ptr.as_ptr() as *mut libc::c_void, size, libc::MADV_FREE);
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        libc::madvise(ptr.as_ptr() as *mut libc::c_void, size, libc::MADV_DONTNEED);
    }
}

/// Under Miri the backing memory is a system allocation; dropping its
/// contents would be a semantic change, so this is a no-op.
#[cfg(miri)]
pub unsafe fn madvise_dontneed(_ptr: NonNull<u8>, _size: usize) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn cpulist_path_places_digit_after_node_prefix() {
        for node in 0..crate::heap::MAX_NODES {
            let path = node_cpulist_path(node);
            let expected = format!("/sys/devices/system/node/node{node}/cpulist\0");
            assert_eq!(&path[..], expected.as_bytes(), "node {node}");
        }
    }

    #[test]
    fn page_size_is_power_of_two() {
        let ps = page_size();
        assert!(ps.is_power_of_two() && ps >= 4096, "page size {ps}");
    }
}
