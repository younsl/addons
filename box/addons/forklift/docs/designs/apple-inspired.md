# Forklift Web Visual Language

Forklift's visual language follows Apple's product design: system neutrals, one accent, hairline separation instead of shadow, and pill-shaped actions. The reference is the community analysis at [getdesign.md/apple](https://getdesign.md/apple/design-md). Do not copy Apple assets: no Apple logo, no bundled SF Pro files, no product imagery.

## Status

**Implementation status: Active convention, in effect.** Adopted on 2026-09-23. It replaces the visual half of [linear-inspired.md](linear-inspired.md) (palette, shape, type). The operational posture there, dense tables and fast scanning, still applies.

## Decisions

Two points diverge from the reference on purpose.

- **The accent stays forklift yellow.** Apple's single-accent rule is kept, but the one accent is `--fx-accent`, not Action Blue. Yellow fills, `--fx-accent-ink` for accent-as-ink, exactly as [tokens.md](tokens.md) describes.
- **Dark stays the default theme.** Apple ships light first. Forklift keeps dark first and draws both themes from Apple's system neutrals.

## Rules

| Area | Rule |
| --- | --- |
| Dark neutrals | Pure black canvas (`#000000`), then Apple's dark system grays: `#1c1c1e`, `#2c2c2e`, `#3a3a3c`. Separator `#38383a`. |
| Light neutrals | Parchment canvas (`#f5f5f7`) under white cards (`#ffffff`), with `#ebebf0` and `#d2d2d7` for hover and selected. Hairline `#d2d2d7`. |
| Ink | `#f5f5f7` / `#1d1d1f` for text, then two quieter steps. Every text ink holds 4.5:1 on every surface in its theme. |
| Type | System stack first (`-apple-system`, SF Pro, Apple SD Gothic Neo), bundled Noto Sans KR elsewhere. Body tracks at `-0.016em`. Page titles are 28px / 600 at `-0.011em`. |
| Shape | 8px for compact controls and inputs, 12px for panels, 18px for cards, full pill for the primary action, badges and search fields. |
| Buttons | The primary (yellow) button is a pill. Secondary, outline and ghost buttons are 8px rects. Every button presses to `scale(0.95)`. |
| Depth | No shadow on chrome: `--fx-panel-highlight` is `none`. Only overlays (dialogs, popovers) keep `--fx-overlay-shadow`. No decorative gradients. |
| Status color | Only status tokens (`--fx-success`, `--fx-warning`, `--fx-danger`). Tailwind palette colors such as `emerald-600` fail contrast on a light surface. |

## Validation

- Rendered-DOM contrast audit across both themes, per [color-contrast-ramp.md](color-contrast-ramp.md): zero text below its WCAG AA threshold.
- `rg "emerald-[0-9]|amber-[0-9]" web/src` returns nothing outside comments.
- No component sets a literal hex color. New values go into `web/src/styles/tokens.css`.
