import sqlite3, sys, json, array
src, out, tag = sys.argv[1], sys.argv[2], sys.argv[3]
con = sqlite3.connect(f"file:{src}?immutable=1", uri=True)
n = 0
with open(out, "w") as f:
    for cid, sess, typ, created, blob in con.execute(
        "select id, session_id, concept_type, created_at, embedding from concepts where embedding is not null"):
        if isinstance(blob, (bytes, bytearray)) and len(blob) == 4096:
            a = array.array("f"); a.frombytes(blob)
        else:
            t = blob.decode() if isinstance(blob, (bytes, bytearray)) else blob
            a = [float(x) for x in t.strip("[]").split(",")]
            if len(a) != 1024: continue
        f.write(json.dumps({"id": cid, "store": tag, "session": sess, "ctype": typ, "created_at": created,
                            "v": [round(x, 6) for x in a]}) + "\n"); n += 1
print(tag, n)
