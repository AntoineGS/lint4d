#!/usr/bin/env python3
"""Reproducible stdlib benchmark runner for pascal-lsp cache baselines.

Drives an ordinary ``pascal-lsp --stdio`` binary over JSON-RPC against an owned
synthetic fixture (or a read-only user manifest) and writes schema-1 JSON-lines
samples. Phase 1 is measurement only: nothing here assumes persistence exists.
See docs/cache-benchmarking.md for what each field and failure means.
"""

import argparse
import collections
import hashlib
import json
import os
import pathlib
import queue
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

SCHEMA = 1
MAX_FRAME_BYTES = 16 * 1024 * 1024
MAX_HEADER_BYTES = 8 * 1024
MAX_STDERR_LINES = 64
MAX_STDERR_LINE_BYTES = 512
MAX_METRICS_LINE_BYTES = 4096
METRICS_PREFIX = b"pascal-lsp analysis-metrics "
MAX_SAMPLE_BYTES = 64 * 1024
MAX_MANIFEST_BYTES = 64 * 1024
MAX_SOURCE_BYTES = 2 * 1024 * 1024
MAX_FIXTURE_UNITS = 20000
MESSAGE_QUEUE_SIZE = 256
KILL_GRACE_SECONDS = 2.0
TIME_FORMAT = "%M %I %O %U %S"

SCENARIOS = (
    "cold",
    "warm",
    "restart",
    "edit",
    "config_switch",
    "two_sessions",
    "budget_pressure",
)
MANIFEST_SCENARIOS = ("cold", "warm", "restart", "two_sessions")
AVAILABILITY_FIELDS = ("counters", "coverage", "storage", "heapPeak", "resources")
BUDGET_PRESSURE_BYTES = 1024 * 1024
WARMUP_REQUESTS = 1


class FrameError(Exception):
    """A malformed, oversized or truncated LSP frame."""


class RunInvalid(Exception):
    """The run produced protocol noise or a protocol failure."""


class RunTimeout(RunInvalid):
    """A request did not complete in time; the child was reaped."""


# ---------------------------------------------------------------- framing


def encode_frame(message):
    body = json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    return f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body


def read_frame(stream, max_bytes=MAX_FRAME_BYTES):
    length = None
    consumed = 0
    first = True
    while True:
        line = stream.readline(MAX_HEADER_BYTES + 1 - consumed)
        if not line:
            if first:
                return None
            raise FrameError("end of stream inside frame header")
        first = False
        consumed += len(line)
        if consumed > MAX_HEADER_BYTES:
            raise FrameError("frame header exceeds %d bytes" % MAX_HEADER_BYTES)
        if not line.endswith(b"\n"):
            raise FrameError("truncated frame header line")
        if line in (b"\r\n", b"\n"):
            break
        name, sep, value = line.decode("latin-1").partition(":")
        if not sep:
            raise FrameError("non-protocol header line")
        if name.strip().lower() == "content-length":
            if length is not None:
                raise FrameError("duplicate Content-Length")
            text = value.strip()
            if not text.isascii() or not text.isdigit():
                raise FrameError("invalid Content-Length")
            length = int(text)
    if length is None:
        raise FrameError("missing Content-Length")
    if length > max_bytes:
        raise FrameError("frame of %d bytes exceeds %d" % (length, max_bytes))
    body = stream.read(length)
    if len(body) != length:
        raise FrameError("end of stream inside frame body")
    try:
        message = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as error:
        raise FrameError("frame body is not JSON: %s" % error) from None
    if not isinstance(message, dict):
        raise FrameError("frame body is not a JSON object")
    return message


# ---------------------------------------------------------------- URIs


def path_to_uri(path):
    return "file://" + urllib.parse.quote(str(pathlib.Path(path).absolute()), safe="/:")


def uri_to_path(uri):
    parsed = urllib.parse.urlparse(uri)
    if parsed.scheme != "file":
        raise ValueError("not a file URI: %r" % uri)
    return pathlib.Path(urllib.parse.unquote(parsed.path))


# ---------------------------------------------------------------- fixture

PROVIDER_SOURCE = "unit Provider;\ninterface\nconst SharedValue = 1;\nimplementation\nend.\n"
PROVIDER_INCLUDE_SOURCE = (
    "unit Provider;\ninterface\n{$I Baseline.inc}\nconst SharedValue = 1;\n"
    "implementation\nend.\n"
)
INCLUDE_SOURCE = "const IncludeValue = 7;\n"
MAIN_SOURCE = "program Baseline; uses Provider; begin end.\n"
PROJECT_XML = """<Project xmlns="http://schemas.microsoft.com/developer/msbuild/2003">
  <PropertyGroup>
    <MainSource>Root.dpr</MainSource>
    <Config Condition="'$(Config)' == ''">Debug</Config>
    <Platform Condition="'$(Platform)' == ''">Win32</Platform>
    <DCC_UnitSearchPath>.</DCC_UnitSearchPath>
    <DCC_Define>BASELINE_{variant}</DCC_Define>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)' == 'Debug'">
    <DCC_Define>$(DCC_Define);BASELINE_DEBUG</DCC_Define>
  </PropertyGroup>
  <PropertyGroup Condition="'$(Config)' == 'Release'">
    <DCC_Define>$(DCC_Define);BASELINE_RELEASE</DCC_Define>
  </PropertyGroup>
</Project>
"""
CONSUMER_BODY = ("  Log(SharedValue);", "  Log(Provider.SharedValue);")


def consumer_source(name):
    return (
        f"unit {name};\ninterface\nprocedure Run;\nimplementation\n"
        "uses Provider;\nprocedure Log(Value: Integer);\nbegin\nend;\n"
        "procedure Run;\nbegin\n" + "\n".join(CONSUMER_BODY) + "\nend;\nend.\n"
    )


