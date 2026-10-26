#!/usr/bin/env python3
"""Tests for build-actions-matrix.py

Run with `python3 -m unittest discover -s tools/scripts -p 'test_*.py'` from the repository root.
"""

import importlib.util
import json
import os
import subprocess
import sys
import unittest

SCRIPTS = os.path.dirname(os.path.abspath(__file__))
TOOLS = os.path.dirname(SCRIPTS)
SCRIPT = os.path.join(SCRIPTS, "build-actions-matrix.py")

spec = importlib.util.spec_from_file_location("build_actions_matrix", SCRIPT)
matrix_script = importlib.util.module_from_spec(spec)
spec.loader.exec_module(matrix_script)


def run_script(manifest, *extra):
    """Run the script on a manifest file and return the completed process."""
    return subprocess.run(
        [sys.executable, SCRIPT, *extra, manifest],
        capture_output=True,
        text=True,
        check=False,
    )


def k8s_spec(image):
    """Build an inline Image spec that runs a container image."""
    return {"Image": {"environment": {"K8s": {"image": image}}}}


class RealToolboxTests(unittest.TestCase):
    """The repository's two toolboxes give the same matrix."""

    def test_both_formats_give_identical_output(self):
        for extra in ([], ["--registry-prefix", "ghcr.io/owner/repo"]):
            legacy = run_script(os.path.join(TOOLS, "toolbox.json"), *extra)
            converted = run_script(os.path.join(TOOLS, "toolbox.v2.json"), *extra)
            self.assertEqual(legacy.returncode, 0, legacy.stderr)
            self.assertEqual(converted.returncode, 0, converted.stderr)
            self.assertEqual(legacy.stdout, converted.stdout)

    def test_the_four_ghidra_base_builds_are_in_the_matrix(self):
        with open(os.path.join(TOOLS, "toolbox.v2.json"), encoding="utf-8") as manifest:
            entries = matrix_script.build_matrix(json.load(manifest))
        versions = sorted(
            entry["version"] for entry in entries if entry["name"] == "ghidra-base"
        )
        self.assertEqual(versions, ["10.3.2", "11.2", "12.0", "12.1"])
        # one matrix entry per buildable tool version, none for build_image = false
        names = {entry["name"] for entry in entries}
        self.assertNotIn("strings-16be", names)
        self.assertIn("clamav", names)


class CurrentFormatTests(unittest.TestCase):
    """Entries of a v2 toolbox."""

    def test_tools_and_builds_become_sorted_entries(self):
        manifest = {
            "format_version": 2,
            "tools": {
                "zeta": {
                    "versions": {
                        "latest": {
                            "spec": k8s_spec("reg/zeta:latest"),
                            "build": {
                                "build_path": "images/zeta",
                                "image_tags": ["reg/zeta:latest", "mirror/zeta:latest"],
                                "base_image": {"allow_override": False},
                            },
                        }
                    }
                },
                "alpha": {
                    "versions": {
                        "1": {
                            "spec": k8s_spec("reg/alpha:1"),
                            "build": {"build_path": "images/alpha"},
                        },
                        "2": {"spec": k8s_spec("reg/alpha:2")},
                    }
                },
            },
            "builds": {
                "base": {
                    "1": {"build_path": "images/base", "image_tags": ["reg/base:1"]},
                    "off": {"build_path": "x", "build_image": False},
                }
            },
        }
        matrix = matrix_script.build_matrix(manifest)
        self.assertEqual(
            matrix,
            [
                {
                    "name": "alpha",
                    "version": "1",
                    "build_path": "images/alpha",
                    "image_name": "reg/alpha:1",
                    "image_tags": ["reg/alpha:1"],
                    "base_image": {},
                },
                {
                    "name": "base",
                    "version": "1",
                    "build_path": "images/base",
                    "image_name": "reg/base:1",
                    "image_tags": ["reg/base:1"],
                    "base_image": {},
                },
                {
                    "name": "zeta",
                    "version": "latest",
                    "build_path": "images/zeta",
                    "image_name": "reg/zeta:latest",
                    "image_tags": ["reg/zeta:latest", "mirror/zeta:latest"],
                    "base_image": {"allow_override": False},
                },
            ],
        )

    def test_harness_specs_contribute_their_image(self):
        manifest = {
            "tools": {
                "h": {
                    "versions": {
                        "latest": {
                            "spec": {
                                "Harness": {
                                    "workload": {
                                        "environment": {"K8s": {"image": "reg/h:1"}}
                                    }
                                }
                            },
                            "build": {"build_path": "h"},
                        }
                    }
                }
            }
        }
        self.assertEqual(
            matrix_script.build_matrix(manifest)[0]["image_tags"], ["reg/h:1"]
        )

    def test_registry_prefix_moves_tags(self):
        manifest = {
            "tools": {
                "t": {
                    "versions": {
                        "latest": {
                            "spec": k8s_spec("ghcr.io/cisagov/thorium/tools/t:latest"),
                            "build": {
                                "build_path": "t",
                                "image_tags": ["ghcr.io/cisagov/thorium/tools/t:latest"],
                            },
                        }
                    }
                }
            }
        }
        matrix = matrix_script.build_matrix(manifest, "ghcr.io/me/fork")
        self.assertEqual(matrix[0]["image_tags"], ["ghcr.io/me/fork/tools/t:latest"])

    def test_an_entry_without_tags_is_an_error(self):
        manifest = {
            "tools": {"t": {"versions": {"latest": {"build": {"build_path": "t"}}}}}
        }
        with self.assertRaises(SystemExit) as raised:
            matrix_script.build_matrix(manifest)
        self.assertEqual(raised.exception.code, 2)

    def test_a_manifest_with_no_sections_is_an_error(self):
        with self.assertRaises(SystemExit) as raised:
            matrix_script.build_matrix({"name": "empty"})
        self.assertEqual(raised.exception.code, 2)


class LegacyFormatTests(unittest.TestCase):
    """Entries of a legacy toolbox keep their file order and format."""

    def test_legacy_order_and_keys_are_unchanged(self):
        manifest = {
            "images": {
                "b": {
                    "latest": {
                        "build_path": "b",
                        "config": {"image": "reg/b:latest"},
                    }
                },
                "a": {
                    "latest": {"build_path": "a", "image_tags": ["reg/a:latest"]},
                    "ref": {"image_from": {"name": "b"}, "build_image": False},
                },
            }
        }
        matrix = matrix_script.build_matrix(manifest)
        self.assertEqual([entry["name"] for entry in matrix], ["b", "a"])
        self.assertEqual(
            list(matrix[0]),
            ["name", "version", "build_path", "image_name", "image_tags", "base_image"],
        )


if __name__ == "__main__":
    unittest.main()
