"""Graft authored web geometry into a town building prefab bundle.

Adds, under the prefab root: one GameObject with Transform + MeshFilter +
MeshRenderer (+ the lightmap-applier MonoBehaviour when the template has one),
a Mesh object that lives INSIDE this bundle (its SerializedType is borrowed from
vfx_sab, same Unity build), and a Material cloned from Bethesda's
`Spider_Webbing_SeeThrough01_alb` with its shader/texture PPtrs re-pointed
through this bundle's externals. No new bundle dependency: the shader and the
webbing texture bundles are already in every house's load closure
(house -> vfx_sab -> animatedobjects_breakables_sab -> its textures).
"""
import copy
import random
import struct

import numpy as np
import UnityPy

WEB_MATERIAL = "Spider_Webbing_SeeThrough01_alb"


def _sf(env):
    return next(f for f in env.file.files.values() if hasattr(f, "externals"))


def _cab(env):
    return next(k for k in env.file.files if k.startswith("CAB-") and "." not in k)


def _ext_index(sf, cab_path):
    name = cab_path.split("/")[-1].lower()
    for i, e in enumerate(sf.externals):
        if e.path.split("/")[-1].lower() == name:
            return i + 1
    new = copy.copy(sf.externals[0])
    new.path = "archive:/%s/%s" % (name, name)
    sf.externals.append(new)
    return len(sf.externals)


def _aabb(v):
    lo, hi = v.min(0), v.max(0)
    c, e = (lo + hi) / 2, (hi - lo) / 2
    return {"m_Center": dict(zip("xyz", map(float, c))), "m_Extent": dict(zip("xyz", map(float, e)))}


def build_mesh_tree(template, name, V, N, UV, C, I):
    """Vertex layout copied from Bethesda's web shell: pos3f nrm3f tan4f col4u8 uv2f (52 B)."""
    t = copy.deepcopy(template)
    n = len(V)
    T = np.zeros((n, 4), np.float32)
    # tangent: any vector perpendicular to the normal, w = 1
    ref = np.where(np.abs(N[:, 1:2]) < 0.9, [[0, 1, 0]], [[1, 0, 0]])
    tg = np.cross(ref, N)
    tg /= np.linalg.norm(tg, axis=1)[:, None] + 1e-9
    T[:, :3], T[:, 3] = tg, 1.0
    buf = bytearray()
    for k in range(n):
        buf += struct.pack("<3f3f4f4B2f", *V[k], *N[k], *T[k], *C[k], *UV[k])
    assert len(buf) == 52 * n
    idx = np.asarray(I, np.uint16).tobytes()
    t["m_Name"] = name
    t["m_VertexData"]["m_VertexCount"] = n
    t["m_VertexData"]["m_DataSize"] = bytes(buf)
    t["m_IndexFormat"] = 0
    t["m_IndexBuffer"] = list(idx)
    ab = _aabb(V)
    t["m_SubMeshes"] = [{"firstByte": 0, "indexCount": len(I), "topology": 0, "baseVertex": 0,
                         "firstVertex": 0, "vertexCount": n, "localAABB": ab}]
    t["m_LocalAABB"] = ab
    t["m_IsReadable"] = False
    t["m_StreamData"] = {"offset": 0, "size": 0, "path": ""}
    t["m_BakedConvexCollisionMesh"], t["m_BakedTriangleCollisionMesh"] = [], []
    return t


