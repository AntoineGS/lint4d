"""Regression tests for dcc switch-probe evidence classification."""

import pathlib
import subprocess
import unittest


HELPER = pathlib.Path(__file__).resolve().parents[1] / "dcc-switch-probe.sh"


def run_helper(function, *args):
    command = f'source "$1"; {function} "$2" "$3" "$4"'
    return subprocess.run(
        ["bash", "-c", command, "probe-test", str(HELPER), *map(str, args)],
        check=False,
        capture_output=True,
        text=True,
    )


class ErrorProbeTests(unittest.TestCase):
    def test_matching_promoted_error_requires_normal_compiler_error_exit(self):
        output = (
            "Embarcadero Delphi for Win32 compiler version 37.0\n"
            "Hint: H2675 Directory not found: c:\\compiler\\lib\n"
            "L4dSwitchUse.pas(7) Error: E1000 "
            "Symbol 'DepMsg' is deprecated: 'use Other'\n"
        )

        result = run_helper("dcc_classify_error_probe", "W1000", 1, output)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            "yes\tL4dSwitchUse.pas(7) Error: E1000 "
            "Symbol 'DepMsg' is deprecated: 'use Other'\n",
        )

    def test_matching_warning_with_success_exit_is_not_error_capable(self):
        output = (
            "Embarcadero Delphi for Win32 compiler version 37.0\n"
            "Hint: H2675 Directory not found: c:\\compiler\\lib\n"
            "L4dSwitchUse.pas(7) Warning: W1000 "
            "Symbol 'DepMsg' is deprecated: 'use Other'\n"
        )

        result = run_helper("dcc_classify_error_probe", "W1000", 0, output)

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            result.stdout,
            "no\tL4dSwitchUse.pas(7) Warning: W1000 "
            "Symbol 'DepMsg' is deprecated: 'use Other'\n",
        )

    def test_timeout_and_empty_output_are_inconclusive(self):
        cases = (
            (124, ""),
            (124, "Error: E1000 partial output before timeout\n"),
            (0, ""),
            (137, "Error: E1000 interrupted\n"),
        )
        for status, output in cases:
            with self.subTest(status=status, output=output):
                result = run_helper(
                    "dcc_classify_error_probe", "W1000", status, output
                )
                self.assertNotEqual(result.returncode, 0)

    def test_unrelated_diagnostics_and_inconsistent_status_are_inconclusive(self):
        cases = (
            (
                1,
                "Error: E2003 Undeclared identifier: 'Missing'\n",
            ),
            (
                1,
                "Warning: W1000 Symbol 'DepMsg' is deprecated\n"
                "Error: E2003 Undeclared identifier: 'Missing'\n",
            ),
            (
                1,
                "Error: E1000 Symbol 'DepMsg' is deprecated\n"
                "Fatal: compiler stopped unexpectedly\n",
            ),
            (
                1,
                "Warning: W1000 Symbol 'DepMsg' is deprecated\n"
                "Error: E1000 Symbol 'DepMsg' is deprecated\n",
            ),
            (0, "Error: E1000 Symbol 'DepMsg' is deprecated\n"),
            (1, "Warning: W1000 Symbol 'DepMsg' is deprecated\n"),
        )
        for status, output in cases:
            with self.subTest(status=status, output=output):
                result = run_helper(
                    "dcc_classify_error_probe", "W1000", status, output
                )
                self.assertNotEqual(result.returncode, 0)

    def test_requested_compiler_version_must_match_banner(self):
        matching = run_helper(
            "dcc_validate_compiler_banner",
            "37.0",
            "Embarcadero Delphi for Win32 compiler version 37.0",
        )
        mismatching = run_helper(
            "dcc_validate_compiler_banner",
            "37.0",
            "Embarcadero Delphi for Win32 compiler version 24.0",
        )

        self.assertEqual(matching.returncode, 0, matching.stderr)
        self.assertNotEqual(mismatching.returncode, 0)


if __name__ == "__main__":
    unittest.main()
