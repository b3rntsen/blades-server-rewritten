#!/usr/bin/env python3
"""Darken the town and main-menu sky for a night season.

WHY THIS EXISTS, AND WHY IT WAS THOUGHT IMPOSSIBLE

`docs/halloween-gap.md` recorded the night sky as "#1 confirmed impossible":
the scene's `LightingSettingsHolder._skybox` resolves to `skybox_sab`, which
ships inside the APK, so the reasoning was that it could never be replaced over
the air. That reasoning asked where the bytes SHIP. The question that decides it
is whether the client FETCHES the bundle, and it does:

    skybox_sab  1d1a7dcc78ca1aa693360719efd0a2bc  44 requests, 44x HTTP 200
    audio_sab   4003b40faa2173d487b129a47647e9ab  44 requests, 44x HTTP 200

`audio_sab` is the control: it is also in the APK, and the Halloween town music
shipped through it. Same request count, same status, both held by the mirror. So
`skybox_sab` is replaceable by the ordinary route and the sky was never locked.

WHAT THIS TOUCHES, AND WHAT IT MUST NOT

The bundle holds 32 materials for EVERY environment — arena, forest, cave,
Ayleid, the events skies. Only four belong to the town and the main menu, and
only those are touched. Darkening the whole bundle would put night in the arena
too, which nobody asked for and which no test would catch.

    Skybox_Procedural_town_01     the town's procedural sky gradient
    Skybox_Procedural_Main_menu   the same shader, front menu
    Landscape_Town_Clouds_00      the town's cloud layer
    Landscape_Town_Mountains_00   the horizon behind it

THE VALUES ARE AUTHORED, NOT MEASURED

There is no capture of retail's Halloween sky, so unlike the rest of this
pipeline these numbers are a judgement, not an observation. `Sky_Night` and
`Sky_Stars` exist in the bundle but are plain mesh materials on the star dome
(`_Color` 0.588 grey) and carry no gradient to copy. What IS measured is the
stock town gradient, and every value below is expressed as a transform of it —
so a reviewer can see the direction of travel rather than a wall of literals:

  * `_Exposure` 3.35 is the master brightness and does most of the work.
  * The gradient runs `_ColorTop` -> `_ColorMid` -> `_ColorBottom`, so pushing
    the top to indigo and keeping a warm bottom band gives the dusk look the
    reference videos show: deep purple-blue overhead, a red band low down.
  * Clouds and mountains are dimmed to match, or they float lit against a dark
    sky and read as a bug.

Rerunnable and deterministic: the same input bundle always produces the same
output, which is what `scripts/tests/test_reskin_determinism.py` requires.

    ./night_sky.py <src bundle> <dst bundle> [exposure_scale] [--retail]

`--retail` swaps the authored dusk for the palette sampled from the retail
Witches Festival capture (HKHvv26lTOo: zenith #00003C, indigo #0D0C51, horizon
glow #562642) and tints the town's cloud and mountain cards moon-blue per
channel instead of one grey scale. Verified on the emulator 2026-09-23.
"""
import sys

import UnityPy
from PIL import Image

#: Master brightness of the procedural sky. Stock is 3.35; a quarter of that is
#: night without being unreadable, which matters because players navigate the
#: town by silhouette.
DEFAULT_EXPOSURE_SCALE = 0.26

#: The two procedural skies (town + main menu) and the horizon backdrop.
#: Anything not named here keeps its daylight, on purpose.
#:
#: Walking the town district scene's renderers and resolving every m_Materials
#: PPtr that points at skybox_sab (they all carry fileID 8) returns exactly
#: four materials and no others: the mountain, cloud and forest cards, and one
#: lower cloud layer. The arena, cave, Ayleid and event skies in the same
#: bundle are never referenced from the town, so darkening them would only risk
#: scenes nobody asked about.
#:
#: Three of those four share shader 6201664213019922083, which **ignores
#: `_Color`** — see TEXTURE_SCALE. They are darkened as textures instead, and
#: only the fourth is left on the material path, because its shader is a
#: different one and has not been shown to ignore anything.
PROCEDURAL = ("Skybox_Procedural_town_01", "Skybox_Procedural_Main_menu")
HORIZON = ("SkyrimCloudsLower04",)

