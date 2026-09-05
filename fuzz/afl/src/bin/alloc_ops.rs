//! AFL++ adapter for the sequential allocator model target.
//! See `numalloc_afl::alloc_ops` for the target body and oracle.
//!
//! Built by `cargo afl build` (which sets `cfg(fuzzing)`) this runs the
//! persistent AFL++ loop. Built by plain `cargo build` it reads one input
//! from stdin and runs the body once, for replay and debugging.

#[cfg(fuzzing)]
fn main() {
    numalloc_afl::warm_up();
    afl::fuzz!(|data: &[u8]| {
        numalloc_afl::alloc_ops(data);
    });
}

#[cfg(not(fuzzing))]
fn main() {
    use std::io::Read;
    let mut data = Vec::new();
    std::io::stdin()
        .read_to_end(&mut data)
        .expect("read input from stdin");
    numalloc_afl::alloc_ops(&data);
}
