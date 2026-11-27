#!/usr/bin/env python3
"""Compose several asset packs into ONE manifest + ``_ExtraData`` pair.

Why this exists: an audience used to point at exactly one pack, so putting
"Halloween" on beta evicted "Inventory cap 9999" and keeping both meant
hand-building a combined pack. Packs are now **layers** — independent sets of
bundle overrides — and an audience is the set of layers enabled for it. The
client still reads one manifest and one ``_ExtraData``, so something has to
merge the layers' variants into one pair. This is that something.

Stdlib only, Python 3.9+, so it can run on a server that has no UnityPy (the
original deployment ran it inside the bundle-mirror service).

## It replays edits, it does not locate anything

A pack builder (``build_pack_v2.py``) already did the hard
part on a workstation: it found each bundle's ``_ExtraData`` record with
``extradata.locate`` (anchors, path-list tail, size tolerance) and repointed the
manifest with UnityPy. Its ``_ExtraData`` differs from stock in exactly the
bytes it edited, in place. So the composer diffs each layer's ``_ExtraData``
against stock and **replays those bytes**, after proving that every changed
byte is explained by the layer's own declaration:

* hash table — one aligned 16-byte slot per declared bundle, now holding that
  bundle's new hash; the stock hash that slot held is the bundle's OLD hash;
* records — every changed byte lies in an 8-byte ``size, crc`` window that
  reads ``(served size of that bundle, 0)``, and the windows appear in the same
  order as the bundles' table slots (records follow table order).

The manifest cannot be diffed — UnityPy re-serialises the SerializedFile on
save, so the variant's layout moves (measured: data offset 4096 -> 4000). Its
half is carried as "old hash -> new hash" on the STOCK manifest, where the old
hash comes from the ``_ExtraData`` slot above and must occur exactly once in
the stock manifest. The variant manifest must name each new hash once, no
longer name the old one, and still name every undeclared bundle's stock hash —
so a variant that repointed something it does not declare is refused rather
than having that change silently dropped.

Anything unexplained refuses that layer. There is no size search over the stock
file anywhere here: a coincidental hit is impossible because only bytes the
builder changed are ever candidates.

## Why the manifest needs a container rewrite

The manifest is a UnityFS bundle whose payload is LZ4-compressed, so a byte
diff of the FILE says nothing. We decompress both, diff the payload, patch the
stock payload, and write the container back with the stock header, flags and
block layout. A block whose content changed is re-encoded as a literal-only LZ4
block — valid LZ4 by the format spec (a stream may end in a literal run and
need contain no match), so the compression flags the client already accepts
stay exactly as they were. Unchanged blocks are copied verbatim.

## Determinism is the versioning

Output depends only on the stock pair and the SET of layers, never their order
or the time, so an unchanged composition always yields the same bytes, the same
md5 and therefore the same bundle version (a mirror can key its version
ledger on that md5). One layer on its own is returned verbatim — the
exact files the builder produced — so today's single-pack audiences keep their
current hash and version across the upgrade.

    python3 scripts/asset_pack_compose.py STOCK_MANIFEST STOCK_EXTRADATA \\
        PACK_DIR [PACK_DIR ...] --out DIR
"""

import hashlib
import json
import os
import struct
import sys

#: Refuse to inflate anything bigger. The real manifest payload is ~170 KB; a
#: pack file is attacker-adjacent input parsed by a root process.
MAX_PAYLOAD = 64 * 1024 * 1024

HASH_LEN = 16


class ComposeError(Exception):
    """A layer cannot be used, or two layers cannot be used together."""


class LayerConflict(ComposeError):
    """Two layers touch the same bundle or the same bytes. Never resolved here."""


# ---------------------------------------------------------------------------
# LZ4 block format
# ---------------------------------------------------------------------------

