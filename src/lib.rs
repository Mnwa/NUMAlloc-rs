mod allocator;
mod freelist;
mod heap;
mod node_heap;
mod platform;
mod size_class;
mod sys_box;
mod thread_heap;
#[cfg(any(test, feature = "internal-testing"))]
pub mod validate;

pub use allocator::NumaAlloc;
