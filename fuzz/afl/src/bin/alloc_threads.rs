//! AFL adapter for the multi-threaded transfer target.
//! Without `cfg(fuzzing)` (plain `cargo build`) it replays one input from stdin.
fn main() {
    #[cfg(fuzzing)]
    afl::fuzz!(|data: &[u8]| {
        numalloc_afl::fuzz_threads(data);
    });
    #[cfg(not(fuzzing))]
    numalloc_afl::fuzz_threads(&numalloc_afl::read_stdin());
}
