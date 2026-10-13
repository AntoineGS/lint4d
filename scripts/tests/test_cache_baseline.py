"""Unit tests for scripts/cache_baseline.py (stdlib only, no package install)."""

import argparse
import contextlib
import importlib.util
import io
import json
import os
import pathlib
import sys
import tempfile
import textwrap
import threading
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "cache_baseline.py"
_spec = importlib.util.spec_from_file_location("cache_baseline", SCRIPT)
baseline = importlib.util.module_from_spec(_spec)
sys.modules["cache_baseline"] = baseline
_spec.loader.exec_module(baseline)

FAKE_CHILD = textwrap.dedent(
    r'''
    import json, os, sys, time

    mode = sys.argv[1]
    log_path = os.environ.get("FAKE_LOG")
    out = sys.stdout.buffer
    inp = sys.stdin.buffer

    def log(text):
        if log_path:
            with open(log_path, "a", encoding="utf-8") as handle:
                handle.write(text + "\n")

    def send(message):
        body = json.dumps(message, ensure_ascii=False).encode("utf-8")
        out.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        out.flush()

    def read():
        length = None
        while True:
            line = inp.readline()
            if not line:
                return None
            if line in (b"\r\n", b"\n"):
                break
            name, _, value = line.decode().partition(":")
            if name.lower() == "content-length":
                length = int(value)
        return json.loads(inp.read(length))

    if mode == "bigstderr":
        chunk = b"x" * 1000 + b"\n"
        for _ in range(5000):
            sys.stderr.buffer.write(chunk)
        sys.stderr.buffer.flush()

    while True:
        message = read()
        if message is None:
            break
        method = message.get("method")
        ident = message.get("id")
        if method:
            log(method)
        if method == "initialize":
            if mode == "garbage":
                out.write(b"this is not a frame\n")
                out.flush()
                continue
            if mode == "oversize":
                out.write(b"Content-Length: 99999999\r\n\r\n")
                out.flush()
                continue
            send({"jsonrpc": "2.0", "id": ident, "result": {
                "capabilities": {},
                "echo": {"rootUri": message["params"]["rootUri"], "argv": sys.argv},
            }})
        elif method == "shutdown":
            send({"jsonrpc": "2.0", "id": ident, "result": None})
        elif method == "exit":
            if os.environ.get("PASCAL_LSP_ANALYSIS_METRICS") == "1" and mode != "nometrics":
                report = {"schema": 1, "counters": {"coreParserInvocations": 3},
                          "saturated": False, "countScope": "process_lifetime",
                          "snapshotConsistency": "independent_atomics",
                          "orderlyShutdown": True, "indexCoverage": None,
                          "storageUsage": None, "decodedPeakBytes": None,
                          "semanticCandidateDocumentsBound": None,
                          "unavailable": {}}
                sys.stderr.write("pascal-lsp analysis-metrics " + json.dumps(report) + "\n")
                sys.stderr.flush()
            break
        elif method == "pascal/testAnalysisMetrics":
            send({"jsonrpc": "2.0", "id": ident, "result": {
                "schema": 1, "counters": {"coreParserInvocations": 1},
                "saturated": False, "countScope": "process_window_endpoint",
                "orderlyShutdown": None}})
        elif method == "textDocument/references":
            if mode == "hang":
                while True:
                    time.sleep(0.05)
            if mode == "apply_edit":
                send({"jsonrpc": "2.0", "id": "srv1", "method": "workspace/applyEdit",
                      "params": {"edit": {"changes": {}}}})
                reply = read()
                log("applyEdit-reply " + json.dumps(reply.get("result")))
            send({"jsonrpc": "2.0", "id": ident, "result": []})
        elif ident is not None:
            send({"jsonrpc": "2.0", "id": ident, "result": None})
    '''
)

