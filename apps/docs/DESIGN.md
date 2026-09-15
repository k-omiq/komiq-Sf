---
name: komiq docs
description: The komiq admin console, extended for long-form reading. Five handbooks, one per reader.
colors:
  coral: '#e0836a'
  doc-link: '#e0836a'
  doc-link-light: '#a8492a'
  bg: '#0c0c0d'
  surface: '#0f0f10'
  surface-2: '#141415'
  surface-4: '#1a1a1b'
  surface-5: '#1f1f21'
  text-bright: '#f7f6f3'
  text-1: '#e7e5e0'
  text-2: '#d4d2cc'
  text-muted: '#b5b3ad'
  text-dim: '#9c9a94'
  border: 'rgba(255, 255, 255, 0.06)'
  border-1: 'rgba(255, 255, 255, 0.08)'
  border-2: 'rgba(255, 255, 255, 0.1)'
  border-3: 'rgba(255, 255, 255, 0.12)'
  border-strong: 'rgba(255, 255, 255, 0.28)'
  hover-fill: 'rgba(255, 255, 255, 0.04)'
  primary: '#f2f1ee'
  on-primary: '#0c0c0d'
  success: '#7fd39a'
  warning: '#e0b354'
  info: '#8fb8ff'
  danger: '#f0808a'
  code-keyword: '#c69cf0'
  code-function: '#5fc8cf'
typography:
  display:
    fontFamily: "'Bricolage Grotesque Variable', 'Bricolage Grotesque', system-ui, sans-serif"
    fontSize: 'clamp(40px, 7vw, 68px)'
    fontWeight: 700
    lineHeight: 1
    letterSpacing: '-0.04em'
  headline:
    fontFamily: "'Bricolage Grotesque Variable', 'Bricolage Grotesque', system-ui, sans-serif"
    fontSize: 'clamp(30px, 4.4vw, 40px)'
    fontWeight: 700
    lineHeight: 1.1
    letterSpacing: '-0.035em'
  section:
    fontFamily: "'Bricolage Grotesque Variable', 'Bricolage Grotesque', system-ui, sans-serif"
    fontSize: '26px'
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: '-0.03em'
  prose-h2:
    fontFamily: "'Bricolage Grotesque Variable', 'Bricolage Grotesque', system-ui, sans-serif"
    fontSize: '23px'
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: '-0.025em'
  prose-h3:
    fontFamily: "'Bricolage Grotesque Variable', 'Bricolage Grotesque', system-ui, sans-serif"
    fontSize: '18px'
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: '-0.015em'
  title:
    fontFamily: "'Bricolage Grotesque Variable', 'Bricolage Grotesque', system-ui, sans-serif"
    fontSize: '21px'
    fontWeight: 700
    lineHeight: 1.2
    letterSpacing: '-0.025em'
  lede:
    fontFamily: "'Manrope Variable', 'Manrope', system-ui, -apple-system, sans-serif"
    fontSize: '18px'
    fontWeight: 400
    lineHeight: 1.55
  body:
    fontFamily: "'Manrope Variable', 'Manrope', system-ui, -apple-system, sans-serif"
    fontSize: '16px'
    fontWeight: 400
    lineHeight: 1.7
  ui:
    fontFamily: "'Manrope Variable', 'Manrope', system-ui, -apple-system, sans-serif"
    fontSize: '14px'
    fontWeight: 600
    lineHeight: 1.5
  ui-sm:
    fontFamily: "'Manrope Variable', 'Manrope', system-ui, -apple-system, sans-serif"
    fontSize: '13px'
    fontWeight: 600
    lineHeight: 1.4
  label:
    fontFamily: "'Manrope Variable', 'Manrope', system-ui, -apple-system, sans-serif"
    fontSize: '11.5px'
    fontWeight: 700
    letterSpacing: '0.05em'
  mono:
    fontFamily: "ui-monospace, 'SF Mono', Menlo, monospace"
    fontSize: '13px'
    fontWeight: 400
    lineHeight: 1.7
rounded:
  sm: '5px'
  base: '8px'
  md: '9px'
  lg: '12px'
  pill: '999px'
spacing:
  '1': '4px'
  '2': '8px'
  '3': '12px'
  '4': '16px'
  '5': '20px'
  '6': '24px'
  '8': '32px'
  '10': '40px'
  '14': '56px'