#: Absolute replacements for the gradient stops. Chosen, not captured.
#: Overhead is near-black indigo; the middle band is a dusk purple; the bottom
#: keeps a warm ember so the horizon still reads as a sunset rather than a wall.
NIGHT_GRADIENT = {
    "_ColorTop": (0.035, 0.030, 0.078),
    "_ColorMid": (0.150, 0.095, 0.205),
    "_ColorA": (0.095, 0.080, 0.155),
    "_ColorB": (0.205, 0.130, 0.240),
    "_ColorBottom": (0.330, 0.140, 0.105),
    "_SkyTint": (0.085, 0.075, 0.125),
    # Left white on the first pass, which is what produced the pale daylight
    # band along the horizon while everything above it was already night. It is
    # a gradient stop like the rest and has to come down with them.
    "_ColorC": (0.120, 0.085, 0.155),
}

#: The bottom of the gradient is multiplied before it is drawn — 3.75 in town,
#: 7.31 on the menu — so darkening the colours alone still leaves a lit band at
#: the horizon, and on the menu a bright glow. Scaled rather than set, because
#: the two scenes deliberately differ and the ratio between them is authored.
BOTTOM_MULTIPLIER_SCALE = 0.30

#: How far to pull the lit horizon down. Multiplicative, so a material that was
#: already dim stays proportionally dim.
HORIZON_SCALE = 0.34

#: The main menu's distant vista — the mountain range, forest, clouds and fog
#: behind the castle — is four GameObjects (Montains_Forest, Trees_Forest,
#: SkyBox_Clouds, SkyBox_Fog) whose materials live here. Their shader ignores
#: `_Color`: setting it to pure black left those pixels bit-identical on the
#: device, across a build that also blacked every landscape material and one
#: that moonlit all of the menu scene's lights. The texture is the only lever
#: that reaches them, so it is darkened directly.
#:
#: The town's own mountain and cloud cards are on that same shader, so their
#: `_Color` never did anything either. They take a gentler scale: the town is
#: played in, and a cloud layer crushed to black reads as a hole in the sky
#: rather than as weather.
VISTA_TEXTURES = ("Landscape_Forest", "Forest_Clouds_Square", "Forest_Fog")
TOWN_TEXTURES = ("Landscape_Town_Clouds_00", "Landscape_Town_Mountains_00")

#: Multiplied per-pixel. UnityPy re-encodes back to the bundle's own ASTC, so
#: this costs no download size worth caring about (4,358,083 -> 4,357,283).
TEXTURE_SCALE = 0.30
TOWN_TEXTURE_SCALE = 0.45

#: `--retail`: sampled from the retail capture rather than chosen.
RETAIL_GRADIENT = {
    "_ColorTop": (0.000, 0.000, 0.235),
    "_ColorMid": (0.051, 0.047, 0.318),
    "_ColorA": (0.040, 0.038, 0.260),
    "_ColorB": (0.160, 0.080, 0.300),
    "_ColorBottom": (0.337, 0.149, 0.259),
    "_SkyTint": (0.050, 0.050, 0.200),
    "_ColorC": (0.090, 0.060, 0.280),
}
#: Per-channel multipliers for the town cards under `--retail`. The stock clouds
#: are near-white (RGB ~ 202/212/208); a single grey scale left them glowing
#: against the night on device, retail shows them slate blue.
RETAIL_TOWN_TINT = {
    "Landscape_Town_Clouds_00": (0.26, 0.28, 0.40),
    "Landscape_Town_Mountains_00": (0.22, 0.22, 0.36),
}


# `m_Colors` and `m_Floats` are lists of TUPLES, so an entry cannot be assigned
# through — the colour dict inside one happens to be mutable, the float is not.
# Both are therefore replaced positionally, which also keeps the two helpers
# symmetrical rather than one mutating and one rebinding.