WRAPPER = textwrap.dedent(
    r'''
    import subprocess, sys
    pid_file, child = sys.argv[1], sys.argv[2:]
    proc = subprocess.Popen([sys.executable] + child)
    with open(pid_file, "w") as handle:
        handle.write(str(proc.pid))
    proc.wait()
    '''
)


def pid_gone(pid, event):
    for _ in range(200):
        try:
            with open(f"/proc/{pid}/stat", "r", encoding="ascii") as handle:
                state = handle.read().rsplit(")", 1)[1].split()[0]
        except FileNotFoundError:
            return True
        if state == "Z":
            return True
        event.wait(0.05)
    return False


class TempCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="cb tést ")
        self.addCleanup(self._tmp.cleanup)
        self.tmp = pathlib.Path(self._tmp.name)

    def fake(self, mode, **kwargs):
        script = self.tmp / "fake_child.py"
        script.write_text(FAKE_CHILD, encoding="utf-8")
        self.log_path = self.tmp / "fake.log"
        os.environ["FAKE_LOG"] = str(self.log_path)
        self.addCleanup(os.environ.pop, "FAKE_LOG", None)
        kwargs.setdefault("timeout_seconds", 10)
        kwargs.setdefault("profiling", False)
        return baseline.LspSession(
            [sys.executable, str(script), mode], **kwargs
        )

    def logged(self):
        if not self.log_path.exists():
            return []
        return self.log_path.read_text(encoding="utf-8").splitlines()


class FramingTests(unittest.TestCase):
    def test_frame_length_uses_utf8_bytes(self):
        message = {"jsonrpc": "2.0", "id": "é", "result": "界"}
        framed = baseline.encode_frame(message)
        header, body = framed.split(b"\r\n\r\n", 1)
        self.assertEqual(int(header.split(b":", 1)[1]), len(body))
        self.assertEqual(baseline.read_frame(io.BytesIO(framed)), message)

    def test_oversized_frame_is_rejected(self):
        framed = b"Content-Length: 100\r\n\r\n" + b"x" * 100
        with self.assertRaises(baseline.FrameError):
            baseline.read_frame(io.BytesIO(framed), max_bytes=10)
        with self.assertRaises(baseline.FrameError):
            baseline.read_frame(io.BytesIO(b"X-Pad: " + b"a" * 9000 + b"\r\n\r\n"))

    def test_malformed_frames_are_rejected(self):
        for raw in (
            b"Content-Length: 5\r\n\r\nab",
            b"Content-Length: -1\r\n\r\n",
            b"Content-Length: 1\r\nContent-Length: 1\r\n\r\n{",
            b"Content-Length: 4\r\n\r\nnope",
            b"Content-Length: 2\r\n\r\n[]",
            b"hello\r\n\r\n",
            b"Content-Length: 5\r\n",
        ):
            with self.assertRaises(baseline.FrameError, msg=raw):
                baseline.read_frame(io.BytesIO(raw))

    def test_clean_eof_is_none(self):
        self.assertIsNone(baseline.read_frame(io.BytesIO(b"")))


