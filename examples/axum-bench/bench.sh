#!/bin/bash
set -e

cd "$(dirname "$0")"

DURATION=10
THREADS=4
CONNECTIONS=100
ENDPOINTS=("/small" "/medium" "/large" "/bulk")
ALLOCATORS=("system" "numalloc" "mimalloc")

echo "======================================"
echo "  Axum Allocator Benchmark"
echo "  wrk: ${THREADS}t / ${CONNECTIONS}c / ${DURATION}s"
echo "======================================"

for alloc in "${ALLOCATORS[@]}"; do
    echo ""
    echo "======================================"
    echo "  Allocator: $alloc"
    echo "======================================"

    cargo build --release --no-default-features --features "$alloc" 2>/dev/null

    # Start the server
    ./target/release/axum-bench &
    SERVER_PID=$!
    sleep 1

    # Verify it started
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "ERROR: Server failed to start for $alloc"
        continue
    fi

    # Memory before load
    RSS_BEFORE=$(ps -o rss= -p $SERVER_PID | tr -d ' ')

    for endpoint in "${ENDPOINTS[@]}"; do
        echo ""
        echo "--- $endpoint ---"
        wrk -t$THREADS -c$CONNECTIONS -d${DURATION}s "http://127.0.0.1:3000${endpoint}" 2>&1
    done

    # Memory after load
    RSS_AFTER=$(ps -o rss= -p $SERVER_PID | tr -d ' ')
    echo ""
    echo "Memory (RSS): before=${RSS_BEFORE} KB, after=${RSS_AFTER} KB"

    kill $SERVER_PID 2>/dev/null
    wait $SERVER_PID 2>/dev/null || true
    sleep 1
done

echo ""
echo "======================================"
echo "  Benchmark complete"
echo "======================================"
