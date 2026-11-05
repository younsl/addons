# Contrast-Anchored Color Ramp

The color token values in `web/src/styles/tokens.css` are derived, not chosen.
This document is the derivation: what a step number means, why the accent splits
into three roles, how a value is checked, and which trade-offs are deliberate.

## Status

**Implementation status: Active convention, in effect.** Verified on 2026-08-28
against `b04378c`. Every rule below holds in `web/src/styles/tokens.css` as
shipped. The Figma source is the forklift-redesign file, key
`3Tbmvu4lQmNPeoVs2zhttY`, modes `Dark v2` and `Light v2`.

## Overview

Read this before changing the value of a color token, adding a hue family, or
deciding that a contrast result is acceptable. [tokens.md](tokens.md) covers
which token to use; this covers what the values are allowed to be.

## Background

The first palette was tuned by eye, one value at a time, and then checked. That
order is why it kept failing: a value chosen to look right on a swatch sheet has
no relationship to the surface it will sit on, so every new component surfaced a
new failure and "which token do I use here" was guesswork.

The ramp inverts the order. Contrast is the input. Each step is solved for a
target ratio against its theme's canvas, so the step number carries a promise
that holds wherever it is used.

## The Core Rule

**A step number is a promise about contrast against that theme's canvas, and the
same step means the same promise in both themes.**

| Step | Promise | Used for |
| --- | --- | --- |
| 0–200 | 1.00–1.12 | Canvas, sidebar, panels, cards |
| 300–500 | 1.25–2.10 | Muted fills, hover, selected, borders |
| 600 | 3.00 | Non-text UI lower bound |
| 700 | 4.50 | Body text on a bare canvas |
| 900 | 7.40 | **Text roles anchor here** |
| 1000–1100 | 10.00–13.50 | Emphasis, primary text in dark |

The consequence worth stating: because the promise is symmetric, one semantic
mapping serves both themes. `accent/900` is a bright gold in dark and a deep gold
in light, and no token needs a light-specific override.

### Why text anchors at 900, not 700

Step 700 is the nominal 4.5:1 line, but it is 4.5:1 against the **bare canvas**.
Text does not sit on the bare canvas; it sits on panels and cards, which are
themselves a step or two up. Anchoring body text at 700 measured 4.3:1 on a
panel — a failure, from a token that promised a pass.

The highest text-bearing surface is around step 400, at 1.55:1 from canvas. So a
text step has to clear `1.55 × 4.5 = 6.98` to be safe everywhere it lands. Step
900 is set to 7.40 rather than 7.00 so that rounding does not eat the margin;
targeting exactly 7.00 produced a measured 4.48.

### Chroma

Chroma is normalised across hue families at each step, so `green/700` and
`blue/700` carry the same visual weight instead of each family being hand-tuned.

**Functional families are exempt.** Red, orange, yellow and green exist to be
told apart, not to match in weight. Normalising their chroma to the weakest hue
collapsed the severity levels into browns; they take what the gamut allows at
that lightness instead.

## The Accent Splits Into Three Roles

Yellow is inherently light. A yellow that reads as yellow cannot be 4.5:1 against
a pale grey — the best a recognisably-yellow value reaches against the light
surfaces is about 2.4:1, and by then it is already olive. So a single accent
token that must also work as body text has to go dark, and the brand color is
gone.

The role splits instead:

| Token | Job | Constraint it must satisfy |
| --- | --- | --- |
| `--fx-accent` | The fill | Carries `--fx-accent-foreground` at 4.5:1 |
| `--fx-accent-foreground` | Ink on that fill | — |
| `--fx-accent-ink` | The accent on a surface | 4.5:1 as text, 3:1 as a border or ring, on every surface |

Light mode's fill is deliberately **off-ramp**: it is the most chroma the gamut
allows at its lightness, because its job is to be recognisably forklift yellow
rather than to keep a contrast promise. It lives in the Figma Palette v2
collection under `brand/light/*`, named so the exception is visible rather than
looking like a ramp step someone mistyped.

Dark mode needs no split. Its surfaces are dark enough that the fill color is
also 4.74:1 as ink on the busiest surface, so `--fx-accent-ink` matches
`--fx-accent` there.

## Alpha Tints

Tailwind's opacity modifier compiles to
`color-mix(in oklab, var(--token) N%, transparent)`, which leaves the color
untouched and sets alpha to N. Confirmed in a browser: the oklab coordinates for
10% and 70% of the same color are identical apart from alpha.

So a tint is always **its base color at the named alpha**. Nothing else. Do not
approximate one by mixing toward a background — that shifts the hue, and the
result no longer tracks its base when the base changes.

