# App icon

- `AppIcon.icon` is the layered Icon Composer document (Xcode 26) with default and dark artwork;
  macOS 26 also derives clear and tinted versions from it. Open it in Icon Composer to edit.
  `scripts/bundle-dev.sh` compiles it with `actool` into `Assets.car` plus a fallback
  `AppIcon.icns`, and `Info.plist` names it with `CFBundleIconName`.
- `AppIcon.icns` is the flat icon, rebuilt by `scripts/build-icon.sh` from `AppIcon.svg` (and
  `AppIcon-small.svg` for 16 and 32 px). The bundle uses it only when `actool` is unavailable,
  because the command line tools alone do not include it. Keep the two designs in step.

The layers in `AppIcon.icon/Assets` are 1024 × 1024 and full bleed, with no mask, rim, shadow or
glow, because the system adds those. Layers with two files switch by appearance through
`image-name-specializations` in `icon.json`. The system generates the clear and tinted versions.

| Group, front to back | Layers, front to back | Default | Dark |
|---|---|---|---|
| Clock | `clock-hands`, `clock-ring`, `clock-face` | ring and hub in Chia green `#32A752` | same |
| Chip | `chip-body`, `pins` | body `#1C2223`, pins `#7A8580` | pins `#A5AEA9` |
| Background | `traces`, `background` | traces `#BFC8C3`, body `#F6F8F7` → `#DCE2DF` | traces `#2C3532`, body `#252B2C` → `#0A0D0D` |

To check the document without Icon Composer's window, compile it or render one appearance
(`actool` needs an absolute input path):

```sh
xcrun actool "$PWD/apps/macos/AppIcon/AppIcon.icon" --compile /tmp/icon-out --platform macosx \
  --minimum-deployment-target 14.0 --app-icon AppIcon --output-partial-info-plist /tmp/icon-out/partial.plist
"/Applications/Xcode.app/Contents/Applications/Icon Composer.app/Contents/Executables/ictool" \
  apps/macos/AppIcon/AppIcon.icon --export-image --output-file dark.png --platform macOS \
  --rendition Dark --width 1024 --height 1024 --scale 1
```