class FixtureTests(TempCase):
    def make(self, **kwargs):
        root = self.tmp / "fixture é root"
        return root, baseline.write_fixture(root, **kwargs)

    def test_default_fixture_shape(self):
        root, info = self.make()
        self.assertEqual(len(list(root.glob("*.pas"))), 500)
        consumers = [
            path
            for path in root.glob("*.pas")
            if "uses Provider;" in path.read_text(encoding="utf-8")
        ]
        self.assertEqual(len(consumers), 5)
        for path in consumers:
            text = path.read_text(encoding="utf-8")
            self.assertIn("Log(SharedValue);", text)
            self.assertIn("Log(Provider.SharedValue);", text)
        self.assertEqual(
            (root / "Root.dpr").read_text(encoding="utf-8").split(),
            "program Baseline; uses Provider; begin end.".split(),
        )
        self.assertTrue((root / "A.dproj").exists() and (root / "B.dproj").exists())
        self.assertIn("BASELINE_A", (root / "A.dproj").read_text(encoding="utf-8"))
        self.assertIn("BASELINE_B", (root / "B.dproj").read_text(encoding="utf-8"))
        self.assertEqual(len(info["expectedLocations"]), 10)
        provider = (root / "Provider.pas").read_text(encoding="utf-8")
        line = provider.splitlines()[info["query"]["line"]]
        at = info["query"]["character"]
        self.assertEqual(line[at : at + len("SharedValue")], "SharedValue")

    def test_expected_locations_point_at_generated_text(self):
        root, info = self.make(units=12, importers=3)
        self.assertEqual(len(info["expectedLocations"]), 6)
        for location in info["expectedLocations"]:
            path = baseline.uri_to_path(location["uri"])
            self.assertEqual(path.parent, root)
            lines = path.read_text(encoding="utf-8").splitlines()
            start, end = location["range"]["start"], location["range"]["end"]
            self.assertEqual(start["line"], end["line"])
            self.assertEqual(
                lines[start["line"]][start["character"] : end["character"]],
                "SharedValue",
            )

    def test_fixture_rejects_bad_counts_before_writing(self):
        for units, importers in ((10, 9), (10, 8 + 1), (1, 0), (10, 0), (10**6, 5)):
            root = self.tmp / f"bad-{units}-{importers}"
            with self.assertRaises(ValueError):
                baseline.write_fixture(root, units=units, importers=importers)
            self.assertFalse(root.exists())

    def test_fixture_refuses_non_empty_directory(self):
        root = self.tmp / "occupied"
        root.mkdir()
        (root / "keep.txt").write_text("x", encoding="utf-8")
        with self.assertRaises(ValueError):
            baseline.write_fixture(root, units=10, importers=2)

    def test_fixture_digest_ignores_temporary_parent(self):
        first = self.tmp / "one"
        second = self.tmp / "two é" / "nested"
        a = baseline.write_fixture(first, units=20, importers=2)
        b = baseline.write_fixture(second, units=20, importers=2)
        self.assertEqual(baseline.fixture_digest(first), baseline.fixture_digest(second))
        self.assertEqual(a["digest"], b["digest"])
        (second / "Provider.pas").write_text("changed", encoding="utf-8")
        self.assertNotEqual(
            baseline.fixture_digest(first), baseline.fixture_digest(second)
        )

    def test_include_variant_positions_come_from_actual_source(self):
        root, info = self.make(units=10, importers=2, with_include=True)
        self.assertIn("IncludeValue", (root / "Baseline.inc").read_text(encoding="utf-8"))
        provider = (root / "Provider.pas").read_text(encoding="utf-8")
        self.assertIn("{$I Baseline.inc}", provider)
        line = provider.splitlines()[info["query"]["line"]]
        at = info["query"]["character"]
        self.assertEqual(line[at : at + 11], "SharedValue")
        self.assertGreater(provider.index("SharedValue"), provider.index("{$I"))


