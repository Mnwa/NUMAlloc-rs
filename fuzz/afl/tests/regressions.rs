//! Replays every committed seed and regression input through the shared
//! target bodies. Runs under plain `cargo test` in `fuzz/afl` (no AFL++).

use std::fs;
use std::path::Path;

fn replay_dir(dir: &Path, target: fn(&[u8])) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut paths: Vec<_> = entries
        .map(|e| e.expect("read_dir entry").path())
        .filter(|p| {
            p.is_file()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("README"))
        })
        .collect();
    paths.sort();
    for path in &paths {
        let data = fs::read(path).expect("read input");
        eprintln!("replaying {}", path.display());
        target(&data);
    }
    paths.len()
}

fn replay_all(kind: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut total = 0;
    for name in numalloc_afl::TARGETS {
        let target = numalloc_afl::target_by_name(name).expect("known target");
        total += replay_dir(&root.join(kind).join(name), target);
    }
    assert!(total > 0, "no {kind} inputs found");
}

#[test]
fn replay_seed_corpus() {
    replay_all("corpus");
}

#[test]
fn replay_regressions() {
    replay_all("regressions");
}
