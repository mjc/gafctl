"""Exercise release validation and publication without external writes."""

import json
import shutil
import subprocess

from test_publish_images import ROOT, ImageFixture


class ReleaseFixture(ImageFixture):
    def setUp(self):
        super().setUp()
        (self.root / "packaging").mkdir()
        for script in (
            "check-release-candidate.sh",
            "version.sh",
            "publish-github-release.sh",
            "publish-images.sh",
            "publish-release.sh",
        ):
            shutil.copyfile(
                ROOT / "packaging" / script, self.root / "packaging" / script
            )
            (self.root / "packaging" / script).chmod(0o755)
        gh = self.root / "gh"
        gh.write_text(
            "#!/usr/bin/env python3\n"
            + (ROOT / "packaging/release_fixture_gh.py").read_text()
        )
        gh.chmod(0o755)
        self.github_log = self.root / "github.jsonl"
        self.github_state = self.root / "github.json"
        self.downloads = self.root / "downloads"
        self.downloads.mkdir()
        self.environment.update(
            GITHUB_REPOSITORY="example/gafctl",
            RELEASE_VERSION="0.1.0",
            RELEASE_TAG="v0.1.0",
            GITHUB_OUTPUT=str(self.root / "output"),
            GITHUB_STUB_LOG=str(self.github_log),
            GITHUB_STUB_STATE=str(self.github_state),
            GITHUB_STUB_DOWNLOADS=str(self.downloads),
        )
        self.state = {"releases": [], "assets": []}
        self.write_state()
        for name, content in (
            ("Cargo.toml", '[package]\nversion = "0.1.0"\n'),
            ("custom_components/gafctl/manifest.json", '{"version":"0.1.0"}'),
            ("home-assistant/config.yaml", 'version: "0.1.0"\n'),
            ("docs/releases/0.1.0.md", "# gafctl 0.1.0\n"),
            (".gitignore", "*\n"),
        ):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content)

    def write_state(self):
        self.github_state.write_text(json.dumps(self.state))

    def run_script(self, script, *args):
        return subprocess.run(
            [str(self.root / "packaging" / script), *args],
            cwd=self.root,
            env=self.environment,
            capture_output=True,
            text=True,
        )

    def github_calls(self):
        return (
            [json.loads(line) for line in self.github_log.read_text().splitlines()]
            if self.github_log.exists()
            else []
        )

    def github_writes(self):
        return [
            call
            for call in self.github_calls()
            if call[:2]
            in (["release", "create"], ["release", "upload"], ["release", "edit"])
        ]