def _set_color(colors, key, rgb):
    """Replace one colour stop, preserving its alpha. True if it was there."""
    for i, (name, value) in enumerate(colors):
        if name == key:
            colors[i] = (name, dict(value, r=rgb[0], g=rgb[1], b=rgb[2]))
            return True
    return False


def _scale_color(colors, key, factor):
    for i, (name, value) in enumerate(colors):
        if name == key:
            colors[i] = (name, dict(
                value, **{ch: round(value[ch] * factor, 6) for ch in "rgb"}))
            return True
    return False


def _scale_float(floats, key, factor):
    for i, (name, value) in enumerate(floats):
        if name == key:
            floats[i] = (name, round(value * factor, 6))
            return True
    return False


def darken(src, dst, exposure_scale=DEFAULT_EXPOSURE_SCALE, retail=False):
    """Write `src` to `dst` with the town and menu skies turned to night."""
    gradient = RETAIL_GRADIENT if retail else NIGHT_GRADIENT
    env = UnityPy.load(src)
    touched = []
    for obj in env.objects:
        if obj.type.name != "Material":
            continue
        tree = obj.read_typetree()
        name = tree.get("m_Name")
        if name not in PROCEDURAL and name not in HORIZON:
            continue
        props = tree["m_SavedProperties"]
        colors = props.get("m_Colors") or []
        floats = props.get("m_Floats") or []

        if name in PROCEDURAL:
            for key, rgb in gradient.items():
                _set_color(colors, key, rgb)
            _scale_float(floats, "_Exposure", exposure_scale)
            _scale_float(floats, "_BottomMultiplier", BOTTOM_MULTIPLIER_SCALE)
        else:
            # _Tint/_TintColor sit at a neutral 0.5 on these shaders, so
            # scaling only _Color leaves a second brightness term untouched.
            for key in ("_Color", "_AmbientTweak", "_Tint", "_TintColor"):
                _scale_color(colors, key, HORIZON_SCALE)

        obj.save_typetree(tree)
        touched.append(name)

    for obj in env.objects:
        if obj.type.name != "Texture2D":
            continue
        tex = obj.read()
        if tex.m_Name in VISTA_TEXTURES:
            scale = (TEXTURE_SCALE,) * 3
        elif tex.m_Name in TOWN_TEXTURES:
            scale = RETAIL_TOWN_TINT[tex.m_Name] if retail else (TOWN_TEXTURE_SCALE,) * 3
        else:
            continue
        pixels = tex.image.convert("RGBA")
        bands = list(pixels.split())
        for i in range(3):                       # RGB only; alpha is a cutout
            bands[i] = bands[i].point(lambda v, s=scale[i]: min(255, int(v * s)))
        tex.image = Image.merge("RGBA", bands)
        tex.save()
        touched.append(tex.m_Name)

    expected = PROCEDURAL + HORIZON + VISTA_TEXTURES + TOWN_TEXTURES
    missing = [n for n in expected if n not in touched]
    if missing:
        raise SystemExit(
            "night_sky: expected materials not found: %s — the bundle layout "
            "changed and this script would silently do nothing" % ", ".join(missing))

    # `packer="original"` keeps the bundle's own compression, as the rest of the
    # reskin tooling does. Without it UnityPy writes it back uncompressed and
    # this bundle goes 4.35 MB -> 7.4 MB — a 70% bigger download for every
    # player, to change twenty-odd floats.
    with open(dst, "wb") as fh:
        fh.write(env.file.save(packer="original"))
    return touched


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__.strip().splitlines()[-1].strip())
    retail = "--retail" in sys.argv
    args = [a for a in sys.argv[1:] if a != "--retail"]
    src, dst = args[0], args[1]
    scale = float(args[2]) if len(args) > 2 else DEFAULT_EXPOSURE_SCALE
    touched = darken(src, dst, scale, retail=retail)
    print("night_sky: %d material(s) -> %s" % (len(touched), dst))
    for n in sorted(touched):
        print("   ", n)


if __name__ == "__main__":
    main()
