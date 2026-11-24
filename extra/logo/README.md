# Application icon

The redesigned icon combines an angular A / terminal prompt with an underscore
cursor. Amber and coral strokes on a graphite tile express the application's
focus on fast GPU-rendered terminal interaction. The outside of the tile is
transparent.

`xaya-icon.png` is the 1024 × 1024 master artwork. Regenerate all platform
assets on macOS with no additional Python dependencies:

```sh
python3 scripts/update-icons.py
```

Exports retain the existing integration paths:

- macOS: `extra/osx/XaYa.app/Contents/Resources/xaya.icns`, referenced by
  the bundle's Info.plist and copied by `make app`.
- Windows: `alacritty/windows/xaya.ico`, used by the executable resource and
  WiX installer; includes 16, 24, 32, 48, 64, 128 and 256 pixel representations.
- Linux: `xaya-term.svg`, used by the installation and release instructions.
  SVG files are self-contained wrappers around the PNG artwork, not editable
  vector drawings. Historical simple/scanline filenames now show the same icon.
- X11: `compat/xaya-term.png`, embedded by the window implementation.
- Repository README: the master PNG.

The ICNS contains 16, 32, 128, 256 and 512 point images at 1× and 2×.
Existing installed applications must be rebuilt/reinstalled to pick up the icon.

## Artwork provenance

Generated with the built-in imagegen tool. Final generation prompt:

> Use case: logo-brand. Create one finished application icon for Alacritty, a fast GPU-accelerated terminal emulator. Square 1024x1024 asset, front-facing orthographic. A beautifully precise dark graphite rounded-square app tile centered with generous transparent outside margin (tile occupies 84 percent width), subtle bevel and premium restrained depth. In the center a large bold custom angular letter A symbol, its peaked chevron evokes a terminal prompt and fast upward movement; a short separate terminal underscore cursor at lower right. Symbol uses luminous warm coral orange to amber gradient, crisp broad geometric strokes and large negative space. Extremely simple memorable silhouette readable at 16px, polished macOS desktop icon craftsmanship. No words, no labels, no fine lines, no tiny details, no extra symbols, no perspective, no surrounding scene, no mockup. Actual transparent background outside the tile.