def filler_source(name):
    return f"unit {name};\ninterface\nconst SharedValue = 2;\nimplementation\nend.\n"


def find_shared_value(text, start_line=0):
    """Return (line, character) of the first `SharedValue` identifier at or after start_line."""
    for number, line in enumerate(text.splitlines()):
        if number >= start_line and "SharedValue" in line:
            return number, line.index("SharedValue")
    raise ValueError("no SharedValue in source")


def reference_ranges(text):
    """Ranges of the `SharedValue` references in a consumer's Run body (declaration excluded)."""
    ranges = []
    for number, line in enumerate(text.splitlines()):
        if line.startswith("  Log(") and "SharedValue" in line:
            start = line.index("SharedValue")
            ranges.append(
                {
                    "start": {"line": number, "character": start},
                    "end": {"line": number, "character": start + len("SharedValue")},
                }
            )
    return ranges


def fixture_names(units, importers):
    names = ["Provider"]
    names += [f"Consumer{i:04d}" for i in range(importers)]
    names += [f"Filler{i:05d}" for i in range(units - 1 - importers)]
    return names


def write_fixture(root, units=500, importers=5, with_include=False):
    root = pathlib.Path(root)
    if not isinstance(units, int) or not isinstance(importers, int):
        raise ValueError("units and importers must be integers")
    if units < 3 or units > MAX_FIXTURE_UNITS:
        raise ValueError("units must be within 3..%d" % MAX_FIXTURE_UNITS)
    if importers < 1 or importers >= units - 1:
        raise ValueError("importers must be within 1..units-2")
    if root.exists() and (not root.is_dir() or any(root.iterdir())):
        raise ValueError("fixture root must be a new or empty directory")
    root.mkdir(parents=True, exist_ok=True)

    provider = PROVIDER_INCLUDE_SOURCE if with_include else PROVIDER_SOURCE
    sources = {"Provider.pas": provider}
    consumers = []
    for name in fixture_names(units, importers)[1:]:
        if name.startswith("Consumer"):
            sources[f"{name}.pas"] = consumer_source(name)
            consumers.append(f"{name}.pas")
        else:
            sources[f"{name}.pas"] = filler_source(name)
    sources["Root.dpr"] = MAIN_SOURCE
    sources["A.dproj"] = PROJECT_XML.replace("{variant}", "A")
    sources["B.dproj"] = PROJECT_XML.replace("{variant}", "B")
    if with_include:
        sources["Baseline.inc"] = INCLUDE_SOURCE
    for name, text in sources.items():
        (root / name).write_text(text, encoding="utf-8", newline="\n")

    line, character = find_shared_value(provider, 3 if with_include else 0)
    expected = [
        {"uri": path_to_uri(root / name), "range": span}
        for name in consumers
        for span in reference_ranges(sources[name])
    ]
    return {
        "units": units,
        "importers": importers,
        "withInclude": with_include,
        "consumers": consumers,
        "digest": fixture_digest(root),
        "query": {"file": "Provider.pas", "line": line, "character": character},
        "expectedLocations": expected,
    }


def fixture_digest(root):
    root = pathlib.Path(root)
    digest = hashlib.sha256()
    for path in sorted(p for p in root.rglob("*") if p.is_file()):
        name = path.relative_to(root).as_posix().encode("utf-8")
        data = path.read_bytes()
        digest.update(len(name).to_bytes(8, "big") + name)
        digest.update(len(data).to_bytes(8, "big") + data)
    return digest.hexdigest()


def normalize_locations(locations, root):
    root = pathlib.Path(root).absolute()
    out = []
    for location in locations:
        uri = location.get("uri")
        try:
            relative = uri_to_path(uri).relative_to(root)
            name = "fixture:" + relative.as_posix()
        except (ValueError, TypeError):
            name = "external:%s" % (uri,)
        out.append({"uri": name, "range": location.get("range")})
    return sorted(out, key=lambda item: json.dumps(item, sort_keys=True))


def result_fingerprint(locations, root):
    canonical = json.dumps(
        normalize_locations(locations, root), sort_keys=True, separators=(",", ":")
    )
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


# ---------------------------------------------------------------- samples


def unavailable(reason):
    if not reason:
        raise ValueError("an unavailable measurement needs a reason")
    return {"value": None, "available": False, "reason": reason}


def available(value, **extra):
    entry = {"value": value, "available": True}
    entry.update(extra)
    return entry


def parse_time_report(text):
    """Parse a GNU time `-o` report (`%M %I %O %U %S`); missing data stays null."""
    if text is None:
        return unavailable("gnu_time_not_selected")
    parts = text.split()
    try:
        if len(parts) != 5:
            raise ValueError
        rss, inputs, outputs = (int(part) for part in parts[:3])
        user, system = float(parts[3]), float(parts[4])
    except ValueError:
        return unavailable("gnu_time_report_malformed")
    return available(
        {
            "maxRssKb": rss,
            "inputBlocks": inputs,
            "outputBlocks": outputs,
            "userSeconds": user,
            "systemSeconds": system,
        },
        scope="process_lifetime",
    )


def counter_window(before, after):
    """Endpoint difference of two process snapshots; never exclusive to one request."""
    names = after.get("counters", {})
    delta = {
        name: names[name] - before.get("counters", {}).get(name, 0) for name in names
    }
    return {
        "scope": "process_window_endpoint",
        "requestExclusive": False,
        "before": before,
        "after": after,
        "delta": delta,
    }


