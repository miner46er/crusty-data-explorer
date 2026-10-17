#!/usr/bin/env python3
"""Assemble .impeccable/design.json (schemaVersion 2) for crusty-data-explorer."""
import json, datetime, sys
import os
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ramps import hex_to_oklch, oklch_hex

ramps = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'ramps.json')))

# quiet-text and selection ramps computed here (not in ramps.json)
for name, h in [("quiet-text-dark", "#8c8c8c"), ("quiet-text-light", "#505050"),
                ("selection-fill-dark", "#3a6ea5"), ("selection-fill-light", "#cfe2f7"),
                ("selection-text-dark", "#ffffff"), ("selection-text-light", "#1e508c"),
                ("disabled-fill-dark", "#232933"), ("disabled-text-dark", "#949ba6"),
                ("disabled-fill-light", "#eaedf1"), ("disabled-text-light", "#5f6973")]:
    L, C, H = hex_to_oklch(h)
    ramps[name] = {"source": h, "oklch": f"oklch({L*100:.1f}% {C:.3f} {H:.1f})",
                   "ramp": [oklch_hex(round(15+(95-15)*i/7)/100, C, H) for i in range(8)]}

# frontmatter name -> (source key in ramps, displayName, role, canonical)
meta_map = {
    "cockpit-window": ("cockpit-window", "Cockpit Window", "neutral", "#181d25"),
    "panel-slate": ("panel-slate", "Panel Slate", "neutral", "#202732"),
    "raised-slate": ("raised-slate", "Raised Slate", "neutral", "#2a323f"),
    "ink-well": ("ink-well", "Ink Well", "neutral", "#12171e"),
    "code-well": ("code-well", "Code Well", "neutral", "#28303c"),
    "paper-window": ("light-window", "Daylight Paper Window", "neutral", "#f2f4f7"),
    "paper-panel": ("light-panel", "Daylight Paper Panel", "neutral", "#eceef2"),
    "paper-raised": ("light-raised", "Daylight Paper Raised", "neutral", "#e4e7ec"),
    "paper-well": ("light-well", "Daylight Paper Well", "neutral", "#e1e4e9"),
    "paper-code-well": ("light-code-well", "Daylight Paper Code", "neutral", "#f0f2f5"),
    "console-blue": ("console-blue", "Console Blue", "primary", "#3a6ea5"),
    "selection-fill-dark": ("selection-fill-dark", "Selection Fill (Dark)", "primary", "#3a6ea5"),
    "selection-fill-light": ("selection-fill-light", "Selection Fill (Light)", "primary", "#cfe2f7"),
    "selection-text-dark": ("selection-text-dark", "Selection Text (Dark)", "accent", "#ffffff"),
    "selection-text-light": ("selection-text-light", "Selection Text (Light)", "accent", "#1e508c"),
    "disabled-fill-dark": ("disabled-fill-dark", "Disabled Fill (Dark)", "neutral", "#232933"),
    "disabled-text-dark": ("disabled-text-dark", "Disabled Text (Dark)", "neutral", "#949ba6"),
    "disabled-fill-light": ("disabled-fill-light", "Disabled Fill (Light)", "neutral", "#eaedf1"),
    "disabled-text-light": ("disabled-text-light", "Disabled Text (Light)", "neutral", "#5f6973"),
    "commit-green-dark": ("commit-green-dark", "Commit Green (Dark)", "success", "#73b473"),
    "commit-green-light": ("commit-green-light", "Commit Green (Light)", "success", "#388e3c"),
    "staging-amber": ("staging-amber", "Staging Amber", "warning", "rgba(227, 179, 65, 0.25)"),
    "caution-amber-dark": ("caution-amber-dark", "Caution Amber (Dark)", "warning", "#e3b341"),
    "caution-amber-light": ("caution-amber-light", "Caution Amber (Light)", "warning", "#966e0f"),
    "guard-red-dark": ("guard-red-dark", "Guard Red (Dark)", "danger", "#e05b5b"),
    "guard-red-light": ("guard-red-light", "Guard Red (Light)", "danger", "#bc3c3c"),
    "advisory-orange-dark": ("advisory-orange-dark", "Advisory Orange (Dark)", "warning", "#ff8f00"),
    "advisory-orange-light": ("advisory-orange-light", "Advisory Orange (Light)", "warning", "#ff6400"),
    "fault-red": ("fault-red", "Fault Red", "danger", "#ff0000"),
    "signal-blue-dark": ("signal-blue-dark", "Signal Blue (Dark)", "syntax", "#78aadc"),
    "signal-blue-light": ("signal-blue-light", "Signal Blue (Light)", "syntax", "#1976d2"),
    "lichen-dark": ("lichen-dark", "Lichen (Dark)", "syntax", "#8cc896"),
    "lichen-light": ("lichen-light", "Lichen (Light)", "syntax", "#2e7d32"),
    "steel-comment-dark": ("steel-comment-dark", "Steel Comment (Dark)", "syntax", "#828c9b"),
    "steel-comment-light": ("steel-comment-light", "Steel Comment (Light)", "syntax", "#6e7781"),
    "brass-dark": ("brass-dark", "Brass (Dark)", "syntax", "#dcaa6e"),
    "brass-light": ("brass-light", "Brass (Light)", "syntax", "#aa641e"),
    "dev-steel-dark": ("dev-steel-dark", "Dev Steel (Dark)", "neutral", "#949ba6"),
    "dev-steel-light": ("dev-steel-light", "Dev Steel (Light)", "neutral", "#5f6973"),
    "quiet-text-dark": ("quiet-text-dark", "Quiet Text (Dark)", "neutral", "#8c8c8c"),
    "quiet-text-light": ("quiet-text-light", "Quiet Text (Light)", "neutral", "#505050"),
}