components:
  button-primary:
    backgroundColor: '{colors.primary}'
    textColor: '{colors.on-primary}'
    typography: '{typography.ui-sm}'
    rounded: '{rounded.base}'
    padding: '0 14px'
    height: '36px'
  button-default:
    backgroundColor: '{colors.surface-4}'
    textColor: '{colors.text-1}'
    typography: '{typography.ui-sm}'
    rounded: '{rounded.base}'
    padding: '0 14px'
    height: '36px'
  button-default-hover:
    backgroundColor: '{colors.surface-5}'
    textColor: '{colors.text-bright}'
  button-ghost:
    backgroundColor: 'transparent'
    textColor: '{colors.text-muted}'
    rounded: '{rounded.base}'
    height: '36px'
  button-ghost-hover:
    backgroundColor: '{colors.hover-fill}'
  search-trigger:
    backgroundColor: '{colors.surface-2}'
    textColor: '{colors.text-dim}'
    typography: '{typography.ui-sm}'
    rounded: '{rounded.base}'
    padding: '0 8px 0 12px'
    height: '36px'
  search-trigger-hero:
    backgroundColor: '{colors.surface-2}'
    textColor: '{colors.text-dim}'
    rounded: '{rounded.lg}'
    padding: '0 12px 0 16px'
    height: '52px'
    width: 'min(100%, 560px)'
  nav-link:
    backgroundColor: 'transparent'
    textColor: '{colors.text-muted}'
    typography: '{typography.ui}'
    rounded: '{rounded.base}'
    padding: '0 12px'
    height: '36px'
  nav-link-current:
    backgroundColor: '{colors.surface-5}'
    textColor: '{colors.text-bright}'
  chapter-link:
    backgroundColor: 'transparent'
    textColor: '{colors.text-dim}'
    typography: '{typography.ui-sm}'
    rounded: '{rounded.base}'
    padding: '4px 12px'
    height: '32px'
  chapter-link-current:
    backgroundColor: '{colors.surface-5}'
    textColor: '{colors.text-bright}'
  panel:
    backgroundColor: '{colors.surface}'
    rounded: '{rounded.lg}'
  code-block:
    backgroundColor: '{colors.surface-2}'
    textColor: '{colors.text-1}'
    typography: '{typography.mono}'
    rounded: '{rounded.md}'
    padding: '6px 16px 16px'
  inline-code:
    backgroundColor: '{colors.surface-4}'
    textColor: '{colors.text-1}'
    rounded: '{rounded.sm}'
    padding: '2px 6px'
  hop-node:
    backgroundColor: '{colors.surface-4}'
    textColor: '{colors.text-bright}'
    typography: '{typography.ui-sm}'
    rounded: '{rounded.base}'
    padding: '3px 10px'
    height: '28px'
  pager-step:
    backgroundColor: 'transparent'
    textColor: '{colors.text-bright}'
    rounded: '{rounded.lg}'
    padding: '14px 16px'
  pager-step-hover:
    backgroundColor: '{colors.hover-fill}'
  search-palette:
    backgroundColor: '{colors.surface-2}'
    textColor: '{colors.text-bright}'
    rounded: '{rounded.lg}'
    width: 'min(calc(100vw - 24px), 640px)'
  kbd:
    backgroundColor: '{colors.surface-4}'
    textColor: '{colors.text-muted}'
    rounded: '{rounded.sm}'
    padding: '0 6px'
    height: '20px'
---

# Design System: komiq docs

## Overview

**Creative North Star: "The Console Handbook"**

The docs are the komiq admin console with a reading column added. The shell, controls, borders, type and motion are the console's, imported from `@komika/ui` (`tokens.css`, `console.css`) and not redefined here. The docs layer (`src/lib/docs.css`) adds only what a console never needed: prose, code blocks, Markdown tables, callouts, the hop-by-hop flow, one link colour (`--doc-link`) and the code token colours. A new docs surface starts from the `--k-*` tokens and the `k-` classes; it reaches for a docs-only style only when the content is long-form.

The surface mode is Read. Density is the console's in the chrome (36px controls, 13 to 14px interface text) and deliberately looser in the reading column (16px text on a 1.7 line, a measure near 70 characters). Dark is the default theme; a light theme ships and flips surfaces, text, borders and shadows while keeping every size, radius and space. The built pages are four templates: home, handbook index, chapter, and error. All four are left-aligned; nothing is centred.