def lz4_decompress(src, size):
    """Decode one LZ4 block into exactly `size` bytes, or raise ComposeError."""
    if size > MAX_PAYLOAD:
        raise ComposeError("LZ4 block claims %d bytes" % size)
    out = bytearray()
    i, n = 0, len(src)
    while i < n:
        token = src[i]
        i += 1
        lit = token >> 4
        if lit == 15:
            while True:
                if i >= n:
                    raise ComposeError("truncated LZ4 literal length")
                b = src[i]
                i += 1
                lit += b
                if b != 255:
                    break
        if i + lit > n:
            raise ComposeError("truncated LZ4 literals")
        out += src[i:i + lit]
        i += lit
        if i >= n:
            break  # the last sequence is literals only
        if i + 2 > n:
            raise ComposeError("truncated LZ4 match offset")
        offset = src[i] | (src[i + 1] << 8)
        i += 2
        if offset == 0 or offset > len(out):
            raise ComposeError("bad LZ4 match offset")
        mlen = token & 15
        if mlen == 15:
            while True:
                if i >= n:
                    raise ComposeError("truncated LZ4 match length")
                b = src[i]
                i += 1
                mlen += b
                if b != 255:
                    break
        mlen += 4
        if len(out) + mlen > size:
            raise ComposeError("LZ4 block overruns its declared size")
        start = len(out) - offset
        if offset >= mlen:
            out += out[start:start + mlen]
        else:  # overlapping copy: byte by byte is the defined behaviour
            for k in range(mlen):
                out.append(out[start + k])
    if len(out) != size:
        raise ComposeError("LZ4 block is %d bytes, header says %d" % (len(out), size))
    return bytes(out)


def lz4_literal_block(data):
    """`data` as a valid LZ4 block made of one literal run and no match."""
    n = len(data)
    if n < 15:
        return bytes([n << 4]) + data
    out = bytearray([0xF0])
    rest = n - 15
    while rest >= 255:
        out.append(255)
        rest -= 255
    out.append(rest)
    return bytes(out) + data


# ---------------------------------------------------------------------------
# UnityFS container
# ---------------------------------------------------------------------------

def _cstring(buf, pos):
    end = buf.find(b"\0", pos)
    if end < 0:
        raise ComposeError("unterminated string in UnityFS header")
    return buf[pos:end], end + 1


def _align(pos, to=16):
    return (pos + to - 1) // to * to


def _inflate(data, flags, size):
    kind = flags & 0x3F
    if kind == 0:
        if len(data) != size:
            raise ComposeError("uncompressed block size mismatch")
        return bytes(data)
    if kind in (2, 3):
        return lz4_decompress(data, size)
    raise ComposeError("unsupported UnityFS compression %d" % kind)


def _deflate(data, flags):
    kind = flags & 0x3F
    if kind == 0:
        return data
    if kind in (2, 3):
        return lz4_literal_block(data)
    raise ComposeError("unsupported UnityFS compression %d" % kind)


def _deflate_or_store(data, flags):
    """`data` encoded for a block whose flags are `flags`, and the flags to write.

    A literal-only LZ4 block is always LONGER than its input (a token plus
    length bytes). For a full 128 KiB block that makes the compressed block
    exceed the block size, and Unity refuses it: "Failed to decompress data for
    the AssetBundle …/Manifests/…" on 2026-09-24, the first time two packs were
    layered (a Pixel stuck on 0-1000-2). The bytes were valid LZ4 — a reference
    decoder read every block — so the limit is Unity's buffer, not the format.

    So when encoding would not make the block smaller, store it UNCOMPRESSED
    and clear the compression bits for that block. UnityFS carries compression
    per block (and separately for the block directory), so an uncompressed
    block among LZ4HC ones is ordinary for the format; every unchanged block
    keeps its original bytes and flags.
    """
    comp = _deflate(data, flags)
    if (flags & 0x3F) and len(comp) >= len(data):
        return bytes(data), flags & ~0x3F
    return comp, flags