class NormalizationTests(TempCase):
    def test_root_is_normalized_and_externals_stay_distinct(self):
        root = self.tmp / "my root é"
        root.mkdir()
        rng = {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}}
        inside = baseline.path_to_uri(root / "A.pas")
        other = self.tmp / "other"
        other.mkdir()
        outside = baseline.path_to_uri(other / "A.pas")
        locations = [
            {"uri": outside, "range": rng},
            {"uri": inside, "range": rng},
            {"uri": inside, "range": rng},
        ]
        normalized = baseline.normalize_locations(locations, root)
        self.assertEqual(len(normalized), 3)
        self.assertTrue(any(item["uri"] == "fixture:A.pas" for item in normalized))
        self.assertTrue(any(item["uri"].startswith("external:") for item in normalized))

    def test_fingerprint_is_parent_independent_and_order_independent(self):
        rng = {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}}
        rng2 = {"start": {"line": 3, "character": 2}, "end": {"line": 3, "character": 5}}
        one, two = self.tmp / "p1", self.tmp / "p2 é"
        one.mkdir()
        two.mkdir()
        locs = lambda r: [
            {"uri": baseline.path_to_uri(r / "A.pas"), "range": rng},
            {"uri": baseline.path_to_uri(r / "A.pas"), "range": rng2},
        ]
        self.assertEqual(
            baseline.result_fingerprint(locs(one), one),
            baseline.result_fingerprint(list(reversed(locs(two))), two),
        )
        self.assertNotEqual(
            baseline.result_fingerprint(locs(one)[:1], one),
            baseline.result_fingerprint(locs(one), one),
        )

    def test_unicode_root_survives_argv_and_uri_round_trip(self):
        root = self.tmp / "root with space é"
        root.mkdir()
        uri = baseline.path_to_uri(root)
        self.assertEqual(baseline.uri_to_path(uri), root)
        session = self.fake("ok")
        result = session.initialize(root, {})
        self.assertEqual(result["echo"]["rootUri"], uri)
        self.assertEqual(baseline.uri_to_path(result["echo"]["rootUri"]), root)
        info = session.close()
        self.assertTrue(info["orderlyShutdown"])


