"""Hang Masser and Secunda in the town sky, as retail's Witches Festival did.

The art is Bethesda's own Arena 06 moons (`Arena_06_Moon2`, `Arena_06_MoonSmall_Half`
in kit_arena_sab). That bundle is not in the town's load closure, so nothing is
referenced across: the two textures and materials are COPIED into the district
prefab bundle, the quad mesh is authored here (Mesh type borrowed from vfx_sab), and
the only external is the moon shader itself, which lives in shaders_sab - the
same shader the town's own cloud cards already use, so it is loaded anyway.

    ./moons.py <levels_districts_towndistrict_0 in> <kit_arena_sab> <vfx_sab> <out>

Run it on the output of `builtin_halloween.py town`, not on the stock bundle.
"""
import copy
import math
import random
import sys

import numpy as np
import UnityPy
from PIL import Image

import graft

#: (texture, material, world direction az/el in degrees, distance m, diameter m, brighten)
MOONS = [
    ("Arena_06_Moon2", "Arena_06_Moon", -22.0, 30.0, 560.0, 120.0, 1.55),
    ("Arena_06_MoonSmall_Half", "Arena_06_MoonSmall", 14.0, 41.0, 560.0, 52.0, 1.45),
]


def quad(size):
    h = size / 2
    V = np.array([(-h, -h, 0), (h, -h, 0), (h, h, 0), (-h, h, 0)] * 2, np.float32)
    N = np.array([(0, 0, 1)] * 4 + [(0, 0, -1)] * 4, np.float32)
    UV = np.array([(0, 0), (1, 0), (1, 1), (0, 1)] * 2, np.float32)
    C = np.full((8, 4), 255, np.uint8)
    I = np.array([0, 1, 2, 0, 2, 3, 4, 6, 5, 4, 7, 6], np.int64)
    return V, N, UV, C, I


def look_quat(fwd, up=(0, 1, 0)):
    z = np.asarray(fwd, float)
    z /= np.linalg.norm(z)
    x = np.cross(up, z)
    x /= np.linalg.norm(x)
    y = np.cross(z, x)
    m = np.stack([x, y, z], 1)
    w = math.sqrt(max(0, 1 + m[0, 0] + m[1, 1] + m[2, 2])) / 2
    return {"x": (m[2, 1] - m[1, 2]) / (4 * w), "y": (m[0, 2] - m[2, 0]) / (4 * w),
            "z": (m[1, 0] - m[0, 1]) / (4 * w), "w": w}


