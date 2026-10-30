"""#175 census invariants: directed topology, session isolation and safe outputs.

    python3 -m unittest discover -s scripts/eval -p test_graph_census.py
"""
from collections import defaultdict
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import graph_census as g


def fixture():
    con = sqlite3.connect(':memory:')
    con.executescript('''
        CREATE TABLE sessions(session_id TEXT PRIMARY KEY, embedding_kind TEXT,
          embedding_model TEXT, embedding_dim INTEGER, mutation_epoch INTEGER);
        CREATE TABLE concepts(id TEXT PRIMARY KEY,session_id TEXT,concept_type TEXT,
          embedding BLOB,chunk_group_id TEXT,origin_interaction TEXT,created_at TEXT);
        CREATE TABLE interactions(id TEXT PRIMARY KEY,session_id TEXT);
        CREATE TABLE edges(id INTEGER PRIMARY KEY,session_id TEXT,source TEXT,
          target TEXT,edge_type TEXT,weight REAL,reinforcements INTEGER);
        INSERT INTO sessions VALUES('public','candle','model',1024,7);
        INSERT INTO sessions VALUES('other','fixture','private',4,9);
        INSERT INTO interactions VALUES('i-public','public');
        INSERT INTO interactions VALUES('i-other','other');
    ''')
    for node in ['a','b','c','d','isolated']:
        con.execute("INSERT INTO concepts VALUES(?,'public','Logic',NULL,NULL,'i-public','2026-01-01')", (node,))
    # a -> b -> c -> b is directed. d -> a. No reverse d edge is invented.
    for u,v,t in [('a','b','Dependency'),('a','b','Causal'),('b','c','CoOccurrence'),
                  ('c','b','Semantic'),('d','a','Hierarchical'),
                  ('i-public','a','Derives'),('a','isolated','Contradicts')]:
        con.execute("INSERT INTO edges(session_id,source,target,edge_type,weight,reinforcements) VALUES('public',?,?,?,0.5,1)", (u,v,t))
    con.execute("INSERT INTO concepts VALUES('private-node','other','Resource',NULL,NULL,'i-other','2026-01-01')")
    return con