class SampleTests(unittest.TestCase):
    def good(self):
        return baseline.make_sample(
            scenario="warm",
            index=0,
            variant=None,
            fixture={"digest": "d" * 64, "units": 10, "importers": 2},
            binary={"path": "x", "sha256": "0" * 64, "sizeBytes": 1,
                    "testServer": False, "profiling": False},
            input_info={"kind": "synthetic", "label": "t"},
            elapsed_ns=1234,
            outcome="ok",
            reason=None,
            result={"count": 4, "fingerprint": "f" * 64,
                    "expectedCount": 4, "expectedFingerprint": "f" * 64},
            counters=baseline.unavailable("not_profiled"),
            resources=baseline.unavailable("gnu_time_not_selected"),
            protocol_error=None,
        )

    def test_good_sample_validates(self):
        baseline.validate_sample(self.good())

    def test_unavailable_requires_reason(self):
        with self.assertRaises(ValueError):
            baseline.unavailable("")
        entry = baseline.unavailable("because")
        self.assertEqual(entry, {"value": None, "available": False, "reason": "because"})

    def test_missing_rss_is_null_not_zero(self):
        entry = baseline.parse_time_report(None)
        self.assertFalse(entry["available"])
        self.assertIsNone(entry["value"])
        self.assertIsNone(baseline.parse_time_report("garbage")["value"])
        ok = baseline.parse_time_report("2048 8 16 0.5 0.25\n")
        self.assertEqual(ok["value"]["maxRssKb"], 2048)
        self.assertEqual(ok["scope"], "process_lifetime")

    def test_validation_rejects_bad_samples(self):
        sample = self.good()
        sample["requestNanos"] = -1
        with self.assertRaises(ValueError):
            baseline.validate_sample(sample)
        sample = self.good()
        sample["schema"] = 2
        with self.assertRaises(ValueError):
            baseline.validate_sample(sample)
        sample = self.good()
        sample["protocolError"] = {"code": -32603}
        with self.assertRaises(ValueError):
            baseline.validate_sample(sample)
        sample = self.good()
        sample["counters"] = {"available": False, "value": None}
        with self.assertRaises(ValueError):
            baseline.validate_sample(sample)
        sample = self.good()
        del sample["coverage"]
        with self.assertRaises(ValueError):
            baseline.validate_sample(sample)
        sample = self.good()
        sample["releaseGateStatus"] = "passed"
        with self.assertRaises(ValueError):
            baseline.validate_sample(sample)

    def test_failed_response_is_not_a_successful_timing(self):
        root = pathlib.Path("/nonexistent-root")
        expected = [{"uri": "file:///nonexistent-root/A.pas",
                     "range": {"start": {"line": 0, "character": 0},
                               "end": {"line": 0, "character": 1}}}]
        error = {"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "boom"}}
        outcome, reason, result, protocol_error = baseline.evaluate_response(
            error, expected, root
        )
        self.assertEqual(outcome, "invalid")
        self.assertEqual(protocol_error["code"], -32603)
        wrong = {"jsonrpc": "2.0", "id": 1, "result": []}
        outcome, reason, result, protocol_error = baseline.evaluate_response(
            wrong, expected, root
        )
        self.assertEqual(outcome, "invalid")
        self.assertIn("mismatch", reason)
        right = {"jsonrpc": "2.0", "id": 1, "result": expected}
        outcome, *_ = baseline.evaluate_response(right, expected, root)
        self.assertEqual(outcome, "ok")
        sample = baseline.make_sample(
            scenario="warm", index=0, variant=None,
            fixture={"digest": "d"}, binary={"path": "x"},
            input_info={"kind": "synthetic"}, elapsed_ns=999, outcome="invalid",
            reason="boom", result=None,
            counters=baseline.unavailable("a"), resources=baseline.unavailable("b"),
            protocol_error={"code": -32603},
        )
        self.assertIsNone(sample["requestNanos"])
        self.assertEqual(sample["outcome"], "invalid")
        baseline.validate_sample(sample)

    def test_process_windows_are_not_request_exclusive(self):
        before = {"schema": 1, "counters": {"coreParserInvocations": 2, "projectStampProbes": 1}}
        after = {"schema": 1, "counters": {"coreParserInvocations": 5, "projectStampProbes": 1}}
        window = baseline.counter_window(before, after)
        self.assertEqual(window["scope"], "process_window_endpoint")
        self.assertIs(window["requestExclusive"], False)
        self.assertEqual(window["delta"]["coreParserInvocations"], 3)
        self.assertEqual(window["before"], before)
        self.assertEqual(window["after"], after)
        self.assertNotIn("reset", json.dumps(window).lower())

    def test_ok_sample_needs_real_result(self):
        for result in (
            {},
            None,
            {"count": None, "fingerprint": None, "expectedCount": None,
             "expectedFingerprint": None},
            {"count": 3, "fingerprint": "f" * 64, "expectedCount": 4,
             "expectedFingerprint": "f" * 64},
            {"count": 4, "fingerprint": "f" * 64, "expectedCount": 4,
             "expectedFingerprint": "e" * 64},
        ):
            sample = self.good()
            sample["result"] = result
            with self.assertRaises(ValueError, msg=result):
                baseline.validate_sample(sample)

    def with_counters(self, snapshot, scope="lifetime"):
        sample = self.good()
        sample["counters"] = baseline.available({scope: snapshot})
        return sample

    @staticmethod
    def snapshot(saturated=False, **overrides):
        counters = {"coreParserInvocations": 3, "projectStampProbes": 1}
        counters.update(overrides)
        return {"schema": 1, "counters": counters, "saturated": saturated,
                "countScope": "process_lifetime", "orderlyShutdown": True}

    def test_saturated_snapshot_cannot_satisfy_parse_gate(self):
        clean = self.with_counters(self.snapshot())
        baseline.validate_sample(clean)
        self.assertTrue(baseline.gate_status(clean, "parse_count")["satisfied"])
        saturated = self.with_counters(self.snapshot(saturated=True))
        baseline.validate_sample(saturated)  # a valid timing sample...
        status = baseline.gate_status(saturated, "parse_count")
        self.assertFalse(status["satisfied"])  # ...that cannot pass the gate
        self.assertIn("saturated", status["reason"])
        for bad in (
            self.snapshot(coreParserInvocations=-1),
            self.snapshot(saturated="no"),
            {"schema": 2, "counters": {}, "saturated": False},
        ):
            with self.assertRaises(ValueError, msg=bad):
                baseline.validate_sample(self.with_counters(bad))

    def test_unknown_coverage_cannot_satisfy_complete_query_gate(self):
        sample = self.good()
        baseline.validate_sample(sample)
        self.assertFalse(sample["coverage"]["available"])
        status = baseline.gate_status(sample, "complete_query")
        self.assertFalse(status["satisfied"])
        self.assertIn("coverage", status["reason"])
        sample["coverage"] = baseline.available({"complete": True})
        self.assertTrue(baseline.gate_status(sample, "complete_query")["satisfied"])
        invalid = self.good()
        invalid.update(outcome="invalid", requestNanos=None, reason="boom")
        invalid["coverage"] = baseline.available({"complete": True})
        self.assertFalse(baseline.gate_status(invalid, "complete_query")["satisfied"])

    def test_unimplemented_storage_is_unavailable(self):
        sample = self.good()
        self.assertEqual(sample["storage"]["reason"], "persistence_not_implemented")
        self.assertIsNone(sample["storage"]["value"])
        self.assertFalse(baseline.gate_status(sample, "storage")["satisfied"])
        zero = self.good()
        zero["storage"] = {"value": 0, "available": False, "reason": "x"}
        with self.assertRaises(ValueError):
            baseline.validate_sample(zero)
        with self.assertRaises(ValueError):
            baseline.gate_status(sample, "no_such_gate")

    def test_sample_records_fixture_binary_and_counter_scope(self):
        sample = self.with_counters(self.snapshot())
        self.assertEqual(sample["fixture"]["digest"], "d" * 64)
        self.assertEqual(sample["binary"]["sha256"], "0" * 64)
        self.assertIn("profiling", sample["binary"])
        self.assertIn("testServer", sample["binary"])
        self.assertEqual(sample["input"]["kind"], "synthetic")
        self.assertEqual(sample["counters"]["value"]["lifetime"]["countScope"],
                         "process_lifetime")
        # counters without a recorded scope cannot be interpreted
        snapshot = self.snapshot()
        del snapshot["countScope"]
        with self.assertRaises(ValueError):
            baseline.validate_sample(self.with_counters(snapshot))
        for field in ("fixture", "binary", "input"):
            broken = self.good()
            broken[field] = None
            with self.assertRaises(ValueError, msg=field):
                baseline.validate_sample(broken)

    def test_write_sample_is_one_bounded_json_line(self):
        stream = io.StringIO()
        baseline.write_sample(stream, self.good())
        line = stream.getvalue()
        self.assertTrue(line.endswith("\n") and line.count("\n") == 1)
        self.assertEqual(json.loads(line)["schema"], 1)
        big = self.good()
        big["reason"] = "x" * 100000
        with self.assertRaises(ValueError):
            baseline.write_sample(io.StringIO(), big)


class OutputTests(TempCase):
    def test_output_existing_file_is_not_overwritten(self):
        path = self.tmp / "out.jsonl"
        path.write_text("keep\n", encoding="utf-8")
        with self.assertRaises(FileExistsError):
            baseline.open_output(path, append=False)
        self.assertEqual(path.read_text(encoding="utf-8"), "keep\n")
        with baseline.open_output(path, append=True) as stream:
            stream.write("more\n")
        self.assertEqual(path.read_text(encoding="utf-8"), "keep\nmore\n")
        with baseline.open_output(self.tmp / "new.jsonl", append=False) as stream:
            stream.write("x")

    def test_manifest_is_checked(self):
        good = {
            "schema": 1, "label": "prod", "root": str(self.tmp),
            "targetUri": baseline.path_to_uri(self.tmp / "a.pas"),
            "line": 1, "character": 2, "initializationOptions": {},
            "fixture": {"revision": "r1"}, "query": {"name": "q"},
        }
        path = self.tmp / "m.json"
        path.write_text(json.dumps(good), encoding="utf-8")
        self.assertEqual(baseline.load_manifest(path)["label"], "prod")
        for mutate in (
            lambda m: m.update(schema=2),
            lambda m: m.pop("targetUri"),
            lambda m: m.update(line=-1),
            lambda m: m.update(initializationOptions=[]),
        ):
            bad = dict(good)
            mutate(bad)
            path.write_text(json.dumps(bad), encoding="utf-8")
            with self.assertRaises(ValueError):
                baseline.load_manifest(path)
        path.write_text("x" * (1 << 20), encoding="utf-8")
        with self.assertRaises(ValueError):
            baseline.load_manifest(path)


class SessionTests(TempCase):
    def test_large_stderr_is_drained_without_deadlock(self):
        session = self.fake("bigstderr")
        session.initialize(self.tmp, {})
        response, elapsed = session.request("textDocument/references", {})
        self.assertEqual(response["result"], [])
        self.assertGreaterEqual(elapsed, 0)
        info = session.close()
        self.assertLessEqual(len(info["stderrLines"]), 64)
        self.assertTrue(all(len(line) <= 512 for line in info["stderrLines"]))

    def test_nonprotocol_stdout_invalidates_run(self):
        session = self.fake("garbage")
        with self.assertRaises(baseline.RunInvalid):
            session.initialize(self.tmp, {})
        info = session.close()
        self.assertFalse(info["orderlyShutdown"])

    def test_oversized_server_frame_invalidates_run(self):
        session = self.fake("oversize")
        with self.assertRaises(baseline.RunInvalid):
            session.initialize(self.tmp, {})
        session.close()

    def test_child_timeout_reaps_process(self):
        session = self.fake("hang", timeout_seconds=0.5)
        session.initialize(self.tmp, {})
        with self.assertRaises(baseline.RunTimeout):
            session.request("textDocument/references", {})
        self.assertIsNotNone(session.returncode())
        info = session.close()
        self.assertFalse(info["orderlyShutdown"])

    @unittest.skipUnless(
        sys.platform.startswith("linux") and hasattr(os, "killpg"),
        "needs /proc and POSIX process groups to observe the grandchild",
    )
    def test_wrapper_timeout_reaps_whole_group(self):
        script = self.tmp / "fake_child.py"
        script.write_text(FAKE_CHILD, encoding="utf-8")
        wrapper = self.tmp / "wrapper.py"
        wrapper.write_text(WRAPPER, encoding="utf-8")
        pid_file = self.tmp / "grandchild.pid"
        session = baseline.LspSession(
            [sys.executable, str(wrapper), str(pid_file), str(script), "hang"],
            timeout_seconds=0.5,
            profiling=False,
            new_session=True,
        )
        session.initialize(self.tmp, {})
        with self.assertRaises(baseline.RunTimeout):
            session.request("textDocument/references", {})
        grandchild = int(pid_file.read_text(encoding="ascii"))
        self.assertTrue(pid_gone(grandchild, threading.Event()))
        session.close()

    @unittest.skipUnless(hasattr(os, "killpg"), "POSIX process groups only")
    def test_group_is_signalled_only_while_leader_is_unreaped(self):
        calls = []
        real = os.killpg

        def checked(pid, sig):
            calls.append((pid, sig, session._proc.returncode))
            return real(pid, sig)

        session = self.fake("ok", new_session=True)
        session.initialize(self.tmp, {})
        with mock.patch.object(os, "killpg", checked):
            session.close()
            self.assertTrue(calls)
            self.assertTrue(all(code is None for _, _, code in calls), calls)
            calls.clear()
            session._signal(True)
            session._signal(False)
        self.assertEqual(calls, [])

    def test_unexpected_apply_edit_is_rejected(self):
        session = self.fake("apply_edit")
        session.initialize(self.tmp, {})
        response, _ = session.request("textDocument/references", {})
        self.assertEqual(response["result"], [])
        info = session.close()
        reply = [line for line in self.logged() if line.startswith("applyEdit-reply")]
        self.assertEqual(len(reply), 1)
        self.assertIn('"applied": false', reply[0])
        self.assertEqual(len(info["violations"]), 1)

    def test_metrics_line_is_captured_on_orderly_exit(self):
        session = self.fake("ok", profiling=True)
        session.initialize(self.tmp, {})
        info = session.close()
        self.assertEqual(info["metrics"]["countScope"], "process_lifetime")
        self.assertEqual(info["metrics"]["counters"]["coreParserInvocations"], 3)

    def test_absent_metrics_line_is_unavailable_not_failure(self):
        session = self.fake("nometrics", profiling=True)
        session.initialize(self.tmp, {})
        info = session.close()
        self.assertIsNone(info["metrics"])
        self.assertTrue(info["orderlyShutdown"])

    def test_snapshot_requires_profiling_and_explicit_test_server(self):
        for profiling, test_server in ((False, False), (True, False), (False, True)):
            session = self.fake("ok", profiling=profiling, test_server=test_server)
            session.initialize(self.tmp, {})
            entry = session.snapshot()
            self.assertFalse(entry["available"])
            session.close()
            self.assertNotIn("pascal/testAnalysisMetrics", self.logged())
            self.log_path.unlink()
        session = self.fake("ok", profiling=True, test_server=True)
        session.initialize(self.tmp, {})
        entry = session.snapshot()
        self.assertTrue(entry["available"])
        self.assertEqual(entry["value"]["countScope"], "process_window_endpoint")
        session.close()
        self.assertIn("pascal/testAnalysisMetrics", self.logged())

    def test_profiling_env_follows_flag(self):
        os.environ["PASCAL_LSP_ANALYSIS_METRICS"] = "1"
        self.addCleanup(os.environ.pop, "PASCAL_LSP_ANALYSIS_METRICS", None)
        session = self.fake("ok", profiling=False)
        session.initialize(self.tmp, {})
        info = session.close()
        self.assertIsNone(info["metrics"])


class ScenarioCleanupTests(TempCase):
    def test_two_sessions_closes_first_when_second_start_fails(self):
        root = self.tmp / "subject"
        root.mkdir()
        target = root / "a.pas"
        target.write_text("unit a;", encoding="utf-8")
        subject = baseline.Subject(root, {}, target, 0, 0, [], {"digest": "d"},
                                   {"kind": "synthetic", "label": "t"})
        args = argparse.Namespace(samples=1, timeout_seconds=5, profiling="off")
        ctx = baseline.Context(args, {"path": "x"})
        modes = iter(["ok", "garbage"])
        opened = []

        def open_session():
            session = self.fake(next(modes))
            opened.append(session)
            return session

        ctx.open_session = open_session
        samples = baseline.scenario_two_sessions(
            ctx, lambda: baseline._SubjectScope(lambda _tmp: subject)
        )
        self.assertEqual(len(opened), 2)
        self.assertEqual([s["outcome"] for s in samples], ["invalid"])
        for session in opened:
            self.assertIsNotNone(session.returncode())


class CliTests(TempCase):
    def test_main_refuses_existing_output_and_missing_binary(self):
        out = self.tmp / "o.jsonl"
        out.write_text("keep", encoding="utf-8")
        with contextlib.redirect_stderr(io.StringIO()):
            code = baseline.main(
                ["--binary", sys.executable, "--output", str(out), "--scenario", "warm"]
            )
        self.assertNotEqual(code, 0)
        self.assertEqual(out.read_text(encoding="utf-8"), "keep")

    def test_test_server_requires_profiling(self):
        with contextlib.redirect_stderr(io.StringIO()):
            code = baseline.main(
                ["--binary", sys.executable, "--output", str(self.tmp / "n.jsonl"),
                 "--scenario", "warm", "--test-server", "--profiling", "off"]
            )
        self.assertNotEqual(code, 0)

    def test_manifest_scenarios_are_read_only_subset(self):
        with self.assertRaises(ValueError):
            baseline.check_scenarios_for_manifest(["warm", "edit"])
        baseline.check_scenarios_for_manifest(["cold", "warm", "restart", "two_sessions"])


if __name__ == "__main__":
    unittest.main()
