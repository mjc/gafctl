"""Check release image publication with a local Docker stub."""

import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


class ImageFixture(unittest.TestCase):
    def setUp(self):
        work = ROOT / "target/publish-images-tests"
        work.mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix="run.", dir=work)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.artifacts = self.root / "images with spaces"
        self.artifacts.mkdir()
        self.log = self.root / "docker.jsonl"
        self.registry = self.root / "registry.json"
        self.registry.write_text("{}")
        stub = self.root / "docker"
        stub.write_text(
            "#!/usr/bin/env python3\n"
            "import hashlib, json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['DOCKER_STUB_LOG'], 'a') as log:\n"
            "    log.write(json.dumps(args) + '\\n')\n"
            "state = Path(os.environ['DOCKER_STUB_STATE'])\n"
            "if args[0] == 'load':\n"
            "    architecture = Path(args[2]).read_text()\n"
            "    if architecture != 'untagged':\n"
            "        state.write_text(architecture)\n"
            "elif args[:2] == ['image', 'rm']:\n"
            "    state.unlink()\n"
            "elif args[:2] == ['image', 'inspect']:\n"
            "    if not state.exists():\n"
            "        sys.exit(1)\n"
            "    architecture = state.read_text().strip()\n"
            "    if '--format' in args and args[args.index('--format') + 1] == '{{json .}}':\n"
            "        digest = hashlib.sha256(architecture.encode()).hexdigest()\n"
            "        print(json.dumps({'Architecture': architecture, 'Os': 'linux', 'Id': 'sha256:' + digest}))\n"
            "    else:\n"
            "        print(architecture)\n"
            "elif args[:2] == ['manifest', 'inspect']:\n"
            "    registry = json.loads(Path(os.environ['DOCKER_STUB_REGISTRY']).read_text())\n"
            "    response = registry.get(args[-1], {'error': 'no such manifest: ' + args[-1]})\n"
            "    if isinstance(response, dict) and 'error' in response:\n"
            "        print(response['error'], file=sys.stderr)\n"
            "        sys.exit(1)\n"
            "    print(response.get('raw') if isinstance(response, dict) and 'raw' in response else json.dumps(response))\n"
        )
        stub.chmod(0o755)
        self.environment = {
            **os.environ,
            "PATH": str(self.root) + os.pathsep + os.environ["PATH"],
            "DOCKER_STUB_LOG": str(self.log),
            "DOCKER_STUB_STATE": str(self.root / "architecture"),
            "DOCKER_STUB_REGISTRY": str(self.registry),
        }

    def artifact(self, arch, actual_arch=None):
        directory = self.artifacts / f"image-{arch}"
        directory.mkdir()
        (directory / "gafctl-image.tar").write_text(actual_arch or arch)
        return directory

    def publish(self, *, check=False):
        return subprocess.run(
            [
                str(ROOT / "packaging/publish-images.sh"),
                "ghcr.io/example/gafctl",
                "0.1.0",
                str(self.artifacts),
                *(["--check"] if check else []),
            ],
            cwd=self.root,
            env=self.environment,
            capture_output=True,
            text=True,
        )

    def calls(self):
        return (
            [json.loads(line) for line in self.log.read_text().splitlines()]
            if self.log.exists()
            else []
        )

    def writes(self):
        return [
            call
            for call in self.calls()
            if call[0] == "push"
            or call[:2] in (["manifest", "create"], ["manifest", "push"])
        ]

    def manifest(self, arch, *, config=None, oci=False):
        return {
            "Descriptor": {"platform": {"architecture": arch, "os": "linux"}},
            "OCIManifest" if oci else "SchemaV2Manifest": {
                "config": {
                    "digest": config
                    or "sha256:" + hashlib.sha256(arch.encode()).hexdigest()
                }
            },
        }


