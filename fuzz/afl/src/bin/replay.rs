//! Replay one or more saved inputs through a target body in a single
//! process, in order, timing each one.
//!
//!     replay <target> <input>...
//!
//! Useful for reproducing a finding that only shows up after a particular
//! sequence of inputs, and for checking that saved hangs really are slow.

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(name) = args.next() else {
        eprintln!("usage: replay <alloc_ops|alloc_threads> <input>...");
        std::process::exit(2);
    };
    let Some(target) = numalloc_afl::target_by_name(&name) else {
        eprintln!("unknown target {name}; known: {:?}", numalloc_afl::TARGETS);
        std::process::exit(2);
    };
    for path in args {
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let start = std::time::Instant::now();
        target(&data);
        println!("{:>8.3} s  {path}", start.elapsed().as_secs_f64());
    }
}
