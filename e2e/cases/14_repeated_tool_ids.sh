#!/usr/bin/env bash
# Exercise the real TUI/storage join with duplicate raw IDs in both sequential
# and parallel tool turns, then verify canonical IDs and wire correlators.
set -uo pipefail
export E2E_REPEAT_CALL_IDS=1
export E2E_BATCHING_CASE_NAME="14_repeated_tool_ids: durable cards and provider correlation"
exec bash "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/12_inspection_batching.sh"
