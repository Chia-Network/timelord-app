#!/usr/bin/env bash
# Paths from `otool -L` and `otool -l` listings.
#
# A universal Mach-O repeats every slice and prints a header before each one,
# for example "libft4222.dylib (architecture arm64):" or a bare
# "(architecture arm64):". Those headers are not load paths. install_name_tool
# -change rewrites every matching load command in every slice, so each install
# name is emitted once. -delete_rpath removes one LC_RPATH per slice, so a path
# is repeated for however many copies the busiest slice has. A thin file has
# one slice: its install names are already unique, and every rpath is kept.

macho_linked_libraries() {
  sed -n 's/^[[:space:]]*\(.*\) (compatibility version.*/\1/p' | awk '!seen[$0]++'
}

macho_rpaths() {
  awk '
    /^(\(architecture [^)]+\):|.* \(architecture [^)]+\):)[[:space:]]*$/ {
      slice = $0
      next
    }
    /LC_RPATH/ {
      getline
      getline
      sub(/^ *path /, "")
      sub(/ \(offset.*$/, "")
      if (++copies[slice, $0] > repeats[$0]) repeats[$0] = copies[slice, $0]
      if (!($0 in seen)) {
        seen[$0] = ++n
        path[n] = $0
      }
    }
    END {
      for (i = 1; i <= n; i++)
        for (j = 0; j < repeats[path[i]]; j++)
          print path[i]
    }
  '
}
