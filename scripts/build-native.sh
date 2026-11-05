#!/usr/bin/env bash
# Build the pinned upstream native client without modifying its source. No USB I/O.
set -euo pipefail

# get-libft4222.sh points src/hw/libft4222 at the download directory and
# libft4222.dylib at the versioned image. rsync -a keeps that directory
# symlink, and copying into it writes each file onto itself. Remove the link
# first. cp follows the dylib symlink and stores a regular file.
# scripts/bundle-inputs-test.sh sources this helper.
stage_ftdi_drivers() {
  local dest="$1"
  shift
  rm -rf "$dest"
  mkdir -p "$dest"
  cp "$@" "$dest/"
}

if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
  return 0
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/integrations/chiavdf/upstream.env"
for tool in git cmake rsync shasum; do
  command -v "$tool" >/dev/null || { echo "Missing build tool on PATH: $tool (see README)." >&2; exit 1; }
done
[[ "$(uname -s)" == Darwin ]] || { echo 'The ASIC client build requires macOS.' >&2; exit 1; }
# CHIAVDF_SOURCE is for local development only; it skips pinned-source verification.
SOURCE="${CHIAVDF_SOURCE:-$ROOT/build/upstream/chiavdf-$CHIAVDF_REVISION}"
if [[ -z "${CHIAVDF_SOURCE:-}" ]]; then
  if [[ ! -d "$SOURCE/.git" ]]; then
    mkdir -p "$SOURCE"
    git -C "$SOURCE" init -q
    git -C "$SOURCE" remote add origin "$CHIAVDF_REPOSITORY"
    git -C "$SOURCE" fetch --depth 1 origin "$CHIAVDF_REVISION"
    git -C "$SOURCE" checkout --detach FETCH_HEAD
  fi
  [[ "$(git -C "$SOURCE" rev-parse HEAD)" == "$CHIAVDF_REVISION" ]] || { echo 'Cached chiavdf revision differs from its revision-specific directory; inspect the cache before retrying.' >&2; exit 1; }
  git -C "$SOURCE" diff --quiet -- src || { echo 'Cached chiavdf source was modified; use CHIAVDF_SOURCE for an explicit development checkout.' >&2; exit 1; }
fi
[[ -f "$SOURCE/src/CMakeLists.txt" ]] || { echo 'CHIAVDF_SOURCE must name a chiavdf source checkout.' >&2; exit 1; }
DRIVERS="${CHIAVDF_DRIVERS:-$SOURCE/src/hw/libft4222}"
if [[ ! -f "$DRIVERS/libft4222.dylib" && -z "${CHIAVDF_SOURCE:-}${CHIAVDF_DRIVERS:-}" ]]; then
  CHIAVDF_ADHOC_SIGN_FTDI=1 bash "$SOURCE/scripts/get-libft4222.sh" install
fi
for name in ftd2xx.h libft4222.h WinTypes.h libftd2xx.dylib libft4222.dylib; do
  [[ -f "$DRIVERS/$name" ]] || { echo "Missing $name. Set CHIAVDF_DRIVERS to the macOS FTDI SDK directory." >&2; exit 1; }
done
STAGE="$ROOT/build/controlled-native-source"
OUTPUT="$ROOT/build/controlled-native"
mkdir -p "$STAGE" "$OUTPUT"
rsync -a --delete --exclude='*.o' --exclude='hw_vdf_client' --exclude='emu_hw_vdf_client' --exclude='hw_test' --exclude='*.key' "$SOURCE/src/" "$STAGE/"
stage_ftdi_drivers "$STAGE/hw/libft4222" "$DRIVERS/"*.h "$DRIVERS/libftd2xx.dylib" "$DRIVERS/libft4222.dylib"
PREFIX="${CMAKE_PREFIX_PATH:-}"
if command -v brew >/dev/null 2>&1; then PREFIX="$(brew --prefix);$PREFIX"; fi
cmake -S "$STAGE" -B "$OUTPUT" -DBUILD_PYTHON=OFF -DBUILD_HW_TOOLS=ON -DBUILD_VDF_CLIENT=OFF -DCMAKE_BUILD_TYPE=Release -DCMAKE_OSX_DEPLOYMENT_TARGET=14.0 -DCMAKE_PREFIX_PATH="$PREFIX" -DCHIAVDF_VERSION="$(git -C "$SOURCE" rev-parse --short HEAD)"
cmake --build "$OUTPUT" --target hw_vdf_client -j "${CMAKE_BUILD_PARALLEL_LEVEL:-4}"
stage_ftdi_drivers "$OUTPUT/hw/libft4222" "$DRIVERS/libftd2xx.dylib" "$DRIVERS/libft4222.dylib"
{
  git -C "$SOURCE" rev-parse HEAD
  git -C "$SOURCE" status --porcelain --untracked-files=no -- src
  shasum -a 256 "$OUTPUT/hw_vdf_client"
  (cd "$STAGE" && find . -type f \( -name '*.cpp' -o -name '*.hpp' -o -name '*.h' \) -exec shasum -a 256 {} +)
} > "$OUTPUT/provenance.txt"
"$OUTPUT/hw_vdf_client" --control-version
