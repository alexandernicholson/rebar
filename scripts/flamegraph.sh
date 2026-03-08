#!/usr/bin/env bash
set -euo pipefail

# Generate flamegraphs from criterion benchmarks.
#
# Usage:
#   ./scripts/flamegraph.sh <bench-name> [bench-filter]
#
# Examples:
#   ./scripts/flamegraph.sh message_passing
#   ./scripts/flamegraph.sh process_table "allocate_pid"
#   ./scripts/flamegraph.sh supervisor "restart"
#
# Prerequisites:
#   cargo install flamegraph
#   (On Linux: install perf via `sudo apt install linux-tools-common linux-tools-generic`)

BENCH_NAME="${1:?Usage: $0 <bench-name> [bench-filter]}"
BENCH_FILTER="${2:-}"

OUTPUT_DIR="bench/flamegraphs"
mkdir -p "$OUTPUT_DIR"

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
OUTPUT_FILE="${OUTPUT_DIR}/${BENCH_NAME}-${TIMESTAMP}.svg"

echo "==> Generating flamegraph for bench: ${BENCH_NAME}"
if [ -n "$BENCH_FILTER" ]; then
    echo "    Filter: ${BENCH_FILTER}"
fi
echo "    Output: ${OUTPUT_FILE}"
echo ""

CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"

FLAMEGRAPH_ARGS=(
    flamegraph
    --bench "$BENCH_NAME"
    -p rebar-core
    -o "$OUTPUT_FILE"
    --
)

if [ -n "$BENCH_FILTER" ]; then
    FLAMEGRAPH_ARGS+=("$BENCH_FILTER")
fi

"$CARGO" "${FLAMEGRAPH_ARGS[@]}"

echo ""
echo "==> Flamegraph saved to: ${OUTPUT_FILE}"
echo "    Open in a browser to explore."
