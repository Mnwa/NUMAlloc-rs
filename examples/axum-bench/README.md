# axum-bench

A simple Axum HTTP server for benchmarking NUMAlloc against system and mimalloc allocators under realistic web workloads.

## Endpoints

| Endpoint  | Response size | Description                              |
|-----------|---------------|------------------------------------------|
| `/small`  | ~32 B         | Minimal JSON struct (id + bool)          |
| `/medium` | ~256 B        | Moderate JSON with strings and vectors   |
| `/large`  | ~16 KB        | Nested JSON with 50 items and a hash map |
| `/bulk`   | ~64 KB        | Array of 200 medium-sized objects        |

## Allocator features

The project supports three mutually exclusive features that select the global allocator:

| Feature    | Allocator              |
|------------|------------------------|
| `system`   | Default system (glibc) |
| `numalloc` | NUMAlloc               |
| `mimalloc` | mimalloc               |

The `system` feature is enabled by default.

## Prerequisites

- [Rust](https://rustup.rs/) (stable toolchain)
- [wrk](https://github.com/wg/wrk) - HTTP benchmarking tool

Install wrk:

```sh
# macOS
brew install wrk

# Debian/Ubuntu
sudo apt install wrk

# Build from source
git clone https://github.com/wg/wrk.git && cd wrk && make && sudo cp wrk /usr/local/bin/
```

## Quick start

Build and run with a specific allocator:

```sh
# NUMAlloc
cargo run --release --no-default-features --features numalloc

# mimalloc
cargo run --release --no-default-features --features mimalloc

# System allocator (default)
cargo run --release
```

The server starts on `http://127.0.0.1:3000`.

## Running benchmarks

### Automated

The included `bench.sh` script builds all three variants, runs [wrk](https://github.com/wg/wrk) against every endpoint, and reports throughput and RSS memory usage:

```sh
bash bench.sh
```

Requires `wrk` to be installed (`brew install wrk` on macOS, `apt install wrk` on Debian/Ubuntu).

### Manual

Start the server in one terminal:

```sh
cargo run --release --no-default-features --features numalloc
```

Run wrk in another:

```sh
wrk -t4 -c100 -d10s http://127.0.0.1:3000/bulk
```

Check memory usage while the server is running:

```sh
ps -o pid,rss,command -p $(pgrep axum-bench)
```

## Results

Measured on a single run with 4 wrk threads, 100 connections, 10 seconds per endpoint:

| Endpoint  | numalloc       | system     | mimalloc   |
|-----------|----------------|------------|------------|
| `/small`  | **49,124 rps** | 48,286 rps | 48,310 rps |
| `/medium` | **49,135 rps** | 48,892 rps | 49,017 rps |
| `/large`  | **46,600 rps** | 46,244 rps | 46,511 rps |
| `/bulk`   | **37,558 rps** | 35,121 rps | 37,155 rps |

| Allocator | RSS after load |
|-----------|----------------|
| system    | 11 MB          |
| numalloc  | 16 MB          |
| mimalloc  | 27 MB          |

numalloc delivers the highest throughput across all endpoints, with the largest gain (+6.9%) on the allocation-heavy `/bulk` path. On a multi-socket NUMA machine the advantage would be greater due to reduced remote memory accesses.
