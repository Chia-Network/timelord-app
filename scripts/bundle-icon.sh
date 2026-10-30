#!/usr/bin/env bash
# Sourceable icon selection. Probe the selected compiler before choosing a format.
layered_icon_supported() {
  xcrun --find actool >/dev/null 2>&1 || return 1
  local version major
  version="$(xcrun actool --version --output-format xml1)" || return 2
  major="$(printf '%s\n' "$version" | sed -n '/<key>short-bundle-version<\/key>/{n;s/.*<string>\([0-9][0-9]*\).*<\/string>.*/\1/p;}')"
  [[ "$major" =~ ^[0-9]+$ ]] || { echo 'Could not determine the selected actool version.' >&2; return 2; }
  [[ "$major" -ge 26 ]]
}

bundle_app_icon() {
  local icon_dir="$1" resources="$2" deployment_target="$3" result log
  if layered_icon_supported; then
    log="$(mktemp)"
    # actool resolves relative input paths against its output directory.
    if ! xcrun actool "$icon_dir/AppIcon.icon" --compile "$resources" \
        --platform macosx --minimum-deployment-target "$deployment_target" --app-icon AppIcon \
        --output-partial-info-plist "$log.plist" --output-format human-readable-text --errors --warnings >"$log" 2>&1 \
      || [[ ! -f "$resources/Assets.car" || ! -f "$resources/AppIcon.icns" ]]; then
      cat "$log" >&2
      rm -f "$log" "$log.plist"
      echo 'Could not compile the layered app icon with the selected actool.' >&2
      return 1
    fi
    rm -f "$log" "$log.plist"
  else
    result=$?
    [[ "$result" -eq 1 ]] || return "$result"
    echo 'The selected tools do not support Icon Composer; bundling the flat AppIcon.icns.' >&2
    cp "$icon_dir/AppIcon.icns" "$resources/AppIcon.icns"
  fi
}
