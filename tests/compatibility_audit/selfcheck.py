"""工具自身的反例检查；显式执行，不进入默认 pytest 收集。"""

import copy
import json
import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from audit_plugin import altered, install_partition_observation
from compare import compare
from results import key, load, verify
from run import manifest, outside, REPO
from partition_boundaries import checked_target, stable_result


class AuditChecks(unittest.TestCase):
    def test_engine_resource_seed_guard_holds_lock_and_rejects_pending_wal_or_links(self):
        spec=importlib.util.spec_from_file_location('partition_resource_tool',REPO/'benchmarks/partition-visibility/run.py')
        tool=importlib.util.module_from_spec(spec);spec.loader.exec_module(tool)
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp)/'seed.db';collection=root/'collections/c'
            (collection/'wal').mkdir(parents=True);(root/'LOCK').touch()
            (collection/'schema.json').write_text('{}')
            (collection/'manifest.json').write_text(json.dumps({'partitions':{'_default':{'data_files':[],'delta_files':[]}}}))
            with tool.lock_seed(root):
                with self.assertRaises(BlockingIOError):tool.lock_seed(root)
            (collection/'wal/pending.arrow').touch()
            with self.assertRaises(AssertionError):tool.lock_seed(root)
            (collection/'wal/pending.arrow').unlink()
            (collection/'link').symlink_to(Path(tmp))
            with self.assertRaises(AssertionError):tool.lock_seed(root)

    def test_partition_benchmark_never_reuses_a_database_or_writes_in_repo(self):
        spec=importlib.util.spec_from_file_location('partition_benchmark',REPO/'benchmarks/partitions/run.py')
        tool=importlib.util.module_from_spec(spec);spec.loader.exec_module(tool)
        with tempfile.TemporaryDirectory() as tmp:
            root=Path(tmp);target=root/'new.db'
            self.assertEqual(tool.database_path(target),target)
            target.mkdir()
            with self.assertRaises(ValueError):tool.database_path(target)
            with self.assertRaises(ValueError):tool.database_path(REPO/'never-create.db')
            with self.assertRaises(ValueError):tool.database_path(root/'missing/new.db')

    def test_partition_benchmark_readback_preserves_types_duplicates_and_order(self):
        spec=importlib.util.spec_from_file_location('partition_benchmark_readback',REPO/'benchmarks/partitions/run.py')
        tool=importlib.util.module_from_spec(spec);spec.loader.exec_module(tool)
        expected=[{'id':0,'value':{'a':[1,False]}},{'id':1,'value':{'a':[]}}]
        tool.check_rows(expected,expected)
        for bad in [expected[:1],expected[::-1],[expected[0],expected[0]],
                    [{'id':0,'value':{'a':[True,0]}},expected[1]],
                    [{'id':0,'value':{'a':[1.0,False]}},expected[1]]]:
            with self.assertRaises(RuntimeError):tool.check_rows(bad,expected)

    def test_partition_reinsert_truth_preserves_both_delete_states(self):
        cases = json.loads((Path(__file__).parent/'fixtures/partition_visibility_v321.json').read_text())['cases']
        for kind in ['int64', 'varchar']:
            for mode in ['active', 'committed']:
                steps = {step['label']: step for step in cases[kind+'/delete-'+mode]}
                step = steps['reinserted']
                old = next(q['expected'] for q in step['queries'] if q['partitions'] == ['p1'])
                self.assertEqual([r['tag'] for r in old], ['old'] if mode == 'active' else [])
                self.assertEqual(step['state']['active_deletes'], [])
                self.assertEqual(bool(step['state']['committed_deletes']), mode == 'committed')

    def test_partition_fixture_normalizes_only_enumeration_leaf(self):
        value = {'name': '..', 'drop': {'code': 1, 'reason': "[Errno 2] No such file or directory: '/owned/test.db/collections/c/partitions/../indexes'"}}
        result = stable_result(value, Path('/owned'))
        self.assertEqual(result['drop'], {'code': 1, 'reason': '[Errno 2] No such file or directory: <removed collection entry>'})
        for change in [{'name': 'ordinary'}, {'drop': {'code': 6, 'reason': 'unexpected error'}}]:
            other = {**value, **change}
            self.assertEqual(stable_result(other, Path('/owned')), json.loads(json.dumps(other).replace('/owned', '<work>')))

    def test_destructive_partition_probe_confines_paths_and_symlinks(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve() / 'sandbox'
            root.mkdir()
            self.assertTrue(checked_target(root, '../escape').is_relative_to(root))
            self.assertEqual(checked_target(root, str(root/'outside-collection')), root/'outside-collection')
            for name in ['../../../../../escape', str(root.parent/'unowned')]:
                with self.assertRaises(ValueError):
                    checked_target(root, name)
            parts = root/'test.db/collections/c/partitions'
            parts.mkdir(parents=True)
            (parts/'link').symlink_to(root.parent, target_is_directory=True)
            with self.assertRaises(ValueError):
                checked_target(root, 'link')

    def test_identity_lists(self):
        for name, count in (("reference.txt", 760), ("excluded.txt", 139), ("schema.txt", 124),
                            ("upstream-selection.txt", 71), ("upstream-identities.txt", 90), ("probes.txt", 49), ("partitions.txt", 62)):
            self.assertEqual(len(manifest(name)), count)
        self.assertTrue(set(manifest("schema.txt")) <= set(manifest("reference.txt")))
        self.assertTrue(set(manifest("partitions.txt")) <= set(manifest("reference.txt")))
        self.assertFalse(set(manifest("excluded.txt")) & set(manifest("reference.txt")))

    def test_original_preserves_attributes(self):
        value = {"nullable": True, "default_value": False, "dim": 2}
        self.assertEqual(altered(101, value, "original"), value)

    def test_scalar_intervention_preserves_vector(self):
        value = {"nullable": True, "default_value": 0, "dim": 2}
        self.assertEqual(altered(101, value, "scalar-nullable"), value)
        self.assertEqual(altered(5, value, "scalar-nullable"), {"default_value": 0, "dim": 2})

    def test_independent_default_intervention(self):
        self.assertEqual(altered(5, {"nullable": True, "default_value": 0}, "defaults"), {"nullable": True})

    def test_dtype_error_is_left_to_sdk(self):
        self.assertEqual(altered(None, {"nullable": True}, "scalar-nullable"), {"nullable": True})

    def test_partition_key_intervention_preserves_all_other_attributes(self):
        value = {"nullable": True, "default_value": 0, "is_partition_key": True}
        self.assertEqual(altered(5, value, "partition-key"), {"nullable": True, "default_value": 0})
        self.assertEqual(altered(5, value, "original"), value)

    def test_partition_trace_preserves_results_errors_and_arguments(self):
        class Client:
            def insert(self, collection_name, data, **kwargs):
                self.received = (collection_name, data, kwargs)
                if kwargs.get("partition_name") == "bad":
                    raise ValueError("unchanged error")
                return data
        install_partition_observation(Client)
        client = Client()
        data = [{"id": 1}]
        with tempfile.TemporaryDirectory() as tmp, patch.dict(os.environ, {"MLR_AUDIT_EVENTS": str(Path(tmp) / "events")}):
            self.assertIs(client.insert("c", data, partition_name="p1"), data)
            self.assertEqual(client.received, ("c", data, {"partition_name": "p1"}))
            with self.assertRaisesRegex(ValueError, "unchanged error"):
                client.insert("c", data, partition_name="bad")
            events = [json.loads(line) for line in (Path(tmp) / "events").read_text().splitlines()]
            self.assertEqual([e["stage"] for e in events], ["enter", "return", "enter", "error"])
            self.assertEqual(events[0]["parameters"], {"collection_name": "c", "partition_name": "p1"})
            self.assertTrue(all("data" not in e["parameters"] for e in events))

    def test_output_must_be_external(self):
        with self.assertRaises(ValueError):
            outside(REPO / "new-output")

    def test_junit_duplicate_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "x.xml"
            path.write_text('<testsuite><testcase classname="a" name="b"/><testcase classname="a" name="b"/></testsuite>')
            with self.assertRaises(ValueError):
                load(path)

    def test_missing_case_rejected(self):
        with self.assertRaises(ValueError):
            verify({}, ["tests/x.py::C::test_x[a.b]"])
        self.assertEqual(key("tests/x.py::C::test_x[a.b]"), "tests.x.C::test_x[a.b]")

    def test_paired_reference_failure_not_counted_as_success(self):
        case = {"status": "passed", "reason": "passed", "intervened": True, "fields": [], "explicit_null_fields": []}
        baseline = {"mode": "original", "cases": {"x": case}}
        ref = {"mode": "defaults", "backend": "reference", "cases": {"x": case}}
        rust = {"mode": "defaults", "backend": "rust", "cases": {"x": case}}
        self.assertEqual(compare(baseline, ref, rust)["categories"], {"paired_pass_diagnostic_only": 1})
        ref = copy.deepcopy(ref)
        ref["cases"]["x"]["status"] = "failure"
        self.assertEqual(compare(baseline, ref, rust)["categories"], {"reference_no_longer_passes": 1})


if __name__ == "__main__":
    unittest.main()