def graft(src, dst, vfx_path, brk_path, V, N, UV, C, I, name="Halloween_Webs",
          color=None, custom_sh=None, lightmap=True, seed=None, shader_pid=None, tint=None):
    vfx, brk = UnityPy.load(vfx_path), UnityPy.load(brk_path)
    vsf, bsf = _sf(vfx), _sf(brk)
    mesh_tmpl = next(o for o in vfx.objects if o.path_id == 5473315868345954330)
    mesh_type = mesh_tmpl.serialized_type
    web_mat = next(o for o in brk.objects if o.type.name == "Material"
                   and o.read_typetree()["m_Name"] == WEB_MATERIAL)

    env = UnityPy.load(src)
    sf = _sf(env)
    objs = sf.objects
    rng = random.Random(seed or src)
    used = set(objs)

    def new_pid():
        while True:
            p = rng.randint(1, 2 ** 62)
            if p not in used:
                used.add(p)
                return p

    ref = lambda fid, pid: {"m_FileID": fid, "m_PathID": pid}  # noqa: E731

    # --- Mesh: borrow the type, then write our tree through it
    if not any(t.class_id == 43 for t in sf.types):
        sf.types.append(copy.copy(mesh_type))
    mesh_tid = next(i for i, t in enumerate(sf.types) if t.class_id == 43)
    mesh_pid = new_pid()
    mo = copy.copy(mesh_tmpl)
    mo.assets_file, mo.path_id = sf, mesh_pid
    mo.type_id, mo.serialized_type = mesh_tid, sf.types[mesh_tid]
    objs[mesh_pid] = mo
    mo.save_typetree(build_mesh_tree(mesh_tmpl.read_typetree(), name, V, N, UV, C, I))

    # --- Material: Bethesda's see-through webbing, PPtrs re-pointed through our externals
    mt = web_mat.read_typetree()

    def remap(p):
        if p["m_FileID"] == 0:
            raise SystemExit("material references a local object in breakables")
        return ref(_ext_index(sf, bsf.externals[p["m_FileID"] - 1].path), p["m_PathID"])

    mt["m_Shader"] = remap(mt["m_Shader"])
    if shader_pid is not None:
        # same shaders_sab external as BGS/Standard; only the object changes
        mt["m_Shader"] = ref(mt["m_Shader"]["m_FileID"], shader_pid)
        mt["m_ShaderKeywords"] = ""
        mt["m_CustomRenderQueue"] = 3000
    for _, te in mt["m_SavedProperties"]["m_TexEnvs"]:
        if te["m_Texture"]["m_PathID"]:
            te["m_Texture"] = remap(te["m_Texture"])
    mt["m_Name"] = name + "_mat"
    cols = dict(mt["m_SavedProperties"]["m_Colors"])
    if color is not None:
        cols["_Color"] = dict(zip("rgba", color))
    if custom_sh is not None:
        for k, v in zip(("_CustomSHAr", "_CustomSHAg", "_CustomSHAb"), custom_sh):
            cols[k] = dict(zip("rgba", v))
    if tint is not None:
        cols["_TintColor"] = dict(zip("rgba", tint))
    mt["m_SavedProperties"]["m_Colors"] = list(cols.items())
    # the Material type may be absent too (castle shops carry none): borrow it
    if not any(t.class_id == 21 for t in sf.types):
        sf.types.append(copy.copy(web_mat.serialized_type))
    mat_tid = next(i for i, t in enumerate(sf.types) if t.class_id == 21)
    mat_pid = new_pid()
    ma = copy.copy(web_mat)
    ma.assets_file, ma.path_id = sf, mat_pid
    ma.type_id, ma.serialized_type = mat_tid, sf.types[mat_tid]
    objs[mat_pid] = ma
    ma.save_typetree(mt)

    # --- GameObject from the prefab's own lit mesh child
    bundle = next(o for o in env.objects if o.type.name == "AssetBundle")
    ab = bundle.read_typetree()
    prefab = [c for c in ab["m_Container"] if c[0].endswith(".prefab")]
    if len(prefab) != 1:
        raise SystemExit("expected one prefab, found %d" % len(prefab))
    root_go = objs[prefab[0][1]["asset"]["m_PathID"]]
    root_xf = next(objs[c["component"]["m_PathID"]] for c in root_go.read_typetree()["m_Component"]
                   if objs[c["component"]["m_PathID"]].type.name == "Transform")
    tmpl = None
    order = sorted(env.objects, key=lambda o: 0 if o.type.name == "GameObject"
                   and o.peek_name() == "Decal_Dirt_02" else 1)
    for o in order:
        if o.type.name != "GameObject":
            continue
        g = o.read_typetree()
        kinds = {objs[c["component"]["m_PathID"]].type.name: objs[c["component"]["m_PathID"]]
                 for c in g["m_Component"] if c["component"]["m_PathID"] in objs}
        if g["m_Layer"] == 0 and g["m_IsActive"] and {"Transform", "MeshFilter", "MeshRenderer",
                                                       "MonoBehaviour"} <= set(kinds):
            if g["m_Name"].startswith("Town_") or g["m_Name"].startswith("Decal"):
                tmpl = (o, g, kinds)
                break
    if tmpl is None:
        raise SystemExit("no layer-0 lit mesh child to clone")
    tgo, tg, comp = tmpl

    go_t = copy.deepcopy(tg)
    xf_t = copy.deepcopy(comp["Transform"].read_typetree())
    mf_t = copy.deepcopy(comp["MeshFilter"].read_typetree())
    mr_t = copy.deepcopy(comp["MeshRenderer"].read_typetree())
    mb_t = copy.deepcopy(comp["MonoBehaviour"].read_typetree())
    go_pid, xf_pid, mf_pid, mr_pid, mb_pid = (new_pid() for _ in range(5))
    go_t["m_Name"] = name
    go_t["m_IsActive"] = True
    go_t["m_Component"] = [{"component": ref(0, p)} for p in (xf_pid, mf_pid, mr_pid, mb_pid)]
    if "m_StaticEditorFlags" in go_t:
        go_t["m_StaticEditorFlags"] = 0
    xf_t.update(m_GameObject=ref(0, go_pid), m_Father=ref(0, root_xf.path_id), m_Children=[],
                m_LocalPosition={"x": 0.0, "y": 0.0, "z": 0.0},
                m_LocalRotation={"x": 0.0, "y": 0.0, "z": 0.0, "w": 1.0},
                m_LocalScale={"x": 1.0, "y": 1.0, "z": 1.0})
    mf_t.update(m_GameObject=ref(0, go_pid), m_Mesh=ref(0, mesh_pid))
    mr_t.update(m_GameObject=ref(0, go_pid), m_Materials=[ref(0, mat_pid)],
                m_LightmapIndex=65535, m_LightmapIndexDynamic=65535,
                m_StaticBatchRoot=ref(0, 0), m_CastShadows=0, m_ReceiveShadows=0)
    if "m_StaticBatchInfo" in mr_t:
        mr_t["m_StaticBatchInfo"] = {"firstSubMesh": 0, "subMeshCount": 0}
    mb_t["m_GameObject"] = ref(0, go_pid)
    mb_t["Renderer"] = ref(0, mr_pid)
    mb_t["LightMapEnabled"] = 1 if lightmap else 0
    for pid, o, t in ((go_pid, tgo, go_t), (xf_pid, comp["Transform"], xf_t),
                      (mf_pid, comp["MeshFilter"], mf_t), (mr_pid, comp["MeshRenderer"], mr_t),
                      (mb_pid, comp["MonoBehaviour"], mb_t)):
        n = copy.copy(o)
        n.path_id = pid
        objs[pid] = n
        n.save_typetree(t)

    rt = root_xf.read_typetree()
    rt["m_Children"].append(ref(0, xf_pid))
    root_xf.save_typetree(rt)

    entry = prefab[0][1]
    at = entry["preloadIndex"] + entry["preloadSize"]
    add = [ref(0, p) for p in (go_pid, xf_pid, mf_pid, mr_pid, mb_pid, mesh_pid, mat_pid)] + \
          [mt["m_Shader"]] + [te["m_Texture"] for _, te in mt["m_SavedProperties"]["m_TexEnvs"]
                              if te["m_Texture"]["m_PathID"]]
    ab["m_PreloadTable"][at:at] = add
    for _, c in ab["m_Container"]:
        if c is not entry and c["preloadIndex"] >= at:
            c["preloadIndex"] += len(add)
    entry["preloadSize"] += len(add)
    bundle.save_typetree(ab)

    data = env.file.save(packer="original")
    with open(dst, "wb") as fh:
        fh.write(data)

    # read back
    back = UnityPy.load(dst)
    bo = {o.path_id: o for o in back.objects}
    m = bo[mesh_pid].read_typetree()
    assert m["m_VertexData"]["m_VertexCount"] == len(V)
    assert len(m["m_IndexBuffer"]) == 2 * len(I)
    assert bo[mf_pid].read_typetree()["m_Mesh"] == ref(0, mesh_pid)
    assert bo[mr_pid].read_typetree()["m_Materials"] == [ref(0, mat_pid)]
    assert ref(0, xf_pid) in bo[root_xf.path_id].read_typetree()["m_Children"]
    mm = bo[mat_pid].read_typetree()
    ext = _sf(back).externals
    assert ext[mm["m_Shader"]["m_FileID"] - 1].path.split("/")[-1].lower() == \
        bsf.externals[web_mat.read_typetree()["m_Shader"]["m_FileID"] - 1].path.split("/")[-1].lower()
    # geometry round-trips through the mesh reader
    mesh_obj = bo[mesh_pid].read()
    from UnityPy.helpers.MeshHelper import MeshHandler
    h = MeshHandler(mesh_obj)
    h.process()
    got = np.array(h.m_Vertices, float).reshape(-1, 3)
    assert np.allclose(got, V, atol=1e-4), "vertex round-trip mismatch"
    return len(data), tg["m_Name"]