This matters in Figma, where the tints exist as their own variables because
`setBoundVariableForPaint` discards a paint's opacity, so the alpha has to live
in the variable's value. It does not matter in CSS, where the modifier does the
work.

## Verification

A palette claim is only worth the measurement behind it, and the measurement has
to be taken where the text actually lands.

**Measure the rendered DOM, not the token pairs.** Walk every element that
renders text, composite each translucent ancestor background down to an opaque
color, and fold in inherited `opacity`. This catches what a token-pair sweep
cannot: tinted fills, `opacity` utilities, colored text on a tint of itself, and
text over a gradient.

**Exempt what WCAG exempts, and nothing more.** Disabled controls and text over
photographs are out of scope. A selectable option that has to be read to be
chosen is not disabled, however faint it looks.

**Compare against the previous state.** A number on its own does not say whether
anything improved. Running the same audit against the branch point is what turns
"0 violations" into a result.

Measured when this ramp landed:

| | Before | After |
| --- | --- | --- |
| Real contrast violations, 24 routes × 2 themes | 147 | 0 |
| — caused by token values | 108 | 0 |
| — caused by component bugs | 39 | 0 |
| Severity separation, dark | 28.1 | 84.2 |
| Severity separation, light | 31.3 | 44.0 |

Measured when the Apple palette replaced the ramp's neutrals (2026-09-23), 19
routes × 2 themes, screen-reader-only labels excluded:

| | Before | After |
| --- | --- | --- |
| Real contrast violations | 3 | 0 |
| Worst text ink against any surface, dark | 5.43 | 5.13 |
| Worst text ink against any surface, light | 4.76 | 5.25 |
| Worst accent or status ink against any surface, dark | 5.40 | 4.61 |
| Worst accent or status ink against any surface, light | 4.72 | 6.01 |

Dark gives up some margin (the accent on surface-3 is the lowest pair, still
above 4.5) for surfaces that separate more, and light gains on every pair.

The dark step ratios against the canvas are now 1.10 / 1.23 / 1.51 / 1.85 for
the sidebar and surface-1 to surface-3, wider than the ramp's 1.04 / 1.11 /
1.22 / 1.49, so stacked surfaces separate more. The three violations before were
Tailwind `emerald-*` status text, replaced by the status tokens.

## Deliberate Trade-offs

These are not defects to fix. Changing them means re-opening the decision.

**Light mode reads flatter than the previous palette.** Canvas against panel went
1.122 → 1.055, traded for the symmetry that lets one step serve both themes. The
deeper stack gained instead: panel against panel-raised went 1.026 → 1.062, which
had made a raised row inside a panel invisible, and surface-2 against surface-3
went 1.159 → 1.244.

**Dark mode's neutrals are hue-neutral and anchored on black.** The ramp as
ported ran warm, from a `#171613` canvas, and that reads as brown rather than as
forklift. The dark neutrals are re-derived on a neutral hue from a `#0a0a0b`
canvas, holding the step ratios the ramp asks for (1.04 / 1.11 / 1.22 / 1.49 for
neutral 100–400 against the canvas) rather than v1's collapsed 1.012 stack. This
costs nothing in contrast: every dark ink lands higher than it did on the warm
canvas, and the lowest text pair on the busiest text-bearing surface went
4.77 → 5.40. Light is unchanged, so the two themes now share step *ratios* and
step *promises* but not a hue.

**The sidebar and the content panel are the same step.** They separate by their
border rather than by value. This is what the step system produces; if the two
should differ, the sidebar moves to a different step rather than getting a
hand-picked value.

**`--fx-input-border` against `--fx-input` is 1.89:1**, under the 3:1 WCAG 1.4.11
asks of a control whose boundary is its only indicator. Raising it to the next
step would make every input outline as loud as a focus ring, so this belongs with
a decision about input styling as a whole rather than with a token value.

**A yellow fill does not outline itself on a pale surface.** The light fill is
1.08–1.44:1 against the surfaces, so a progress bar or switch track filled with
it falls short of 1.4.11's 3:1. No yellow clears that against a light grey.
Controls that need a visible boundary take an `--fx-accent-ink` hairline.

**Light-mode severity separates by 44.0 against dark's 84.2.** Light orange and
light yellow converge, and widening the hue gap turns "medium" olive. Severity
must not rely on color alone in light mode.

## Changing a Value

1. Decide which promise the role needs — a contrast target, not a color.
2. Take the step that carries that promise, in both themes.
3. If no step fits because the requirement is not about contrast (the brand
   yellow is the only current case), place it outside the ramp and name it so the
   exception is visible.
4. Re-measure the rendered DOM in both themes, and compare against the state
   before the change.
5. Update [tokens.md](tokens.md) in the same change, and the Figma Theme
   collection so the two do not drift.
