---
name: crusty-data-explorer
description: A native, local-first SQL client where color is a signal, not decoration.
colors:
  # Tonal stack, dark (default theme)
  cockpit-window: "#181d25"
  panel-slate: "#202732"
  raised-slate: "#2a323f"
  ink-well: "#12171e"
  code-well: "#28303c"
  # Daylight stack, light theme
  paper-window: "#f2f4f7"
  paper-panel: "#eceef2"
  paper-raised: "#e4e7ec"
  paper-well: "#e1e4e9"
  paper-code-well: "#f0f2f5"
  # Structural accent
  console-blue: "#3a6ea5"
  selection-fill-dark: "#3a6ea5"
  selection-fill-light: "#cfe2f7"
  selection-text-dark: "#ffffff"
  selection-text-light: "#1e508c"
  # Signal family
  commit-green-dark: "#73b473"
  commit-green-light: "#388e3c"
  staging-amber: "rgba(227, 179, 65, 0.25)"
  caution-amber-dark: "#e3b341"
  caution-amber-light: "#966e0f"
  guard-red-dark: "#e05b5b"
  guard-red-light: "#bc3c3c"
  advisory-orange-dark: "#ff8f00"
  advisory-orange-light: "#ff6400"
  fault-red: "#ff0000"
  # Syntax tints
  signal-blue-dark: "#78aadc"
  signal-blue-light: "#1976d2"
  lichen-dark: "#8cc896"
  lichen-light: "#2e7d32"
  steel-comment-dark: "#828c9b"
  steel-comment-light: "#6e7781"
  brass-dark: "#dcaa6e"
  brass-light: "#aa641e"
  # Neutrals and text
  dev-steel-dark: "#949ba6"
  dev-steel-light: "#5f6973"
  quiet-text-dark: "#8c8c8c"
  quiet-text-light: "#505050"
typography:
  heading:
    fontFamily: "Ubuntu-Light (egui embedded)"
    fontSize: "16px"
  body:
    fontFamily: "Ubuntu-Light (egui embedded)"
    fontSize: "13px"
  button:
    fontSize: "13px"
  small:
    fontSize: "11px"
  mono:
    fontFamily: "Hack (egui embedded)"
    fontSize: "12.5px"
rounded:
  window: "6px"
  widget: "4px"
  badge: "3px"
  cell: "0px"
spacing:
  item: "8px 4px"
  button-padding: "10px 6px"
  indent: "16px"
components:
  button-run:
    backgroundColor: "{colors.commit-green-dark}"
    textColor: "#ffffff"
    typography: "{typography.button}"
    rounded: "{rounded.widget}"
    padding: "10px 6px"
  button-create:
    backgroundColor: "{colors.console-blue}"
    textColor: "#ffffff"
    typography: "{typography.button}"
    rounded: "{rounded.widget}"
    padding: "10px 6px"
  button-default:
    backgroundColor: "#3c3c3c"
    textColor: "#b4b4b4"
    typography: "{typography.button}"
    rounded: "{rounded.widget}"
    padding: "10px 6px"
  button-small:
    backgroundColor: "#3c3c3c"
    textColor: "#b4b4b4"
    typography: "{typography.small}"
    rounded: "{rounded.widget}"
  env-badge:
    backgroundColor: "rgba(224, 91, 91, 0.2)"
    textColor: "{colors.guard-red-dark}"
    typography: "{typography.small}"
    rounded: "{rounded.badge}"
    padding: "4px 1px"
---

# Design System: crusty-data-explorer

## Overview

**Creative North Star: "The Instrument Flight Deck"**

This is a dark calibrated cockpit for a single operator working against real, sometimes dangerous databases. The interface is quiet at rest: a cold blue-gray panel stack, one structural blue, and a semaphore of green, amber, and red that stays dark until state or safety demands it speak. Nothing is decorative. Every tint on screen is reporting something: a staged cell, a transaction, a production connection, a row that differs. The mood is quiet precision, and the discipline that produces it is subtraction. Color arrives late and leaves early.

The system is dark-first and tonal. Depth is carried by four measured steps of the same blue-gray family, not by shadow or glow; shadow exists only where a surface genuinely floats (dialogs, menus, the command palette, tooltips). Data is always monospace and always the loudest thing on screen by sheer density, never by color. Chrome controls stay neutral gray, so the single green Run button reads as the only commitment device in the app. The light theme is a daylight translation of the same instrument, not a second identity.

The incumbent implementation is a Rust/egui desktop application. The tokens here were extracted from the shipped code: `src/ui/mod.rs` owns both themes and the environment palette, and every module reads colors through it. Values below are that theme's exact literals.

