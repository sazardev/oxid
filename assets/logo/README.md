# Oxid logo

A geometric hexagon — representing a container/port — with its top edge left
open, standing in for an open branch or fork. Flat, line-art, no gradients,
per [`DESIGN.md`](../DESIGN.md).

| File | Use |
|---|---|
| `oxid-mark.svg` | Icon only, fixed **Oxid Orange** (`#DE5236`) stroke, transparent background. Default choice for most contexts. |
| `oxid-mark-mono.svg` | Same icon using `currentColor` instead of a fixed color — recolor it by setting CSS `color` on the `<svg>` or a parent element. |
| `oxid-icon.svg` | Icon on a **Carbon Black** (`#121212`) rounded square — app icons, favicons, social previews. |
| `favicon.svg` | Same mark, tuned stroke weight for legibility at 16–32px. |
| `oxid-wordmark.svg` | Icon + "oxid" wordmark in Fira Sans, Steel White text — for **dark** backgrounds. |
| `oxid-wordmark-light.svg` | Same lockup with Carbon Black text — for **light** backgrounds. |

## Replicating / restyling it

The mark is a single open `<path>` on a `0 0 120 120` viewBox — six points of a
flat-top hexagon with the top edge omitted:

```
M37.5,21.03 L15,60 L37.5,98.97 L82.5,98.97 L105,60 L82.5,21.03
```

Stroke width `9`, `round` linecaps and linejoins, no fill. Change the `stroke`
color (or use `oxid-mark-mono.svg` with `currentColor`) to adapt it to any
theme — the geometry itself shouldn't change.

## Palette reference (see `DESIGN.md` §1)

| Name | Hex |
|---|---|
| Oxid Orange | `#DE5236` |
| Carbon Black | `#121212` |
| Iron Gray | `#262626` |
| Steel White | `#F4F4F5` |
| Patina Green | `#4A9E79` |
| Ash Gray | `#6B7280` |

All assets here are released under the same [0BSD license](../LICENSE) as the
rest of the repository — use, modify, and redistribute freely, with or
without attribution.