class UnityFS(object):
    """Just enough of a UnityFS bundle to swap bytes inside its payload."""

    def __init__(self, raw, max_payload=MAX_PAYLOAD):
        # Every malformed input must surface as ComposeError: callers drop
        # the layer on that, and anything else escapes as "resolver crashed".
        try:
            self._parse(bytes(raw), max_payload)
        except ComposeError:
            raise
        except (struct.error, IndexError, ValueError, OverflowError, MemoryError) as e:
            raise ComposeError("malformed UnityFS bundle: %s" % e)

    def _parse(self, raw, max_payload):
        self.raw = raw
        buf = self.raw
        sig, pos = _cstring(buf, 0)
        if sig != b"UnityFS":
            raise ComposeError("not a UnityFS bundle")
        (self.fmt,) = struct.unpack_from(">I", buf, pos)
        pos += 4
        _, pos = _cstring(buf, pos)  # player version
        _, pos = _cstring(buf, pos)  # engine revision
        self.size_at = pos
        size, csize, usize, flags = struct.unpack_from(">qIII", buf, pos)
        pos += 20
        if size != len(buf):
            raise ComposeError("UnityFS says %d bytes, file is %d" % (size, len(buf)))
        if flags & 0x80:
            # Directory at the end of the file. Never seen in this game's
            # bundles; refusing is cheaper than an untested writer.
            raise ComposeError("UnityFS directory-at-end layout is not supported")
        self.flags = flags
        self.header_end = pos
        if self.fmt >= 7:
            pos = _align(pos)
        self.info_at = pos
        if usize > 1024 * 1024:
            raise ComposeError("UnityFS directory claims %d bytes" % usize)
        info = _inflate(buf[pos:pos + csize], flags, usize)
        pos += csize
        if flags & 0x200:
            pos = _align(pos)
        self.data_at = pos

        self.info_hash = info[:16]
        (nblocks,) = struct.unpack_from(">i", info, 16)
        if not 0 < nblocks <= (len(info) - 20) // 10:
            raise ComposeError("UnityFS declares %d blocks its directory cannot hold"
                               % nblocks)
        p = 20
        self.blocks = []
        for _ in range(nblocks):
            bu, bc, bf = struct.unpack_from(">IIH", info, p)
            p += 10
            self.blocks.append((bu, bc, bf))
        self.nodes_raw = info[p:]
        # Checked BEFORE inflating anything: a small upload of 0xFF runs can
        # otherwise inflate to the cap on a 4 GB box.
        if sum(b[0] for b in self.blocks) > max_payload:
            raise ComposeError("UnityFS payload too large")

        chunks, at = [], self.data_at
        for bu, bc, bf in self.blocks:
            if at + bc > len(buf):
                raise ComposeError("UnityFS block runs past the end of the file")
            chunks.append(_inflate(buf[at:at + bc], bf, bu))
            at += bc
        self.block_payloads = chunks
        self.payload = b"".join(chunks)

    def with_payload(self, payload):
        """The same container holding `payload` (which must be the same length)."""
        if len(payload) != len(self.payload):
            raise ComposeError("payload length changed")
        if payload == self.payload:
            return self.raw
        buf = self.raw
        blocks_out, info_blocks, at, src = [], [], 0, self.data_at
        for (bu, bc, bf), old in zip(self.blocks, self.block_payloads):
            new = payload[at:at + bu]
            at += bu
            if new == old:
                comp = buf[src:src + bc]
            else:
                comp, bf = _deflate_or_store(new, bf)
            src += bc
            blocks_out.append(comp)
            info_blocks.append(struct.pack(">IIH", bu, len(comp), bf))
        info = (self.info_hash + struct.pack(">i", len(self.blocks))
                + b"".join(info_blocks) + self.nodes_raw)
        info_c, head_flags = _deflate_or_store(info, self.flags)

        head = bytearray(buf[:self.size_at])
        head += b"\0" * 20  # size, csize, usize, flags: filled in below
        if self.fmt >= 7:
            head += b"\0" * (_align(len(head)) - len(head))
        head += info_c
        if self.flags & 0x200:
            head += b"\0" * (_align(len(head)) - len(head))
        body = b"".join(blocks_out)
        total = len(head) + len(body)
        struct.pack_into(">qIII", head, self.size_at, total, len(info_c), len(info),
                         head_flags)
        out = bytes(head) + body
        # Read it back. A writer bug here is a manifest no client can open, so
        # it is caught now, on the box, instead of on a player's title screen.
        if UnityFS(out).payload != payload:
            raise ComposeError("rewritten manifest does not read back")
        return out


# ---------------------------------------------------------------------------
# Diffing and per-layer analysis
# ---------------------------------------------------------------------------

def changed_positions(a, b, lo=0, hi=None):
    """Indices in [lo, hi) where equal-length `a` and `b` differ."""
    hi = len(a) if hi is None else hi
    out, step = [], 4096
    for start in range(lo, hi, step):
        end = min(start + step, hi)
        if a[start:end] == b[start:end]:
            continue
        for k in range(start, end):
            if a[k] != b[k]:
                out.append(k)
    return out


