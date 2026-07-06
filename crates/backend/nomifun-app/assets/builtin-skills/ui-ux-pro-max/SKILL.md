---
name: ui-ux-pro-max
description: Professional UI/UX design research skill with a bundled searchable database for product patterns, visual styles, color palettes, typography, landing-page structure, dashboard charts, UX practices, and stack guidance. Use when designing, building, reviewing, or improving interfaces.
---

# UI/UX Pro Max

Use this skill for UI/UX design work: visual direction, landing pages, dashboards, app screens, responsive layouts, design critique, and frontend implementation guidance.

## Search First

The local database lives in this skill package. In Nomi workspaces the skill is linked at `.nomi/skills/ui-ux-pro-max/`.

```bash
python3 .nomi/skills/ui-ux-pro-max/scripts/search.py "<keywords>" --domain <domain> -n 5
```

If you are already inside this skill directory, the shorter form also works:

```bash
python3 scripts/search.py "<keywords>" --domain <domain> -n 5
```

Available domains:

- `product` - product type patterns and structure
- `style` - visual style systems and composition
- `typography` - font pairings and type hierarchy
- `color` - palette recommendations with roles
- `landing` - landing-page section structure
- `chart` - dashboard and data visualization patterns
- `ux` - interaction, accessibility, responsive, and state guidance
- `stack` - framework-specific implementation guidance
- `prompt` - design brief and generation prompt structure

Examples:

```bash
python3 .nomi/skills/ui-ux-pro-max/scripts/search.py "saas dashboard enterprise" --domain product
python3 .nomi/skills/ui-ux-pro-max/scripts/search.py "glassmorphism minimal" --domain style
python3 .nomi/skills/ui-ux-pro-max/scripts/search.py "healthcare calm trust" --domain color
python3 .nomi/skills/ui-ux-pro-max/scripts/search.py "layout responsive" --stack html-tailwind
python3 .nomi/skills/ui-ux-pro-max/scripts/search.py --list-domains
```

## Workflow

1. Extract product type, audience, visual tone, industry, and stack from the user request.
2. Search `product` first, then search the most relevant of `style`, `typography`, `color`, `landing`, `chart`, `ux`, and `stack`.
3. Combine results into a concrete design direction: layout, hierarchy, palette, type, states, and component behavior.
4. If implementing code, follow the existing project's framework and design conventions first; use this database to resolve gaps.
5. Verify mobile and desktop layout, text fit, contrast, focus states, hover states, loading/empty/error states, and any chart or media rendering.

## Design Rules

- Build the actual usable screen first, not a marketing explanation of the screen.
- Use domain-specific layout density: dashboards and operational tools should be quiet and scannable; games and playful products can be more expressive.
- Prefer stable responsive constraints over viewport-scaled font sizes.
- Keep typography readable: explicit heading/body hierarchy, no negative letter spacing, no text overflow.
- Keep palettes intentional: one dominant color, one support color, one accent, plus neutrals; avoid one-hue UIs unless the brand requires it.
- Use real product, place, object, state, chart, or gameplay imagery when the page needs inspection or trust.
- Treat accessibility, keyboard focus, contrast, and empty/loading/error states as part of the design, not cleanup.