class GithubReleaseTests(ReleaseFixture):
    def setUp(self):
        super().setUp()
        self.dist = self.root / "dist"
        self.dist.mkdir()
        self.assets = [
            name
            for arch in ("amd64", "arm64")
            for name in (
                f"gafctl_0.1.0_{arch}.deb",
                f"gafctl_0.1.0_linux_{arch}.tar.gz",
                f"provenance-{arch}.json",
            )
        ]
        for name in self.assets:
            (self.dist / name).write_text(name)
        checksums = subprocess.run(
            ["sha256sum", *self.assets],
            cwd=self.dist,
            check=True,
            capture_output=True,
            text=True,
        ).stdout
        (self.dist / "SHA256SUMS").write_text(checksums)
        self.assets.append("SHA256SUMS")
        for name in self.assets:
            shutil.copyfile(self.dist / name, self.downloads / name)

    def release(self, *, draft=False, prerelease=False, assets=None):
        self.state.update(
            releases=[
                {
                    "id": 1,
                    "tag_name": "v0.1.0",
                    "draft": draft,
                    "prerelease": prerelease,
                }
            ],
            assets=self.assets if assets is None else assets,
        )
        self.write_state()

    def test_new_draft_and_published_releases(self):
        for state in ("new", "draft", "published"):
            for check in (True, False):
                with self.subTest(state=state, check=check):
                    self.state = {"releases": [], "assets": []}
                    if state != "new":
                        self.release(
                            draft=state == "draft",
                            prerelease=state == "draft",
                            assets=self.assets[:1] if state == "draft" else self.assets,
                        )
                    self.write_state()
                    self.github_log.unlink(missing_ok=True)
                    result = self.run_script(
                        "publish-github-release.sh", *(["--check"] if check else [])
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    writes = self.github_writes()
                    self.assertEqual(
                        bool(writes), not check and state != "published", writes
                    )
                    if writes:
                        self.assertIn("--prerelease=false", writes[-1])
                        self.assertEqual(
                            json.loads(self.github_state.read_text())["releases"][0][
                                "prerelease"
                            ],
                            False,
                        )

    def test_rejected_release_states_do_not_write(self):
        for draft, prerelease, assets in (
            (False, True, self.assets),
            (False, False, self.assets[:-1]),
            (False, False, [*self.assets, "extra.bin"]),
            (True, False, ["extra.bin"]),
        ):
            for check in (True, False):
                with self.subTest(
                    draft=draft, prerelease=prerelease, assets=assets, check=check
                ):
                    self.release(draft=draft, prerelease=prerelease, assets=assets)
                    self.github_log.unlink(missing_ok=True)
                    result = self.run_script(
                        "publish-github-release.sh", *(["--check"] if check else [])
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertEqual(self.github_writes(), [])

    def test_changed_published_bytes_fail_before_image_publication(self):
        self.release()
        (self.downloads / self.assets[0]).write_text("different build")
        result = self.run_script(
            "publish-release.sh", "ghcr.io/example/gafctl", "0.1.0", str(self.artifacts)
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.github_writes(), [])
        self.assertEqual(self.calls(), [])

    def test_registry_failure_prevents_github_publication(self):
        self.artifact("amd64")
        self.registry.write_text(
            json.dumps({"ghcr.io/example/gafctl:0.1.0": {"error": "denied"}})
        )
        result = self.run_script(
            "publish-release.sh", "ghcr.io/example/gafctl", "0.1.0", str(self.artifacts)
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.github_writes(), [])
        self.assertEqual(self.writes(), [])

    def test_complete_publication_and_identical_retry(self):
        image = "ghcr.io/example/gafctl"
        registry = {}
        for arch in ("amd64", "arm64"):
            self.artifact(arch)
            registry[f"{image}:0.1.0-{arch}"] = self.manifest(arch)
        registry[f"{image}:0.1.0"] = list(registry.values())
        result = self.run_script(
            "publish-release.sh", image, "0.1.0", str(self.artifacts)
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.writes()), 4)
        self.assertEqual(
            [call[:2] for call in self.github_writes()], [["release", "create"]]
        )
        self.registry.write_text(json.dumps(registry))
        self.log.unlink()
        self.github_log.unlink()
        result = self.run_script(
            "publish-release.sh", image, "0.1.0", str(self.artifacts)
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.writes(), [])
        self.assertEqual(self.github_writes(), [])

    def test_missing_or_corrupt_local_assets_do_not_write(self):
        for name in (self.assets[0], "SHA256SUMS", "../docs/releases/0.1.0.md"):
            with self.subTest(name=name):
                path = self.dist / name
                original = path.read_bytes()
                path.write_bytes(b"")
                result = self.run_script("publish-github-release.sh")
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.github_writes(), [])
                path.write_bytes(original)
        (self.dist / self.assets[0]).write_text("corrupt nonempty package")
        result = self.run_script("publish-github-release.sh")
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.github_writes(), [])

    def test_api_failure_and_duplicate_releases_do_not_write(self):
        self.release()
        for change in (
            {"api_error": "connection failed"},
            {"releases": self.state["releases"] * 2},
        ):
            with self.subTest(change=change):
                original = self.state.copy()
                self.state.update(change)
                self.write_state()
                result = self.run_script("publish-github-release.sh")
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(self.github_writes(), [])
                self.state = original


