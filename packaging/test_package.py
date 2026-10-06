"""Check packaging rejects incompatible binaries before creating artifacts."""

import os
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class PackageTests(unittest.TestCase):
    def setUp(self):
        work = ROOT / "target/package-tests"
        work.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(dir=work)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        shutil.copytree(ROOT / "packaging", self.root / "packaging")
        for name in ("Cargo.toml", "LICENSE-QUICKCONNECT-REFERENCE.txt"):
            shutil.copyfile(ROOT / name, self.root / name)
        (self.root / "LICENSE").write_text("Test project license\n")
        (self.root / "THIRD-PARTY-NOTICES.txt").write_text("Test dependency notices\n")
        (self.root / "LICENSE-RUST-STDLIB.html").write_text("Test Rust notices\n")
        for name in (
            "custom_components/gafctl",
            "home-assistant",
            "target/release",
            "bin",
        ):
            (self.root / name).mkdir(parents=True)
        shutil.copyfile(
            ROOT / "custom_components/gafctl/manifest.json",
            self.root / "custom_components/gafctl/manifest.json",
        )
        shutil.copyfile(
            ROOT / "home-assistant/config.yaml",
            self.root / "home-assistant/config.yaml",
        )
        self.environment = {
            **os.environ,
            "PATH": f"{self.root / 'bin'}:{os.environ['PATH']}",
        }
        self.binary("gafctl", "0.1.0")
        self.binary("gafctl-server", "0.1.0")
        self.command(
            "od",
            'if [ "$2" = -N6 ]; then echo \'7f 45 4c 46 02 01\'; else echo "${ELF_MACHINE:-3e 00}"; fi\n',
        )
        self.command("dpkg-deb", 'touch "${PWD}/deb-called"\n')

    def command(self, name, body):
        path = self.root / "bin" / name
        path.write_text("#!/bin/sh\nset -eu\n" + body)
        path.chmod(0o755)

    def binary(self, name, version):
        path = self.root / "target/release" / name
        path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{name} {version}'\n")
        path.chmod(0o755)

    def package(self, arch="amd64"):
        return subprocess.run(
            [str(self.root / "packaging/package.sh"), arch],
            env=self.environment,
            capture_output=True,
            text=True,
        )

    def assert_rejected(self, result):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertFalse((self.root / "deb-called").exists())
        self.assertFalse((self.root / "tar-called").exists())

    def test_wrong_architecture_is_rejected_before_packaging(self):
        self.environment["ELF_MACHINE"] = "b7 00"
        self.assert_rejected(self.package())

    def test_server_version_mismatch_is_rejected_before_packaging(self):
        self.binary("gafctl-server", "0.0.9")
        self.assert_rejected(self.package())

    def test_cli_version_mismatch_is_rejected_before_packaging(self):
        self.binary("gafctl", "0.0.9")
        self.assert_rejected(self.package())

    def test_non_elf_binary_is_rejected_before_packaging(self):
        self.command("od", "echo '23 21 2f 62 69 6e'\n")
        self.assert_rejected(self.package())

    def test_supported_architectures_include_license_notices(self):
        for arch, machine in (("amd64", "3e 00"), ("arm64", "b7 00")):
            with self.subTest(arch=arch):
                self.environment["ELF_MACHINE"] = machine
                result = self.package(arch)
                self.assertEqual(result.returncode, 0, result.stderr)
                licenses = (
                    self.root
                    / f"target/packages/gafctl_0.1.0_{arch}/usr/share/doc/gafctl"
                )
                self.assertEqual(
                    (licenses / "LICENSE").read_text(), "Test project license\n"
                )
                self.assertEqual(
                    (licenses / "LICENSE-QUICKCONNECT-REFERENCE.txt").read_bytes(),
                    (ROOT / "LICENSE-QUICKCONNECT-REFERENCE.txt").read_bytes(),
                )
                self.assertEqual(
                    (licenses / "THIRD-PARTY-NOTICES.txt").read_text(),
                    "Test dependency notices\n",
                )
                self.assertEqual(
                    (licenses / "LICENSE-RUST-STDLIB.html").read_text(),
                    "Test Rust notices\n",
                )
                with tarfile.open(
                    self.root / f"dist/gafctl_0.1.0_linux_{arch}.tar.gz"
                ) as archive:
                    self.assertEqual(
                        set(archive.getnames()),
                        {
                            "gafctl",
                            "gafctl-server",
                            "LICENSE",
                            "LICENSE-QUICKCONNECT-REFERENCE.txt",
                            "THIRD-PARTY-NOTICES.txt",
                            "LICENSE-RUST-STDLIB.html",
                        },
                    )


if __name__ == "__main__":
    unittest.main()
