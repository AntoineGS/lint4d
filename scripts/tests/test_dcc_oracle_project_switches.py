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
