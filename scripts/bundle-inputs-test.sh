#!/usr/bin/env bash
# Parser and FTDI staging checks. No macOS tooling: fixtures stand in for otool.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$ROOT/scripts/macho-load-commands.sh"
source "$ROOT/scripts/build-native.sh"

fail() { printf 'FAIL %s\n' "$1" >&2; exit 1; }

assert_eq() {
  local name="$1" expected="$2" actual="$3"
  if [[ "$expected" != "$actual" ]]; then
    printf 'FAIL %s\n--- expected\n%s\n--- actual\n%s\n' "$name" "$expected" "$actual" >&2
    exit 1
  fi
}

# Previous bundle-dev.sh parsers, used to lock thin-file output.
legacy_linked_libraries() {
  sed '1d; s/^[[:space:]]*//; s/ (compatibility version.*$//'
}
legacy_rpaths() {
  awk '/LC_RPATH/{getline;getline;sub(/^ *path /,"");sub(/ \(offset.*$/,"");print}'
}

thin_libs="$(printf '%s\n' \
  'hw_vdf_client:' \
  $'\t/opt/homebrew/opt/gmp/lib/libgmp.10.dylib (compatibility version 16.0.0, current version 16.0.0)' \
  $'\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1351.0.0)' \
  $'\t@rpath/libft4222.dylib (compatibility version 1.0.0, current version 1.4.4)' \
  $'\t@rpath/libftd2xx.dylib (compatibility version 1.4.0, current version 1.4.4, weak)')"
expected_thin_libs="$(printf '%s\n' \
  '/opt/homebrew/opt/gmp/lib/libgmp.10.dylib' \
  '/usr/lib/libSystem.B.dylib' \
  '@rpath/libft4222.dylib' \
  '@rpath/libftd2xx.dylib')"
actual_thin_libs="$(printf '%s\n' "$thin_libs" | macho_linked_libraries)"
legacy_thin_libs="$(printf '%s\n' "$thin_libs" | legacy_linked_libraries)"
assert_eq "thin install names" "$expected_thin_libs" "$actual_thin_libs"
assert_eq "thin install names match the previous parser" "$legacy_thin_libs" "$actual_thin_libs"

# The failure on Apple Silicon: the second slice header was consumed as a path,
# and each real install name was listed again.
fat_libs="$(printf '%s\n' \
  '/build/Frameworks/libft4222.dylib (architecture x86_64):' \
  $'\t@rpath/libft4222.dylib (compatibility version 1.0.0, current version 1.4.4)' \
  $'\t@rpath/libftd2xx.dylib (compatibility version 1.4.0, current version 1.4.4)' \
  $'\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1319.0.0)' \
  '/build/Frameworks/libft4222.dylib (architecture arm64):' \
  $'\t@rpath/libft4222.dylib (compatibility version 1.0.0, current version 1.4.4)' \
  $'\t@rpath/libftd2xx.dylib (compatibility version 1.4.0, current version 1.4.4)' \
  $'\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1319.0.0)')"
expected_fat_libs="$(printf '%s\n' \
  '@rpath/libft4222.dylib' \
  '@rpath/libftd2xx.dylib' \
  '/usr/lib/libSystem.B.dylib')"
actual_fat_libs="$(printf '%s\n' "$fat_libs" | macho_linked_libraries)"
assert_eq "universal install names" "$expected_fat_libs" "$actual_fat_libs"
[[ "$actual_fat_libs" == *'(architecture arm64):'* ]] && fail "architecture header leaked as a dependency"

alt_libs="$(printf '%s\n' \
  'libft4222.dylib:' \
  '(architecture x86_64):' \
  $'\t@rpath/libftd2xx.dylib (compatibility version 1.4.0, current version 1.4.4)' \
  $'\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1319.0.0)' \
  '(architecture arm64):' \
  $'\t@rpath/libftd2xx.dylib (compatibility version 1.4.0, current version 1.4.4)' \
  $'\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1319.0.0)')"
expected_alt_libs="$(printf '%s\n' \
  '@rpath/libftd2xx.dylib' \
  '/usr/lib/libSystem.B.dylib')"
assert_eq "universal install names, separate architecture header" \
  "$expected_alt_libs" "$(printf '%s\n' "$alt_libs" | macho_linked_libraries)"

thin_rpaths="$(cat <<'EOF'
          cmd LC_RPATH
      cmdsize 32
         path /opt/homebrew/lib (offset 12)
          cmd LC_RPATH
      cmdsize 32
         path /opt/homebrew/lib (offset 12)
          cmd LC_RPATH
      cmdsize 56
         path /Library/Application Support/gmp (offset 12)
EOF
)"
expected_thin_rpaths="$(printf '%s\n' \
  '/opt/homebrew/lib' \
  '/opt/homebrew/lib' \
  '/Library/Application Support/gmp')"
actual_thin_rpaths="$(printf '%s\n' "$thin_rpaths" | macho_rpaths)"
legacy_thin_rpaths="$(printf '%s\n' "$thin_rpaths" | legacy_rpaths)"
assert_eq "thin rpaths" "$expected_thin_rpaths" "$actual_thin_rpaths"
assert_eq "thin rpaths match the previous parser" "$legacy_thin_rpaths" "$actual_thin_rpaths"