class Topology(unittest.TestCase):
    def test_directed_depth_and_cycles(self):
        adj={'a':{'b'},'b':{'c'},'c':{'b'},'d':{'a'}}
        self.assertEqual(g.reachable('a',adj,0),{'a'})
        self.assertEqual(g.reachable('a',adj,1),{'a','b'})
        self.assertEqual(g.reachable('a',adj,2),{'a','b','c'})
        self.assertEqual(g.reachable('a',adj,99),{'a','b','c'})
        self.assertEqual(g.reachable('isolated',adj,2),{'isolated'})
        self.assertEqual(g.reachable('d',adj,2),{'d','a','b'})
        self.assertNotIn('d',g.reachable('a',adj,99))

    def test_scc_against_mutual_reachability_on_all_three_node_graphs(self):
        nodes=['a','b','c']
        pairs=[(u,v) for u in nodes for v in nodes if u!=v]
        for mask in range(1 << len(pairs)):
            out,inc=defaultdict(set),defaultdict(set)
            for bit,(u,v) in enumerate(pairs):
                if mask & (1 << bit):
                    out[u].add(v); inc[v].add(u)
            classes={frozenset(v for v in nodes if v in g.reachable(u,out,3)
                               and u in g.reachable(v,out,3)) for u in nodes}
            self.assertEqual(g.strong_component_sizes(nodes,out,inc),
                             sorted(map(len,classes),reverse=True))

    def test_type_filter_dedup_session_isolation_and_summary(self):
        con=fixture()
        self.addCleanup(con.close)
        r=g.census(con,'public')
        self.assertEqual(r['concepts'],5)
        self.assertEqual(r['interactions'],1)
        self.assertEqual(r['edges']['all'],7)
        self.assertEqual(r['edges']['traversable_rows'],5)
        self.assertEqual(r['edges']['distinct_directed_pairs'],4)
        self.assertEqual(r['components']['weak_largest_10'],[4,1])
        self.assertEqual(r['components']['strong_largest_10'],[2,1,1,1])
        self.assertEqual(r['single_seed']['depth2_graph_only']['zero'],1)
        self.assertEqual(r['single_seed']['seeds_with_second_hop_additions'],2)
        self.assertEqual(r['single_seed']['seeds_with_multiple_second_hop_paths'],0)
        self.assertEqual(r['quality']['missing_origin_interactions'],0)
        # A larger unrelated session cannot change selected-session results.
        for k in range(20):
            con.execute("INSERT INTO concepts VALUES(?,'other','Logic',NULL,NULL,'i-other','2026-01-01')", (f'private-{k}',))
        self.assertEqual(r,g.census(con,'public'))
        self.assertNotIn('private',json.dumps(r))
        def leaves(value):
            if isinstance(value,dict):
                return [x for v in value.values() for x in leaves(v)]
            if isinstance(value,list):
                return [x for v in value for x in leaves(v)]
            return [value]
        self.assertNotIn('isolated',leaves(r))  # IDs never become report values

    def test_refuse_missing_session_or_broken_endpoints(self):
        con=fixture()
        self.addCleanup(con.close)
        with self.assertRaisesRegex(ValueError,'does not exist'):
            g.census(con,'missing')
        con.execute("INSERT INTO edges VALUES(99,'public','a','private-node','Dependency',0.5,1)")
        with self.assertRaisesRegex(ValueError,'cross-session'):
            g.census(con,'public')

    def test_refuse_traversable_interaction_edge(self):
        con=fixture()
        self.addCleanup(con.close)
        con.execute("INSERT INTO edges VALUES(99,'public','i-public','a','Causal',0.5,1)")
        with self.assertRaisesRegex(ValueError,'outside concept graph'):
            g.census(con,'public')

    def test_empty_graph_quantiles_and_edgeless_session(self):
        self.assertIsNone(g.distribution([])['p50'])
        con=fixture()
        self.addCleanup(con.close)
        con.execute("DELETE FROM edges")
        r=g.census(con,'public')
        self.assertEqual(r['single_seed']['depth2_graph_only']['zero'],5)
        self.assertEqual(r['hubs']['count'],0)
        self.assertEqual(r['components']['weak_largest_10'],[1]*5)

    def test_traversal_types_match_production(self):
        import re
        source=(HERE.parent.parent/'src/recall/expand.rs').read_text()
        declared=source.split('pub const TRAVERSAL_ORDER:',1)[1].split('];',1)[0]
        self.assertEqual(tuple(re.findall(r'EdgeType::(\w+)',declared)),g.TRAVERSAL_ORDER)


class Command(unittest.TestCase):
    def test_read_only_deterministic_refuses_overwrites_and_sidecars(self):
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)
            store=root/'copy.db'
            src=fixture(); dest=sqlite3.connect(store)
            src.commit(); src.backup(dest); src.close(); dest.close()
            before=store.read_bytes()
            args=[sys.executable,str(HERE/'graph_census.py'),'--store',str(store),
                  '--session','public','--revision','fixture']
            for name in ['a.json','b.json']:
                subprocess.run(args+['--out',str(root/name)],check=True,capture_output=True)
            self.assertEqual((root/'a.json').read_bytes(),(root/'b.json').read_bytes())
            self.assertEqual(before,store.read_bytes())
            failed=subprocess.run(args+['--out',str(store)],capture_output=True)
            self.assertNotEqual(failed.returncode,0)
            self.assertEqual(before,store.read_bytes())
            (root/'copy.db-wal').touch()
            failed=subprocess.run(args+['--out',str(root/'refused.json')],capture_output=True)
            self.assertNotEqual(failed.returncode,0)
            self.assertFalse((root/'refused.json').exists())


if __name__=='__main__':
    unittest.main()