def evaluate_response(response, expected, root):
    """Return (outcome, reason, result, protocolError) for a references response."""
    expected_fp = result_fingerprint(expected, root)
    result = {
        "count": None,
        "fingerprint": None,
        "expectedCount": len(expected),
        "expectedFingerprint": expected_fp,
    }
    if response is None:
        return "invalid", "no response", result, None
    if "error" in response:
        error = response["error"] if isinstance(response["error"], dict) else {"message": str(response["error"])}
        return "invalid", "protocol error response", result, error
    locations = response.get("result")
    if not isinstance(locations, list):
        return "invalid", "result is not a location list", result, None
    result["count"] = len(locations)
    result["fingerprint"] = result_fingerprint(locations, root)
    if result["fingerprint"] != expected_fp or len(locations) != len(expected):
        return "invalid", "result mismatch against expected locations", result, None
    return "ok", None, result, None


def make_sample(
    scenario,
    index,
    variant,
    fixture,
    binary,
    input_info,
    elapsed_ns,
    outcome,
    reason,
    result,
    counters,
    resources,
    protocol_error,
    extra=None,
):
    sample = {
        "schema": SCHEMA,
        "scenario": scenario,
        "variant": variant,
        "sampleIndex": index,
        "fixture": fixture,
        "binary": binary,
        "input": input_info,
        "outcome": outcome,
        "reason": reason,
        "requestNanos": elapsed_ns if outcome == "ok" else None,
        "result": result,
        "protocolError": protocol_error,
        "counters": counters,
        "coverage": unavailable("index_coverage_not_instrumented"),
        "storage": unavailable("persistence_not_implemented"),
        "heapPeak": unavailable("not_instrumented"),
        "resources": resources,
        "releaseGateStatus": "not_evaluated",
    }
    if extra:
        sample["extra"] = extra
    return sample


def validate_sample(sample):
    if not isinstance(sample, dict):
        raise ValueError("sample must be an object")
    if sample.get("schema") != SCHEMA:
        raise ValueError("unsupported sample schema")
    if sample.get("outcome") not in ("ok", "invalid"):
        raise ValueError("outcome must be ok or invalid")
    nanos = sample.get("requestNanos", "missing")
    if nanos == "missing":
        raise ValueError("requestNanos is required")
    if nanos is not None and (not isinstance(nanos, int) or isinstance(nanos, bool) or nanos < 0):
        raise ValueError("requestNanos must be a non-negative integer or null")
    if sample["outcome"] == "ok":
        if nanos is None:
            raise ValueError("an ok sample needs a timing")
        if sample.get("protocolError") is not None:
            raise ValueError("an ok sample cannot contain a protocol error")
        result = sample.get("result")
        if not isinstance(result, dict):
            raise ValueError("an ok sample needs a result")
        fingerprint = result.get("fingerprint")
        if not isinstance(fingerprint, str) or not fingerprint:
            raise ValueError("an ok sample needs a result fingerprint")
        if fingerprint != result.get("expectedFingerprint"):
            raise ValueError("an ok sample must match its expected result")
        count = result.get("count")
        if not isinstance(count, int) or isinstance(count, bool) or count != result.get("expectedCount"):
            raise ValueError("an ok sample must match its expected result count")
    elif nanos is not None:
        raise ValueError("an invalid sample cannot carry a timing")
    if sample.get("releaseGateStatus") != "not_evaluated":
        raise ValueError("phase-1 samples must have releaseGateStatus not_evaluated")
    for field in AVAILABILITY_FIELDS:
        entry = sample.get(field)
        if not isinstance(entry, dict) or not isinstance(entry.get("available"), bool):
            raise ValueError("availability entry %r is missing or unlabelled" % field)
        if entry["available"]:
            if entry.get("value") is None:
                raise ValueError("available entry %r has no value" % field)
        elif entry.get("value") is not None or not entry.get("reason"):
            raise ValueError("unavailable entry %r needs a null value and a reason" % field)


def write_sample(stream, sample):
    validate_sample(sample)
    line = json.dumps(sample, sort_keys=True, separators=(",", ":")) + "\n"
    if len(line.encode("utf-8")) > MAX_SAMPLE_BYTES:
        raise ValueError("sample exceeds %d bytes" % MAX_SAMPLE_BYTES)
    stream.write(line)
    stream.flush()


def open_output(path, append):
    return open(path, "a" if append else "x", encoding="utf-8", newline="\n")


def load_manifest(path):
    with open(path, "rb") as handle:
        raw = handle.read(MAX_MANIFEST_BYTES + 1)
    if len(raw) > MAX_MANIFEST_BYTES:
        raise ValueError("input manifest exceeds %d bytes" % MAX_MANIFEST_BYTES)
    try:
        manifest = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as error:
        raise ValueError("input manifest is not JSON: %s" % error) from None
    if not isinstance(manifest, dict) or manifest.get("schema") != SCHEMA:
        raise ValueError("input manifest must be an object with schema 1")
    for key in ("label", "root", "targetUri"):
        if not isinstance(manifest.get(key), str) or not manifest[key]:
            raise ValueError("input manifest needs a string %r" % key)
    for key in ("line", "character"):
        value = manifest.get(key)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            raise ValueError("input manifest %r must be a non-negative integer" % key)
    for key in ("initializationOptions", "fixture", "query"):
        if not isinstance(manifest.get(key), dict):
            raise ValueError("input manifest %r must be an object" % key)
    return manifest


def check_scenarios_for_manifest(scenarios):
    for name in scenarios:
        if name not in MANIFEST_SCENARIOS:
            raise ValueError(
                "scenario %r mutates sources or settings; manifests allow only %s"
                % (name, ", ".join(MANIFEST_SCENARIOS))
            )


def sha256_file(path):
    digest = hashlib.sha256()
    size = 0
    with open(path, "rb") as handle:
        while True:
            chunk = handle.read(1 << 20)
            if not chunk:
                break
            size += len(chunk)
            digest.update(chunk)
    return digest.hexdigest(), size


# ---------------------------------------------------------------- LSP session

