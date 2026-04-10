use std::ptr::NonNull;

/// NUMA topology information.
pub struct NumaTopology {
    pub num_nodes: usize,
}

/// Detect the NUMA topology of the current system.
pub fn detect_topology() -> NumaTopology {
    #[cfg(target_os = "linux")]
    {
        let num_nodes = detect_numa_nodes_linux().unwrap_or(1);
        NumaTopology { num_nodes }
    }
    #[cfg(not(target_os = "linux"))]
    {
        NumaTopology { num_nodes: 1 }
    }
}

#[cfg(target_os = "linux")]
fn detect_numa_nodes_linux() -> std::io::Result<usize> {
    let mut count = 0usize;
    for entry in std::fs::read_dir("/sys/devices/system/node/")? {
        let name = entry?.file_name();
        if name.to_string_lossy().starts_with("node") {
            count += 1;
        }
    }
    Ok(count.max(1))
}

/// Allocate anonymous memory via `mmap`.
///
/// # Safety
/// Caller must ensure `size > 0`.
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
pub unsafe fn munmap(ptr: NonNull<u8>, size: usize) {
    unsafe { libc::munmap(ptr.as_ptr() as *mut libc::c_void, size) };
}

/// Bind a memory region to a specific NUMA node via `mbind`.
///
/// # Safety
/// `ptr` and `size` must describe a valid, mmap'd region.
#[cfg(target_os = "linux")]
pub unsafe fn bind_to_node(ptr: NonNull<u8>, size: usize, node: usize) {
    let nodemask: u64 = 1u64 << node;
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

#[cfg(not(target_os = "linux"))]
pub unsafe fn bind_to_node(_ptr: NonNull<u8>, _size: usize, _node: usize) {}

/// Bind the calling thread to all CPUs belonging to `node`.
#[cfg(target_os = "linux")]
pub fn bind_thread_to_node(node: usize) {
    unsafe {
        let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
        if let Ok(cpulist) =
            std::fs::read_to_string(format!("/sys/devices/system/node/node{node}/cpulist"))
        {
            for range in cpulist.trim().split(',') {
                if let Some((start, end)) = range.split_once('-') {
                    let s: usize = start.parse().unwrap_or(0);
                    let e: usize = end.parse().unwrap_or(s);
                    for cpu in s..=e {
                        libc::CPU_SET(cpu, &mut cpuset);
                    }
                } else if let Ok(cpu) = range.parse::<usize>() {
                    libc::CPU_SET(cpu, &mut cpuset);
                }
            }
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpuset);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn bind_thread_to_node(_node: usize) {}

/// Return the system page size (cached after first call).
pub fn page_size() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    let val = CACHED.load(Ordering::Relaxed);
    if val != 0 {
        return val;
    }
    let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
    CACHED.store(ps, Ordering::Relaxed);
    ps
}

