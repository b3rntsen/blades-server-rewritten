"""A town building prefab's triangles in prefab-root space (metres, Y up).

The prefab bundle holds only transforms and renderers; the meshes live in other
bundles (`town_houses_sab`, ...). `lib_dir` is a directory of stock bundles —
any file name — and each one is indexed by the CAB it carries, which is how the
prefab's externals name them.
"""
import os

import numpy as np
import UnityPy
from UnityPy.helpers.MeshHelper import MeshHandler

#: Children that are not the building: foliage, clutter, VFX, lower LODs, decals.
SKIP = ('ivy', 'decal', 'lod1', 'lod2', 'lod3', 'collision', 'shrub', 'flora', 'tree', 'bush',
        'plant', 'grass', 'flower', 'vfx', 'clutter', 'l1_', 'l2_', 'banner', 'flag')

_index = {}
_libs = {}


def _cab_index(lib_dir):
    if lib_dir not in _index:
        idx = {}
        for n in sorted(os.listdir(lib_dir)):
            p = os.path.join(lib_dir, n)
            if not os.path.isfile(p):
                continue
            try:
                env = UnityPy.load(p)
            except Exception:
                continue
            for k in getattr(env.file, 'files', {}):
                if k.startswith('CAB-') and '.' not in k:
                    idx[k.lower()] = p
        _index[lib_dir] = idx
    return _index[lib_dir]


def _lib(path):
    if path not in _libs:
        _libs[path] = {o.path_id: o for o in UnityPy.load(path).objects}
    return _libs[path]


def quat_mat(q):
    x, y, z, w = q['x'], q['y'], q['z'], q['w']
    return np.array([[1 - 2 * (y * y + z * z), 2 * (x * y - z * w), 2 * (x * z + y * w)],
                     [2 * (x * y + z * w), 1 - 2 * (x * x + z * z), 2 * (y * z - x * w)],
                     [2 * (x * z - y * w), 2 * (y * z + x * w), 1 - 2 * (x * x + y * y)]])


def trs(t):
    m = np.eye(4)
    s = t['m_LocalScale']
    m[:3, :3] = quat_mat(t['m_LocalRotation']) @ np.diag([s['x'], s['y'], s['z']])
    p = t['m_LocalPosition']
    m[:3, 3] = [p['x'], p['y'], p['z']]
    return m


def mesh_tris(o):
    h = MeshHandler(o.read())
    h.process()
    v = np.array(h.m_Vertices, dtype=float).reshape(-1, 3)
    faces = np.concatenate([np.array(t, dtype=int).reshape(-1, 3) for t in h.get_triangles()])
    return v, faces


def load(house_path, lib_dir, skip=SKIP):
    """[(name, chain, vertices Nx3 in root space, faces Mx3)] for every building mesh."""
    env = UnityPy.load(house_path)
    sf = next(f for f in env.file.files.values() if hasattr(f, 'externals'))
    objs = {o.path_id: o for o in env.objects}
    idx = _cab_index(lib_dir)
    ext = [idx.get(e.path.split('/')[-1].lower()) for e in sf.externals]
    xfs = {o.path_id: o.read_typetree() for o in env.objects if o.type.name == 'Transform'}
    parent = {pid: t['m_Father']['m_PathID'] for pid, t in xfs.items()}
    go_of = {pid: t['m_GameObject']['m_PathID'] for pid, t in xfs.items()}
    names = {o.path_id: o.read_typetree()['m_Name'] for o in env.objects if o.type.name == 'GameObject'}
    xf_of_go = {g: x for x, g in go_of.items()}

    def world(pid):
        chain = []
        while pid:
            chain.append(pid)
            pid = parent.get(pid, 0)
        m = np.eye(4)
        for p in reversed(chain[:-1]):     # prefab-root space: the root itself excluded
            m = m @ trs(xfs[p])
        return m, [names.get(go_of[p], '?') for p in chain]

    parts = []
    for o in env.objects:
        if o.type.name != 'MeshFilter':
            continue
        t = o.read_typetree()
        gid = t['m_GameObject']['m_PathID']
        m, chain = world(xf_of_go[gid])
        if any(s in '/'.join(chain).lower() for s in skip):
            continue
        ref = t['m_Mesh']
        if ref['m_FileID'] == 0:
            src = objs.get(ref['m_PathID'])
        else:
            lib = ext[ref['m_FileID'] - 1]
            src = _lib(lib).get(ref['m_PathID']) if lib else None
        if src is None:
            continue
        v, f = mesh_tris(src)
        vw = (m @ np.c_[v, np.ones(len(v))].T).T[:, :3]
        parts.append((names.get(gid, ''), chain, vw, f))
    return parts
