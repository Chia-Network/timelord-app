# Layered app icon sources

Flat layers for building a macOS 26 layered icon (`.icon`) in Apple's Icon Composer, with
light and dark appearances. The app bundle still ships `../AppIcon.icns`, and
`scripts/bundle-dev.sh` does not compile `.icon` files yet.

Each SVG is 1024 × 1024 and full bleed: the icon body fills the canvas with no rounded corners,
rim, shadow or glow, because Icon Composer applies the mask and lighting itself. The geometry
matches `../AppIcon.svg`.

| Group | Layers, back to front | Light | Dark |
|---|---|---|---|
| Background | `background-light.svg` / `background-dark.svg` | `#F6F8F7` → `#DCE2DF` | `#252B2C` → `#0A0D0D` |
| | `traces-light.svg` / `traces-dark.svg` | `#BFC8C3` | `#2C3532` |
| Chip | `pins-light.svg` / `pins-dark.svg` | `#7A8580` | `#A5AEA9` |
| | `chip-body.svg` | `#1C2223` | same |
| Clock | `clock-face.svg`, `clock-ring.svg`, `clock-hands.svg` | ring and hub in Chia green `#32A752` | same |

In Icon Composer, add the three groups in that order. Where a layer has `-light` and `-dark`
files, add both and hide the one that does not match each appearance, or use one file and set
its fill per appearance. The background can also come from Icon Composer's own background fill
instead of the SVG.

`preview-light.svg` and `preview-dark.svg` stack the layers under a plain rounded mask for
reference only; they are not inputs.