color_meta = {}
for fm_name, (key, display, role, canonical) in meta_map.items():
    color_meta[fm_name] = {
        "role": role,
        "displayName": display,
        "canonical": canonical,
        "tonalRamp": ramps[key]["ramp"],
    }

typography_meta = {
    "heading": {"displayName": "Heading", "purpose": "Panel and window headings, dialog titles (16px, Ubuntu-Light)."},
    "body": {"displayName": "Body", "purpose": "Labels, buttons, menu items, form prose (13px, Ubuntu-Light)."},
    "button": {"displayName": "Button", "purpose": "Button and toolbar labels (13px)."},
    "small": {"displayName": "Small", "purpose": "Section labels, state text, hints, badges, diff labels (11px); often at 60% alpha."},
    "mono": {"displayName": "Mono", "purpose": "SQL editor, grid cells, DDL, plan stats, JSON, record forms (12.5px, Hack)."},
}

shadows = [
    {"name": "floating-layer", "value": "10px 20px 15px 0px rgba(0, 0, 0, 0.376)",
     "purpose": "egui's window shadow. Dialogs, menus, the command palette, and tooltips only; never panels, cards, or cells."},
]
motion = [
    {"name": "ease-default", "value": "0.2s eased transition (egui animation_time)",
     "purpose": "Hover, selection, and window fades. The app sets no custom easing."},
    {"name": "tooltip-delay", "value": "0.5s",
     "purpose": "Hover tooltips appear after a still pause."},
]

