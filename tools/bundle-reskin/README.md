# bundle-reskin — modding tools for the Blades client's asset bundles

Scripts for changing what the Blades client draws and plays — seasonal re-skins,
night lighting, authored meshes, client-side data tables — by editing its Unity
asset bundles and serving the result from this server's own bundle mirror.

They were developed against client **1.31** (bundle version
`1.31.0.4306485.26246488`, Unity 2019.4) and moved here from the Newblades
capture project so that anyone running this server can use and improve them.
They are covered by this repository's licence (AGPL-3.0-only) and contribution
terms — see [Contributing](#contributing).

**No game data is included.** Every script reads Bethesda's bundles from *your*
copy of the game (your APK, your device's cache, or the mirror your server
serves) and writes modified copies next to them. Never commit bundles,
textures, meshes, audio, extracted data or screenshots of them; `.gitignore`
here already excludes the usual inputs and outputs.

## Setup

    python3 -m venv .venv && . .venv/bin/activate
    pip install -r requirements.txt

`astc-encoder-py` is pinned to **0.1.12** on purpose (`astc_real.py` uses its
API directly). `TypeTreeGeneratorAPI` is only needed by the three scripts that
edit MonoBehaviour data (`patch_common.py`, `patch_inventory_capacity.py`,
`newcontent.py`); they also need your APK unzipped somewhere, given as
`APK_EXTRACT=<dir>` (they read `libil2cpp.so` and `global-metadata.dat`).

Check the install without any game files:

    python3 -m unittest discover -s tests -v   # graders are deterministic, no machine paths
    ./pumpkin.py && ./uvprobe.py               # writes preview PNGs from authored art

## Inputs: where the bundles come from

The server serves bundles from `<--static-data>/bundles.blades.bgs.services/`
(`server/src/main.rs`). Under `Android/` that mirror holds:

| path | what |
| --- | --- |
| `MetaManifest.json` | which bundle versions exist, and each one's manifest cache key |
| `Manifests/<version>` | the `AssetBundleManifest` — itself a plain UnityFS bundle: bundle name -> `Hash128` |
| `Manifests/<version>_ExtraData` | per-bundle size + CRC + asset paths (format below) |
| `Bundles/<first two hex>/<hash>` | each bundle, filed under its `Hash128` |

Bundle files are named by hash, so grepping the mirror for `town` finds nothing.
Get the name -> hash catalogue from the manifest:

    ./fetch_pack_inputs.py --from-dir <mirror>/Android                       # manifest, _ExtraData, bundle_hashes.json, bundle_sizes.json
    ./fetch_pack_inputs.py --from-dir <mirror>/Android town_textures_sab     # ...plus bisect/town_textures_sab.orig

Two things that look wrong and are correct:

* **`Hash128` is a cache key, not a checksum.** Hashing a downloaded bundle will
  not reproduce it. The client re-downloads a bundle exactly when the
  manifest's hash for it differs from what its cache holds.
* **The APK's `assets/Bundles/` copies are a stale build-time snapshot** (117
  "built-in" bundles: UI textures, menus, `dungeonsettings`, `common_client`…).
  They hash and size differently from the served copies; patch the served copy.
  A built-in bundle is only fetched once the manifest names a hash different
  from the one built in — confirm in the server log that the client actually
  requested your hash before concluding anything from the screen.

## The method

### 1. Find the target by measurement, never by name

A texture's name is not evidence of what it draws (the main menu's ground is
`Town_rocks_02_alb`, not the road texture). Two probes turn guessing into a
measurement:

| Script | What it answers |
| --- | --- |
| `noise_textures_only.py` | Does this bundle draw anything visible? Noises only the byte ranges Texture2D objects declare, so meshes sharing the same `.resS` stay exact and the result is attributable. |
| `colormap_bundle.py` | *Which* texture draws *where*. Paints every albedo a distinct flat colour and writes a `colour -> name` map; one screenshot names every surface. |
| `uvprobe.py` + `apply_named.py` | How a texture is laid out on the geometry: four bright quadrants in a black border. Shows tiling and atlas packing at a glance. |

**Which bundle owns an asset.** Walk the scene's renderers, group their
`m_Materials` PPtrs by `m_FileID`, map each file ID to a CAB name through the
SerializedFile's externals, then find the bundle that **owns** that CAB. The
owner contains it as uppercase `CAB-<id>`; a lowercase `cab-<id>` hit is a
bundle that merely *references* it, and will name an unrelated bundle with
complete confidence.

### 2. Pick the lever that actually reaches the pixel

1. **Material colour** — only if the shader reads it. Several sky/backdrop cards
   share a shader that ignores `_Color`: set to black, the pixels stay
   bit-identical. Group materials by `m_Shader` first and confirm on a device.
2. **Texture** — the reliable fallback (below).
3. **Realtime lights** (`night_lights.py`) — reaches lit geometry only.
4. **Lightmaps / RenderSettings** — the main-menu scene carries none, so menu
   ambient and fog live in the APK and are out of reach.

The hub/main menu is `levels_mainmenu_mainmenu`; the walkable town is
`levels_districts_towndistrict_0`, which is a **prefab** bundle (container
`*_level.prefab`), so new objects go in as root children within the preload
range (`webs/graft.py` does this).

### 3. A season is a palette re-grade, not a picture

`uvprobe.py` shows the town's architecture is tiled and atlas-packed: each wall
face samples an arbitrary region of its texture, so an image pasted into a
texture is shredded across unrelated faces. What survives that layout is a
change of *palette*:

