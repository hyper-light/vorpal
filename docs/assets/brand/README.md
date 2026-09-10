# Vorpal blade

Converted to native SVG on 2026-09-09 from the original artwork at `9dd873d`.
The blade silhouette, rounded hilt, flowers, stems, and curling flourishes are
preserved. This is a contour conversion of that artwork, with the same
proportions and orientation.

## Files and construction

- `vorpal-blade.svg` is the editable monochrome vector master.
- `vorpal-blade-light.svg` and `vorpal-blade-dark.svg` use identical contours in
  `#1f2328` and `#f0f6fc`. The README displays these vectors directly.
- `vorpal-blade-transparent.png` is a 648 × 1080 export of the vector master.
- `vorpal-blade-preview.png` shows both themes enlarged and at the existing
  54 × 90 README size, with the heading and tagline beneath the blade.
- `vorpal-blade-comparison.png` places the original raster rendering beside the
  vector conversion, in both themes and at both sizes. It was visually checked
  on 2026-09-09, including the hilt and every floral opening.

The trace follows the original PNG's alpha boundary at 127.5 out of 255. All
five closed contours are retained. Contour simplification uses a 0.4-source-pixel
tolerance, about 0.03 pixels at README size. The path contains 496 vertices;
the even-odd fill rule preserves the transparent floral openings and the gap
between blade and hilt.

The original `293 36 712 1187` viewBox and the README's 54 × 90 dimensions are
unchanged. The vector artwork uses solid theme ink with transparent cutouts.
There are no embedded bitmaps, filters, fonts, scripts, or external resources.

Reproduce the transparent export from the repository root with:

```sh
rsvg-convert --width 648 --height 1080 \
  --output docs/assets/brand/vorpal-blade-transparent.png \
  docs/assets/brand/vorpal-blade.svg
```

The root README selects the theme with a `<picture>` element. Clicking the
blade opens the preview; all paths are relative to the repository.

## Original source

The unmodified original PNG is retained at
`9dd873d:docs/assets/brand/vorpal-blade-transparent.png` in git history.

The previous theme SVGs embedded that PNG and recolored its alpha channel with
a filter. The new SVGs express the same design as native contours.

## Treatment examples

[Two surface-treatment previews](vorpal-examples.md) retain the original blade,
hilt, flowers, and flourishes. They are separate from the current README artwork.