components = [
    {
        "name": "Run Button",
        "kind": "button",
        "refersTo": "button-run",
        "description": "The single filled commit action: Commit Green, strong label. One per screen.",
        "html": '<button class="ds-btn-run">&#9654; Run</button>',
        "css": ".ds-btn-run { background: #73b473; color: #ffffff; font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 13px; font-weight: 500; padding: 6px 10px; border: 1px solid transparent; border-radius: 4px; cursor: pointer; transition: border-color 0.2s; } .ds-btn-run:hover { border-color: #969696; } .ds-btn-run:active { border-color: #ffffff; } .ds-btn-run:focus-visible { outline: 2px solid #ffffff; outline-offset: 1px; }",
    },
    {
        "name": "Create Button",
        "kind": "button",
        "refersTo": "button-create",
        "description": "Console Blue fill for create actions ('+ New connection', 'New query'). Blue means this makes something.",
        "html": '<button class="ds-btn-create">+ New connection</button>',
        "css": ".ds-btn-create { background: #3a6ea5; color: #ffffff; font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 13px; font-weight: 500; padding: 6px 10px; border: 1px solid transparent; border-radius: 4px; cursor: pointer; transition: border-color 0.2s; } .ds-btn-create:hover { border-color: #969696; } .ds-btn-create:active { border-color: #ffffff; } .ds-btn-create:focus-visible { outline: 2px solid #ffffff; outline-offset: 1px; }",
    },
    {
        "name": "Default Button",
        "kind": "button",
        "refersTo": "button-default",
        "description": "Neutral chrome: quiet gray fill, one step brighter text; hovers a tonal step up.",
        "html": '<button class="ds-btn">Apply filter</button>',
        "css": ".ds-btn { background: #3c3c3c; color: #b4b4b4; font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 13px; padding: 6px 10px; border: 1px solid transparent; border-radius: 4px; cursor: pointer; transition: background 0.2s, border-color 0.2s; } .ds-btn:hover { background: #464646; color: #f0f0f0; border-color: #969696; } .ds-btn:active { background: #373737; color: #ffffff; border-color: #ffffff; } .ds-btn:focus-visible { outline: 2px solid #ffffff; outline-offset: 1px; }",
    },
    {
        "name": "Small Action Button",
        "kind": "button",
        "refersTo": "button-small",
        "description": "Destructive and secondary actions stay understated: same neutral fill, 11px text, flush vertical padding. 'Delete' is never red.",
        "html": '<button class="ds-btn-sm">Delete</button>',
        "css": ".ds-btn-sm { background: #3c3c3c; color: #b4b4b4; font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 11px; padding: 1px 10px; border: 1px solid transparent; border-radius: 4px; cursor: pointer; transition: background 0.2s, border-color 0.2s; } .ds-btn-sm:hover { background: #464646; color: #f0f0f0; border-color: #969696; } .ds-btn-sm:active { background: #373737; color: #ffffff; border-color: #ffffff; }",
    },
    {
        "name": "Environment Badge",
        "kind": "chip",
        "refersTo": "env-badge",
        "description": "The most important fact on a connection card, in six pixels of type. 20% fill, 60% stroke, full-color 11px text, 3px corners.",
        "html": '<span class="ds-badge ds-badge--dev">Dev</span> <span class="ds-badge ds-badge--test">Test</span> <span class="ds-badge ds-badge--prod">Prod</span>',
        "css": ".ds-badge { display: inline-block; font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 11px; font-weight: 500; padding: 1px 4px; border-radius: 3px; border: 1px solid; line-height: 1.4; } .ds-badge--dev { color: #949ba6; background: rgba(148, 155, 166, 0.2); border-color: rgba(148, 155, 166, 0.6); } .ds-badge--test { color: #e3b341; background: rgba(227, 179, 65, 0.2); border-color: rgba(227, 179, 65, 0.6); } .ds-badge--prod { color: #e05b5b; background: rgba(224, 91, 91, 0.2); border-color: rgba(224, 91, 91, 0.6); }",
    },
    {
        "name": "Data Cell",
        "kind": "custom",
        "description": "The square machined surface: monospace values; in the dark theme selection switches the text to white with a white outline; translucent amber for staged edits; strikethrough for staged deletes.",
        "html": '<table class="ds-grid"><tr><td class="ds-cell">1</td><td class="ds-cell ds-cell--selected">ada@example.com</td><td class="ds-cell ds-cell--staged">pending</td><td class="ds-cell ds-cell--deleted">old@example.com</td><td class="ds-cell">NULL</td></tr></table>',
        "css": ".ds-grid { border-collapse: collapse; background: #202732; } .ds-cell { font-family: 'Hack', monospace; font-size: 12.5px; color: #8c8c8c; padding: 2px 8px; border-radius: 0; } .ds-cell--selected { background: #3a6ea5; color: #ffffff; outline: 1px solid #ffffff; outline-offset: -1px; } .ds-cell--staged { background: rgba(227, 179, 65, 0.25); } .ds-cell--deleted { text-decoration: line-through; color: rgba(140, 140, 140, 0.6); }",
    },
    {
        "name": "Editor Tab",
        "kind": "nav",
        "description": "Tabs are selectable labels: the active tab fills Console Blue with white strong text; inactive tabs are quiet at 60% alpha. An open transaction adds a small strong 'tx' marker.",
        "html": '<div class="ds-tabs"><button class="ds-tab ds-tab--active">SELECT * FROM users</button><span class="ds-tab-tx">tx</span><button class="ds-tab">SELECT 1</button><button class="ds-tab-x">&times;</button></div>',
        "css": ".ds-tabs { display: flex; align-items: center; gap: 8px; } .ds-tab { font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 13px; background: #3c3c3c; color: rgba(140, 140, 140, 0.6); padding: 6px 10px; border: 1px solid transparent; border-radius: 4px; cursor: pointer; } .ds-tab:hover { background: #464646; border-color: #969696; } .ds-tab--active { background: #3a6ea5; color: #ffffff; font-weight: 500; } .ds-tab--active:hover { background: #3a6ea5; border-color: transparent; } .ds-tab-tx { font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 11px; font-weight: 500; color: #ffffff; background: #3c3c3c; border-radius: 4px; padding: 1px 4px; } .ds-tab-x { background: none; border: none; color: #b4b4b4; font-size: 13px; cursor: pointer; }",
    },
    {
        "name": "Section Label",
        "kind": "custom",
        "description": "The one uppercase element in the system: 11px sidebar section headers at 60% alpha. Nothing else uppercases.",
        "html": '<div class="ds-section">CONNECTIONS</div>',
        "css": ".ds-section { font-family: 'Ubuntu-Light', system-ui, sans-serif; font-size: 11px; color: rgba(140, 140, 140, 0.6); letter-spacing: 0; }",
    },
    {
        "name": "Editor Well",
        "kind": "input",
        "description": "The SQL surface: recessed into the Ink Well, 4px corners, 6px margin, mono 12.5px, muted syntax tints below the semaphore.",
        "html": '<div class="ds-well"><pre><span class="ds-kw">SELECT</span> id, <span class="ds-str">\'ada\'</span> <span class="ds-kw">FROM</span> users <span class="ds-kw">WHERE</span> n &gt; <span class="ds-num">10</span>; <span class="ds-cmt">-- recent signups</span></pre></div>',
        "css": ".ds-well { background: #12171e; border: 1px solid #3c3c3c; border-radius: 4px; padding: 6px; } .ds-well pre { margin: 0; font-family: 'Hack', monospace; font-size: 12.5px; color: #8c8c8c; } .ds-kw { color: #78aadc; } .ds-str { color: #8cc896; } .ds-cmt { color: #828c9b; } .ds-num { color: #dcaa6e; }",
    },
    {
        "name": "Heavy Path Plan Node",
        "kind": "card",
        "description": "Plan trees render as bordered group frames; nodes on the heavy path take a 2px Console Blue stroke and a strong operation name. Cost share draws as a thin bar.",
        "html": '<div class="ds-plan"><div class="ds-plan-op">Hash Join</div><div class="ds-plan-stats">cost 142.31 &middot; rows 1200</div><div class="ds-track"><div class="ds-bar"></div></div></div>',
        "css": ".ds-plan { background: #202732; border: 2px solid #3a6ea5; border-radius: 4px; padding: 6px; width: 100%; max-width: 280px; font-family: 'Ubuntu-Light', system-ui, sans-serif; } .ds-plan-op { color: #ffffff; font-weight: 500; font-size: 13px; } .ds-plan-stats { color: #8c8c8c; font-size: 13px; margin-top: 2px; } .ds-track { background: #2a323f; border-radius: 3px; height: 6px; margin-top: 4px; } .ds-bar { background: #3a6ea5; border-radius: 3px; height: 6px; width: 72%; }",
    },
]

