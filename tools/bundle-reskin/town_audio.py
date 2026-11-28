#!/usr/bin/env python3
"""Give the town a season's music by re-pointing the audio kit it already uses.

Bethesda authored three town audio kits and they all still ship in `audio_sab`:
`AudioKitTown_Setting`, `AudioKitTown_WinterSetting` and
`AudioKitTown_WitchFestivalSetting` -- the last one carrying
PLAY_MUS_TOWN_WITCHFESTIVAL, the SB_Town_WitchFestival bank and its own
spookier ambience list. The bank itself ships too, as the remote bundle
`wwise_sb_town_witchfestival`.

Retail picked between them by swapping the town's *Kit* (`TownKit_Night` is the
one whose `_audioSetting` is the WitchFestival kit). We cannot reach that
switch: the kit is chosen in client code, and the choice never crosses the
wire -- 61,122 captured bodies contain no kit uid at all, not even the default
TownKit's, which would appear on every town load if kits were server-driven.

So instead of choosing a different kit, this rewrites the kit the town already
loads. `_soundBanks`, `_music` and `_ambience` are copied wholesale from the
season kit onto `AudioKitTown_Setting`, which leaves the selection logic
untouched and needs nothing but a bundle swap.

`audio_sab` is the right bundle to carry it: it is one of the few that ship in
the APK *and* are provably fetched over the wire (the mirror holds a
client-downloaded copy), so a pack that advertises a new hash for it is taken.

    ./town_audio.py <src> <dst> [witchfestival|winter|stock]
"""
import sys

import UnityPy

TOWN = "AudioKitTown_Setting"
SEASONS = {
    "witchfestival": "AudioKitTown_WitchFestivalSetting",
    "winter": "AudioKitTown_WinterSetting",
    "stock": TOWN,
}
# Everything else on the kit -- surface switch, exploration stingers -- is
# shared between the seasons, so copying it would only add ways to be wrong.
FIELDS = ("_soundBanks", "_music", "_ambience")


def main():
    src, dst = sys.argv[1], sys.argv[2]
    season = sys.argv[3] if len(sys.argv) > 3 else "witchfestival"
    if season not in SEASONS:
        raise SystemExit("unknown season %r (want %s)" % (season, ", ".join(SEASONS)))
    source_name = SEASONS[season]

    env = UnityPy.load(src)
    found = {}
    for o in env.objects:
        if o.type.name != "MonoBehaviour":
            continue
        t = o.read_typetree()
        name = t.get("m_Name")
        if name in (TOWN, source_name):
            found[name] = (o, t)

    for need in (TOWN, source_name):
        if need not in found:
            raise SystemExit("%s not found in %s — refusing to write" % (need, src))

    town_obj, town = found[TOWN]
    _, source = found[source_name]

    for f in FIELDS:
        town[f] = source[f]
    town_obj.save_typetree(town)

    banks = [b.get("BankName") for b in town["_soundBanks"]]
    music = [e["EventID"] & 0xFFFFFFFF for e in town["_music"]["_playEvents"]]
    data = env.file.save(packer="original")
    open(dst, "wb").write(data)
    print("  %s <- %s" % (TOWN, source_name))
    print("     banks    %s" % (banks,))
    print("     music    %s" % (music,))
    print("     ambience %d event(s)" % len(town["_ambience"]["_playEvents"]))
    print("  wrote %s (%d bytes)" % (dst, len(data)))


if __name__ == "__main__":
    main()
