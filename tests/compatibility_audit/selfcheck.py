"""工具自身的反例检查；显式执行，不进入默认 pytest 收集。"""

import copy
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from audit_plugin import altered, install_partition_observation
from compare import compare
from results import key, load, verify
from run import manifest, outside, REPO


class AuditChecks(unittest.TestCase):
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
