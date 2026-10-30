"""Private sampling must not turn adjacency into relevance or leak topology to labels."""
from pathlib import Path
import stat
import sys
import tempfile
import unittest

HERE=Path(__file__).resolve().parent
sys.path.insert(0,str(HERE))
import graph_cases as cases
from test_graph_census import fixture


class Cases(unittest.TestCase):
    def test_sampling_is_deterministic_unique_and_unlabelled(self):
        con=fixture()
        self.addCleanup(con.close)
        con.execute('ALTER TABLE concepts ADD COLUMN content TEXT')
        con.execute("UPDATE concepts SET content='public source #175 ' || id WHERE session_id='public'")
        con.execute("UPDATE concepts SET content='PRIVATE TEXT' WHERE session_id='other'")
        a,key=cases.prepare(con,'public',1,175)
        self.assertEqual((a,key),cases.prepare(con,'public',1,175))
        seeds=[x['seed'] for x in key['cases']]
        self.assertEqual(len(seeds),len(set(seeds)))
        self.assertIn('two_hop',key['selected_counts'])
        self.assertIn('sparse',key['selected_counts'])
        for item in a:
            self.assertIsNone(item['query'])
            self.assertIsNone(item['split'])
            self.assertIsNone(item['publishable'])
            self.assertNotIn('stratum',item)
            for c in item['candidates']:
                self.assertIsNone(c['grade'])
                self.assertNotIn('PRIVATE',c['text'])
                self.assertEqual(set(c),{'id','text','grade'})

    def test_private_outputs_are_exclusive_and_mode_600(self):
        with tempfile.TemporaryDirectory() as tmp:
            out=Path(tmp)/'private.json'
            cases.write_private(out,{'example':'raw memory'})
            self.assertEqual(stat.S_IMODE(out.stat().st_mode),0o600)
            with self.assertRaises(FileExistsError):
                cases.write_private(out,{'replacement':True})
            self.assertIn('raw memory',out.read_text())


if __name__=='__main__':
    unittest.main()
