#!/usr/bin/env bash
# Boots the real binary against the isolated env and proves it lands in the
# chat view (not an error / empty state), with the composer meta line present.
set -uo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/lib.sh"
e2e_begin "00_startup: boots to chat"

echo "wide terminal (140x40): full chat view"
tui_start 140 40 || e2e_done
expect      "welcome banner renders"   "Welcome to bonsai"
expect_meta "agent axis is shown"      "Agent ·"
forbid      "no panic in pane"         "panicked"

echo "short terminal (120x8): compact header pill instead of composer"
# The meta line is hidden on short terminals, so wait on the header instead.
tui_start 120 8 "✦ bonsai" || e2e_done
expect "compact ready pill"            "ready"
expect "compact header names the app"  "bonsai"

e2e_done
