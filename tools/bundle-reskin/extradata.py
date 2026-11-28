#!/usr/bin/env python3
"""Locate a bundle's ``size``/``CRC`` record inside ``<version>_ExtraData``.

## Why this replaced a byte search

The first decoder found a record by searching for the bundle's **current size**
as a u32 and demanding it occur exactly once. That is not a locator, it is a
coincidence detector: the file is 1.7 MB, so a four-byte value hits by chance
often enough, and the "exactly once" guard passes just as happily on a chance hit
in the Hash128 table as on the real record. It also cannot find a record at all
when the served bytes have drifted from the recorded size — which is the case for
``common``, whose mirror copy is 429 bytes larger than its record says.

## The format, as far as this needs it

    u32  version (1)
    u16  bundle count (1698 on Android)
    ...  Hash128 table, count * 16 bytes, ALPHABETICAL by bundle name
    ...  one variable-length record per bundle, in that same order:
             u32  size
             u32  crc          (Unity's build CRC, over uncompressed content)
             u16  asset-path count
             ...  that many length-prefixed (LEB128) UTF-8 paths
             u16  reference count
             ...  that many u16 pairs
    
The records are variable-length, so the i-th one cannot be computed — it has to
be found. Walking the whole chain would mean trusting every field in it; instead
this triangulates from **anchors**: bundles whose served size is known and occurs
exactly once in the file. An anchor at index ``a`` found at offset ``o`` implies
``base = o - 10*a``, and records sitting in one unbroken run share a base. Two
independent anchors that agree, plus a target record whose tail parses as a
path list, is enough to patch eight bytes with confidence.

Anchors come from the mirror, which is the only place the served sizes exist.
"""

import struct

RECORD_HEAD = 10  # u32 size + u32 crc + u16 path count


def _unique_offset(blob, value):
    pat = struct.pack("<I", value)
    first = blob.find(pat)
    if first < 0 or blob.find(pat, first + 1) >= 0:
        return None
    return first


def _tail_is_path_list(blob, offset):
    """Whether the record at `offset` is followed by a plausible path list.

    A real record's path count is small and its first path is a printable,
    length-prefixed asset path. Random bytes almost never satisfy both.
    """
    try:
        count = struct.unpack_from("<H", blob, offset + 8)[0]
    except struct.error:
        return False
    if count == 0:
        return True  # plenty of bundles carry no paths; nothing more to check
    if count > 4096:
        return False
    i = offset + RECORD_HEAD
    length, shift = 0, 0
    while True:
        if i >= len(blob):
            return False
        byte = blob[i]
        i += 1
        length |= (byte & 0x7F) << shift
        shift += 7
        if not byte & 0x80:
            break
        if shift > 28:
            return False
    if not 1 <= length <= 512 or i + length > len(blob):
        return False
    path = blob[i:i + length]
    return all(32 <= c < 127 for c in path)