class PublishImagesTests(ImageFixture):
    def test_publish_all_artifacts_and_manifest_members(self):
        for arch in ("amd64", "arm64", "ppc64le"):
            self.artifact(arch)
        result = self.publish()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        members = [
            f"ghcr.io/example/gafctl:0.1.0-{arch}"
            for arch in ("amd64", "arm64", "ppc64le")
        ]
        self.assertEqual(
            [call for call in calls if call[0] == "tag"],
            [["tag", "gafctl:local", member] for member in members],
        )
        self.assertEqual(
            [call for call in calls if call[0] == "push"],
            [["push", member] for member in members],
        )
        self.assertEqual(
            calls[-2], ["manifest", "create", "ghcr.io/example/gafctl:0.1.0", *members]
        )
        self.assertEqual(
            calls[-1], ["manifest", "push", "ghcr.io/example/gafctl:0.1.0"]
        )
        self.assertEqual(
            [call for call in calls if call[0] == "load"],
            [
                [
                    "load",
                    "--input",
                    str(self.artifacts / f"image-{arch}/gafctl-image.tar"),
                ]
                for arch in ("amd64", "arm64", "ppc64le")
            ],
        )

    def test_empty_artifact_set_fails_before_docker(self):
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No image artifacts", result.stderr)
        self.assertFalse(self.log.exists())

    def test_invalid_artifact_name_fails_before_docker(self):
        self.artifact("amd64")
        self.artifact("bad-name")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid image artifact directory", result.stderr)
        self.assertFalse(self.log.exists())

    def test_extra_artifact_contents_fail_before_docker(self):
        self.artifact("amd64")
        (self.artifact("arm64") / "unexpected.tar").touch()
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid image artifact contents", result.stderr)
        self.assertFalse(self.log.exists())

    def test_missing_archive_fails_before_docker(self):
        (self.artifacts / "image-amd64").mkdir()
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid image artifact contents", result.stderr)
        self.assertFalse(self.log.exists())

    def test_symlink_archive_fails_before_docker(self):
        outside = self.root / "outside.tar"
        outside.write_text("amd64")
        directory = self.artifacts / "image-amd64"
        directory.mkdir()
        (directory / "gafctl-image.tar").symlink_to(outside)
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid image artifact contents", result.stderr)
        self.assertFalse(self.log.exists())

    def test_architecture_mismatch_does_not_push(self):
        self.artifact("amd64", "arm64")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Image architecture mismatch", result.stderr)
        self.assertEqual([call[0] for call in self.calls()], ["image", "load", "image"])

    def test_untagged_archive_cannot_publish_preexisting_staging_image(self):
        self.artifact("amd64", "untagged")
        Path(self.environment["DOCKER_STUB_STATE"]).write_text("amd64")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("did not load gafctl:local", result.stderr)
        self.assertFalse(
            any(call[0] in ("tag", "push", "manifest") for call in self.calls())
        )
        self.assertIn(["image", "rm", "--force", "gafctl:local"], self.calls())

    def test_later_architecture_mismatch_does_not_push_earlier_image(self):
        self.artifact("amd64")
        self.artifact("arm64", "amd64")
        result = self.publish()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Image architecture mismatch", result.stderr)
        self.assertEqual(self.writes(), [])

    def test_existing_images_and_check_mode(self):
        for arch in ("amd64", "arm64"):
            self.artifact(arch)
        image = "ghcr.io/example/gafctl:0.1.0"
        matching = {
            f"{image}-{arch}": self.manifest(arch, oci=arch == "arm64")
            for arch in ("amd64", "arm64")
        }
        matching[image] = [matching[f"{image}-arm64"], matching[f"{image}-amd64"]]
        for check in (False, True):
            for case, registry, success, writes in (
                ("missing", {}, True, not check),
                ("identical", matching, True, False),
                (
                    "resume",
                    {f"{image}-amd64": matching[f"{image}-amd64"]},
                    True,
                    not check,
                ),
                (
                    "member conflict",
                    {
                        f"{image}-arm64": self.manifest(
                            "arm64", config="sha256:" + "f" * 64
                        )
                    },
                    False,
                    False,
                ),
                ("index conflict", {image: [self.manifest("amd64")]}, False, False),
                (
                    "extra architecture",
                    {
                        image: [
                            self.manifest(arch)
                            for arch in ("amd64", "arm64", "ppc64le")
                        ]
                    },
                    False,
                    False,
                ),
                ("invalid index entry", {image: [*matching[image], {}]}, False, False),
                (
                    "single manifest as index",
                    {image: self.manifest("amd64")},
                    False,
                    False,
                ),
                ("malformed", {image: {"raw": "invalid json"}}, False, False),
                ("null response", {image: None}, False, False),
                ("authentication", {image: {"error": "denied"}}, False, False),
                ("connection", {image: {"error": "connection refused"}}, False, False),
            ):
                with self.subTest(check=check, case=case):
                    self.registry.write_text(json.dumps(registry))
                    self.log.unlink(missing_ok=True)
                    result = self.publish(check=check)
                    self.assertEqual(result.returncode == 0, success, result.stderr)
                    self.assertEqual(bool(self.writes()), writes, self.calls())
                    if case == "resume" and not check:
                        self.assertEqual(
                            [call for call in self.writes() if call[0] == "push"],
                            [["push", f"{image}-arm64"]],
                        )


if __name__ == "__main__":
    unittest.main()