_PASSIVE_REQUESTS = {
    "window/workDoneProgress/create",
    "client/registerCapability",
    "client/unregisterCapability",
    "workspace/diagnostic/refresh",
    "workspace/semanticTokens/refresh",
    "workspace/inlayHint/refresh",
    "workspace/codeLens/refresh",
    "workspace/inlineValue/refresh",
    "workspace/foldingRange/refresh",
}


class LspSession:
    def __init__(
        self,
        argv,
        timeout_seconds,
        profiling,
        test_server=False,
        *,
        new_session=False,
        gnu_time=None,
    ):
        self._timeout = float(timeout_seconds)
        self.profiling = bool(profiling)
        self.test_server = bool(test_server)
        self._settings = {}
        self._violations = []
        self._stderr = collections.deque(maxlen=MAX_STDERR_LINES)
        self._metrics = None
        self._messages = queue.Queue(MESSAGE_QUEUE_SIZE)
        self._stop = threading.Event()
        self._next_id = 1
        self._closed_info = None
        self._time_file = None
        env = dict(os.environ)
        env.pop("PASCAL_LSP_ANALYSIS_METRICS", None)
        if self.profiling:
            env["PASCAL_LSP_ANALYSIS_METRICS"] = "1"
        argv = list(argv)
        if gnu_time:
            handle, name = tempfile.mkstemp(prefix="l4d-time-")
            os.close(handle)
            self._time_file = name
            argv = [gnu_time, "-o", name, "-f", TIME_FORMAT] + argv
            new_session = True
        self._own_group = bool(new_session) and hasattr(os, "killpg")
        self._proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            shell=False,
            start_new_session=self._own_group,
        )
        self._threads = [
            threading.Thread(target=self._read_stdout, daemon=True),
            threading.Thread(target=self._read_stderr, daemon=True),
        ]
        for thread in self._threads:
            thread.start()

    # reader threads -------------------------------------------------

    def _put(self, item, droppable=False):
        while not self._stop.is_set():
            try:
                self._messages.put(item, timeout=0.1)
                return
            except queue.Full:
                if droppable:
                    return

    def _read_stdout(self):
        try:
            while True:
                message = read_frame(self._proc.stdout)
                if message is None:
                    self._put(("eof", None))
                    return
                droppable = "method" in message and "id" not in message
                self._put(("message", message), droppable=droppable)
        except (FrameError, OSError, ValueError) as error:
            self._put(("error", str(error)))

    def _read_stderr(self):
        stream = self._proc.stderr
        try:
            while True:
                line = stream.readline(MAX_METRICS_LINE_BYTES + 1)
                if not line:
                    return
                if line.startswith(METRICS_PREFIX):
                    self._take_metrics(line)
                else:
                    self._stderr.append(line[:MAX_STDERR_LINE_BYTES].decode("utf-8", "replace").rstrip("\r\n"))
                if len(line) > MAX_STDERR_LINE_BYTES and not line.endswith(b"\n"):
                    while True:  # consume the excess of an over-long line
                        rest = stream.readline(MAX_METRICS_LINE_BYTES)
                        if not rest or rest.endswith(b"\n"):
                            break
        except (OSError, ValueError):
            return

    def _take_metrics(self, line):
        if len(line) > MAX_METRICS_LINE_BYTES or not line.endswith(b"\n"):
            return
        try:
            report = json.loads(line[len(METRICS_PREFIX) :].decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            return
        if isinstance(report, dict) and report.get("schema") == SCHEMA:
            self._metrics = report

    # child lifetime -------------------------------------------------

    def returncode(self):
        return self._proc.returncode

    def _exited(self):
        """True once the leader exited; a group leader is not reaped here."""
        if self._proc.returncode is not None:
            return True
        if self._own_group and hasattr(os, "waitid"):
            return os.waitid(os.P_PID, self._proc.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT) is not None
        return self._proc.poll() is not None

    def _wait_exit(self, timeout):
        deadline = time.monotonic() + timeout
        while not self._exited():
            if time.monotonic() >= deadline:
                raise subprocess.TimeoutExpired("child", timeout)
            self._stop.wait(0.01)

    def _signal(self, force):
        """Signal the owned child (or its group) only while the leader is unreaped.

        An unreaped leader keeps its pid, and so the process group id, reserved;
        after the leader is reaped the id could belong to an unrelated group.
        """
        if self._proc.returncode is not None:
            return
        try:
            if self._own_group:
                os.killpg(self._proc.pid, signal.SIGKILL if force else signal.SIGTERM)
            elif force:
                self._proc.kill()
            else:
                self._proc.terminate()
        except (ProcessLookupError, PermissionError):
            pass

    def _reap(self):
        """Kill any remaining group members while the leader is still unreaped, then reap."""
        if self._own_group:
            self._signal(True)
        self._proc.wait()

    def _abort(self):
        self._signal(False)
        try:
            self._wait_exit(KILL_GRACE_SECONDS)
        except subprocess.TimeoutExpired:
            self._signal(True)
        self._reap()
        self._stop.set()

    def _fail(self, error):
        self._abort()
        raise error

    # protocol ---------------------------------------------------------

    def _send(self, message):
        try:
            self._proc.stdin.write(encode_frame(message))
            self._proc.stdin.flush()
        except (OSError, ValueError) as error:
            self._fail(RunInvalid("write to server failed: %s" % error))

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def _answer(self, message):
        method, ident = message["method"], message["id"]
        if method in _PASSIVE_REQUESTS:
            reply = {"jsonrpc": "2.0", "id": ident, "result": None}
        elif method == "workspace/configuration":
            items = message.get("params", {}).get("items", [])
            reply = {"jsonrpc": "2.0", "id": ident, "result": [self._settings for _ in items]}
        elif method == "workspace/applyEdit":
            self._violations.append("rejected workspace/applyEdit")
            reply = {
                "jsonrpc": "2.0",
                "id": ident,
                "result": {"applied": False, "failureReason": "benchmark runner never applies edits"},
            }
        else:
            reply = {
                "jsonrpc": "2.0",
                "id": ident,
                "error": {"code": -32601, "message": "unsupported client request"},
            }
        self._send(reply)

    def request(self, method, params):
        ident = self._next_id
        self._next_id += 1
        start = time.perf_counter_ns()
        self._send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
        deadline = time.monotonic() + self._timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                self._fail(RunTimeout("%s timed out after %.1fs" % (method, self._timeout)))
            try:
                kind, payload = self._messages.get(timeout=remaining)
            except queue.Empty:
                self._fail(RunTimeout("%s timed out after %.1fs" % (method, self._timeout)))
            if kind == "error":
                self._fail(RunInvalid("non-protocol server output: %s" % payload))
            if kind == "eof":
                self._fail(RunInvalid("server closed stdout during %s" % method))
            if "method" in payload and "id" in payload:
                self._answer(payload)
            elif "method" not in payload and payload.get("id") == ident:
                return payload, time.perf_counter_ns() - start

    def initialize(self, root, options):
        root = pathlib.Path(root)
        self._settings = dict(options)
        response, _ = self.request(
            "initialize",
            {
                "processId": os.getpid(),
                "rootUri": path_to_uri(root),
                "workspaceFolders": [{"uri": path_to_uri(root), "name": "baseline"}],
                "capabilities": {},
                "initializationOptions": dict(options),
            },
        )
        if "error" in response:
            self._fail(RunInvalid("initialize failed: %r" % (response["error"],)))
        self.notify("initialized", {})
        return response.get("result", {})

    def snapshot(self):
        """Window-endpoint counters from the explicit test server only."""
        if not (self.profiling and self.test_server):
            return unavailable("test_server_snapshot_not_selected")
        response, _ = self.request("pascal/testAnalysisMetrics", {})
        if "error" in response or not isinstance(response.get("result"), dict):
            return unavailable("test_server_snapshot_error")
        return available(response["result"], scope="process_window_endpoint")

    def close(self):
        if self._closed_info is not None:
            return self._closed_info
        shutdown_ok = False
        if not self._exited() and not self._stop.is_set():
            try:
                response, _ = self.request("shutdown", None)
                if "error" not in response:
                    self.notify("exit", None)
                    try:
                        self._proc.stdin.close()
                    except OSError:
                        pass
                    self._wait_exit(self._timeout)
                    shutdown_ok = True
            except (RunInvalid, subprocess.TimeoutExpired):
                pass
        if self._proc.returncode is None and not self._exited():
            self._abort()
        else:
            self._reap()
        orderly = shutdown_ok and self._proc.returncode == 0
        self._stop.set()
        for stream in (self._proc.stdin, self._proc.stdout, self._proc.stderr):
            try:
                stream.close()
            except (OSError, ValueError):
                pass
        for thread in self._threads:
            thread.join(KILL_GRACE_SECONDS)
        resources = parse_time_report(None)
        if self._time_file:
            try:
                with open(self._time_file, "r", encoding="ascii", errors="replace") as handle:
                    resources = parse_time_report(handle.read(1024))
            except OSError:
                resources = unavailable("gnu_time_report_missing")
            finally:
                try:
                    os.unlink(self._time_file)
                except OSError:
                    pass
        self._closed_info = {
            "orderlyShutdown": orderly,
            "exitCode": self._proc.returncode,
            "metrics": self._metrics if orderly else None,
            "stderrLines": list(self._stderr),
            "violations": list(self._violations),
            "resources": resources,
        }
        return self._closed_info

    def violations(self):
        return list(self._violations)


# ---------------------------------------------------------------- scenarios


class Subject:
    """What a scenario queries: an owned fixture copy or a read-only manifest input."""

    def __init__(self, root, options, target, line, character, expected, fixture, input_info):
        self.root = pathlib.Path(root)
        self.options = options
        self.target = pathlib.Path(target)
        self.line = line
        self.character = character
        self.expected = expected
        self.fixture = fixture
        self.input_info = input_info

    def references_params(self):
        return {
            "textDocument": {"uri": path_to_uri(self.target)},
            "position": {"line": self.line, "character": self.character},
            "context": {"includeDeclaration": False},
        }


def synthetic_options(root, extra=None):
    options = {
        "projectFile": str(pathlib.Path(root) / "A.dproj"),
        "buildConfig": "Debug",
        "platform": "Win32",
        "sourcePaths": [str(root)],
    }
    options.update(extra or {})
    return options


def fixture_subject(root, args, extra_options=None, with_include=False):
    info = write_fixture(
        root, units=args.fixture_units, importers=args.fixture_importers, with_include=with_include
    )
    identity = {
        "digest": info["digest"],
        "units": info["units"],
        "importers": info["importers"],
        "withInclude": with_include,
    }
    return Subject(
        root,
        synthetic_options(root, extra_options),
        pathlib.Path(root) / info["query"]["file"],
        info["query"]["line"],
        info["query"]["character"],
        info["expectedLocations"],
        identity,
        {
            "kind": "synthetic",
            "label": "SharedValue references, includeDeclaration=false",
            "query": info["query"],
        },
    )


def manifest_subject(manifest):
    target = uri_to_path(manifest["targetUri"])
    expected = manifest.get("expected")
    return Subject(
        manifest["root"],
        manifest["initializationOptions"],
        target,
        manifest["line"],
        manifest["character"],
        expected.get("locations") if isinstance(expected, dict) else None,
        dict(manifest["fixture"]),
        {"kind": "manifest", "label": manifest["label"], "query": manifest["query"]},
    )


def read_text_bounded(path):
    with open(path, "rb") as handle:
        raw = handle.read(MAX_SOURCE_BYTES + 1)
    if len(raw) > MAX_SOURCE_BYTES:
        raise RunInvalid("target document exceeds %d bytes" % MAX_SOURCE_BYTES)
    return raw.decode("utf-8")


class Context:
    def __init__(self, args, binary_info):
        self.args = args
        self.binary = binary_info
        self.reference = None  # manifest first-sample (count, fingerprint)

    def open_session(self):
        args = self.args
        return LspSession(
            [args.binary, "--stdio"],
            args.timeout_seconds,
            args.profiling == "on",
            args.test_server,
            gnu_time=args.gnu_time,
        )


def open_target(session, subject):
    text = read_text_bounded(subject.target)
    session.notify(
        "textDocument/didOpen",
        {
            "textDocument": {
                "uri": path_to_uri(subject.target),
                "languageId": "pascal",
                "version": 1,
                "text": text,
            }
        },
    )


def start(ctx, subject):
    session = ctx.open_session()
    try:
        session.initialize(subject.root, subject.options)
        open_target(session, subject)
    except RunInvalid:
        session.close()
        raise
    return session


def timed_references(session, subject):
    """One references request; counter snapshots stay outside the timed window."""
    before = session.snapshot() if session.profiling and session.test_server else None
    response, elapsed = session.request("textDocument/references", subject.references_params())
    after = session.snapshot() if before is not None else None
    window = None
    if before is not None:
        if before["available"] and after["available"]:
            window = available(counter_window(before["value"], after["value"]))
        else:
            window = unavailable("test_server_snapshot_error")
    return response, elapsed, window


def counters_entry(close_info, window, profiling):
    value = {}
    if window is not None and window["available"]:
        value["window"] = window["value"]
    if close_info is not None and close_info.get("metrics"):
        value["lifetime"] = close_info["metrics"]
    if value:
        return available(value)
    if not profiling:
        return unavailable("profiling_off")
    return unavailable("metrics_not_reported_by_binary_or_run_not_orderly")


def build(ctx, subject, scenario, index, variant, response, elapsed, window, close_info, extra=None, expected=None):
    expected = subject.expected if expected is None else expected
    invalid_reason = None
    if expected is None:  # manifest without fixed expectation: first sample defines it
        locations = response.get("result") if isinstance(response, dict) else None
        if isinstance(response, dict) and "error" in response:
            outcome, invalid_reason, result, perr = evaluate_response(response, [], subject.root)
        elif isinstance(locations, list):
            fingerprint = result_fingerprint(locations, subject.root)
            if ctx.reference is None:
                ctx.reference = (len(locations), fingerprint)
            result = {
                "count": len(locations),
                "fingerprint": fingerprint,
                "expectedCount": ctx.reference[0],
                "expectedFingerprint": ctx.reference[1],
            }
            perr = None
            outcome = "ok" if fingerprint == ctx.reference[1] else "invalid"
            invalid_reason = None if outcome == "ok" else "result mismatch against first sample"
        else:
            outcome, invalid_reason, result, perr = evaluate_response(response, [], subject.root)
    else:
        outcome, invalid_reason, result, perr = evaluate_response(response, expected, subject.root)
    if close_info is not None:
        if close_info["violations"]:
            outcome, invalid_reason = "invalid", "; ".join(close_info["violations"])
        elif not close_info["orderlyShutdown"]:
            outcome, invalid_reason = "invalid", "server did not shut down orderly"
    resources = close_info["resources"] if close_info else unavailable("gnu_time_not_selected")
    return make_sample(
        scenario, index, variant, subject.fixture, ctx.binary, subject.input_info,
        elapsed, outcome, invalid_reason, result,
        counters_entry(close_info, window, ctx.args.profiling == "on"),
        resources, perr, extra,
    )


def invalid_sample(ctx, subject, scenario, index, variant, reason):
    result = {"count": None, "fingerprint": None, "expectedCount": None, "expectedFingerprint": None}
    return make_sample(
        scenario, index, variant, subject.fixture, ctx.binary, subject.input_info,
        None, "invalid", reason, result,
        unavailable("run_invalid"), unavailable("run_invalid"), None,
    )


def _owned_subject(ctx, tmp, name="fixture", extra_options=None, with_include=False):
    return fixture_subject(
        pathlib.Path(tmp) / name, ctx.args, extra_options=extra_options, with_include=with_include
    )


def scenario_cold(ctx, subject_maker):
    samples = []
    for index in range(ctx.args.samples):
        with subject_maker() as subject:
            samples.append(_one_shot(ctx, subject, "cold", index, None))
    return samples


def _one_shot(ctx, subject, scenario, index, variant):
    try:
        session = start(ctx, subject)
    except RunInvalid as error:
        return invalid_sample(ctx, subject, scenario, index, variant, str(error))
    try:
        response, elapsed, window = timed_references(session, subject)
    except RunInvalid as error:
        session.close()
        return invalid_sample(ctx, subject, scenario, index, variant, str(error))
    info = session.close()
    return build(ctx, subject, scenario, index, variant, response, elapsed, window, info)


def scenario_warm(ctx, subject_maker):
    samples = []
    with subject_maker() as subject:
        try:
            session = start(ctx, subject)
            for _ in range(WARMUP_REQUESTS):
                session.request("textDocument/references", subject.references_params())
        except RunInvalid as error:
            return [invalid_sample(ctx, subject, "warm", 0, None, str(error))]
        pending = []
        for index in range(ctx.args.samples):
            try:
                pending.append((index, *timed_references(session, subject)))
            except RunInvalid as error:
                pending.append((index, None, None, None, str(error)))
                break
        info = session.close()
        for entry in pending:
            index = entry[0]
            if entry[1] is None:
                samples.append(invalid_sample(ctx, subject, "warm", index, None, entry[4]))
                continue
            # process-lifetime totals belong to the whole process, so only the last sample carries them
            last = index == pending[-1][0]
            samples.append(
                build(ctx, subject, "warm", index, None, entry[1], entry[2], entry[3],
                      info if last else None,
                      extra={"warmupRequests": WARMUP_REQUESTS, "sharedProcess": True})
            )
    return samples


def scenario_restart(ctx, subject_maker):
    samples = []
    with subject_maker() as subject:
        for index in range(ctx.args.samples):
            try:
                first = start(ctx, subject)
                first.request("textDocument/references", subject.references_params())
                first.close()
            except RunInvalid as error:
                samples.append(invalid_sample(ctx, subject, "restart", index, None, str(error)))
                continue
            samples.append(_one_shot(ctx, subject, "restart", index, None))
    return samples


def _notify_files_changed(session, paths):
    session.notify(
        "workspace/didChangeWatchedFiles",
        {"changes": [{"uri": path_to_uri(path), "type": 2} for path in paths]},
    )


def scenario_edit(ctx, subject_maker):
    samples = []
    if ctx.args.fixture_importers < 1:
        return samples
    for variant in ("source", "include", "metadata"):
        with tempfile.TemporaryDirectory(prefix="l4d cache baseline é ") as tmp:
            subject = _owned_subject(ctx, tmp, with_include=(variant == "include"))
            root = subject.root
            consumer = root / sorted(p.name for p in root.glob("Consumer*.pas"))[0]
            for index in range(ctx.args.samples):
                samples.append(_edit_sample(ctx, subject, variant, index, consumer))
    return samples


def _edit_sample(ctx, subject, variant, index, consumer):
    original = {p: p.read_bytes() for p in (consumer, subject.root / "A.dproj", subject.root / "Baseline.inc") if p.exists()}
    expected = subject.expected
    changed = []
    try:
        session = start(ctx, subject)
        session.request("textDocument/references", subject.references_params())
        if variant == "source":
            text = consumer.read_text(encoding="utf-8")
            consumer.write_text(
                text.replace("end;\nend.", "  Log(SharedValue);\nend;\nend."), encoding="utf-8", newline="\n"
            )
            changed = [consumer]
            expected = subject.expected + [
                {"uri": path_to_uri(consumer), "range": reference_ranges(consumer.read_text(encoding="utf-8"))[-1]}
            ]
        elif variant == "include":
            include = subject.root / "Baseline.inc"
            include.write_text(include.read_text(encoding="utf-8") + "const IncludeOther = 8;\n", encoding="utf-8", newline="\n")
            changed = [include]
        else:
            project = subject.root / "A.dproj"
            project.write_text(project.read_text(encoding="utf-8").replace("BASELINE_A", "BASELINE_A2"), encoding="utf-8", newline="\n")
            changed = [project]
        _notify_files_changed(session, changed)
        response, elapsed, window = timed_references(session, subject)
        info = session.close()
        return build(ctx, subject, "edit", index, variant, response, elapsed, window, info, expected=expected)
    except RunInvalid as error:
        try:
            session.close()
        except UnboundLocalError:
            pass
        return invalid_sample(ctx, subject, "edit", index, variant, str(error))
    finally:
        for path, data in original.items():
            path.write_bytes(data)


def scenario_config_switch(ctx, subject_maker):
    samples = []
    with subject_maker() as subject:
        root = subject.root
        a, b = path_to_uri(root / "A.dproj"), path_to_uri(root / "B.dproj")
        target = {"uri": path_to_uri(subject.target)}

        def select(session, project):
            response, _ = session.request(
                "pascal/selectProject", {"textDocument": target, "projectUri": project}
            )
            if "error" in response:
                raise RunInvalid("selectProject rejected: %r" % (response["error"],))
            return response.get("result", {}).get("selectedProjectUri")

        def configure(session, config):
            session.notify(
                "workspace/didChangeConfiguration",
                {"settings": {"pascalLsp": {**subject.options, "buildConfig": config}}},
            )

        steps = (
            ("project_a_to_b", lambda s: ("B", select(s, b))),
            ("project_b_to_a", lambda s: ("A", select(s, a))),
            ("debug_to_release", lambda s: ("Release", configure(s, "Release"))),
            ("release_to_debug", lambda s: ("Debug", configure(s, "Debug"))),
        )
        try:
            session = start(ctx, subject)
            session.request("textDocument/references", subject.references_params())
        except RunInvalid as error:
            return [invalid_sample(ctx, subject, "config_switch", 0, None, str(error))]
        index = 0
        for _ in range(ctx.args.samples):
            for name, step in steps:
                try:
                    requested, acknowledged = step(session)
                    response, elapsed, window = timed_references(session, subject)
                except RunInvalid as error:
                    samples.append(invalid_sample(ctx, subject, "config_switch", index, name, str(error)))
                    session = None
                    break
                extra = {
                    "requestedSelection": requested,
                    "serverAcknowledgement": acknowledged if acknowledged else "none_exposed_by_api",
                    "effectiveContextEvidence": "not_exposed_by_current_api",
                }
                samples.append(build(ctx, subject, "config_switch", index, name, response, elapsed, window, None, extra=extra))
                index += 1
            if session is None:
                break
        if session is not None:
            info = session.close()
            if samples and (info["violations"] or not info["orderlyShutdown"]):
                samples[-1]["outcome"] = "invalid"
                samples[-1]["requestNanos"] = None
                samples[-1]["reason"] = "server did not shut down orderly or violated the client contract"
    return samples


def scenario_two_sessions(ctx, subject_maker):
    samples = []
    with subject_maker() as subject:
        for index in range(ctx.args.samples):
            sessions, outputs = [], [None, None]
            try:
                for _ in range(2):
                    sessions.append(start(ctx, subject))
            except RunInvalid as error:
                for session in sessions:
                    session.close()
                samples.append(invalid_sample(ctx, subject, "two_sessions", index, "session0", str(error)))
                continue
            barrier = threading.Barrier(2)

            def work(slot):
                try:
                    barrier.wait(ctx.args.timeout_seconds)
                    outputs[slot] = timed_references(sessions[slot], subject)
                except (RunInvalid, threading.BrokenBarrierError) as error:
                    outputs[slot] = str(error)

            threads = [threading.Thread(target=work, args=(slot,)) for slot in (0, 1)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            infos = [session.close() for session in sessions]
            for slot in (0, 1):
                variant = "session%d" % slot
                if isinstance(outputs[slot], str) or outputs[slot] is None:
                    samples.append(invalid_sample(ctx, subject, "two_sessions", index, variant, str(outputs[slot])))
                else:
                    response, elapsed, window = outputs[slot]
                    samples.append(build(ctx, subject, "two_sessions", index, variant, response, elapsed, window, infos[slot],
                                         extra={"concurrentSessions": 2}))
    return samples


def scenario_budget_pressure(ctx, subject_maker):
    samples = []
    with tempfile.TemporaryDirectory(prefix="l4d cache baseline é ") as tmp:
        subject = _owned_subject(ctx, tmp, extra_options={"maxCacheBytes": BUDGET_PRESSURE_BYTES})
        try:
            session = start(ctx, subject)
            session.request("textDocument/references", subject.references_params())
        except RunInvalid as error:
            return [invalid_sample(ctx, subject, "budget_pressure", 0, None, str(error))]
        pending = []
        for index in range(ctx.args.samples):
            try:
                pending.append((index, *timed_references(session, subject)))
            except RunInvalid as error:
                pending.append((index, None, None, None, str(error)))
                break
        info = session.close()
        for entry in pending:
            index = entry[0]
            if entry[1] is None:
                samples.append(invalid_sample(ctx, subject, "budget_pressure", index, None, entry[4]))
                continue
            samples.append(build(ctx, subject, "budget_pressure", index, None, entry[1], entry[2], entry[3],
                                 info if index == pending[-1][0] else None,
                                 extra={"requestedMaxCacheBytes": BUDGET_PRESSURE_BYTES,
                                        "effectiveBudgetEvidence": "not_exposed_by_current_api"}))
    return samples


_RUNNERS = {
    "cold": scenario_cold,
    "warm": scenario_warm,
    "restart": scenario_restart,
    "edit": scenario_edit,
    "config_switch": scenario_config_switch,
    "two_sessions": scenario_two_sessions,
    "budget_pressure": scenario_budget_pressure,
}


class _SubjectScope:
    def __init__(self, make, cleanup=None):
        self._make, self._cleanup = make, cleanup

    def __enter__(self):
        if self._cleanup is not None:
            self._tmp = tempfile.TemporaryDirectory(prefix="l4d cache baseline é ")
            self._subject = self._make(self._tmp.name)
            return self._subject
        return self._make(None)

    def __exit__(self, *exc):
        if self._cleanup is not None:
            self._tmp.cleanup()


def run_scenario(args, scenario):
    binary_info = getattr(args, "binary_info", None)
    if binary_info is None:
        digest, size = sha256_file(args.binary)
        binary_info = {
            "path": str(args.binary),
            "sha256": digest,
            "sizeBytes": size,
            "testServer": bool(args.test_server),
            "profiling": args.profiling == "on",
        }
    ctx = Context(args, binary_info)
    manifest = getattr(args, "manifest", None)
    if manifest is not None:
        check_scenarios_for_manifest([scenario])

        def subject_maker():
            return _SubjectScope(lambda _tmp: manifest_subject(manifest))
    else:

        def subject_maker():
            return _SubjectScope(lambda tmp: _owned_subject(ctx, tmp), cleanup=True)

    return _RUNNERS[scenario](ctx, subject_maker)


# ---------------------------------------------------------------- CLI


def build_parser():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--scenario", default="all", choices=("all",) + SCENARIOS)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--fixture-units", type=int, default=500)
    parser.add_argument("--fixture-importers", type=int, default=5)
    parser.add_argument("--timeout-seconds", type=float, default=120.0)
    parser.add_argument("--input-manifest")
    parser.add_argument("--append", action="store_true")
    parser.add_argument("--profiling", choices=("on", "off"), default="on")
    parser.add_argument("--test-server", action="store_true")
    parser.add_argument("--gnu-time", help="optional GNU time binary for per-child RSS/IO")
    return parser


def main(argv=None):
    parser = build_parser()
    args = parser.parse_args(argv)
    scenarios = list(SCENARIOS) if args.scenario == "all" else [args.scenario]
    try:
        if args.samples < 1 or args.samples > 1000:
            raise ValueError("--samples must be within 1..1000")
        if args.timeout_seconds <= 0:
            raise ValueError("--timeout-seconds must be positive")
        if args.test_server and args.profiling != "on":
            raise ValueError("--test-server requires --profiling on")
        if not os.path.isfile(args.binary):
            raise ValueError("--binary %r is not a file" % args.binary)
        args.manifest = None
        if args.input_manifest:
            args.manifest = load_manifest(args.input_manifest)
            if args.scenario == "all":
                scenarios = list(MANIFEST_SCENARIOS)
            check_scenarios_for_manifest(scenarios)
        digest, size = sha256_file(args.binary)
        args.binary_info = {
            "path": str(args.binary),
            "sha256": digest,
            "sizeBytes": size,
            "testServer": args.test_server,
            "profiling": args.profiling == "on",
        }
        output = open_output(args.output, args.append)
    except (ValueError, OSError) as error:
        print("cache_baseline: %s" % error, file=sys.stderr)
        return 2
    invalid = 0
    with output:
        for scenario in scenarios:
            for sample in run_scenario(args, scenario):
                write_sample(output, sample)
                invalid += sample["outcome"] != "ok"
    print("cache_baseline: wrote %s (%d invalid sample(s))" % (args.output, invalid))
    return 1 if invalid else 0


if __name__ == "__main__":
    raise SystemExit(main())
