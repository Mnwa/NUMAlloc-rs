use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;

// --- Allocators under test ---------------------------------------------------

static NUMA_ALLOC: numalloc::NumaAlloc = numalloc::NumaAlloc::new();
static JEMALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
static MIMALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Allocator {
    name: &'static str,
    inner: &'static (dyn GlobalAlloc + Sync),
}

static ALLOCATORS: &[Allocator] = &[
    Allocator {
        name: "system",
        inner: &System,
    },
    Allocator {
        name: "numalloc",
        inner: &NUMA_ALLOC,
    },
    Allocator {
        name: "mimalloc",
        inner: &MIMALLOC,
    },
    Allocator {
        name: "jemalloc",
        inner: &JEMALLOC,
    },
];

// --- Send wrapper for raw pointers -------------------------------------------

/// Wrapper to send `*mut u8` across threads in benchmarks.
///
/// # Safety
/// The pointer must remain valid and not be used concurrently.
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}

// --- Helpers -----------------------------------------------------------------

const ALL_SIZES: &[usize] = &[8, 64, 256, 1024, 4096, 16384, 65536, 262_144];

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size, 8).unwrap()
}

// --- Single-threaded benchmarks ----------------------------------------------

fn bench_alloc_dealloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread/alloc_dealloc");
    for &size in ALL_SIZES {
        for a in ALLOCATORS {
            group.bench_with_input(BenchmarkId::new(a.name, size), &size, |b, &sz| {
                let lay = layout(sz);
                b.iter(|| unsafe {
                    let ptr = a.inner.alloc(lay);
                    assert!(!ptr.is_null());
                    black_box(ptr);
                    a.inner.dealloc(ptr, lay);
                });
            });
        }
    }
    group.finish();
}

fn bench_bulk_alloc_then_free(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread/bulk_alloc_free");
    const COUNT: usize = 1000;
    for &size in ALL_SIZES {
        for a in ALLOCATORS {
            group.bench_with_input(BenchmarkId::new(a.name, size), &size, |b, &sz| {
                let lay = layout(sz);
                b.iter(|| unsafe {
                    let mut ptrs = Vec::with_capacity(COUNT);
                    for _ in 0..COUNT {
                        let ptr = a.inner.alloc(lay);
                        assert!(!ptr.is_null());
                        ptrs.push(ptr);
                    }
                    for ptr in ptrs {
                        a.inner.dealloc(ptr, lay);
                    }
                });
            });
        }
    }
    group.finish();
}

fn bench_realloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread/realloc");
    let grow_pairs: &[(usize, usize)] = &[(64, 256), (256, 4096), (4096, 65536)];
    for &(from, to) in grow_pairs {
        let label = format!("{from}->{to}");
        for a in ALLOCATORS {
            group.bench_with_input(
                BenchmarkId::new(a.name, &label),
                &(from, to),
                |b, &(f, t)| {
                    let lay_from = layout(f);
                    let lay_to = layout(t);
                    b.iter(|| unsafe {
                        let ptr = a.inner.alloc(lay_from);
                        assert!(!ptr.is_null());
                        let ptr2 = a.inner.realloc(ptr, lay_from, lay_to.size());
                        assert!(!ptr2.is_null());
                        black_box(ptr2);
                        a.inner.dealloc(ptr2, lay_to);
                    });
                },
            );
        }
    }
    group.finish();
}

// --- Multi-threaded benchmarks -----------------------------------------------

fn bench_mt_alloc_dealloc(c: &mut Criterion) {
    let mut group = c.benchmark_group("multi_thread/alloc_dealloc");
    let thread_counts: &[usize] = &[2, 4, 8];
    const OPS_PER_THREAD: usize = 10_000;

    for &size in &[64usize, 1024, 4096] {
        for &nthreads in thread_counts {
            let label = format!("sz{size}_t{nthreads}");
            for a in ALLOCATORS {
                let alloc = a.inner;
                group.bench_with_input(
                    BenchmarkId::new(a.name, &label),
                    &(size, nthreads),
                    |b, &(sz, nt)| {
                        let lay = layout(sz);
                        b.iter(|| {
                            std::thread::scope(|s| {
                                for _ in 0..nt {
                                    s.spawn(|| {
                                        for _ in 0..OPS_PER_THREAD {
                                            unsafe {
                                                let ptr = alloc.alloc(lay);
                                                assert!(!ptr.is_null());
                                                black_box(ptr);
                                                alloc.dealloc(ptr, lay);
                                            }
                                        }
                                    });
                                }
                            });
                        });
                    },
                );
            }
        }
    }
    group.finish();
}

fn bench_mt_producer_consumer(c: &mut Criterion) {
    let mut group = c.benchmark_group("multi_thread/producer_consumer");
    const COUNT: usize = 5_000;

    for &size in &[64usize, 4096] {
        for a in ALLOCATORS {
            let alloc = a.inner;
            group.bench_with_input(BenchmarkId::new(a.name, size), &size, |b, &sz| {
                let lay = layout(sz);
                b.iter(|| {
                    let (tx, rx) = std::sync::mpsc::sync_channel::<SendPtr>(256);
                    std::thread::scope(|s| {
                        // Producer: allocates on this thread
                        s.spawn(|| {
                            for _ in 0..COUNT {
                                let ptr = unsafe { alloc.alloc(lay) };
                                assert!(!ptr.is_null());
                                tx.send(SendPtr(ptr)).unwrap();
                            }
                            drop(tx);
                        });
                        // Consumer: deallocates on different thread (cross-thread dealloc)
                        let rx = rx;
                        s.spawn(move || {
                            while let Ok(SendPtr(ptr)) = rx.recv() {
                                unsafe {
                                    alloc.dealloc(ptr, lay);
                                }
                            }
                        });
                    });
                });
            });
        }
    }
    group.finish();
}

// --- Different page-size aligned allocations ---------------------------------

fn bench_page_aligned(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread/page_aligned");
    let page_sizes: &[(usize, &str)] = &[
        (4096, "4K"),
        (16384, "16K"),
        (65536, "64K"),
        (2 * 1024 * 1024, "2M"),
    ];
    const COUNT: usize = 100;

    for &(page_size, label) in page_sizes {
        for a in ALLOCATORS {
            group.bench_with_input(BenchmarkId::new(a.name, label), &page_size, |b, &ps| {
                let lay = Layout::from_size_align(ps, ps).unwrap();
                b.iter(|| unsafe {
                    let mut ptrs = Vec::with_capacity(COUNT);
                    for _ in 0..COUNT {
                        let ptr = a.inner.alloc(lay);
                        assert!(!ptr.is_null());
                        ptrs.push(ptr);
                    }
                    for ptr in ptrs {
                        a.inner.dealloc(ptr, lay);
                    }
                });
            });
        }
    }
    group.finish();
}

// --- Criterion setup ---------------------------------------------------------

criterion_group!(
    benches,
    bench_alloc_dealloc,
    bench_bulk_alloc_then_free,
    bench_realloc,
    bench_mt_alloc_dealloc,
    bench_mt_producer_consumer,
    bench_page_aligned,
);
criterion_main!(benches);