def _find_all(blob, needle, lo=0, hi=None):
    hi = len(blob) if hi is None else hi
    hits, at = [], blob.find(needle, lo, hi)
    while at >= 0:
        hits.append(at)
        at = blob.find(needle, at + 1, hi)
    return hits


def _hash_bytes(value):
    try:
        raw = bytes.fromhex(value)
    except (TypeError, ValueError):
        raise ComposeError("not a hash: %r" % (value,))
    if len(raw) != HASH_LEN:
        raise ComposeError("not a Hash128: %r" % (value,))
    return raw


def _extradata_count(blob):
    if len(blob) < 6:
        raise ComposeError("_ExtraData is too short")
    version, count = struct.unpack_from("<IH", blob, 0)
    if version != 1 or not count or 6 + HASH_LEN * count > len(blob):
        raise ComposeError("_ExtraData header is not version 1 with a table")
    return count


class Layer(object):
    """One pack's edits, proven against the stock pair."""

    def __init__(self, pack_id, bundles, manifest_slots, table_slots, windows):
        self.pack_id = pack_id
        self.bundles = bundles              # {name: new hash hex}
        self.manifest_slots = manifest_slots  # {payload offset: 16 new bytes}
        self.table_slots = table_slots      # {table index: 16 new bytes}
        self.windows = windows              # {record offset: 8 new bytes}


def analyse_layer(pack_id, stock_fs, stock_ed, manifest, extra, bundles, sizes):
    """Prove `pack_id`'s variant pair is exactly its declared overrides.

    Raises ComposeError, and only ComposeError, for anything wrong with it.
    """
    try:
        return _analyse_layer(pack_id, stock_fs, stock_ed, manifest, extra,
                              bundles, sizes)
    except ComposeError:
        raise
    except (struct.error, IndexError, ValueError, TypeError, AttributeError,
            OverflowError) as e:
        raise ComposeError("%s: malformed layer: %s" % (pack_id, e))