Structure is carried by 1px alpha rules and by one-step surface changes, not by boxes inside boxes. Lists of things to open are ruled index rows; things with internal structure (code, tables, flows, the chapter list) sit in one bordered panel with hairline dividers inside.

**Key Characteristics:**

- One world: console tokens and `k-` controls, with a reading layer on top.
- Dark by default, light supported; every colour comes from a token that both themes define.
- Bricolage Grotesque for headings and titles, Manrope for everything read or clicked, system monospace for code.
- Ruled rows and single bordered panels; hairlines at 6 to 12 percent alpha.
- Coral marks position and response (current chapter, open handbook, hover, links), never a filled area.
- Motion is state change at 160ms on one ease-out curve; the request trace is the single staged sequence.

## Colors

Near-black neutrals with warm off-white text, one coral for place and emphasis, and a small set of semantic and code hues that only appear inside callouts and code. The frontmatter holds the dark (default) values; light-theme counterparts are in `.impeccable/design.json` under `colorMeta.<token>.light`.

### Primary

- **Komiq Coral** (`coral`, `--k-accent`): the brand mark's colour. Used for the focus ring, the current-chapter dot on the sidebar hairline, the open handbook's icon, the active "On this page" rule, arrows and icon tiles on hover, the selected search hit's icon, the wire arrow in a flow, and the search-match highlight at 14 percent (`--k-accent-soft`).
- **Reading Link** (`doc-link` / `doc-link-light`, `--doc-link`): the underline colour of links in running text and the hover colour of those links. Equal to the coral in the dark theme; darkened to `doc-link-light` in the light theme so coral-coloured text holds contrast on pale surfaces.

### Secondary

- **Semantic tones** (`success`, `warning`, `info`, `danger`): only inside callouts (text, a 12 percent fill, a 34 percent border), the "Copied" state of a code block's copy button (`success`), and the failed-index message in search (`danger`).
- **Code hues** (`code-keyword`, `code-function`, plus `success` for strings and `warning` for constants): syntax colours inside code blocks only. They reuse hues the product already owns; comments use `text-dim`, punctuation `text-muted`, everything else `text-1`.

### Neutral

- **Page** (`bg`): the main column's background.
- **Raised surface** (`surface`): the sidebar, table panels, the flow panel, the chapter index.
- **Field surface** (`surface-2`): search triggers, code blocks, table header rows, the search palette, the folded "On this page" box.
- **Chip surface** (`surface-4`): inline code, flow nodes, key hints, default buttons, resting icon tiles.
- **Selected surface** (`surface-5`): the current page in the sidebar, the selected search hit, the pressed segment.
- **Hover fill** (`hover-fill`): the hover wash on any row or ghost control.
- **Headline text** (`text-bright`): headings, titles, strong text, link text, current items.
- **Body text** (`text-2`): prose paragraphs. `text-1` is used for code, table first columns and secondary links.
- **Supporting text** (`text-muted`): ledes, section intros, resting sidebar handbooks, flow descriptions.
- **Quiet text** (`text-dim`): chapter links at rest, counts, breadcrumbs, table headers, list markers, footer text, placeholder-style labels.
- **Hairlines** (`border` through `border-3`): `border` divides rows inside a panel; `border-1` outlines panels and rules the page head and footer; `border-2` rules index lists and outlines quiet boxes; `border-3` outlines controls. `border-strong` is the hover outline.

### Named Rules

**The Place Marker Rule.** Coral says where you are or what just responded: current, open, hovered, focused, matched, linked. It is a dot, a 1px rule, an icon, an underline or a 14 percent wash. The only filled button is the inverted off-white primary (`primary` on `on-primary`); coral is never a button fill or a panel background.

**The Dark Underline Rule.** A link in running text is `text-bright` at weight 600 with a 1.5px underline in `--doc-link`, offset 3px; the text itself turns `--doc-link` on hover. Coloured text in the light theme uses `--doc-link`, not `--k-accent`.

**The Borrowed Hue Rule.** A hue other than coral appears only where it carries meaning the reader already knows: a callout's tone or a code token. No hue is introduced for decoration, and none is added that the console tokens do not already define.

## Typography

**Display Font:** Bricolage Grotesque Variable (with system-ui, sans-serif)
**Body Font:** Manrope Variable (with system-ui, -apple-system, sans-serif)
**Label/Mono Font:** the system monospace stack (ui-monospace, SF Mono, Menlo) for code, addresses, key hints and code-language tags

