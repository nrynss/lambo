"""Read-only access to a lambo SQLite store copy, for the #190 owner tools.

Handles both vector codecs: the bracketed TEXT list of stores written before
0.7.0 and the packed little-endian f32 of #55 (`store_meta.vector_codec =
'f32le'`). The codec is read from store_meta, never sniffed from the bytes.
"""
from __future__ import annotations

import hashlib
import math
import sqlite3
import struct
from typing import Any


def connect(store: str) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{store}?mode=ro", uri=True)


def sha256(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def codec(con: sqlite3.Connection) -> str:
    """`store_meta.vector_codec`; a store with no store_meta table or no
    codec row is the legacy text codec. Any other value is refused."""
    try:
        row = con.execute("SELECT value FROM store_meta WHERE key = 'vector_codec'").fetchone()
    except sqlite3.OperationalError as e:
        if "no such table" in str(e):
            return "text"
        raise
    value = (row[0] if row else None) or "text"
    if value not in ("text", "f32le"):
        raise SystemExit(f"unknown store_meta.vector_codec {value!r}: expected text or f32le")
    return value


def decode(blob: Any, packed: bool) -> list[float] | None:
    if blob is None:
        return None
    if packed:
        raw = bytes(blob)
        if not raw or len(raw) % 4:
            return None
        return list(struct.unpack(f"<{len(raw) // 4}f", raw))
    text = (blob.decode("utf-8") if isinstance(blob, bytes) else str(blob)).strip()
    if not (text.startswith("[") and text.endswith("]")) or not text[1:-1].strip():
        return None
    try:
        return [float(x) for x in text[1:-1].split(",")]
    except ValueError:
        return None


def concepts(store: str, session: str | None) -> list[tuple[str, str, list[float] | None]]:
    con = connect(store)
    try:
        packed = codec(con) == "f32le"
        sql = "SELECT id, content, embedding FROM concepts"
        params: tuple[Any, ...] = ()
        if session:
            sql += " WHERE session_id = ?"
            params = (session,)
        rows = con.execute(sql + " ORDER BY id", params).fetchall()
    finally:
        con.close()
    return [(i, c, decode(e, packed)) for i, c, e in rows]


def cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return 0.0 if na == 0.0 or nb == 0.0 else dot / (na * nb)


def pairwise_cosines(vectors: list[list[float]]):
    """A cosine lookup by row index: one matrix product with numpy when it is
    installed, the plain loop otherwise (minutes for a few thousand rows)."""
    try:
        import numpy as np
    except ImportError:
        return lambda i, j: cosine(vectors[i], vectors[j])
    widths = {len(v) for v in vectors}
    if len(widths) != 1:
        return lambda i, j: cosine(vectors[i], vectors[j])
    m = np.asarray(vectors, dtype=np.float64)
    norms = np.linalg.norm(m, axis=1)
    norms[norms == 0.0] = 1.0
    m = m / norms[:, None]
    sims = m @ m.T
    return lambda i, j: float(sims[i, j])
