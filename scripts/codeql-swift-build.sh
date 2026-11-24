#!/usr/bin/env bash
# Build the macOS Swift sources for CodeQL while tracing only the compiler.
#
# CodeQL's Swift extractor injects a tracer into every process the build
# starts. Running SwiftPM under that tracer costs several minutes in a silent
# pause before it prints anything, and on this package the extraction of the
# few source files is cheaper than that planning overhead.
#
# Instead of driving SwiftPM, compile the two modules directly through a
# swiftc wrapper that loads the tracer. There are only two compiler
# invocations, both use whole-module compilation, and neither needs linking:
# CodeQL only needs the compiler to see the sources.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$PROJECT_ROOT"

if [[ -n "${DYLD_INSERT_LIBRARIES:-}" ]]; then
    echo "note: clearing DYLD_INSERT_LIBRARIES so the build runs outside the tracer" >&2
fi
unset DYLD_INSERT_LIBRARIES
: "${SEMMLE_PRELOAD_libtrace:?CodeQL tracing is not initialized}"

scratch="build/codeql-swift"
mkdir -p "$scratch"
scratch="$(cd "$scratch" && pwd)"
wrapper="$scratch/bin/swiftc"
wrapper_calls="$scratch/wrapper-calls"
modules="$scratch/modules"
objects="$scratch/objects"
module_cache="$scratch/module-cache"
rm -rf "${scratch:?}/bin" "${modules:?}" "${objects:?}" "${module_cache:?}"
mkdir -p "$scratch/bin" "$modules" "$objects" "$module_cache"

swiftc="$(xcrun --find swiftc)"
sdk="$(xcrun --show-sdk-path)"
target="$(uname -m)-apple-macos14.0"

# Keep every CodeQL/SEMMLE setting on the compiler wrapper, including the
# extractor's trap and source-archive directories. Exec the compiler directly
# with the tracer preloaded; SwiftPM, Xcode and the separate CODEQL_RUNNER
# process all add work that this analysis does not need.
{
    echo '#!/bin/bash'
    printf 'printf x >> %q\n' "$wrapper_calls"
    while IFS= read -r name; do
        printf 'export %s=%q\n' "$name" "${!name}"
    done < <(compgen -e | grep -E '^(CODEQL|SEMMLE)_' || true)
    printf 'export DYLD_INSERT_LIBRARIES=%q\n' "$SEMMLE_PRELOAD_libtrace"
    printf 'exec %q "$@"\n' "$swiftc"
} > "$wrapper"
chmod +x "$wrapper"
: > "$wrapper_calls"

presentation=()
while IFS= read -r source; do
    presentation+=("$source")
done < <(find apps/macos/Sources/TimelordPresentation -name '*.swift' | sort)
mac=()
while IFS= read -r source; do
    mac+=("$source")
done < <(find apps/macos/Sources/TimelordMac -name '*.swift' | sort)

expected_sources="$(find apps/macos/Sources -name '*.swift' | wc -l | tr -d ' ')"
found_sources="$(( ${#presentation[@]} + ${#mac[@]} ))"
if [[ "$found_sources" -ne "$expected_sources" ]]; then
    echo "error: compiled $found_sources of $expected_sources Swift sources; update the module list in this script" >&2
    exit 1
fi

common=(
    -c -whole-module-optimization -Onone -parse-as-library
    -sdk "$sdk"
    -target "$target"
    -swift-version 5
    -D SWIFT_PACKAGE -D DEBUG
    -Xcc -DSWIFT_PACKAGE
    -module-cache-path "$module_cache"
)

"$wrapper" "${common[@]}" \
    -module-name TimelordPresentation \
    -emit-module -emit-module-path "$modules/TimelordPresentation.swiftmodule" \
    -o "$objects/TimelordPresentation.o" \
    "${presentation[@]}"

"$wrapper" "${common[@]}" \
    -module-name TimelordMac \
    -I "$modules" \
    -emit-module -emit-module-path "$modules/TimelordMac.swiftmodule" \
    -o "$objects/TimelordMac.o" \
    "${mac[@]}"

trap_count="$(find "${CODEQL_EXTRACTOR_SWIFT_TRAP_DIR:?}" -type f | wc -l | tr -d ' ')"
if [[ "$trap_count" -eq 0 ]]; then
    echo "error: the traced Swift compilation produced no CodeQL trap files" >&2
    exit 1
fi
calls="$(wc -c < "$wrapper_calls" | tr -d ' ')"
echo "compiler wrapper invocations: $calls"
echo "CodeQL Swift extraction produced $trap_count trap file(s)"
