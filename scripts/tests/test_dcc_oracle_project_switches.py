"""Regression tests for .dproj warning-switch extraction."""

import pathlib
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPTS = pathlib.Path(__file__).resolve().parents[1]
HELPER = SCRIPTS / "dcc-oracle-project-switches.py"
SWITCH_TABLE = (
    REPO_ROOT
    / "crates"
    / "pascal-lsp"
    / "src"
    / "navigation"
    / "dcc_switches"
    / "37.0.txt"
)


class ProjectSwitchTests(unittest.TestCase):
    def test_project_flags_are_sorted(self):
        project_path = (
            REPO_ROOT
            / "crates"
            / "pascal-lsp"
            / "tests"
            / "dcc_oracle"
            / "dproj_warning_switches"
            / "DprojWarningSwitches.dproj"
        )
        result = subprocess.run(
            [
                sys.executable,
                str(HELPER),
                str(project_path),
                str(SWITCH_TABLE),
                "Debug",
                "Win32",
            ],
            check=False,
            capture_output=True,
            text=True,
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        flags = result.stdout.splitlines()
        self.assertEqual(flags, sorted(flags))
        self.assertEqual(flags, ["-H-", "-W-SYMBOL_DEPRECATED"])

    def test_config_and_platform_are_required_for_project_switches(self):
        project = """\
<Project>
  <PropertyGroup>
    <DCC_Warnings>false</DCC_Warnings>
  </PropertyGroup>
</Project>
"""
        for config, platform, missing in [
            ("", "Win32", "config"),
            ("Debug", "", "platform"),
        ]:
            with self.subTest(missing=missing), tempfile.TemporaryDirectory() as directory:
                project_path = pathlib.Path(directory) / "MissingSelection.dproj"
                project_path.write_text(project, encoding="utf-8")
                result = subprocess.run(
                    [
                        sys.executable,
                        str(HELPER),
                        str(project_path),
                        str(SWITCH_TABLE),
                        config,
                        platform,
                    ],
                    check=False,
                    capture_output=True,
                    text=True,
                )

            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"case.toml must set {missing}", result.stderr)

    def test_nested_property_group_is_rejected_instead_of_applied(self):
        project = """\
<Project>
  <PropertyGroup>
    <DCC_SYMBOL_DEPRECATED>false</DCC_SYMBOL_DEPRECATED>
  </PropertyGroup>
  <Choose>
    <When Condition="'$(Config)'=='Debug'">
      <PropertyGroup>
        <DCC_SYMBOL_DEPRECATED>true</DCC_SYMBOL_DEPRECATED>
      </PropertyGroup>
    </When>
  </Choose>
</Project>
"""
        with tempfile.TemporaryDirectory() as directory:
            project_path = pathlib.Path(directory) / "Nested.dproj"
            project_path.write_text(project, encoding="utf-8")
            result = subprocess.run(
                [
                    sys.executable,
                    str(HELPER),
                    str(project_path),
                    str(SWITCH_TABLE),
                    "Debug",
                    "Win32",
                ],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("root-level PropertyGroup", result.stderr)


if __name__ == "__main__":
    unittest.main()