class CandidateTests(ReleaseFixture):
    def git(self, *args):
        return subprocess.run(
            ["git", *args], cwd=self.root, check=True, capture_output=True, text=True
        ).stdout.strip()

    def setUp(self):
        super().setUp()
        self.git("init", "--initial-branch=main")
        for key, value in (
            ("user.name", "Release Test"),
            ("user.email", "test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", str(self.root / "no-hooks")),
        ):
            self.git("config", key, value)
        self.git(
            "add",
            "--force",
            "Cargo.toml",
            "custom_components",
            "home-assistant",
            "docs",
            "packaging",
            ".gitignore",
        )
        self.git("commit", "-m", "fixture")
        remote = self.root / "remote.git"
        self.git("init", "--bare", str(remote))
        self.git("remote", "add", "origin", str(remote))
        self.git("tag", "-a", "v0.1.0", "-m", "fixture release")
        self.git("push", "origin", "main", "v0.1.0")
        self.commit = self.git("rev-parse", "HEAD")
        self.tag_object = self.git("rev-parse", "v0.1.0")
        self.state.update(
            tag={
                "verification": {"verified": True},
                "object": {"type": "commit", "sha": self.commit},
                "tag": "v0.1.0",
            },
            commit={"commit": {"verification": {"verified": True}}},
        )
        self.write_state()

    def test_verified_candidate_records_exact_objects(self):
        self.environment.update(
            EXPECTED_RELEASE_COMMIT=self.commit,
            EXPECTED_RELEASE_TAG_OBJECT=self.tag_object,
        )
        result = self.run_script("check-release-candidate.sh", "v0.1.0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            (self.root / "output").read_text(),
            f"commit={self.commit}\nversion=0.1.0\ntag_object={self.tag_object}\n",
        )

    def test_rejects_invalid_candidate_metadata(self):
        cases = (
            (
                "unverified tag",
                {"tag": {**self.state["tag"], "verification": {"verified": False}}},
                {},
            ),
            (
                "unverified commit",
                {"commit": {"commit": {"verification": {"verified": False}}}},
                {},
            ),
            ("wrong tag name", {"tag": {**self.state["tag"], "tag": "v0.2.0"}}, {}),
            (
                "wrong tag target type",
                {
                    "tag": {
                        **self.state["tag"],
                        "object": {"type": "tag", "sha": self.commit},
                    }
                },
                {},
            ),
            (
                "wrong api commit",
                {
                    "tag": {
                        **self.state["tag"],
                        "object": {"type": "commit", "sha": "0" * 40},
                    }
                },
                {},
            ),
            ("changed tag", {}, {"EXPECTED_RELEASE_TAG_OBJECT": "0" * 40}),
            ("changed commit", {}, {"EXPECTED_RELEASE_COMMIT": "0" * 40}),
            ("api unavailable", {"api_error": "connection failed"}, {}),
        )
        for case, state, environment in cases:
            with self.subTest(case=case):
                original_state, original_env = (
                    self.state.copy(),
                    self.environment.copy(),
                )
                self.state.update(state)
                self.environment.update(environment)
                self.write_state()
                result = self.run_script("check-release-candidate.sh", "v0.1.0")
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "output").exists())
                self.state, self.environment = original_state, original_env

    def test_rejects_invalid_tag_names_and_dirty_checkout(self):
        for tag in ("0.1.0", "v01.1.0", "v0.1.0-rc.1", "v0.1.0+build"):
            with self.subTest(tag=tag):
                self.assertNotEqual(
                    self.run_script("check-release-candidate.sh", tag).returncode, 0
                )
        (self.root / "Cargo.toml").write_text("modified")
        result = self.run_script("check-release-candidate.sh", "v0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be clean", result.stderr)

    def test_rejects_lightweight_tag(self):
        self.git("tag", "--delete", "v0.1.0")
        self.git("tag", "v0.1.0")
        self.git("push", "--force", "origin", "v0.1.0")
        result = self.run_script("check-release-candidate.sh", "v0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("annotated and signed", result.stderr)

    def test_rejects_commit_outside_main(self):
        self.git("checkout", "-b", "unmerged")
        self.git("commit", "--allow-empty", "-m", "unmerged")
        self.git("tag", "--force", "-a", "v0.1.0", "-m", "unmerged")
        self.git("push", "--force", "origin", "v0.1.0")
        self.state["tag"]["object"]["sha"] = self.git("rev-parse", "HEAD")
        self.write_state()
        result = self.run_script("check-release-candidate.sh", "v0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be on main", result.stderr)

    def test_rejects_version_mismatch_and_missing_notes(self):
        for name, content in (
            ("Cargo.toml", '[package]\nversion = "0.2.0"\n'),
            ("custom_components/gafctl/manifest.json", '{"version":"0.2.0"}'),
            ("home-assistant/config.yaml", 'version: "0.2.0"\n'),
            ("docs/releases/0.1.0.md", ""),
        ):
            with self.subTest(name=name):
                path = self.root / name
                original = path.read_text()
                path.write_text(content)
                self.git("commit", "-am", "invalid candidate")
                self.git("tag", "--force", "-a", "v0.1.0", "-m", "fixture")
                self.git("push", "--force", "origin", "main", "v0.1.0")
                self.state["tag"]["object"]["sha"] = self.git("rev-parse", "HEAD")
                self.write_state()
                result = self.run_script("check-release-candidate.sh", "v0.1.0")
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse((self.root / "output").exists())
                path.write_text(original)

    def test_rejects_checkout_not_at_tag_and_fetch_failure(self):
        self.git("commit", "--allow-empty", "-m", "ahead of tag")
        result = self.run_script("check-release-candidate.sh", "v0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checked-out commit", result.stderr)
        self.git("remote", "set-url", "origin", str(self.root / "missing.git"))
        result = self.run_script("check-release-candidate.sh", "v0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse((self.root / "output").exists())