def _analyse_layer(pack_id, stock_fs, stock_ed, manifest, extra, bundles, sizes):
    """See `analyse_layer`.

    `bundles` is ``{bundle name: new hash}``; `sizes` is ``{new hash: served
    byte count}`` — the size of the file the mirror will actually serve for it,
    which is what its ``_ExtraData`` record must say.

    ``_ExtraData`` is edited in place by the builder, so it is checked by a
    byte diff. The manifest is NOT: UnityPy re-serialises the whole
    SerializedFile when it saves (on the real manifest the metadata shrinks by 8
    bytes and the data offset moves from 4096 to 4000), so the variant's layout
    says nothing about where the stock hash sits. The manifest half is instead
    carried as "replace the stock hash with the new one", where the stock hash
    is read from the ``_ExtraData`` table slot the builder repointed and must
    occur exactly once in the stock manifest — the two halves have to agree on
    which bundle moved, or the layer is refused.
    """
    if not bundles:
        raise ComposeError("%s declares no bundles" % pack_id)
    if len(extra) != len(stock_ed):
        raise ComposeError("%s _ExtraData is not the stock length" % pack_id)
    count = _extradata_count(stock_ed)
    if extra[:6] != stock_ed[:6]:
        raise ComposeError("%s _ExtraData header differs from stock" % pack_id)
    table_end = 6 + HASH_LEN * count

    def slot(blob, i):
        return blob[6 + HASH_LEN * i:6 + HASH_LEN * (i + 1)]

    # The hash table: aligned slots, one per declared bundle.
    table_slots, index_of, old_hash = {}, {}, {}
    for name, new_hex in sorted(bundles.items()):
        new = _hash_bytes(new_hex)
        slots = [i for i in range(count) if slot(extra, i) == new]
        if len(slots) != 1:
            raise ComposeError("%s: %s's hash occurs %d times in its _ExtraData table"
                               % (pack_id, name, len(slots)))
        i = slots[0]
        if slot(stock_ed, i) == new:
            raise ComposeError("%s: %s is not repointed in its _ExtraData" % (pack_id, name))
        table_slots[i] = new
        index_of[name] = i
        old_hash[name] = slot(stock_ed, i)
    covered = set()
    for i in table_slots:
        covered.update(range(6 + HASH_LEN * i, 6 + HASH_LEN * (i + 1)))
    stray = [k for k in changed_positions(stock_ed, extra, 0, table_end) if k not in covered]
    if stray:
        raise ComposeError("%s _ExtraData table changes %d byte(s) no declared bundle "
                           "explains" % (pack_id, len(stray)))

    # The records: an 8-byte (size, crc 0) window per bundle, only where the
    # builder changed bytes, in table order.
    # Two bundles of one pack can have the same served size, and then their
    # windows read identically; records follow table order, so the k-th such
    # window belongs to the k-th such bundle by table index.
    by_size = {}
    for name, new_hex in bundles.items():
        size = sizes.get(new_hex.lower())
        if not size:
            raise ComposeError("%s: no served size for %s (%s)" % (pack_id, name, new_hex))
        by_size.setdefault(size, []).append(name)
    windows, found = {}, []
    for size, names in sorted(by_size.items()):
        want = struct.pack("<II", size, 0)
        hits = [o for o in _find_all(extra, want, table_end)
                if stock_ed[o:o + 8] != want]
        if len(hits) != len(names):
            raise ComposeError("%s: %s: %d changed (size %d, crc 0) record(s), need %d"
                               % (pack_id, ", ".join(sorted(names)), len(hits), size,
                                  len(names)))
        for o, name in zip(sorted(hits), sorted(names, key=lambda n: index_of[n])):
            windows[o] = want
            found.append((index_of[name], o, name))
    found.sort()
    offsets = [o for _, o, _ in found]
    if offsets != sorted(offsets):
        raise ComposeError("%s: records are not in table order (%s)"
                           % (pack_id, ", ".join(n for _, _, n in found)))
    for a, b in zip(offsets, offsets[1:]):
        if b - a < 10:  # a record is at least size + crc + u16 path count
            raise ComposeError("%s: two record windows overlap" % pack_id)
    covered = set()
    for o in windows:
        covered.update(range(o, o + 8))
    stray = [k for k in changed_positions(stock_ed, extra, table_end) if k not in covered]
    if stray:
        raise ComposeError("%s _ExtraData records change %d byte(s) no declared bundle "
                           "explains (first at 0x%x)" % (pack_id, len(stray), stray[0]))

    # The manifest. Its payload must be about the stock size (it holds the
    # same entries); anything far bigger is refused before it is inflated.
    try:
        layer_p = UnityFS(manifest, max_payload=2 * len(stock_fs.payload) + 65536).payload
    except ComposeError as e:
        raise ComposeError("%s manifest: %s" % (pack_id, e))
    stock_p = stock_fs.payload
    manifest_slots = {}
    for name, new_hex in sorted(bundles.items()):
        new, old = _hash_bytes(new_hex), old_hash[name]
        if layer_p.count(new) != 1:
            raise ComposeError("%s: %s's new hash occurs %d times in its manifest"
                               % (pack_id, name, layer_p.count(new)))
        if old in layer_p:
            raise ComposeError("%s: its manifest still names %s's stock hash"
                               % (pack_id, name))
        hits = _find_all(stock_p, old)
        if len(hits) != 1:
            raise ComposeError("%s: %s's stock hash occurs %d times in the stock manifest"
                               % (pack_id, name, len(hits)))
        manifest_slots[hits[0]] = new
    # Every bundle the layer does NOT declare must still be named by its stock
    # hash: a variant that repointed something it does not declare would have
    # that change silently dropped here, so it is refused instead.
    undeclared = [i for i in range(count)
                  if i not in table_slots and slot(stock_ed, i) in stock_p
                  and slot(stock_ed, i) not in layer_p]
    if undeclared:
        raise ComposeError("%s manifest repoints %d bundle(s) it does not declare "
                           "(first: stock hash %s)"
                           % (pack_id, len(undeclared), slot(stock_ed, undeclared[0]).hex()))
    return Layer(pack_id, dict(bundles), manifest_slots, table_slots, windows)


