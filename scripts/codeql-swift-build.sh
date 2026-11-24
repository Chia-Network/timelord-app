#!/usr/bin/env bash
# Build the macOS Swift sources for CodeQL while tracing only the compiler.
#
# CodeQL's Swift extractor works by injecting a tracer into every process the
# build starts. That makes the build itself (SwiftPM planning, the manifest
# sandbox, linking) many times slower, and on this package it spends most of
# the step in a long silent pause before SwiftPM prints anything.
#
# This script clears DYLD_INSERT_LIBRARIES so SwiftPM plans and links natively,
# and builds through a shadow copy of the active toolchain whose swiftc is a
# wrapper that starts the compiler under the tracer. The wrapper carries the
# CodeQL variables itself because SwiftPM starts the manifest compiler with a
# reduced environment.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

if [[ -n "${DYLD_INSERT_LIBRARIES:-}" ]]; then
    echo "note: clearing DYLD_INSERT_LIBRARIES so the build runs outside the tracer" >&2
fi
unset DYLD_INSERT_LIBRARIES
: "${SEMMLE_PRELOAD_libtrace:?CodeQL tracing is not initialized}"

scratch="build/codeql-swift"
swiftc="$(xcrun --find swiftc)"
case "$swiftc" in
    *.xctoolchain/usr/bin/swiftc) ;;
    *)
        echo "error: xcrun did not locate an Xcode toolchain compiler (got $swiftc)" >&2
        exit 1
        ;;
esac
mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd)"
toolchain="$scratch/toolchain"
shadow_swiftc="$toolchain/usr/bin/swiftc"
wrapper_calls="$scratch/wrapper-calls"
real_toolchain="$(dirname "$(dirname "$(dirname "$swiftc")")")"

# The shadow toolchain is a symlink farm around the active Xcode toolchain with
# one difference: usr/bin/swiftc is the tracing wrapper. Keeping the wrapper's
# name exactly "swiftc" matters because the Swift driver validates argv[0].
rm -rf "$toolchain"
mkdir -p "$toolchain/usr/bin"
for entry in "$real_toolchain"/*; do
    name="$(basename "$entry")"
    [[ "$name" == usr ]] && continue
    ln -s "$entry" "$toolchain/$name"
done
for entry in "$real_toolchain"/usr/*; do
    name="$(basename "$entry")"
    [[ "$name" == bin ]] && continue
    ln -s "$entry" "$toolchain/usr/$name"
done
for entry in "$real_toolchain"/usr/bin/*; do
    name="$(basename "$entry")"
    [[ "$name" == swiftc ]] && continue
    ln -s "$entry" "$toolchain/usr/bin/$name"
done

# Keep every CodeQL/SEMMLE setting on the compiler wrapper, including the
# extractor's trap and source-archive directories. Exec the compiler directly
# with the tracer preloaded: routing every compile through the separate
# CODEQL_RUNNER process made each compiler invocation start a fresh extractor
# and was slower than tracing the whole build.
{
    echo '#!/bin/bash'
    printf 'printf x >> %q\n' "$wrapper_calls"
    while IFS= read -r name; do
        printf 'export %s=%q\n' "$name" "${!name}"
    done < <(compgen -e | grep -E '^(CODEQL|SEMMLE)_' || true)
    printf 'export DYLD_INSERT_LIBRARIES=%q\n' "$SEMMLE_PRELOAD_libtrace"
    printf 'exec %q "$@"\n' "$swiftc"
} > "$shadow_swiftc"
chmod +x "$shadow_swiftc"
: > "$wrapper_calls"

# The native build system honours --toolchain for compilation. The default
# Swift Build engine registers the Xcode toolchain with its build service and
# ignores the override. Build one architecture, as CodeQL's Swift
# documentation recommends.
SWIFT_EXEC="$shadow_swiftc" \
SWIFT_EXEC_MANIFEST="$shadow_swiftc" \
    swift build \
    --package-path apps/macos \
    --scratch-path "$scratch" \
    --build-system native \
    --toolchain "$toolchain" \
    --arch "$(uname -m)"

trap_count="$(find "${CODEQL_EXTRACTOR_SWIFT_TRAP_DIR:?}" -type f | wc -l | tr -d ' ')"
if [[ "$trap_count" -eq 0 ]]; then
    echo "error: the traced Swift build produced no CodeQL trap files" >&2
    exit 1
fi
calls="$(wc -c < "$wrapper_calls" | tr -d ' ')"
echo "compiler wrapper invocations: $calls"
echo "CodeQL Swift extraction produced $trap_count trap file(s)"
