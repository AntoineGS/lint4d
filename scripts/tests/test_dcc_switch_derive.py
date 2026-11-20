"""Regression tests for dcc warning-switch table derivation."""

import pathlib
import re
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
DERIVE_SCRIPT = ROOT / "scripts" / "dcc-switch-derive.sh"


class DeriveScriptTests(unittest.TestCase):
    def test_flow_codes_use_their_oracle_fixtures_as_probe_triggers(self):
        script = DERIVE_SCRIPT.read_text()
        single_unit_triggers = script.split("write_single_unit_trigger() {", 1)[1].split(
            "\ntrigger_source=''", 1
        )[0]
        fixtures = {
            "H2164": ("h2164_unused_local", "UnusedLocal.pas"),
            "H2077": ("h2077_overwritten", "Overwritten.pas"),
            "H2135": ("h2135_for_constant_empty", "ForConstantEmpty.pas"),
            "W1035": ("w1035_no_result", "NoResult.pas"),
            "W1036": ("w1036_scalar_read_before_write", "ScalarReadBeforeWrite.pas"),
            "W1037": ("w1037_read_after_loop", "ReadAfterLoop.pas"),
        }

        for code, (case, source) in fixtures.items():
            with self.subTest(code=code):
                fixture = (
                    ROOT
                    / "crates"
                    / "pascal-lsp"
                    / "tests"
                    / "dcc_oracle"
                    / case
                    / source
                )
                self.assertIn(f"{code})", single_unit_triggers)
                self.assertIn(f"{case}/{source}", single_unit_triggers)
                self.assertTrue(fixture.is_file())

    def test_property_mapping_accepts_whitespace_around_equals(self):
        script = DERIVE_SCRIPT.read_text()
        grep_pattern = re.search(
            r"if grep -oE '([^']+)' \"\$task_block\"",
            script,
        )
        sed_pattern = re.search(r"""sed -E \\\n\s*'([^']+)'""", script)
        self.assertIsNotNone(grep_pattern)
        self.assertIsNotNone(sed_pattern)

        result = subprocess.run(
            [
                "bash",
                "-c",
                'grep -oE "$1" | sed -E "$2"',
                "property-mapping-test",
                grep_pattern.group(1),
                sed_pattern.group(1),
            ],
            input='DependencyCheckOutputName = "$(DCC_DependencyCheckOutputName)"\n',
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            "DCC_DependencyCheckOutputName DependencyCheckOutputName\n",
        )

    def test_property_and_specific_suppressors_are_ambiguous(self):
        lines = DERIVE_SCRIPT.read_text().splitlines()
        start = next(
            index
            for index, line in enumerate(lines)
            if line.strip() == 'mapfile -t suppressors < "$suppressors_file"'
        )
        end = next(
            index
            for index, line in enumerate(lines[start:], start)
            if line.strip() == "if [[ ${identifiers[$code]} != '-' ]]; then"
        )
        selection = "\n".join(lines[start:end])

        with tempfile.TemporaryDirectory() as temp_dir:
            temp = pathlib.Path(temp_dir)
            suppressors_file = temp / "suppressors.txt"
            suppressors_file.write_text(
                "SYMBOL_DEPRECATED\nOTHER_SPECIFIC_ID\nALL\n"
            )
            properties_file = temp / "properties.txt"
            properties_file.write_text(
                "DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\n"
            )
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    "\n".join(
                        (
                            "set -eu",
                            "fail() { printf 'derive: %s\\n' \"$*\" >&2; exit 2; }",
                            "suppressors_file=$1",
                            "properties_file=$2",
                            "code=W1000",
                            "declare -A identifiers",
                            selection,
                            'printf "identifier=%s\\n" "${identifiers[$code]}"',
                        )
                    ),
                    "suppressor-selection-test",
                    str(suppressors_file),
                    str(properties_file),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("multiple non-umbrella suppressors", result.stderr)
        self.assertIn("SYMBOL_DEPRECATED", result.stderr)
        self.assertIn("OTHER_SPECIFIC_ID", result.stderr)

    def test_code_without_a_specific_suppressor_records_no_id(self):
        lines = DERIVE_SCRIPT.read_text().splitlines()
        start = next(
            index
            for index, line in enumerate(lines)
            if line.strip() == 'mapfile -t suppressors < "$suppressors_file"'
        )
        end = next(
            index
            for index, line in enumerate(lines[start:], start)
            if line.strip() == "if [[ ${identifiers[$code]} != '-' ]]; then"
        )
        selection = "\n".join(lines[start:end])

        with tempfile.TemporaryDirectory() as temp_dir:
            temp = pathlib.Path(temp_dir)
            suppressors_file = temp / "suppressors.txt"
            suppressors_file.write_text("ALL\n")
            properties_file = temp / "properties.txt"
            properties_file.write_text(
                "DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED\n"
            )
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    "\n".join(
                        (
                            "set -eu",
                            "fail() { printf 'derive: %s\\n' \"$*\" >&2; exit 2; }",
                            "suppressors_file=$1",
                            "properties_file=$2",
                            "code=W1000",
                            "declare -A identifiers",
                            selection,
                            'printf "identifier=%s\\n" "${identifiers[$code]}"',
                        )
                    ),
                    "missing-suppressor-test",
                    str(suppressors_file),
                    str(properties_file),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "identifier=-\n")
        self.assertEqual(result.stderr, "")

    def test_h2443_probe_uses_the_recorded_fixture_and_its_provider_units(self):
        script = DERIVE_SCRIPT.read_text()
        codes = re.search(r"^codes=\(([^)]*)\)$", script, re.MULTILINE)
        self.assertIsNotNone(codes)
        self.assertIn("H2443", codes.group(1).split())

        def function_body(name, following):
            start = script.index(f"{name}() {{")
            end = script.index(f"\n{following}", start)
            return script[start:end]

        single_unit = function_body("write_single_unit_trigger", "trigger_source=")
        dispatch = function_body("write_trigger", "validate_candidate_ids")
        with tempfile.TemporaryDirectory() as temp_dir:
            temp = pathlib.Path(temp_dir)
            work_dir = temp / "work"
            work_dir.mkdir()
            result = subprocess.run(
                [
                    "bash",
                    "-c",
                    "\n".join(
                        (
                            "set -eu",
                            "fail() { printf 'derive: %s\\n' \"$*\" >&2; exit 2; }",
                            "source_root=$1",
                            "work_dir=$2",
                            single_unit,
                            dispatch,
                            "write_trigger H2443 $'{$HINTS OFF}\\n'",
                        )
                    ),
                    "h2443-trigger-test",
                    str(ROOT / "crates/pascal-lsp/tests/dcc_oracle"),
                    str(work_dir),
                ],
                check=False,
                capture_output=True,
                text=True,
            )

            self.assertEqual(result.returncode, 0, result.stderr)
            trigger = (work_dir / "InlUse.pas").read_text()
            self.assertIn("{$HINTS OFF}\n", trigger)
            self.assertIn("  I := UsesImplUnit;", trigger)
            self.assertTrue((work_dir / "InlProv.pas").is_file())
            self.assertTrue((work_dir / "InlHelper.pas").is_file())


if __name__ == "__main__":
    unittest.main()
