#!/usr/bin/env python3
"""Generate 8-step OKLCH tonal ramps (dark to light) for the DESIGN.md sidecar."""
import json
import math

def srgb_to_linear(c):
    return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4

def linear_to_srgb(c):
    return 12.92 * c if c <= 0.0031308 else 1.055 * (c ** (1 / 2.4)) - 0.055

def hex_to_oklch(h):
    h = h.lstrip('#')
    r, g, b = (int(h[i:i+2], 16) / 255 for i in (0, 2, 4))
    r, g, b = srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)
    l = 0.4122214708*r + 0.5363325363*g + 0.0514459929*b
    m = 0.2119034982*r + 0.6806995451*g + 0.1073969566*b
    s = 0.0883024619*r + 0.2817188376*g + 0.6299787005*b
    l_, m_, s_ = l ** (1/3), m ** (1/3), s ** (1/3)
    L = 0.2104542553*l_ + 0.7936177850*m_ - 0.0040720468*s_
    a = 1.9779984951*l_ - 2.4285922050*m_ + 0.4505937099*s_
    bb = 0.0259040371*l_ + 0.7827717662*m_ - 0.8086757660*s_
    C = math.hypot(a, bb)
    H = math.degrees(math.atan2(bb, a)) % 360
    return L, C, H

def oklch_to_linear(L, C, H):
    a = C * math.cos(math.radians(H))
    b = C * math.sin(math.radians(H))
    l_ = L + 0.3963377774*a + 0.2158037573*b
    m_ = L - 0.1055613458*a - 0.0638541728*b
    s_ = L - 0.0894841775*a - 1.2914855480*b
    l, m, s = l_**3, m_**3, s_**3
    r =  4.0767416621*l - 3.3077115913*m + 0.2309699292*s
    g = -1.2684380046*l + 2.6097574011*m - 0.3413193965*s
    b2 = -0.0041960863*l - 0.7034186147*m + 1.7076147010*s
    return r, g, b2

def in_gamut(rgb):
    return all(-1e-4 <= c <= 1 + 1e-4 for c in rgb)

def oklch_hex(L, C, H):
    # clamp chroma into sRGB gamut by bisection
    lo, hi = 0.0, C
    if not in_gamut(oklch_to_linear(L, C, H)):
        for _ in range(40):
            mid = (lo + hi) / 2
            if in_gamut(oklch_to_linear(L, mid, H)):
                lo = mid
            else:
                hi = mid
        C = lo
    r, g, b = oklch_to_linear(L, C, H)
    r, g, b = (max(0.0, min(1.0, c)) for c in (r, g, b))
    return '#%02x%02x%02x' % tuple(round(linear_to_srgb(c) * 255) for c in (r, g, b))

COLORS = {
    "cockpit-window": "#181d25",
    "panel-slate": "#202732",
    "raised-slate": "#2a323f",
    "ink-well": "#12171e",
    "code-well": "#28303c",
    "light-window": "#f2f4f7",
    "light-panel": "#eceef2",
    "light-raised": "#e4e7ec",
    "light-well": "#e1e4e9",
    "light-code-well": "#f0f2f5",
    "console-blue": "#3a6ea5",
    "selection-outline-dark": "#c0deff",
    "selection-outline-light": "#1e508c",
    "commit-green-dark": "#73b473",
    "commit-green-light": "#388e3c",
    "staging-amber": "#e3b341",
    "signal-blue-dark": "#78aadc",
    "signal-blue-light": "#1976d2",
    "lichen-dark": "#8cc896",
    "lichen-light": "#2e7d32",
    "steel-comment-dark": "#828c9b",
    "steel-comment-light": "#6e7781",
    "brass-dark": "#dcaa6e",
    "brass-light": "#aa641e",
    "dev-steel-dark": "#949ba6",
    "dev-steel-light": "#5f6973",
    "caution-amber-dark": "#e3b341",
    "caution-amber-light": "#966e0f",
    "guard-red-dark": "#e05b5b",
    "guard-red-light": "#bc3c3c",
    "advisory-orange-dark": "#ff8f00",
    "advisory-orange-light": "#ff6400",
    "fault-red": "#ff0000",
}

STEPS = 8
out = {}
for name, hexval in COLORS.items():
    L, C, H = hex_to_oklch(hexval)
    ramp = []
    for i in range(STEPS):
        li = round(15 + (95 - 15) * i / (STEPS - 1)) / 100
        ramp.append(oklch_hex(li, C, H))
    out[name] = {"source": hexval, "oklch": f"oklch({L*100:.1f}% {C:.3f} {H:.1f})", "ramp": ramp}

print(json.dumps(out, indent=1))
