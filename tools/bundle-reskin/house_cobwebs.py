#!/usr/bin/env python3
"""Hang Bethesda's own cobweb shells on a town house.

`vfx_sab` ships two meshes nothing in 1.31 instantiates:
`VFX_House_A_00_spiderWeb` and `VFX_House_B_00_spiderWeb` — house-sized web
shells, base at y=0, centred on the origin, i.e. authored in a house prefab's
local space. Their renderer points at `Spider_Webbing_SeeThrough01_alb`
(alpha-blended) in `animatedobjects_breakables_sab`. That is the event dressing
the capture shows draped over the shops; the placement was cut, the art was not.

This adds one child to the house prefab's root: GameObject + Transform +
MeshFilter + MeshRenderer, cloned from an existing mesh child of the same
bundle so every serialized field is shaped exactly as this Unity build expects,
then repointed:

  * MeshFilter.m_Mesh      -> the web mesh in vfx_sab (already an external here)
  * MeshRenderer material  -> Bethesda's see-through webbing (added as an external;
                              the loader already pulls that bundle in, because
                              house -> vfxprefabs_sab -> vfx_sab -> breakables)
  * no lightmap, no static batching, no shadow casting — a translucent sheet

and appends the four new objects plus the two external references to the
prefab's preload range, so loading the prefab loads the web with it.

    ./house_cobwebs.py <house bundle in> <vfx_sab> <breakables bundle> <out> A|B
"""
import copy
import random
import sys

import UnityPy

WEB_MATERIAL = "Spider_Webbing_SeeThrough01_alb"
WEB_MESH = {"A": "VFX_House_A_00_spiderWeb", "B": "VFX_House_B_00_spiderWeb"}


def _sf(env):
    return next(f for f in env.file.files.values() if hasattr(f, "externals"))


def _cab(env):
    return next(k for k in env.file.files if k.startswith("CAB-") and "." not in k)


def _find(env, type_name, name):
    hits = []
    for o in env.objects:
        if o.type.name != type_name:
            continue
        try:
            if o.read_typetree().get("m_Name") == name:
                hits.append(o)
        except Exception:
            pass
    if len(hits) != 1:
        raise SystemExit("%s %r: found %d, expected 1" % (type_name, name, len(hits)))
    return hits[0]


def _web_mesh_pid(vfx, which):
    go = _find(vfx, "GameObject", WEB_MESH[which]).read_typetree()
    objs = {o.path_id: o for o in vfx.objects}
    for c in go["m_Component"]:
        o = objs[c["component"]["m_PathID"]]
        if o.type.name == "MeshFilter":
            return o.read_typetree()["m_Mesh"]["m_PathID"]
    raise SystemExit("no MeshFilter on %s" % WEB_MESH[which])


def _external_index(sf, cab):
    """1-based FileID of `cab`, appending it as an external if absent."""
    for i, e in enumerate(sf.externals):
        if e.path.split("/")[-1].lower() == cab.lower():
            return i + 1
    new = copy.copy(sf.externals[0])  # same guid/type shape as its siblings
    new.path = "archive:/%s/%s" % (cab.lower(), cab.lower())
    sf.externals.append(new)
    return len(sf.externals)