narrative = {
    "northStar": "The Instrument Flight Deck",
    "overview": "This is a dark calibrated cockpit for a single operator working against real, sometimes dangerous databases. The interface is quiet at rest: a cold blue-gray panel stack, one structural blue, and a semaphore of green, amber, and red that stays dark until state or safety demands it speak. Nothing is decorative. Every tint on screen is reporting something: a staged cell, a transaction, a production connection, a row that differs. The mood is quiet precision, and the discipline that produces it is subtraction. Color arrives late and leaves early.\n\nThe system is dark-first and tonal. Depth is carried by four measured steps of the same blue-gray family, not by shadow or glow; shadow exists only where a surface genuinely floats (dialogs, menus, the command palette, tooltips). Data is always monospace and always the loudest thing on screen by sheer density, never by color. Chrome controls stay neutral gray, so the single green Run button reads as the only commitment device in the app. The light theme is a daylight translation of the same instrument, not a second identity.\n\nThe incumbent implementation is a Rust/egui desktop application. The tokens here were extracted from the shipped code: src/ui/mod.rs owns both themes and the environment palette, and every module reads colors through it. Values below are that theme's exact literals.",
    "keyCharacteristics": [
        "Dark-first blue-gray tonal stack; light theme is a parallel \"daylight paper\" translation.",
        "One structural accent (Console Blue); saturation is reserved for signals.",
        "A safety semaphore: green commits, amber stages and cautions, red guards and faults.",
        "Monospace data everywhere a value can be copied, aliased, or diffed.",
        "Flat panels, 1px strokes; shadow only on true floating layers.",
        "Small, tight type (11px to 16px); emphasis through weight and brightness, never size.",
        "Unicode glyphs as the entire icon set: \u25b6 Run, \u2605 pin, \u00d7 close, + new, \u25cf/\u25cb connection state.",
    ],
    "rules": [
        {"name": "The Semaphore Rule", "body": "Saturation is a message. Green means committing, amber means staged, cautioned, or changed, red means guarded or failed. If a surface is saturated and none of those is true, the color is wrong.", "section": "colors"},
        {"name": "The Two Reds Rule", "body": "Guards wear the softened Guard Red; raw Fault Red is reserved for terminal text about failure. Never swap them.", "section": "colors"},
        {"name": "The Data Is Monospace Rule", "body": "If a value can be copied, aliased, diffed, or executed, it renders in Hack. If it cannot, it renders in Ubuntu-Light. No exceptions to date.", "section": "typography"},
        {"name": "The Weight Rule", "body": "Emphasis is weight and brightness (strong() brightens, weak() dims), never a bigger font. Every size is fixed; the app has no fluid type.", "section": "typography"},
        {"name": "The Instrument Frame Rule", "body": "The four panels are the cockpit. Work lands inside them; it does not reshape them.", "section": "layout"},
        {"name": "The Tonal-First Rule", "body": "Depth is a tone difference, and shadow is reserved for true floating layers. A panel with a shadow is a bug.", "section": "elevation"},
        {"name": "The One Well Rule", "body": "Inputs, editors, and any surface that accepts text recess into the Ink Well (light theme: paper well). Nothing that accepts typing is ever raised or tinted.", "section": "elevation"},
        {"name": "The Quiet Edge Rule", "body": "Rounding is a whisper: 6px for surfaces, 4px for widgets, 3px for badges, 0 for data. Nothing is a pill and nothing has a double border.", "section": "shapes"},
    ],
    "dos": [
        "Do keep Commit Green to one control per screen: the action that commits work.",
        "Do switch selected-cell text to the theme's Selection Text (#ffffff dark, #1e508c light); quiet gray text never sits on a selection fill.",        "Do paint staged-but-unapplied changes with the Staging Amber tint (rgba(227, 179, 65, 0.25)); translucency is reserved for this meaning.",
        "Do render every value in Hack monospace and every label in Ubuntu-Light, at the fixed sizes 11 / 13 / 16 (and 12.5 mono).",
        "Do carry depth with the four tonal steps and 1px strokes; reserve shadow for dialogs, menus, the palette, and tooltips.",
        "Do keep text quiet (weak() at 60% for anything ignorable) so the semaphore and the data own the contrast budget.",
        "Do keep badges at 20% fill / 60% stroke with full-color 11px strong text, and keep Prod red softened (Guard Red), not raw.",
        "Do use the Unicode glyph set (\u25b6, \u2605, \u00d7, +, \u25cf, \u25cb) instead of introducing an icon font or SVG sprite.",
        "Do keep destructive actions understated, and put danger into environment badges, read-only flags, and confirm modals instead.",
    ],
    "donts": [
        "Don't introduce a saturated color outside the semaphore: blue locates, green commits, amber stages or cautions, red guards or faults. There is no decorative fifth color.",
        "Don't apply shadow, gradient, or glow to panels, cards, or cells. A raised panel is off-system by definition.",
        "Don't use pill radii, double borders, or any stroke heavier than the 2px heavy-path accent.",
        "Don't fill destructive buttons with red or style errors into field backgrounds; Fault Red is text-only, and it is never used for a standing identity (that is Guard Red's job).",
        "Don't add a second font family, a display face, or fluid type; the embedded Ubuntu-Light / Hack pairing is the identity.",
        "Don't uppercase anything except the sidebar section labels.",
        "Don't use fullwidth or halfwidth forms (U+FF01 to U+FFEF) in UI text; the embedded fonts lack them and they render as hollow boxes. Use the ASCII forms; tests/glyph_set.rs guards this.",
        "Don't spend Console Blue as decoration: it marks location (the active cell, the selected card's stroke, the dark theme's selection fill) and structure (create actions, heavy path), and it should read as rare on any given screen.",
        "Don't round grid cells or tint them for anything except staged state.",
    ],
}

doc = {
    "schemaVersion": 2,
    "generatedAt": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "title": "Design System: crusty-data-explorer",
    "extensions": {
        "colorMeta": color_meta,
        "typographyMeta": typography_meta,
        "shadows": shadows,
        "motion": motion,
    },
    "components": components,
    "narrative": narrative,
}

with open('.impeccable/design.json', 'w') as f:
    json.dump(doc, f, indent=2)
    f.write('\n')
print("wrote .impeccable/design.json")
