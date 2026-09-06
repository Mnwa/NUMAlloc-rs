//! Replays every committed seed and regression input through the exact
//! harness bodies used by AFL.  Runs under plain `cargo test` in `fuzz/afl`.
use std::path::Path;

fn replay_dir(dir: &str, f: fn(&[u8])) -> usize {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    let mut paths: Vec<_> = entries.map(|e| e.unwrap().path()).collect();
    paths.sort();
    let mut n = 0;
    for p in paths {
        if p.is_file() && p.extension().is_none_or(|e| e != "md") {
            let data = std::fs::read(&p).unwrap();
            eprintln!("replaying {}", p.display());
            f(&data);
            n += 1;
        }
    }
    n
}

#[test]
fn corpus_alloc_ops() {
    assert!(replay_dir("corpus/alloc_ops", numalloc_afl::fuzz_ops) > 0);
}

#[test]
fn corpus_alloc_threads() {
    assert!(replay_dir("corpus/alloc_threads", numalloc_afl::fuzz_threads) > 0);
}

#[test]
fn regressions_alloc_ops() {
    replay_dir("regressions/alloc_ops", numalloc_afl::fuzz_ops);
}

#[test]
fn regressions_alloc_threads() {
    replay_dir("regressions/alloc_threads", numalloc_afl::fuzz_threads);
}

#[test]
fn empty_and_tiny_inputs() {
    for data in [&[][..], &[0][..], &[0, 0][..], &[255][..], &[7, 3, 0][..]] {
        numalloc_afl::fuzz_ops(data);
        numalloc_afl::fuzz_threads(data);
    }
}