**Key Characteristics:**
- Dark-first blue-gray tonal stack; light theme is a parallel "daylight paper" translation.
- One structural accent (Console Blue); saturation is reserved for signals.
- A safety semaphore: green commits, amber stages and cautions, red guards and faults.
- Monospace data everywhere a value can be copied, aliased, or diffed.
- Flat panels, 1px strokes; shadow only on true floating layers.
- Small, tight type (11px to 16px); emphasis through weight and brightness, never size.
- Unicode glyphs as the entire icon set: `▶ Run`, `★` pin, `×` close, `+` new, `●`/`○` connection state.

## Colors

A cold blue-gray night panel, one structural blue, and a semaphore that speaks only when something needs attention.

### Primary
- **Console Blue** (#3a6ea5, identical in both themes): the system's single structural accent. The dark theme's selection fill (light draws its selection as the pale tint below), pinned `★` marks, hyperlinks and keywords in dark mode (#78aadc / #1976d2), `+ New connection` and `New query` fills, the selected connection card stroke, charts (2px stroke, 2.5px dots), and the heavy-path stroke in plan views (2px). If a screen has more than a few touches of this blue, it is misusing it: the blue marks *where you are*, not what is important.

### Signals
- **Commit Green** (#73b473 dark / #388e3c light): the fill of the `▶ Run` button, the "transaction open" note, and success text. It appears at most once per screen, on the one control that commits work.
- **Staging Amber** (rgba(227, 179, 65, 0.25)): the translucent tint over staged (edited but unapplied) grid cells. The only translucent color in the system; translucency is what makes "not yet real" legible.
- **Caution Amber** (#e3b341 dark / #966e0f light): the `Test` environment badge and the `Changed` diff-row label.
- **Guard Red** (#e05b5b dark / #bc3c3c light): the `Prod` environment badge. A softened red, reserved for the standing identity of a dangerous connection.
- **Advisory Orange** (#ff8f00 dark / #ff6400 light, egui default): warning text: row-cap notices, partial diffs.
- **Fault Red** (#ff0000, egui default): failure text only: a query error, a `Removed` diff row, an invalid cell editor value. Rawer than Guard Red on purpose.

**The Semaphore Rule.** Saturation is a message. Green means committing, amber means staged, cautioned, or changed, red means guarded or failed. If a surface is saturated and none of those is true, the color is wrong.

**The Two Reds Rule.** Guards wear the softened Guard Red; raw Fault Red is reserved for terminal text about failure. Never swap them.

### Syntax (dark values; light counterparts in parentheses)
- **Signal Blue Keywords** (#78aadc / #1976d2): SQL keywords and hyperlinks.
- **Lichen Strings** (#8cc896 / #2e7d32), **Steel Comments** (#828c9b / #6e7781), **Brass Numbers** (#dcaa6e / #aa641e): the editor's remaining tints. All stay muted enough to sit below the semaphore: no syntax color is as hot as Guard Red.

### Neutral
- **Cockpit Window** (#181d25; light **Daylight Paper Window** #f2f4f7): the outer window fill behind every panel.
- **Panel Slate** (#202732; light #eceef2): panel fills (sidebar, editor panel, status bar).
- **Raised Slate** (#2a323f; light #e4e7ec): the raised tone: table striping, connection-card fill, plan cost-bar track, hover fills.
- **Ink Well** (#12171e; light #e1e4e9): the recessed tone: every text input, the editor's framed well. Inputs sink here; they never lift.
- **Code Well** (#28303c; light #f0f2f5): code background token, for inset code surfaces.
- **Quiet Text** (#8c8c8c dark / #505050 light, egui default): normal label text. Most secondary text renders at 60% of this (egui `weak()`); plain button text runs a step brighter (#b4b4b4 dark / #3c3c3c light), and `strong` labels (Run, New connection, the active tab) render at the theme's maximum contrast: white on dark, black on light. Text is quiet so state can be loud.
- **Dev Steel** (#949ba6 dark / #5f6973 light): the `Dev` badge, neutral indicators.
- **Selection Fill** (#3a6ea5 dark / #cfe2f7 light): the selected-row and selected-widget fill. The dark theme selects with saturated Console Blue; the light theme selects with a pale tint, egui's own light-theme convention, so dark text can stay legible on it.
- **Selection Text** (#ffffff dark / #1e508c light): one value per theme for every stroke and label that rides a selection. Each theme's `selection.stroke` is set from this token, so it is the selected-widget text color, the focused text-field ring, and the active-cell outline at once. White holds 5.3 to 1 on the dark fill; #1e508c holds 6.2 to 1 on the light fill and 6.4 to 1 as the ring on the light field.

## Typography

**Display Font:** Ubuntu-Light (embedded with egui, the proportional default)
**Body Font:** Ubuntu-Light, same face at every level
**Mono Font:** Hack (embedded with egui, the monospace default)

**Character:** One calm proportional voice for chrome and one engineered monospace voice for data. The app installs no custom fonts: the embedded defaults are the identity, and future work must not introduce a third face or a display face.

### Hierarchy
- **Heading** (regular weight, 16px): panel and window headings, dialog titles.
- **Body** (regular weight, 13px): labels, buttons, menu items, prose in forms.
- **Small** (regular weight, 11px): the workhorse secondary size: section labels, state text, hints, badges, diff labels. `weak()` (60% alpha) for anything the operator should be able to ignore.
- **Mono** (12.5px): every SQL editor, grid cell, DDL pane, plan stat, JSON viewer, and record form. Data is monospace at every altitude.

**The Data Is Monospace Rule.** If a value can be copied, aliased, diffed, or executed, it renders in Hack. If it cannot, it renders in Ubuntu-Light. No exceptions to date.

**The Weight Rule.** Emphasis is weight and brightness (`strong()` brightens, `weak()` dims), never a bigger font. Every size above is fixed; the app has no fluid type.

## Layout

A fixed four-panel instrument frame, drawn in this order: command palette on top of everything, then the left sidebar (240px default, resizable, holding connections, schema, saved queries, history, bookmarks), the bottom status bar (stats left; theme toggle and backend label right), the resizable top panel (tab bar, toolbar, editor), and the results grid in the center filling the rest. New surfaces are panels, tabs, or modal windows inside this frame; the frame itself is the app's skeleton and does not change.

Density is deliberate: item spacing 8px horizontal by 4px vertical, button padding 10px by 6px (chunky targets for a keyboard-first operator), tree indent 16px, and a 6px inner margin around the framed editor well. The command palette is the one floating layout: a 460px window anchored center-top 60px down, filter box on top, a selection list capped at 320px tall with the selected row scrolled into view. Diff rows, record forms, and import mapping tables use striped (Raised Slate) grids. The grid is the only element allowed to consume all remaining space.

**The Instrument Frame Rule.** The four panels are the cockpit. Work lands inside them; it does not reshape them.

## Elevation & Depth

Tonal-first. Panels are flat and separated by tone alone: window, panel, raised, and the recessed Ink Well, with 1px strokes (egui's gray-60 border dark / gray-190 light) marking group and card edges. There is no glow, no gradient, and no shadow vocabulary of the app's own. Depth appears only when a surface truly floats above the frame: dialogs, menus, the command palette, and tooltips carry egui's default window shadow (offset 10, 20; blur 15; black at 96 alpha) and its 6px corner radius. Hover feedback is tonal too: interactive surfaces step up 10 gray levels rather than lighting up.

**The Tonal-First Rule.** Depth is a tone difference, and shadow is reserved for true floating layers. A panel with a shadow is a bug.

**The One Well Rule.** Inputs, editors, and any surface that accepts text recess into the Ink Well (light theme: paper well). Nothing that accepts typing is ever raised or tinted.

### Shadow Vocabulary
- **Floating layer shadow** (`offset 10px 20px, blur 15px, rgba(0, 0, 0, 0.376)`): egui's window shadow, present only on dialogs, menus, the palette, and tooltips. The app never applies a shadow to panels or cards.

## Shapes

Lightly rounded instrument edges, nothing softer. Windows and cards take 6px corners; widgets (buttons, inputs, tabs, frames) take 4px in both themes (an explicit override of egui's 2px default); environment badges take 3px; plan cost bars take 3px. Grid cells are the deliberate square element: selection fills and active-cell outlines paint with 0 radius so data reads as a machined surface, not a set of buttons. Strokes are uniformly 1px except two sanctioned exceptions: the active-cell outline (1px selection stroke in the theme's selection text color) and the heavy-path plan node (2px Console Blue). No pills, no circles except status dots and chart points, no heavy borders.

**The Quiet Edge Rule.** Rounding is a whisper: 6px for surfaces, 4px for widgets, 3px for badges, 0 for data. Nothing is a pill and nothing has a double border.

## Components

### Buttons
Small and calm; the fill carries the meaning, never the shape.
- **Shape:** 4px radius, 10px by 6px padding, 13px text; small variants drop to 11px text.
- **Primary (Run):** the system's single filled commit action: Commit Green fill, `▶ Run` in strong weight. It is the only green control in the app.
- **Create (`+ New connection`, `New query`):** Console Blue fill, strong weight. Blue means "this makes something", green means "this commits something".
- **Default:** neutral gray fill (#3c3c3c dark / #e6e6e6 light), quiet text; hovers one gray step up.
- **Destructive:** deliberately unstyled: `Delete` is a plain small button with no red fill and no red text. Danger is signalled by the Prod badge and confirm modals, not by the button.

### Badges (Environment)
Six pixels of typography carrying the most important fact on the card. Fill is the badge color at 20% alpha, stroke is the same color at 60% (1px), text is the full color at 11px strong weight, corners 3px, padding 4px by 1px. `Dev` is Dev Steel, `Test` is Caution Amber, `Prod` is Guard Red. The badge never grows, animates, or gains a shadow; its restraint is its authority.

### Cards (Connection)
6px radius, 8px inner margin, Raised Slate fill with a 1px neutral stroke; the selected card swaps to a tinted fill (35% Console Blue in dark, the full pale selection fill in light) with a full Console Blue stroke. Contains: state dot (`●` connected / `○` offline), name, environment badge, state text in `small().weak()`, then action buttons; the live schema tree nests inside with 16px indents.

### Inputs / Fields
Recessed Ink Well fill on the 4px widget radius with a 1px stroke (egui's window stroke) and 13px text. Focus is egui's automatic treatment; validation errors render as Fault Red text beside or above the field, never as recolored field fills.

### Navigation
The sidebar is a stack of uppercase 11px `weak()` section labels (`CONNECTIONS`, `HISTORY`, `SAVED`, `BOOKMARKS`) over collapsible groups. Tree rows step 16px per depth; folders are drag targets and their headers read "drop here for root" in weak text while a drag is live. Tabs are selectable labels: the active tab is strong weight, inactive tabs weak, an open transaction adds a small strong `tx` marker beside the title, and `×` closes.

### Signature components
- **Data cell (grid):** square; monospace 12.5px; selection paints the theme's selection fill and switches the label to Selection Text (white in dark, #1e508c in light), with a 1px Selection Text stroke on the active cell; staged edits overlay the translucent Staging Amber tint; staged deletes render strikethrough and weak; NULL is the literal word `NULL`. Cells are the only place translucency and strikethrough appear.
- **Editor well:** the SQL surface recesses into the Ink Well inside a 4px-radius frame with 6px margin; syntax tints from the Syntax palette; the cursor's line is the only line-level highlight. The completion popup and find bar are the only overlays inside it.
- **Heavy-path plan node:** plan trees render as bordered group frames; nodes on the heavy path take a 2px Console Blue stroke, and their operation name turns strong. Each node's cost share draws as a thin bar: Raised Slate track, Console Blue fill when heavy, weak text color otherwise.
- **Command palette:** the floating layer done right: 460px wide, centered 60px from the top, recessed filter input, then a list of rows where the selection is egui's standard selectable highlight. No accent color is spent here beyond the platform's own selection fill.
- **Diff rows (compare):** one-line labeled rows: kind label in 11px strong, colored by signal (`Added` weak, `Changed` Advisory Orange, `Removed` Fault Red), with `+n more` in weak text when truncated.
- **Status bar:** the flight strip: left side reports connection and row counts in body text; right side holds the theme toggle and the backend name in `small().weak()`. It never grows taller than one line.

## Do's and Don'ts

### Do:
- **Do** keep Commit Green to one control per screen: the action that commits work.
- **Do** switch selected-cell text to the theme's Selection Text (#ffffff dark, #1e508c light); quiet gray text never sits on a selection fill.
- **Do** paint staged-but-unapplied changes with the Staging Amber tint (rgba(227, 179, 65, 0.25)); translucency is reserved for this meaning.
- **Do** render every value in Hack monospace and every label in Ubuntu-Light, at the fixed sizes 11 / 13 / 16 (and 12.5 mono).
- **Do** carry depth with the four tonal steps and 1px strokes; reserve shadow for dialogs, menus, the palette, and tooltips.
- **Do** keep text quiet (`weak()` at 60% for anything ignorable) so the semaphore and the data own the contrast budget.
- **Do** keep badges at 20% fill / 60% stroke with full-color 11px strong text, and keep Prod red softened (Guard Red), not raw.
- **Do** use the Unicode glyph set (`▶`, `★`, `×`, `+`, `●`, `○`) instead of introducing an icon font or SVG sprite.
- **Do** keep destructive actions understated, and put danger into environment badges, read-only flags, and confirm modals instead.

### Don't:
- **Don't** introduce a saturated color outside the semaphore: blue locates, green commits, amber stages or cautions, red guards or faults. There is no decorative fifth color.
- **Don't** apply shadow, gradient, or glow to panels, cards, or cells. A raised panel is off-system by definition.
- **Don't** use pill radii, double borders, or any stroke heavier than the 2px heavy-path accent.
- **Don't** fill destructive buttons with red or style errors into field backgrounds; Fault Red is text-only, and it is never used for a standing identity (that is Guard Red's job).
- **Don't** add a second font family, a display face, or fluid type; the embedded Ubuntu-Light / Hack pairing is the identity.
- **Don't** uppercase anything except the sidebar section labels.
- **Don't** spend Console Blue as decoration: it marks location (the active cell, the selected card's stroke, the dark theme's selection fill) and structure (create actions, heavy path), and it should read as rare on any given screen.
- **Don't** round grid cells or tint them for anything except staged state.