* `retheme.py` / `apply_skin.py <src> <dst> <grader>` decode each albedo, push
  it through a ramp keyed off its own luminance, blend it back over the
  original so material detail survives, and re-encode with real ASTC
  (`astc_real.py`) at exactly the size the Texture2D declares, spliced in place
  so no offset moves. Normal maps are never graded.
* Graders: `halloween.py`, `winter.py`, `easter.py` pick a ramp per material
  family from the texture's name (grass, roof, brick, stone, wood…). Repeating
  *patterns* (eggs, pumpkins, snow) go on ground textures only, wrapped on all
  edges so the tile still repeats seamlessly.
* Pictures belong on **objects**: Bethesda shipped Halloween as decoration
  prefabs, not paint. `webs/` authors cobweb meshes from each building's
  triangles and grafts them in; `webs/moons.py` hangs Masser and Secunda from the
  game's own arena moon art; `house_cobwebs.py` re-places cut web shells.
* `builtin_halloween.py` authors nothing: it switches on the night variants the
  client already ships (kits, menu rig, town moon rig). `night_sky.py --retail`,
  `night_landscape.py` and `town_audio.py` finish the night look and sound.

Texture writing: `astc_real.py` (real encoder, use for anything near the
camera) or `astc66.py` (no dependency; one flat colour per 6×6 block via ASTC
void-extent blocks — ideal for probes). `splice_resS.py` /
`apply_texture_art.py` / `patch_bundle_texture.py` put the bytes in place.
Save bundles with the original packer: UnityPy's default `packer=None` writes
them uncompressed and several times larger.

### 4. Client data, not just art

`patch_common.py` and `patch_inventory_capacity.py` edit client-side tables in
`common` / `duplicates_sab` (backpack tiers, donation cooldown, level table) that
the server cannot change. `newcontent.py` applies a JSON content definition
(enemy swaps, text) and refuses if a stock value is not what the definition
expects, so a definition written for one build cannot silently land on another.

## `_ExtraData` and delivery over the air

Before downloading a bundle the client looks it up in `<version>_ExtraData`
**by `Hash128`**, and hands the stored CRC to Unity's cached download, which
refuses a mismatch. Format (Android; the iOS file is a different, smaller table):

    u32 version (1) | u16 count | count × raw Hash128, alphabetical
    then one VARIABLE-length record per bundle, same order:
      u32 size | u32 crc | u16 pathCount | length-prefixed asset paths | u16 refCount | u16 pairs

* The CRC is Unity's build-pipeline CRC over the uncompressed content (it
  matches the `CRC:` line in a bundle's build `.manifest`), not `zlib.crc32`
  of the file. `crc_definition.py` and `record_layout.py` reproduce that proof.
* **CRC 0 means "do not verify"**, which is what lets an authored bundle be
  downloaded at all.
* Records are variable-length, so the i-th record cannot be computed, and
  searching for a bundle's size is a coincidence detector. `extradata.locate`
  works from the bundle's index, needs two agreeing anchors (from
  `bundle_sizes.json`) and refuses rather than guesses.

So a pack is three things, which `build_pack_v2.py <id> "<name>" name=patched.bundle[#offset] …`
produces (files named by content hash, plus a `pack.json`):

1. the patched bundles;
2. a manifest variant whose `Hash128` for each patched bundle is the new hash
   (`repoint_manifest.py`);
3. an `_ExtraData` variant with each record's new size, CRC 0, and the old
   hash slot repointed to the new one (`extradata.py`,
   `repoint_extradata_hash.py`).

`asset_pack_compose.py STOCK_MANIFEST STOCK_EXTRADATA pack1/ pack2/ --out DIR`
merges several packs into one manifest + `_ExtraData` pair (stdlib only).

### Delivering a pack

**Quick render test, no server change:** place the bundle in the device's
Unity cache entry for that bundle. The cache path never consults the CRC, so
this tests rendering alone.

**Over the air from your server's mirror** (back up what you replace):

1. Copy each patched bundle to `Android/Bundles/<first two hex>/<hash>`.
2. Write the manifest and `_ExtraData` variants under a **new, higher version**
   in `Android/Manifests/` and advertise it in `MetaManifest.json` (bump that
   version's `manifestHash` too, so clients refetch the manifest). Write the
   file via a temp file and rename — every client reads it.
3. Watch the server log, not the screen: `MetaManifest.json`, then the
   manifest, `_ExtraData` and your new bundle hashes within a minute means it
   worked. `MetaManifest.json` followed by silence means the client rejected
   the manifest.

Rules that each cost a debugging session:

* **Versions only ever go up.** The client caches `_ExtraData` by version
  string and never refetches it. A client that has seen a higher version and is
  then offered a lower one sits on the title screen requesting nothing — which
  looks exactly like a broken APK. Roll back by rolling *forward* to a new,
  higher version that serves stock content; never by withdrawing the version.
* **A compressed block may not grow.** Unity refuses an LZ4 block larger than
  its uncompressed size; store such a block raw instead
  (`asset_pack_compose.py` handles this).
* The client requests manifests by its `Client-Version`, while captured
  manifests are named by the `GLD-Version` of the same build; a mirror may need
  to serve one under the other's name.

Docstrings occasionally cite design notes (`docs/*.md`) or issue numbers from
the original capture project; the substance of each is summarised above.

## Contributing

These tools are part of this repository and are licensed **AGPL-3.0-only**
like the server (`LICENSE`, `docs/LICENSING.md`). Contributions follow
[`CONTRIBUTING.md`](../../CONTRIBUTING.md): accept the [`CLA`](../../CLA.md) once
with a comment on your first pull request, and sign off your commits
(`git commit -s`). Per `CLA.md` §5, do not include game-client data — submit
the script and describe how to obtain its input from one's own game files.