**Character:** A characterful grotesque set tight and bold for anything that names a thing, over a plain, even sans for anything read at length. Headings are always weight 700 with negative tracking that grows with size; body text never tracks.

### Hierarchy

- **Display** (700, clamp(40px, 7vw, 68px), 1.0, -0.04em): the home page title only. The handbook index title steps down to clamp(34px, 5.5vw, 52px) at 1.05 and -0.035em.
- **Headline** (700, clamp(30px, 4.4vw, 40px), 1.1, -0.035em): the chapter title and the error title.
- **Section** (700, 26px, 1.2, -0.03em): section headings on the home page.
- **Prose headings** (700; h2 23px at -0.025em, h3 18px at -0.015em): headings inside a chapter. h2 takes 56px above, h3 36px, and 12px below either. A prose h4 is Manrope 700 at 15px with no tracking.
- **Title** (700, 21px, -0.025em): the name of a row or panel that leads somewhere (a handbook row, an app column). Steps to 17px in the chapter index and 16px in the previous/next pager, at -0.015em.
- **Lede** (400, 18px, 1.55, `text-muted`, 46 to 62ch): the one paragraph under a page title. The home lede scales clamp(17px, 2vw, 20px) at 1.5.
- **Body** (400, 16px, 1.7, `text-2`): prose, capped at 70ch (700px in the chapter column). Blocks are 18px apart; list items 8px.
- **UI** (600, 14px, 1.5) and **UI small** (600, 13px): sidebar handbooks, row labels, table cells, search results; 13px for chapter links, breadcrumbs, counts, buttons and the footer. 12px (`--k-text-xs`) for code-language tags, the copy button and pager direction labels.
- **Label** (700, 11.5px, 0.05em, uppercase): Markdown table header cells, matching the console's table headers.
- **Mono** (400, 13px, 1.7 in blocks; 0.86em inline): code. Tab size 2.

### Named Rules

