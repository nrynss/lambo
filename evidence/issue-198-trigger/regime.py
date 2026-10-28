import json,re,unicodedata,sys
def fold(s):
    out,gap=[],False
    for ch in unicodedata.normalize("NFC",s):
        if ch in "_-" or ch.isspace(): gap=True; continue
        if gap and out: out.append(" ")
        gap=False; out.append(ch.lower())
    return "".join(out)
def ident(f):
    t2=[]
    for t in f.split(" "):
        t=re.sub(r"^[^\w/.]+|[^\w/.]+$","",t).rstrip(".")
        if t and any((c.isascii() and c.isdigit()) or c in "/." for c in t): t2.append(t)
    return t2
def same(a,b):
    fa,fb=fold(a),fold(b); return re.findall(r"[0-9]+",fa)==re.findall(r"[0-9]+",fb) and ident(fa)==ident(fb)
def masked(s):
    f=fold(s); toks=[]
    for t in f.split(" "):
        t=re.sub(r"^[^\w/.]+|[^\w/.]+$","",t).rstrip(".")
        if not t: continue
        if any((c.isascii() and c.isdigit()) or c in "/." for c in t): t="<id>"
        toks.append(t)
    return set(toks)
def jac(a,b):
    A,B=masked(a),masked(b); return len(A&B)/len(A|B)
P=json.load(open("fixtures/eval/pairs.json"))
rows=[]
for p in P:
    if p.get("relation") in ("supersedes","distinct_entity") or p["family"]=="same-event":
        ct=p.get("concept_types") or []
        reg = len(ct)==2 and ct[0]==ct[1] and ct[0] in ("Logic","Constraint","Observation")
        rows.append((p["id"],p.get("relation") or p["family"],p["split"],reg,not same(p["a"],p["b"]),round(jac(p["a"],p["b"]),2),"review" in p))
import collections
for kind in ("supersedes","distinct_entity","same-event"):
    rs=[r for r in rows if r[1]==kind]
    inr=[r for r in rs if r[3] and r[4] and not r[6]]
    print(kind,"total",len(rs),"in-regime&refused&adjudicated",len(inr),"splits",collections.Counter(r[2] for r in inr))
    js=sorted(r[5] for r in inr); 
    if js: print("  jaccard min/median/max",js[0],js[len(js)//2],js[-1])
    for r in rs:
        if not (r[3] and r[4]): print("  OUT",r)
print()
for r in sorted(rows,key=lambda r:r[5]):
    if r[3] and r[4] and r[0].startswith("p-198") or (r[1]=="supersedes" and r[3] and r[4]):
        print(r)
