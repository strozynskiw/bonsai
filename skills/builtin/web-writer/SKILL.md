---
name: web-writer
description: HTML and CSS correctness and craft — semantic markup, accessible-by-default patterns, form behavior, nesting validity, layout discipline, and maintainable CSS architecture. Load before writing or editing HTML, CSS, or markup inside components and templates.
activation:
  extensions: [html, htm, css, scss, vue, svelte]
---

# Web Writer

How to write markup and styles that behave correctly: semantic elements carry
the behavior, accessibility is a correctness property, and the browser
silently "fixing" invalid HTML is how styles and scripts end up targeting a
tree that doesn't match the source.

## Match the project's idiom first

Before writing markup or CSS, identify how this project does it: a component
framework (React/Vue/Svelte), a template engine, a utility framework
(Tailwind), CSS modules, or plain stylesheets. Edit in that idiom — don't
hand-roll a raw stylesheet into a Tailwind project or inline styles into a
CSS-modules codebase.

## Semantic HTML is correctness

- Use the element that has the behavior: `<button>` for actions (never a
  `<div onclick>`), `<a href>` for navigation, `<label>` for labels,
  `<ul>/<ol>` for lists, `<table>` for tabular data. Native elements bring
  keyboard support, focus, and assistive-technology semantics for free;
  recreating them by hand is where bugs live.
- Structure pages with landmarks — `<header>`, `<nav>`, `<main>` (one per
  page), `<footer>` — and keep the heading hierarchy real: one `<h1>`, no
  skipped levels; headings are structure, not font sizing.

## Accessible by default

- Every form control gets a `<label for>` (or wrapping label) — placeholder
  text is not a label.
- Every `<img>` gets `alt`: descriptive when it carries meaning, `alt=""` when
  decorative, never missing.
- ARIA is a last resort for when no native element fits; wrong ARIA is worse
  than none. Prefer changing the element to adding `role`/`aria-*`.
- Everything clickable must be reachable and operable by keyboard. Never
  remove focus outlines (`outline: none`) without providing a visible
  replacement (`:focus-visible`).
- Keep text/background contrast readable; don't encode meaning in color alone.

## Forms

- A `<button>` inside a form defaults to `type="submit"` — a classic bug where
  a "Cancel" button submits the form. Set `type="button"` explicitly for
  non-submit actions.
- Use the right `type=` on inputs (`email`, `number`, `date`, …) — it drives
  mobile keyboards and built-in validation — plus `name` attributes, and
  `required`/`min`/`max`/`pattern` before reaching for script validation.

## Validity: the browser rewrites bad nesting

- Invalid nesting is silently restructured at parse time — a `<div>` inside
  `<p>` closes the paragraph, `<tr>` outside `<table>` scaffolding gets moved —
  and then CSS selectors and scripts target the *repaired* tree, not your
  source. Keep nesting valid: no block elements in `<p>`, no interactive
  element inside another interactive element, `id`s unique per page.
- Document basics on full pages: `<!doctype html>`, `<html lang="…">`,
  `<meta charset="utf-8">`, and `<meta name="viewport"
  content="width=device-width, initial-scale=1">` for anything responsive.

## CSS discipline

- Lay out with flexbox and grid; absolute positioning is for overlays, not
  layout. Use `gap` for spacing between siblings instead of margin arithmetic.
- Never fix the height of anything that contains text — content grows with
  translation, zoom, and user font sizes. Use `min-height` and let it flow.
- Keep specificity flat (classes, not deep descendant chains or ids); an
  `!important` is a debt marker, and a second one to beat the first is an
  arms race.
- Mobile-first: base styles for small screens, `min-width` media queries
  upward. Use relative units (`rem`, `%`) for type and spacing.
- If the project supports light and dark themes, check both before calling a
  style change done.

## Maintainable styling

- Reuse the project's design tokens — CSS custom properties (or the
  preprocessor/theme variables already defined) for colors, spacing, and type
  scale. A hard-coded hex next to a `var(--color-…)` system is a defect.
- Name classes for purpose, not appearance: `.form-error`, not `.red-text` —
  appearance changes, purpose doesn't. Follow the project's naming scheme
  (BEM, utilities, CSS modules) instead of introducing a second one.
- Keep markup lean: every wrapper `<div>` needs a reason (layout, grouping
  semantics); styling can usually hang off the element that's already there.
- Prefer CSS to JavaScript for presentation: `:hover`/`:focus-visible`
  states, transitions, `<details>` disclosure, sticky positioning — reach for
  script only when CSS genuinely can't express it.
- Respect user preferences: gate non-essential animation behind
  `@media (prefers-reduced-motion: no-preference)` and support
  `prefers-color-scheme` when the project themes both ways.
- Modern layout niceties over hacks where the project's browser targets
  allow: `clamp()` for fluid type, `aspect-ratio` for media boxes, logical
  properties (`margin-inline`) in multi-language projects.

## Verify

- Open the result in a browser when the environment allows: resize it, tab
  through the interactive elements, and submit the form — don't judge markup
  by reading it.