def check_conflicts(layers):
    """Raise LayerConflict if any two layers overlap, by name or by byte."""
    owner = {}
    for layer in layers:
        for name in layer.bundles:
            if name in owner:
                raise LayerConflict("%s and %s both override %s"
                                    % (owner[name], layer.pack_id, name))
            owner[name] = layer.pack_id
    for field, width in (("manifest_slots", HASH_LEN), ("table_slots", 1),
                         ("windows", 8)):
        claimed = {}
        for layer in layers:
            for at in getattr(layer, field):
                for k in range(at, at + width):
                    if k in claimed:
                        raise LayerConflict("%s and %s edit the same bytes (%s)"
                                            % (claimed[k], layer.pack_id, field))
                    claimed[k] = layer.pack_id


def compose_layers(stock_manifest, stock_extra, layers_input):
    """Merge several packs into one (manifest bytes, _ExtraData bytes).

    `layers_input` is a list of dicts: ``id``, ``manifest`` (bytes), ``extra``
    (bytes), ``bundles`` ({name: hash}), ``sizes`` ({hash: served bytes}).
    One layer is returned verbatim. Raises ComposeError / LayerConflict.
    """
    if not layers_input:
        raise ComposeError("nothing to compose")
    if len(layers_input) == 1:
        only = layers_input[0]
        return bytes(only["manifest"]), bytes(only["extra"])
    stock_fs = UnityFS(stock_manifest)
    _extradata_count(stock_extra)
    layers = [analyse_layer(li["id"], stock_fs, stock_extra, li["manifest"],
                            li["extra"], li["bundles"], li["sizes"])
              for li in sorted(layers_input, key=lambda x: x["id"])]
    check_conflicts(layers)

    payload = bytearray(stock_fs.payload)
    extra = bytearray(stock_extra)
    for layer in layers:
        for at, new in layer.manifest_slots.items():
            payload[at:at + HASH_LEN] = new
        for i, new in layer.table_slots.items():
            extra[6 + HASH_LEN * i:6 + HASH_LEN * (i + 1)] = new
        for at, new in layer.windows.items():
            extra[at:at + 8] = new
    return stock_fs.with_payload(bytes(payload)), bytes(extra)


def md5_hex(data):
    return hashlib.md5(data).hexdigest()


# ---------------------------------------------------------------------------
# Workstation CLI: compose pack folders the way the mirror would, and write the
# result as a pack folder. Useful to inspect a composition before it is live.
# ---------------------------------------------------------------------------

def _load_pack_dir(path):
    with open(os.path.join(path, "pack.json")) as fh:
        desc = json.load(fh)

    def read(h):
        with open(os.path.join(path, h), "rb") as fh:
            return fh.read()

    bundles = {b["bundleName"]: b["bundleHash"].lower() for b in desc["bundles"]}
    sizes = {}
    for b in desc["bundles"]:
        p = os.path.join(path, b["bundleHash"])
        sizes[b["bundleHash"].lower()] = (os.path.getsize(p) if os.path.isfile(p)
                                          else int(b.get("sizeBytes") or 0))
    return desc, {"id": desc["id"], "manifest": read(desc["manifestHash"]),
                  "extra": read(desc["extraDataHash"]), "bundles": bundles,
                  "sizes": sizes}


def main(argv=None):
    import argparse
    ap = argparse.ArgumentParser(description="Compose asset packs into one pair.")
    ap.add_argument("stock_manifest")
    ap.add_argument("stock_extradata")
    ap.add_argument("packs", nargs="+", help="pack folders (each with pack.json)")
    ap.add_argument("--out", required=True, help="directory to write the composed pair")
    args = ap.parse_args(argv)
    with open(args.stock_manifest, "rb") as fh:
        stock_m = fh.read()
    with open(args.stock_extradata, "rb") as fh:
        stock_e = fh.read()
    loaded = [_load_pack_dir(p) for p in args.packs]
    try:
        manifest, extra = compose_layers(stock_m, stock_e, [li for _, li in loaded])
    except ComposeError as e:
        print("refused: %s" % e, file=sys.stderr)
        return 2
    os.makedirs(args.out, exist_ok=True)
    mh, eh = md5_hex(manifest), md5_hex(extra)
    for h, data in ((mh, manifest), (eh, extra)):
        with open(os.path.join(args.out, h), "wb") as fh:
            fh.write(data)
    print("manifest %s (%d bytes)\nextradata %s (%d bytes)\nlayers: %s"
          % (mh, len(manifest), eh, len(extra), ", ".join(sorted(d["id"] for d, _ in loaded))))
    return 0


if __name__ == "__main__":
    sys.exit(main())