def locate(blob, names, target, sizes, min_anchors=2, window=24, tolerance=0.25):
    """Offset of `target`'s record, or raise.

    `names` is every bundle name in hash-table (alphabetical) order; `sizes` maps
    a bundle name to the size of the bytes we serve for it. Only names near the
    target are used as anchors, because records sit in unbroken runs and an
    anchor from a different run votes for a different base.

    Several bases therefore get votes, and the winner is the one whose NEAREST
    supporter is nearest — a run containing the target necessarily has a
    supporter closer to it than any run that does not. The result must also be
    followed by a plausible path list, and, when the target's own served size is
    known, be within `tolerance` of it: served bytes drift from the record (the
    `common` copy on the mirror is 429 bytes larger than its record says), but
    they do not drift by an order of magnitude.
    """
    index = {name: i for i, name in enumerate(names)}
    if target not in index:
        raise KeyError("no such bundle: %s" % target)
    want = index[target]

    votes = {}
    for name, i in index.items():
        if name == target or not 0 < abs(i - want) <= window:
            continue
        size = sizes.get(name)
        if not size:
            continue
        offset = _unique_offset(blob, size)
        if offset is None:
            continue
        votes.setdefault(offset - RECORD_HEAD * i, []).append((abs(i - want), i, name))

    ranked = sorted(
        ((min(d for d, _, _ in who), -len(who), base, who)
         for base, who in votes.items()))
    if not ranked:
        raise SystemExit("%s: no usable anchors within %d records" % (target, window))
    nearest, negcount, base, who = ranked[0]
    if -negcount < min_anchors:
        raise SystemExit("%s: only %d anchor(s) agree, need %d — refusing to guess"
                         % (target, -negcount, min_anchors))
    if len(ranked) > 1 and ranked[1][0] == nearest:
        raise SystemExit("%s: two candidate runs are equally near — refusing to guess"
                         % target)

    offset = base + RECORD_HEAD * want
    if not 0 <= offset <= len(blob) - RECORD_HEAD:
        raise SystemExit("%s: record offset 0x%x is outside the file" % (target, offset))
    if not _tail_is_path_list(blob, offset):
        raise SystemExit("%s: record at 0x%x is not followed by a path list"
                         % (target, offset))
    # Nothing so far proves the target is INSIDE the run its anchors sit in: a
    # run that stops short of the target votes for a base that is wrong by
    # whatever separates the two runs, and a record of eight arbitrary bytes
    # followed by a zero path count looks perfectly well formed. One of two
    # independent facts has to close that hole.
    served = sizes.get(target)
    supporters = [i for _, i, _ in who]
    bracketed = any(i < want for i in supporters) and any(i > want for i in supporters)
    if served:
        recorded = struct.unpack_from("<I", blob, offset)[0]
        if abs(recorded - served) > tolerance * served:
            raise SystemExit(
                "%s: record at 0x%x says %d bytes but we serve %d — wrong record?"
                % (target, offset, recorded, served))
    elif not bracketed:
        raise SystemExit(
            "%s: no served size to check against, and every anchor is on the same "
            "side — refusing to guess" % target)
    return offset, [n for _, _, n in sorted(who)]


def _paths_at(blob, offset):
    """The asset paths a record at `offset` lists, or None if it does not parse."""
    try:
        count = struct.unpack_from("<H", blob, offset + 8)[0]
    except struct.error:
        return None
    i, out = offset + RECORD_HEAD, []
    for _ in range(count):
        length, shift = 0, 0
        while True:
            if i >= len(blob):
                return None
            byte = blob[i]
            i += 1
            length |= (byte & 0x7F) << shift
            shift += 7
            if not byte & 0x80:
                break
            if shift > 28:
                return None
        if not 1 <= length <= 512 or i + length > len(blob):
            return None
        out.append(blob[i:i + length].decode("utf-8", "replace"))
        i += length
    return out


def locate_by_identity(blob, target, served_size, needle):
    """Offset of `target`'s record, proven by the record NAMING the bundle's assets.

    `locate` needs two unambiguous anchors nearby, and the scene bundles
    (`levels_*`) sit in a run where only one exists, so it refuses them. A
    record carries its own asset paths, though, and a scene bundle's paths name
    the scene (`.../Levels_MainMenu_MainMenu/Levels_MainMenu_MainMenu_Level.prefab`).
    So: every offset where the served size occurs, keep the ones whose path list
    parses AND contains `needle`, and demand exactly one. A chance hit on the
    size would have to also be followed by a path list naming this very scene.
    """
    pat = struct.pack("<I", served_size)
    hits, at = [], blob.find(pat)
    while at >= 0:
        paths = _paths_at(blob, at)
        if paths and any(needle.lower() in p.lower() for p in paths):
            hits.append(at)
        at = blob.find(pat, at + 1)
    if len(hits) != 1:
        raise SystemExit("%s: %d records of size %d name %r — refusing to guess"
                         % (target, len(hits), served_size, needle))
    return hits[0]


def read(blob, offset):
    size, crc, paths = struct.unpack_from("<IIH", blob, offset)
    return size, crc, paths


def write(blob, offset, size, crc=0):
    """Return a copy with this record's size and CRC replaced.

    CRC 0 is Unity's documented "do not verify", which is the only option for
    bytes we authored: Bethesda's build CRC cannot be reproduced.
    """
    out = bytearray(blob)
    struct.pack_into("<II", out, offset, size, crc)
    return bytes(out)