def patch(src, vfx_path, brk_path, dst, which):
    vfx, brk = UnityPy.load(vfx_path), UnityPy.load(brk_path)
    mesh_pid = _web_mesh_pid(vfx, which)
    mat_pid = _find(brk, "Material", WEB_MATERIAL).path_id

    env = UnityPy.load(src)
    sf = _sf(env)
    objs = sf.objects
    vfx_fid = _external_index(sf, _cab(vfx))
    brk_fid = _external_index(sf, _cab(brk))

    bundle = next(o for o in env.objects if o.type.name == "AssetBundle")
    ab = bundle.read_typetree()
    prefab = [c for c in ab["m_Container"] if c[0].endswith(".prefab")]
    if len(prefab) != 1:
        raise SystemExit("expected one prefab in the container, found %d" % len(prefab))
    root_go = objs[prefab[0][1]["asset"]["m_PathID"]]
    root_t = root_go.read_typetree()
    root_xf = next(objs[c["component"]["m_PathID"]] for c in root_t["m_Component"]
                   if objs[c["component"]["m_PathID"]].type.name == "Transform")

    # Template: a mesh child to clone. An ivy LOD child (the first match) loads
    # but never draws on device; the house's own Decal_Dirt_02 (layer 0, with its
    # lightmap-applier MonoBehaviour) is the proven-rendering shape.
    tmpl = None
    for o in env.objects:
        if o.type.name != "GameObject":
            continue
        g = o.read_typetree()
        if TEMPLATE and g["m_Name"] != TEMPLATE:
            continue
        kinds = [objs[c["component"]["m_PathID"]].type.name for c in g["m_Component"]
                 if c["component"]["m_PathID"] in objs]
        if {"Transform", "MeshFilter", "MeshRenderer"} <= set(kinds):
            tmpl = (o, g)
            break
    if tmpl is None:
        raise SystemExit("no mesh child to clone")
    tgo, tg = tmpl
    comp = {objs[c["component"]["m_PathID"]].type.name: objs[c["component"]["m_PathID"]]
            for c in tg["m_Component"]}
    mb_src = comp.get("MonoBehaviour")

    rng = random.Random(src + which)  # deterministic for a given input
    used = set(objs)

    def new_pid():
        while True:
            p = rng.randint(-(2 ** 62), 2 ** 62)
            if p and p not in used:
                used.add(p)
                return p

    go_t = copy.deepcopy(tg)
    xf_t = copy.deepcopy(comp["Transform"].read_typetree())
    mf_t = copy.deepcopy(comp["MeshFilter"].read_typetree())
    mr_t = copy.deepcopy(comp["MeshRenderer"].read_typetree())
    go_pid, xf_pid, mf_pid, mr_pid = new_pid(), new_pid(), new_pid(), new_pid()
    mb_pid = new_pid() if mb_src is not None else None
    ref = lambda fid, pid: {"m_FileID": fid, "m_PathID": pid}  # noqa: E731

    go_t["m_Name"] = WEB_MESH[which]
    go_t["m_IsActive"] = True
    go_t["m_Component"] = [{"component": ref(0, p)} for p in (xf_pid, mf_pid, mr_pid, mb_pid) if p]
    xf_t.update(m_GameObject=ref(0, go_pid), m_Father=ref(0, root_xf.path_id), m_Children=[],
                m_LocalPosition={"x": 0.0, "y": 0.0, "z": 0.0},
                m_LocalRotation={"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                m_LocalScale={"x": 1.0, "y": 1.0, "z": 1.0})
    mf_t.update(m_GameObject=ref(0, go_pid), m_Mesh=ref(vfx_fid, mesh_pid))
    mr_t.update(m_GameObject=ref(0, go_pid), m_Materials=[ref(brk_fid, mat_pid)],
                m_LightmapIndex=65535, m_LightmapIndexDynamic=65535,
                m_StaticBatchRoot=ref(0, 0), m_CastShadows=0, m_ReceiveShadows=0)
    if "m_StaticBatchInfo" in mr_t:
        mr_t["m_StaticBatchInfo"] = {"firstSubMesh": 0, "subMeshCount": 0}
    if "m_StaticEditorFlags" in go_t:
        go_t["m_StaticEditorFlags"] = 0

    todo = [(go_pid, tgo, go_t), (xf_pid, comp["Transform"], xf_t),
            (mf_pid, comp["MeshFilter"], mf_t), (mr_pid, comp["MeshRenderer"], mr_t)]
    if mb_pid:
        mb_t = copy.deepcopy(mb_src.read_typetree())
        mb_t["m_GameObject"] = ref(0, go_pid)
        if "Renderer" in mb_t:
            mb_t["Renderer"] = ref(0, mr_pid)
        todo.append((mb_pid, mb_src, mb_t))
    for pid, o, t in todo:
        n = copy.copy(o)
        n.path_id = pid
        objs[pid] = n
        n.save_typetree(t)

    rt = root_xf.read_typetree()
    rt["m_Children"].append(ref(0, xf_pid))
    root_xf.save_typetree(rt)

    # Preload: insert at the end of the prefab's range, shift everything after it.
    entry = prefab[0][1]
    at = entry["preloadIndex"] + entry["preloadSize"]
    add = [ref(0, p) for p in (go_pid, xf_pid, mf_pid, mr_pid, mb_pid) if p] + \
          [ref(vfx_fid, mesh_pid), ref(brk_fid, mat_pid)]
    ab["m_PreloadTable"][at:at] = add
    for _, c in ab["m_Container"]:
        if c is entry:
            continue
        if c["preloadIndex"] >= at:
            c["preloadIndex"] += len(add)
    entry["preloadSize"] += len(add)
    bundle.save_typetree(ab)

    data = env.file.save(packer="original")
    open(dst, "wb").write(data)

    # Read back: the web child is under the root and points where we said.
    back = UnityPy.load(dst)
    bo = {o.path_id: o for o in back.objects}
    g = bo[go_pid].read_typetree()
    mf = bo[mf_pid].read_typetree()
    mr = bo[mr_pid].read_typetree()
    ext = _sf(back).externals
    assert g["m_Name"] == WEB_MESH[which]
    assert ref(0, xf_pid) in bo[root_xf.path_id].read_typetree()["m_Children"]
    assert mf["m_Mesh"] == ref(vfx_fid, mesh_pid)
    assert ext[vfx_fid - 1].path.split("/")[-1].lower() == _cab(vfx).lower()
    assert ext[brk_fid - 1].path.split("/")[-1].lower() == _cab(brk).lower()
    assert mr["m_Materials"] == [ref(brk_fid, mat_pid)]
    print("  %s: + %s (mesh fid %d, material fid %d) %d bytes"
          % (src.split("/")[-1], WEB_MESH[which], vfx_fid, brk_fid, len(data)))


TEMPLATE = None

if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--template=")]
    for a in sys.argv[1:]:
        if a.startswith("--template="):
            TEMPLATE = a.split("=", 1)[1]
    patch(*args[:5])
