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


if __name__ == "__main__":
    unittest.main()
