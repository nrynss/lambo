#!/usr/bin/env python3
"""#175: prepare private, unlabelled graph cases after a census.

Samples topology strata, not successful retrievals. No propagation arm is run.
The sheet deliberately contains no scores, degree counts or hop labels; its
separate key retains those sampling facts and graph IDs. Questions, grades,
facets, topic splits and publication decisions remain unset for a labeller.
Both files contain raw project memory and must remain outside the repository.
"""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import json
import math
import os
from pathlib import Path
import random
import re
import sqlite3

from graph_census import TRAVERSAL_ORDER, census, file_hash, reachable

FAMILIES = ('two_hop', 'mixed_sender_fanout', 'hub_reaching', 'sparse', 'identifier_bearing')
IDENTIFIER = re.compile(r'\b[0-9a-f]{7,40}\b|[\w.-]+/[\w./-]+|#\d+')


def prepare(con, session, per_family, seed):
    if per_family < 1:
        raise ValueError('per-family must be positive')
    # Reuse the session-boundary and malformed-edge checks of the census.
    report = census(con, session)
    rows = con.execute('SELECT id,content FROM concepts WHERE session_id=? ORDER BY id', (session,)).fetchall()
    texts = dict(rows)
    adj, inc = defaultdict(set), defaultdict(set)
    for u,v,t in con.execute('SELECT source,target,edge_type FROM edges WHERE session_id=?', (session,)):
        if t in TRAVERSAL_ORDER and u in texts and v in texts:
            adj[u].add(v); inc[v].add(u)
    incident = {u: len(adj[u] | inc[u]) for u in texts}
    cutoff = sorted(incident.values(),reverse=True)[math.ceil(len(texts)*.01)-1]
    hubs = {u for u in texts if incident[u] >= max(1,cutoff)}
    pools = {f: [] for f in FAMILIES}
    for u in sorted(texts):
        one, two = reachable(u,adj,1), reachable(u,adj,2)
        if two-one:
            pools['two_hop'].append(u)
        senders = {u} | (adj[u]-{u})
        if len({len(adj[v]) for v in senders if adj[v]}) > 1:
            pools['mixed_sender_fanout'].append(u)
        if (two-{u}) & hubs:
            pools['hub_reaching'].append(u)
        if not adj[u]:
            pools['sparse'].append(u)
        if IDENTIFIER.search(texts[u]):
            pools['identifier_bearing'].append(u)
    rng = random.Random(seed)
    sheet, key, used = [], [], set()
    for family in FAMILIES:
        pool = list(pools[family])
        rng.shuffle(pool)
        selected = [u for u in pool if u not in used][:per_family]
        for u in selected:
            used.add(u)
            case_id = f'graph175-{len(sheet)+1:03d}'
            members = sorted(reachable(u,adj,2))
            rng.shuffle(members)
            local_ids = {v: f'{case_id}-c{k+1:02d}' for k,v in enumerate(members)}
            sheet.append(dict(id=case_id, seed_text=texts[u], query=None,
                              topic=None, split=None, facets=[],
                              candidates=[dict(id=local_ids[v],text=texts[v],grade=None) for v in members],
                              labeller=None,labelled_on=None,publishable=None,note=None))
            key.append(dict(id=case_id,stratum=family,seed=u,
                            nodes={local_ids[v]: v for v in members},
                            directed_edges=[dict(source=local_ids[v],target=local_ids[w])
                                            for v in sorted(members) for w in sorted(adj[v]) if w in local_ids],
                            first_hop=len(adj[u]),second_hop_additions=len(set(members)-reachable(u,adj,1))))
    rng.shuffle(sheet)
    return sheet, dict(session=session,seed=seed,per_family=per_family,
                       pool_counts={f: len(pools[f]) for f in FAMILIES},
                       selected_counts=dict(sorted(Counter(k['stratum'] for k in key).items())),
                       selection='stratified distinct seeds; not a representative query workload',
                       labelling='No inferred relevance. Author questions and grade texts blind to arms; '
                                 'choose topic-disjoint dev/held-out groups before tuning. Pool actual '
                                 'phase-1 and expansion candidates after authoring; these neighborhoods '
                                 'are a starting sheet, not a complete retrieval relevance universe.',
                       snapshot_concepts=report['concepts'],cases=key)


def write_private(path, value):
    fd = os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)
    with os.fdopen(fd,'w') as f:
        json.dump(value,f,indent=2,ensure_ascii=False,allow_nan=False)
        f.write('\n')


def main():
    ap=argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--store',required=True,type=Path)
    ap.add_argument('--session',required=True)
    ap.add_argument('--per-family',type=int,default=20)
    ap.add_argument('--seed',type=int,default=175)
    ap.add_argument('--out',required=True,type=Path)
    ap.add_argument('--key',required=True,type=Path)
    a=ap.parse_args()
    repo=Path(__file__).resolve().parents[2]
    # Neither output may put raw memory in the checkout. Resolve symlinked parents.
    for path in (a.out,a.key):
        if path.resolve().is_relative_to(repo):
            ap.error('raw case sheets and keys must be outside the repository')
    if a.out.resolve() == a.key.resolve() or a.out.exists() or a.key.exists():
        ap.error('sheet/key must be separate new files')
    store=a.store.resolve(strict=True)
    if Path(str(store)+'-wal').exists() or Path(str(store)+'-journal').exists():
        ap.error('use a standalone offline snapshot')
    before=file_hash(store)
    con=sqlite3.connect(store.as_uri()+'?mode=ro&immutable=1',uri=True)
    try:
        sheet,key=prepare(con,a.session,a.per_family,a.seed)
    finally:
        con.close()
    if file_hash(store) != before:
        raise ValueError('snapshot changed during case preparation')
    key['snapshot_sha256']=before
    write_private(a.out,sheet)
    write_private(a.key,key)
    print(json.dumps(dict(unlabelled_cases=len(sheet),selected_counts=key['selected_counts'],
                          pool_counts=key['pool_counts'])))


if __name__=='__main__':
    main()
