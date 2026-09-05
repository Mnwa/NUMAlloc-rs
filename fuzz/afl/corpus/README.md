# Seed corpus

Each file is a program of 4-byte operations `[opcode, a, b, c]` decoded by
`numalloc_afl` (see `src/lib.rs`, `decode_op`). Seeds are hand-written to
cover: every size class and its ±1 boundaries, `alloc_zeroed` on both paths,
realloc chains crossing classes, over-page alignments, churn, region
exhaustion (fallback to mmap), and large-object cache reuse.

Regenerate/minimize with `scripts/cmin.sh` after a campaign; keep this
directory small.
