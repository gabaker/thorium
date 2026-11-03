#!/usr/bin/env python3
"""Offline tests of check_image_arch.sh, the core image's architecture check.

Each test runs the script with a stub `docker` first on PATH that reports a chosen platform and
copies out fake binaries carrying a chosen ELF machine, so no Docker daemon is needed.

Run from anywhere: python3 -m unittest discover -s .github/scripts -p 'test_*.py'
"""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

# the script under test, beside this file
SCRIPT = Path(__file__).resolve().parent / "check_image_arch.sh"
# the binaries the script checks in /app
BINARIES = [
    "crane",
    "thorium-api",
    "thorium-operator",
    "thoradm",
    "thorium-scaler",
    "thorium-search-streamer",
    "thorium-event-handler",
]
# the ELF machine field of each architecture's binaries, little-endian
MACHINES = {"amd64": b"\x3e\x00", "arm64": b"\xb7\x00"}
# a stub docker: `image inspect` prints $STUB_PLATFORM, `create` prints an id, `cp` copies the
# file of that name from $STUB_FILES, and `rm` records the removed container in $STUB_LOG
STUB_DOCKER = """#!/bin/sh
case "$1" in
  image) echo "$STUB_PLATFORM" ;;
  create) echo stub-container ;;
  cp) name=${2##*/}; [ -f "$STUB_FILES/$name" ] || exit 1; cp "$STUB_FILES/$name" "$3" ;;
  rm) echo "rm $2" >> "$STUB_LOG" ;;
  *) exit 64 ;;
esac
"""


def elf(machine):
    """Return the first bytes of an ELF file with the given machine field.

    # Arguments

    * `machine` - The two machine bytes
    """
    # magic, class, data, version, padding to 16 bytes, then e_type and e_machine
    return b"\x7fELF\x02\x01\x01" + b"\x00" * 9 + b"\x02\x00" + machine + b"\x00" * 44


class CheckImageArchTest(unittest.TestCase):
    """Runs check_image_arch.sh against the stub docker."""

    def run_check(self, platform, arch, machines, missing=()):
        """Run the script and return the completed process and the stub's log.

        # Arguments

        * `platform` - The platform the stub reports for the image
        * `arch` - The architecture the script is asked to check for
        * `machines` - The ELF machine bytes of each fake binary, by name, or one value for all
        * `missing` - The binaries the image does not have
        """
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            bin_dir = root / "bin"
            files = root / "files"
            bin_dir.mkdir()
            files.mkdir()
            # put the stub docker first on PATH
            stub = bin_dir / "docker"
            stub.write_text(STUB_DOCKER, encoding="utf-8")
            stub.chmod(0o755)
            # write each fake binary the image has
            for binary in BINARIES:
                if binary in missing:
                    continue
                machine = machines[binary] if isinstance(machines, dict) else machines
                (files / binary).write_bytes(elf(machine))
            log = root / "log"
            log.touch()
            env = {
                "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '/usr/bin:/bin')}",
                "STUB_PLATFORM": platform,
                "STUB_FILES": str(files),
                "STUB_LOG": str(log),
            }
            result = subprocess.run(
                ["sh", str(SCRIPT), "registry/thorium:tag", arch],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            return result, log.read_text(encoding="utf-8")

    def test_matching_image_passes(self):
        """An image whose platform and binaries all match its architecture passes."""
        for arch, machine in MACHINES.items():
            with self.subTest(arch=arch):
                result, log = self.run_check(f"linux/{arch}", arch, machine)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("rm stub-container", log)

    def test_wrong_platform_fails(self):
        """An image built for the other platform fails before any container is created."""
        result, log = self.run_check("linux/amd64", "arm64", MACHINES["arm64"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("is for linux/amd64, not linux/arm64", result.stderr)
        self.assertEqual(log, "")

    def test_wrong_binary_fails(self):
        """One binary of the other architecture fails the check and is named."""
        machines = {binary: MACHINES["arm64"] for binary in BINARIES}
        machines["thorium-api"] = MACHINES["amd64"]
        result, log = self.run_check("linux/arm64", "arm64", machines)
        self.assertEqual(result.returncode, 1)
        self.assertIn("/app/thorium-api has ELF machine '3e 00', not 'b7 00'", result.stderr)
        self.assertIn("rm stub-container", log)

    def test_missing_binary_fails(self):
        """A binary the image lacks fails the check and is named."""
        result, _ = self.run_check("linux/amd64", "amd64", MACHINES["amd64"], missing=("crane",))
        self.assertEqual(result.returncode, 1)
        self.assertIn("has no /app/crane", result.stderr)

    def test_unknown_architecture_is_a_usage_error(self):
        """An architecture with no known ELF machine is refused."""
        result, _ = self.run_check("linux/s390x", "s390x", MACHINES["amd64"])
        self.assertEqual(result.returncode, 2)


if __name__ == "__main__":
    unittest.main()