fat_rpaths="$(cat <<'EOF'
/build/hw_vdf_client (architecture x86_64):
Load command 14
          cmd LC_RPATH
      cmdsize 48
         path @executable_path/hw/libft4222 (offset 12)
Load command 15
          cmd LC_RPATH
      cmdsize 32
         path /opt/homebrew/lib (offset 12)
/build/hw_vdf_client (architecture arm64):
Load command 14
          cmd LC_RPATH
      cmdsize 48
         path @executable_path/hw/libft4222 (offset 12)
Load command 15
          cmd LC_RPATH
      cmdsize 32
         path /opt/homebrew/lib (offset 12)
EOF
)"
expected_fat_rpaths="$(printf '%s\n' \
  '@executable_path/hw/libft4222' \
  '/opt/homebrew/lib')"
actual_fat_rpaths="$(printf '%s\n' "$fat_rpaths" | macho_rpaths)"
assert_eq "universal rpaths" "$expected_fat_rpaths" "$actual_fat_rpaths"
legacy_fat_rpaths="$(printf '%s\n' "$fat_rpaths" | legacy_rpaths)"
[[ "$actual_fat_rpaths" == "$legacy_fat_rpaths" ]] && fail "universal rpaths were still repeated per slice"

alt_rpaths="$(cat <<'EOF'
client:
(architecture x86_64):
          cmd LC_RPATH
      cmdsize 32
         path /usr/local/lib (offset 12)
(architecture arm64):
          cmd LC_RPATH
      cmdsize 32
         path /usr/local/lib (offset 12)
EOF
)"
assert_eq "universal rpaths, separate architecture header" "/usr/local/lib" \
  "$(printf '%s\n' "$alt_rpaths" | macho_rpaths)"

# Upstream layout: hw/libft4222 -> download dir, libft4222.dylib -> versioned image.
# rsync -a preserves the directory symlink, so a plain copy hits the same inode.
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
sdk="$tmp/src/libft4222"
mkdir -p "$sdk" "$tmp/src/hw" "$tmp/stage/hw"
printf 'ftd2xx.h\n' > "$sdk/ftd2xx.h"
printf 'libft4222.h\n' > "$sdk/libft4222.h"
printf 'WinTypes.h\n' > "$sdk/WinTypes.h"
printf 'ftd2xx-bytes\n' > "$sdk/libftd2xx.dylib"
printf 'ft4222-bytes\n' > "$sdk/libft4222.1.4.4.190.dylib"
ln -s "libft4222.1.4.4.190.dylib" "$sdk/libft4222.dylib"
ln -s "$sdk" "$tmp/src/hw/libft4222"
ln -s "$sdk" "$tmp/stage/hw/libft4222"
if cp "$sdk/ftd2xx.h" "$tmp/stage/hw/libft4222/" 2>/dev/null; then
  fail "copying onto the symlinked SDK should hit the same file"
fi
stage_ftdi_drivers "$tmp/stage/hw/libft4222" \
  "$tmp/src/hw/libft4222/"*.h \
  "$tmp/src/hw/libft4222/libftd2xx.dylib" \
  "$tmp/src/hw/libft4222/libft4222.dylib"
[[ -d "$tmp/stage/hw/libft4222" && ! -L "$tmp/stage/hw/libft4222" ]] || fail "staged drivers are not a real directory"
[[ -f "$tmp/stage/hw/libft4222/libft4222.dylib" && ! -L "$tmp/stage/hw/libft4222/libft4222.dylib" ]] || fail "versioned dylib was not copied as a regular file"
assert_eq "staged libft4222 contents" "ft4222-bytes" "$(cat "$tmp/stage/hw/libft4222/libft4222.dylib")"
assert_eq "staged header" "WinTypes.h" "$(cat "$tmp/stage/hw/libft4222/WinTypes.h")"
[[ -L "$sdk/libft4222.dylib" ]] || fail "staging replaced the SDK symlink"
assert_eq "original versioned image" "ft4222-bytes" "$(cat "$sdk/libft4222.1.4.4.190.dylib")"

plain="$tmp/plain"
mkdir -p "$plain"
printf 'ftd2xx.h\n' > "$plain/ftd2xx.h"
printf 'libft4222.h\n' > "$plain/libft4222.h"
printf 'WinTypes.h\n' > "$plain/WinTypes.h"
printf 'thin-ftd2xx\n' > "$plain/libftd2xx.dylib"
printf 'thin-ft4222\n' > "$plain/libft4222.dylib"
stage_ftdi_drivers "$tmp/plain-out" "$plain/"*.h "$plain/libftd2xx.dylib" "$plain/libft4222.dylib"
assert_eq "regular SDK copy" "thin-ft4222" "$(cat "$tmp/plain-out/libft4222.dylib")"
assert_eq "regular SDK left in place" "thin-ft4222" "$(cat "$plain/libft4222.dylib")"
