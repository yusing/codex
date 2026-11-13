"""The native fork's maintenance commands must not process unused products."""

import importlib.util
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


class NativeMaintenanceTests(unittest.TestCase):
    def test_schema_generation_does_not_invoke_sdk_tools(self):
        module = load(
            "native_schema_writer",
            ROOT / "codex-rs/app-server-protocol/scripts/write_schema_fixtures.py",
        )
        with tempfile.TemporaryDirectory() as scratch:
            for args in ([], ["--experimental"], ["--schema-root", scratch]):
                with (
                    self.subTest(args=args),
                    patch.object(sys, "argv", ["write-schema", *args]),
                    patch.object(module.subprocess, "run") as run,
                ):
                    module.main()
                    run.assert_called_once()
                    command = run.call_args.args[0]
                    self.assertEqual(
                        command[:5],
                        ["cargo", "test", "-p", "codex-app-server-protocol", "--lib"],
                    )

    def test_formatters_do_not_process_unused_products(self):
        module = load("native_formatter", ROOT / "scripts/format.py")
        for check in (False, True):
            groups = module.formatter_groups(check=check)
            self.assertEqual(
                [group.name for group in groups], ["Just", "Rust", "Python scripts"]
            )
            for group in groups:
                for command in group.commands:
                    self.assertNotIn("sdk/python", command.args)
                    self.assertNotIn("buildifier", command.args)
            rust = groups[1].commands[0]
            self.assertEqual(rust.args[:3], ("cargo", "fmt", "--all"))

    def test_default_targets_are_native_entry_points(self):
        workspace = tomllib.loads((ROOT / "codex-rs/Cargo.toml").read_text())[
            "workspace"
        ]
        self.assertEqual(
            workspace["default-members"], ["app-server", "code-mode-host", "bwrap"]
        )
        self.assertTrue(
            {"cli", "tui", "exec", "voice-host"}.issubset(workspace["exclude"])
        )
        self.assertFalse(set(workspace["members"]) & set(workspace["exclude"]))


if __name__ == "__main__":
    unittest.main()
