# Brand assets

| File | Use |
| --- | --- |
| `clipcut.ico` | Windows app icon — 16/24/32/48/64/128/256 px, 32-bit, PNG-compressed. Embedded into the .exe by `build.rs`. |
| `png/clipcut-*.png` | Individual raster sizes on the light plaque background. `clipcut-256.png` is the window icon, referenced from `ui/app.slint`. |
| `clipcut-mark-512.png`, `clipcut-mark-1024.png` | Transparent background, brightened for dark surfaces. |
| `clipcut-banner-1280x400.png` | README header. |
| `clipcut-icon.svg`, `clipcut-mark.svg` | Vector source. Regenerate the rasters from these rather than upscaling a PNG. |

The `.ico` is the only file the build depends on; the rest are referenced by path,
so moving one means updating `ui/app.slint` or `README.md` to match.
