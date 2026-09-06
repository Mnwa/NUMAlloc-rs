//! AFL adapter for the sequential state-machine target.
//! Without `cfg(fuzzing)` (plain `cargo build`) it replays one input from stdin.
fn main() {
    #[cfg(fuzzing)]
    afl::fuzz!(|data: &[u8]| {
        numalloc_afl::fuzz_ops(data);
    });
    #[cfg(not(fuzzing))]
    numalloc_afl::fuzz_ops(&numalloc_afl::read_stdin());
}