**The Two Voices Rule.** Bricolage Grotesque names things (page titles, section headings, row titles, the pager's chapter name, the chapter index numerals). Manrope is everything else, including buttons, navigation and ledes. A prose h4 is Manrope: by that depth it is a label, not a title.

**The Tight As It Grows Rule.** Heading tracking runs from -0.015em at 16 to 18px to -0.04em at 68px, and line height from 1.2 down to 1.0. Body text is never tracked and never set below a 1.5 line.

**The Tabular Count Rule.** Numbers that sit in a column or change in place (chapter counts, "Chapter 4 of 22", hop numbers, ordered-list markers) use tabular figures.

## Layout

The shell is a two-column grid: a sticky 264px sidebar, full viewport height with its own scroll, and a main column capped at 1180px with 56px above, 40px at the sides and 96px below. Inside the main column, home and the handbook index cap at 980px; a chapter's article caps at 700px, and from 1240px wide a sticky 200px "On this page" rail sits at the far edge of the main column. Everything is left-aligned to the main column's edge.

Spacing is the console's 4px scale (`--k-space-1` to `--k-space-14`). Larger, page-level gaps are plain values: 88px between home sections (64px at 760px and below), 48px between a handbook's head and its chapter list, 40px and 32px around a chapter head's rule, 72px above the pager. Row rhythm is fixed: 54px minimum for flow rows, 46px for the app columns' rows, 36px for sidebar handbooks, 32px for sidebar chapters.

Responsive behaviour, as built:

- **900px and below:** the sidebar becomes an off-canvas drawer (min(86vw, 300px), slides in over 220ms behind a scrim) opened from a sticky 56px top bar with a blurred translucent background. Main padding drops to 32px above, 16px at the sides. Sidebar rows grow to 44px and 40px.
- **1240px and above:** the "On this page" rail appears; below that the same list folds into a disclosure box above the prose.
- **1080px and below:** handbook rows drop their first-chapters column. **760px and below:** two-column sections stack and rules change from vertical to horizontal. **640px and below:** a flow row wraps, with the description on its own line. **560px and below:** the pager stacks and the chapter index drops its arrows.
- **Coarse pointers:** `k-` controls take a 44px minimum; key hints are hidden.

**The One Measure Rule.** Running prose never exceeds 70ch. A block may run wider than the measure only when it is not prose: a flow, or a code block set beside its explanation.

## Elevation & Depth

Flat, with tonal layering. Depth is a step along the surface ladder (`bg` to `surface` to `surface-2` to `surface-4` to `surface-5`) plus a 1px alpha border. Shadows are reserved for things that leave the page plane.

### Shadow Vocabulary

- **Pressed segment** (`--k-shadow-1`, `0 1px 2px rgba(0, 0, 0, 0.32)`): the selected button in a segmented control.
- **Floating layer** (`--k-shadow-3`, `0 24px 64px rgba(0, 0, 0, 0.55)`): the search palette and the mobile sidebar drawer, always with the scrim (`--k-scrim`) behind.
- **Knockout ring** (`0 0 0 3px` in the sidebar's surface colour): cuts the hairline around the current-chapter dot. Not a shadow in effect; it is a gap.

### Named Rules

**The Floats Only Rule.** A shadow means the element sits above the page and something is dimmed behind it. Panels, rows, code blocks and buttons at rest have none; hover never adds one.

## Shapes

Softly rounded rectangles on a short scale, all outlined in 1px. Controls, nav rows and flow nodes use 8px (`base`); code blocks and the segmented track 9px (`md`); panels, the pager steps, icon tiles, the hero search field and the search palette 12px (`lg`); inline code and key hints 5px (`sm`). The pill radius is used once, for the "Docs" tag beside the wordmark. The only circle is the 7px current-chapter dot.

Borders are always 1px and always an alpha of white (dark) or black (light), so they hold on any surface step. Two exceptions carry meaning: a key hint has a 2px bottom border to read as a key cap, and an empty state would use a dashed border (console rule; not yet used in the docs). Panels clip their contents so inner rows and header fills meet the rounded corner.

Icons are the console's set: inline SVG on a 24px grid, 2px stroke, round caps and joins, drawn at 12 to 22px in `currentColor`.

## Components

### Buttons

The console's `k-btn`, unchanged.

- **Shape:** 36px tall, 8px radius, 1px border, 0 14px padding; 13px weight 600. Small is 30px with 12px text, large 42px with 14px text.
- **Primary:** inverted, `primary` on `on-primary` (off-white on near-black in dark, flipped in light). One per view: "Start reading", "Install with Docker", "Search the docs" on the error page.
- **Default:** `surface-4` with a `border-3` outline; hover steps to `surface-5` and `border-strong`.
- **Ghost / icon:** transparent, `text-muted`, `hover-fill` on hover. Used for the menu, top-bar search and theme buttons.
- **Press / Focus:** scale 0.98 while pressed; 2px coral outline offset 2px on keyboard focus (every focusable element, not only buttons).

### Search triggers and palette

- **Trigger:** a button styled as a field (`surface-2`, 1px border, `text-dim` label, search icon leading, key hint trailing). 36px with 8px radius in the sidebar; 52px with 12px radius and 16px text under the home title. Hover lifts the border to `border-strong`.
- **Palette:** a modal dialog 640px wide at most, 16vh from the top, `surface-2` with a `border-3` outline, 12px radius and the floating shadow. A 54px borderless input row, then results. A result is an 8px-radius row with a file or hash icon, a 14px title, a right-aligned location in 12px, and a two-line 13px snippet; the selected row is `surface-5` with a coral icon. Matches are highlighted with the 14 percent coral wash. Opens with Cmd/Ctrl+K or `/`; drops in 8px over 160ms.

### Navigation

- **Sidebar:** brand lockup, search trigger, five handbook rows (icon, title, chapter count), then a foot with the site link and a theme button. A handbook row is 36px, 14px weight 600, `text-muted`; hover is `hover-fill` and `text-bright`.
- **Open handbook:** its icon turns coral and its chapters, and only its chapters, list beneath, hanging off a 1px hairline under the icon. Chapter links are 32px, 13px weight 500, `text-dim`. The current chapter is `surface-5`, weight 600, with a 7px coral dot sitting on the hairline.
- **On this page:** a 200px rail with a 1px left rule; the active heading's segment of the rule turns coral and its text `text-bright`. Sub-headings indent 24px.
- **Breadcrumb:** handbook name, a chevron, "Chapter n of total", in 13px `text-dim`.
- **Pager:** two 12px-radius outlined steps (previous left, next right-aligned), each a 12px direction label over a 16px display-face chapter name. The first chapter's "previous" goes back to its handbook.

### Index rows

The docs' way of listing things to open, used for handbooks on the home page, the two app columns, and (inside a panel) a handbook's chapters.

- **Structure:** rows separated by 1px rules, no gaps; a display-face title, a `text-dim` or `text-muted` description, a trailing arrow icon.
- **Hover:** the row takes `hover-fill`; the arrow turns coral and moves 3px right; a leading icon tile, if present, goes from `surface-4` to the coral wash with a coral border.
- **Icon tile:** 40 to 44px square, 12px radius, 1px border, icon centred.
- **Numbered variant:** the chapter index sits in one `surface` panel and leads each row with a display-face tabular numeral, because order is the content.

### Panels

- **Corner Style:** 12px.
- **Background:** `surface`, with `surface-2` for a header row.
- **Shadow Strategy:** none (see Elevation).
- **Border:** `border-1` outside, `border` between rows.
- **Internal Padding:** 16px inline; 10 to 11px block in tables and flows, 18px by 20px in the chapter index.
- **Tables:** Markdown tables render in a panel that scrolls sideways and is keyboard-focusable. Header cells are the uppercase label style on `surface-2`; cells are 14px on a 1.55 line, top-aligned; the first column is `text-1`.

### Code

- **Block:** `surface-2`, 1px `border-1`, 9px radius. A top row holds the language tag (12px mono, `text-dim`) on the left and a ghost "Copy" button on the right that turns `success` and reads "Copied" for 1.6s. Code is 13px mono on a 1.7 line and scrolls sideways; it never wraps.
- **Inline:** `surface-4` chip with a `border-1` outline and 5px radius, 0.86em. Inside a callout the chip takes its colour from the callout's text.

### Callouts

The console's `k-notice` at 15px: a 16px stroke icon, then a bold label on its own line, then the body. Tone sets text, a 12 percent fill and a 34 percent border for warning, danger, success and info. A plain note keeps the neutral `surface-2` treatment with a `text-bright` label. Links and inline code inside inherit the tone.

### Flow (signature)

A path that data takes, one hop per row, in a single panel. Each row: a tabular hop number in `text-dim`, a from-node, a coral wire arrow, a to-node, and a `text-muted` sentence saying what crosses. Nodes are `surface-4` chips with a `border-3` outline and 8px radius, 13px weight 600. Columns align across rows by subgrid, so every arrow sits in one vertical line. On the home page the flow is the request trace: a segmented control switches between two paths, and the rows wipe in left to right over 560ms, 130ms apart, once when scrolled into view and again on each switch. With reduced motion, or without script, the rows are simply present.

### Segmented control

The console's `k-segmented`: a `surface-2` track with 3px padding and 9px radius; 28px segments in 12px weight 700; the pressed segment is `surface-5`, `text-bright`, with the pressed-segment shadow.

## Do's and Don'ts

### Do:

- **Do** build from the `--k-*` tokens and `k-` classes in `@komika/ui`; add a docs-only style only for long-form content, and put it in `src/lib/docs.css`.
- **Do** keep both themes working: take every colour from a token, and use `--doc-link` for coral-coloured text so the light theme gets its darker value.
- **Do** list things to open as ruled index rows, and put structured content (code, tables, flows, numbered chapters) in one 12px-radius panel with hairline dividers.
- **Do** keep running prose within 70ch at 16px on a 1.7 line.
- **Do** set headings and row titles in Bricolage Grotesque at weight 700 with negative tracking, and everything else in Manrope.
- **Do** use the 160ms `--k-ease` transition for state changes, and honour reduced motion.
- **Do** show a key hint beside anything with a shortcut, and hide it on coarse pointers.
- **Do** keep interactive rows at 44px or taller on touch layouts.

### Don't:

- **Don't** redefine a console token or restyle a `k-` control inside the docs; change it in `@komika/ui` so the admin console moves with it.
- **Don't** fill a button, panel or section with coral; the filled button is the inverted primary.
- **Don't** add a hue the tokens do not define, or use a semantic or code hue outside a callout or a code block.
- **Don't** put a shadow on anything that is not floating over a scrim, apart from the pressed segment.
- **Don't** nest a bordered panel inside another bordered panel; divide with a hairline.
- **Don't** centre page content or headings; every template is left-aligned to the main column.
- **Don't** list another handbook's chapters in the sidebar; only the open handbook expands.
- **Don't** let code wrap or a table squeeze; both scroll sideways inside their panel.