def main(src, arena_path, vfx_path, dst):
    arena, vfx = UnityPy.load(arena_path), UnityPy.load(vfx_path)
    asf = graft._sf(arena)
    by_name = {}
    for o in arena.objects:
        if o.type.name in ("Texture2D", "Material"):
            by_name[(o.type.name, o.peek_name())] = o
    mesh_tmpl = next(o for o in vfx.objects if o.path_id == 5473315868345954330)

    env = UnityPy.load(src)
    sf = next(f for f in env.file.files.values() if hasattr(f, "types") and len(f.objects) > 100)
    objs = sf.objects
    rng = random.Random("moons")
    used = set(objs)

    def pid():
        while True:
            p = rng.randint(1, 2 ** 62)
            if p not in used:
                used.add(p)
                return p
    ref = lambda f, p: {"m_FileID": f, "m_PathID": p}  # noqa: E731

    if not any(t.class_id == 43 for t in sf.types):
        sf.types.append(copy.copy(mesh_tmpl.serialized_type))
    mesh_tid = next(i for i, t in enumerate(sf.types) if t.class_id == 43)
    tex_tmpl = next(o for o in objs.values() if o.type.name == "Texture2D")
    mat_tmpl = next(o for o in objs.values() if o.type.name == "Material")
    # component shapes from the scene's own cloud card (a sky object already)
    clouds = None
    for o in objs.values():
        if o.type.name == "GameObject" and o.peek_name() == "Clouds":
            g = o.read_typetree()
            kinds = {objs[c["component"]["m_PathID"]].type.name: objs[c["component"]["m_PathID"]]
                     for c in g["m_Component"] if c["component"]["m_PathID"] in objs}
            if {"MeshFilter", "MeshRenderer"} <= set(kinds):
                clouds = (o, g, kinds)
                break
    assert clouds, "no Clouds card in the scene"
    cgo, cg, ck = clouds
    bundle = next(o for o in objs.values() if o.type.name == "AssetBundle")
    ab = bundle.read_typetree()
    prefab = [c for c in ab["m_Container"] if c[0].endswith("_level.prefab")]
    assert len(prefab) == 1, [c[0] for c in ab["m_Container"]]
    root_go = objs[prefab[0][1]["asset"]["m_PathID"]]
    root_xf = next(objs[c["component"]["m_PathID"]] for c in root_go.read_typetree()["m_Component"]
                   if objs[c["component"]["m_PathID"]].type.name == "Transform")
    rx = root_xf.read_typetree()
    assert rx["m_LocalPosition"] == {"x": 0.0, "y": 0.0, "z": 0.0}, rx["m_LocalPosition"]
    assert abs(rx["m_LocalRotation"]["w"]) > 0.9999, rx["m_LocalRotation"]
    added = []
    made = []
    for tex_name, mat_name, az, el, dist, diam, gain in MOONS:
        # texture: decoded from the arena bundle, brightened, re-encoded inline here
        at = by_name[("Texture2D", tex_name)]
        img = at.read().image.convert("RGBA")
        a = np.array(img).astype(float)
        a[..., :3] = np.clip(a[..., :3] * gain, 0, 255)
        tt = at.read_typetree()
        tpid = pid()
        to = copy.copy(tex_tmpl)
        to.path_id = tpid
        objs[tpid] = to
        tt["m_StreamData"] = {"offset": 0, "size": 0, "path": ""}
        to.save_typetree(tt)
        tex = to.read()
        tex.image = Image.fromarray(a.astype(np.uint8), "RGBA")
        tex.m_Name = tex_name
        tex.save()

        mt = by_name[("Material", mat_name)].read_typetree()
        sh = mt["m_Shader"]
        mt["m_Shader"] = ref(graft._ext_index(sf, asf.externals[sh["m_FileID"] - 1].path), sh["m_PathID"])
        for _, te in mt["m_SavedProperties"]["m_TexEnvs"]:
            if te["m_Texture"]["m_PathID"]:
                te["m_Texture"] = ref(0, tpid) if _ == "_MainTex" else ref(0, 0)
        mt["m_Name"] = mat_name + "_Town"
        mpid = pid()
        mo = copy.copy(mat_tmpl)
        mo.path_id = mpid
        objs[mpid] = mo
        mo.save_typetree(mt)

        V, N, UV, C, I = quad(diam)
        mspid = pid()
        ms = copy.copy(mesh_tmpl)
        ms.assets_file, ms.path_id, ms.type_id, ms.serialized_type = sf, mspid, mesh_tid, sf.types[mesh_tid]
        objs[mspid] = ms
        ms.save_typetree(graft.build_mesh_tree(mesh_tmpl.read_typetree(), mat_name + "_Quad", V, N, UV, C, I))

        d = np.array([math.sin(math.radians(az)) * math.cos(math.radians(el)), math.sin(math.radians(el)),
                      -math.cos(math.radians(az)) * math.cos(math.radians(el))])
        pos = d * dist
        go_pid, xf_pid, mf_pid, mr_pid = pid(), pid(), pid(), pid()
        go_t = copy.deepcopy(cg)
        go_t["m_Name"] = "Moon_" + mat_name
        go_t["m_IsActive"] = True
        go_t["m_Component"] = [{"component": ref(0, p)} for p in (xf_pid, mf_pid, mr_pid)]
        if "m_StaticEditorFlags" in go_t:
            go_t["m_StaticEditorFlags"] = 0
        xf_t = copy.deepcopy(ck["Transform"].read_typetree())
        xf_t.update(m_GameObject=ref(0, go_pid), m_Father=ref(0, root_xf.path_id), m_Children=[],
                    m_LocalPosition=dict(zip("xyz", map(float, pos))),
                    m_LocalRotation=look_quat(-d), m_LocalScale={"x": 1.0, "y": 1.0, "z": 1.0})
        mf_t = copy.deepcopy(ck["MeshFilter"].read_typetree())
        mf_t.update(m_GameObject=ref(0, go_pid), m_Mesh=ref(0, mspid))
        mr_t = copy.deepcopy(ck["MeshRenderer"].read_typetree())
        mr_t.update(m_GameObject=ref(0, go_pid), m_Materials=[ref(0, mpid)], m_StaticBatchRoot=ref(0, 0),
                    m_LightmapIndex=65535, m_LightmapIndexDynamic=65535, m_CastShadows=0, m_ReceiveShadows=0)
        if "m_StaticBatchInfo" in mr_t:
            mr_t["m_StaticBatchInfo"] = {"firstSubMesh": 0, "subMeshCount": 0}
        for p_, o, t in ((go_pid, cgo, go_t), (xf_pid, ck["Transform"], xf_t),
                         (mf_pid, ck["MeshFilter"], mf_t), (mr_pid, ck["MeshRenderer"], mr_t)):
            n = copy.copy(o)
            n.path_id = p_
            objs[p_] = n
            n.save_typetree(t)
        made.append((go_t["m_Name"], pos.round(0).tolist(), diam))
        rx["m_Children"].append(ref(0, xf_pid))
        added += [ref(0, p) for p in (go_pid, xf_pid, mf_pid, mr_pid, mspid, mpid, tpid)] + [mt["m_Shader"]]

    root_xf.save_typetree(rx)
    entry = prefab[0][1]
    at = entry["preloadIndex"] + entry["preloadSize"]
    ab["m_PreloadTable"][at:at] = added
    for _, c in ab["m_Container"]:
        if c is not entry and c["preloadIndex"] >= at:
            c["preloadIndex"] += len(added)
    entry["preloadSize"] += len(added)
    bundle.save_typetree(ab)

    open(dst, "wb").write(env.file.save(packer="original"))
    back = UnityPy.load(dst)
    names = [o.peek_name() for o in back.objects if o.type.name == "GameObject"]
    for m in made:
        assert m[0] in names, m
    texs = {o.peek_name(): o for o in back.objects if o.type.name == "Texture2D"}
    for tex_name, *_ in MOONS:
        t = texs[tex_name].read()
        im = np.array(t.image.convert("RGBA"))
        assert im.shape[0] >= 256 and t.m_StreamData.size == 0 and im[..., 3].max() == 255, tex_name
    print("moons:", made)


if __name__ == "__main__":
    main(*sys.argv[1:5])
