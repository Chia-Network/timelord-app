#!/usr/bin/env bash
# Build the macOS Swift sources for CodeQL while tracing only the compiler.
#
# CodeQL's Swift extractor works by injecting a tracer into every process the
# build starts. That makes the build itself (SwiftPM planning, the manifest
# sandbox, linking) many times slower, and on this package it spends most of
# the step in a long silent pause before SwiftPM prints anything.
#
# This script clears DYLD_INSERT_LIBRARIES so SwiftPM plans and links natively,
# and routes every Swift compilation through a generated swiftc wrapper that
# starts the compiler under the tracer. The wrapper carries the CodeQL
# variables itself because SwiftPM starts the manifest compiler with a reduced
# environment.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

if [[ -n "${DYLD_INSERT_LIBRARIES:-}" ]]; then
    echo "note: clearing DYLD_INSERT_LIBRARIES so the build runs outside the tracer" >&2
fi
unset DYLD_INSERT_LIBRARIES
: "${CODEQL_RUNNER:?CodeQL tracing is not initialized}"
: "${SEMMLE_PRELOAD_libtrace:?CodeQL tracing is not initialized}"

scratch="build/codeql-swift"
traced_swiftc="$scratch/traced-swiftc"
swiftc="$(xcrun --find swiftc)"
mkdir -p "$scratch"

# Keep every CodeQL/SEMMLE setting on the compiler wrapper, including the
# extractor's trap and source-archive directories.
{
    echo '#!/bin/bash'
    while IFS= read -r name; do
        printf 'export %s=%q\n' "$name" "${!name}"
    done < <(compgen -e | grep -E '^(CODEQL|SEMMLE)_' || true)
    printf 'export DYLD_INSERT_LIBRARIES=%q\n' "$SEMMLE_PRELOAD_libtrace"
    printf 'exec %q %q "$@"\n' "$CODEQL_RUNNER" "$swiftc"
} > "$traced_swiftc"
chmod +x "$traced_swiftc"

# The native build system is the one that honours SWIFT_EXEC for compilation.
# The default Swift Build engine registers the Xcode toolchain with its build
# service and ignores the override. Build one architecture, as CodeQL's Swift
# documentation recommends.
SWIFT_EXEC="$traced_swiftc" \
SWIFT_EXEC_MANIFEST="$traced_swiftc" \
    swift build \
    --package-path apps/macos \
    --scratch-path "$scratch" \
    --build-system native \
    --arch "$(uname -m)"

trap_count="$(find "${CODEQL_EXTRACTOR_SWIFT_TRAP_DIR:?}" -type f | wc -l | tr -d ' ')"
if [[ "$trap_count" -eq 0 ]]; then
    echo "error: the traced Swift build produced no CodeQL trap files" >&2
    exit 1
fi
echo "CodeQL Swift extraction produced $trap_count trap file(s)"
