import copy
import contextlib
import importlib.util
import io
import json
import pathlib
import shutil
import sys
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parents[2]
FIXTURES = pathlib.Path(__file__).with_name("fixtures")
UPDATER = ROOT / "scripts" / "update-managed-tools.py"


def load_updater():
    spec = importlib.util.spec_from_file_location("update_managed_tools", UPDATER)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"could not load {UPDATER}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def fixture(name):
    return json.loads((FIXTURES / name).read_text())


def copy_repository(directory):
    repo = pathlib.Path(directory) / "repo"
    shutil.copytree(FIXTURES / "repository", repo)
    return repo


def snapshot(repo):
    return {
        path.relative_to(repo): path.read_bytes()
        for path in repo.rglob("*")
        if path.is_file()
    }


class UpdateManagedToolsTests(unittest.TestCase):
    def test_github_token_is_sent_only_to_the_github_api_without_redirecting(self):
        updater = load_updater()
        requests = []

        def respond(request):
            requests.append(request)
            return io.BytesIO(b"{}")

        with (
            mock.patch.dict(updater.os.environ, {"GITHUB_TOKEN": "secret"}),
            mock.patch.object(updater.urllib.request, "urlopen", side_effect=respond),
        ):
            updater._request_json("https://api.github.com/repos/owner/repo/releases")
            updater._request_json("https://pypi.org/pypi/project/json")

        self.assertEqual(requests[0].get_header("Authorization"), "Bearer secret")
        self.assertIn("Authorization", requests[0].unredirected_hdrs)
        self.assertIsNone(requests[1].get_header("Authorization"))

    def test_release_validation_checks_the_committed_pins_not_moving_latest_versions(self):
        updater = load_updater()
        with tempfile.TemporaryDirectory() as directory:
            repo = copy_repository(directory)
            with mock.patch.object(updater, "_validate_upstream_version") as validate:
                versions = updater.validate_pinned_versions(repo)

        self.assertEqual(versions["air"], "0.10.0")
        self.assertEqual(versions["ruff"], "0.14.0")
        validate.assert_has_calls(
            [mock.call(name, versions[name]) for name in updater.TOOLS]
        )

    def test_latest_release_parsers_choose_stable_versions(self):
        updater = load_updater()

        self.assertEqual(
            updater.latest_github_version(fixture("github-releases.json")),
            "1.4.0",
        )
        self.assertEqual(
            updater.latest_pypi_version(fixture("pypi-project.json")),
            "4.2.0",
        )

    def test_release_assets_require_every_platform_and_checksum(self):
        updater = load_updater()
        assets = fixture("panache-assets.json")
        checksums = (FIXTURES / "panache-SHA256SUMS.txt").read_text()

        updater.validate_release_assets("panache", "3.14.0", assets)
        updater.validate_checksum_contents("panache", "3.14.0", assets, checksums)
        with self.assertRaisesRegex(updater.UpdateError, "SHA256SUMS"):
            updater.validate_release_assets(
                "panache", "3.14.0", [name for name in assets if name != "SHA256SUMS"]
            )
        with self.assertRaisesRegex(updater.UpdateError, "aarch64-pc-windows-msvc"):
            updater.validate_release_assets(
                "panache",
                "3.14.0",
                [name for name in assets if "aarch64-pc-windows-msvc" not in name],
            )
        with self.assertRaisesRegex(updater.UpdateError, "aarch64-pc-windows-msvc"):
            updater.validate_checksum_contents(
                "panache",
                "3.14.0",
                assets,
                checksums.replace(
                    "6666666666666666666666666666666666666666666666666666666666666666  "
                    "panache-aarch64-pc-windows-msvc.zip\n",
                    "",
                ),
            )
        with self.assertRaisesRegex(updater.UpdateError, "checksum"):
            updater.validate_checksum_contents(
                "panache",
                "3.14.0",
                assets,
                checksums.replace("1111111111111111", "not-a-checksum", 1),
            )
        digest = "a" * 64
        archive = "air-x86_64-apple-darwin.tar.gz"
        updater.validate_single_checksum(archive, digest)
        updater.validate_single_checksum(archive, f"{digest}  {archive}\n")
        with self.assertRaisesRegex(updater.UpdateError, "different archive"):
            updater.validate_single_checksum(archive, f"{digest}  another.tar.gz\n")

    def test_current_versions_are_a_no_op(self):
        updater = load_updater()
        with tempfile.TemporaryDirectory() as directory:
            repo = copy_repository(directory)
            before = snapshot(repo)

            result = updater.apply_updates(
                repo,
                {"air": "0.10.0", "deptry": "0.25.1"},
                installed_on="2026-10-08",
            )

            self.assertFalse(result.changed)
            self.assertEqual(result.changed_files, ())
            self.assertEqual(snapshot(repo), before)

    def test_downgrades_and_invalid_versions_are_rejected_without_writes(self):
        updater = load_updater()
        for latest in (
            {"air": "0.9.9", "deptry": "0.25.1"},
            {"air": "next", "deptry": "0.25.1"},
        ):
            with self.subTest(latest=latest), tempfile.TemporaryDirectory() as directory:
                repo = copy_repository(directory)
                before = snapshot(repo)
                with self.assertRaises(updater.UpdateError):
                    updater.apply_updates(repo, latest, installed_on="2026-10-08")
                self.assertEqual(snapshot(repo), before)

    def test_missing_mirror_prevents_partial_writes(self):
        updater = load_updater()
        with tempfile.TemporaryDirectory() as directory:
            repo = copy_repository(directory)
            repo.joinpath("crates/togi/tests/tools.rs").write_text("missing mirrors\n")
            before = snapshot(repo)

            with self.assertRaisesRegex(updater.UpdateError, "tools.rs"):
                updater.apply_updates(
                    repo,
                    {"air": "0.11.0", "deptry": "0.26.0"},
                    installed_on="2026-10-08",
                )

            self.assertEqual(snapshot(repo), before)

    def test_updates_all_mirrors_and_patch_bumps_the_release(self):
        updater = load_updater()
        with tempfile.TemporaryDirectory() as directory:
            repo = copy_repository(directory)

            result = updater.apply_updates(
                repo,
                {"air": "0.11.0", "deptry": "0.26.0"},
                installed_on="2026-10-08",
            )

            self.assertTrue(result.changed)
            self.assertEqual(result.previous_release, "1.2.3")
            self.assertEqual(result.next_release, "1.2.4")
            self.assertEqual(
                set(result.changed_files),
                {
                    "Cargo.lock",
                    "Cargo.toml",
                    "README.md",
                    "crates/togi-core/src/tools/versions.rs",
                    "crates/togi/tests/format_lint.rs",
                    "crates/togi/tests/tools.rs",
                },
            )
            for relative in (
                "crates/togi-core/src/tools/versions.rs",
                "crates/togi/tests/format_lint.rs",
                "crates/togi/tests/tools.rs",
            ):
                text = repo.joinpath(relative).read_text()
                self.assertIn('"0.11.0"', text)
                self.assertIn('"0.26.0"', text)
            readme = repo.joinpath("README.md").read_text()
            self.assertIn("air        0.11.0", readme)
            self.assertIn("deptry     0.26.0", readme)
            self.assertIn("installed 2026-10-08", readme)
            self.assertIn("togi 1.2.4", readme)
            self.assertIn("  air 0.11.0", readme)
            self.assertIn("  deptry 0.26.0", readme)
            self.assertIn('version = "1.2.4"', repo.joinpath("Cargo.toml").read_text())
            self.assertIn(
                'togi-core = { path = "crates/togi-core", version = "1.2.4" }',
                repo.joinpath("Cargo.toml").read_text(),
            )
            self.assertEqual(
                repo.joinpath("Cargo.lock").read_text().count('version = "1.2.4"'), 2
            )

            after_first_update = snapshot(repo)
            repeated = updater.apply_updates(
                repo,
                {"air": "0.11.0", "deptry": "0.26.0"},
                installed_on="2026-11-08",
            )
            self.assertFalse(repeated.changed)
            self.assertEqual(snapshot(repo), after_first_update)

    def test_release_plan_is_versioned_idempotent_and_retryable(self):
        updater = load_updater()
        sha = "a" * 40

        for workspace, latest in (("1.2.3", "1.2.3"), ("1.2.2", "1.2.3")):
            with self.subTest(workspace=workspace, latest=latest):
                plan = updater.plan_release(
                    workspace_version=workspace,
                    latest_release=latest,
                    merge_sha=sha,
                    existing_tag_sha=None,
                    release_exists=False,
                )
                self.assertFalse(plan.create_tag)
                self.assertFalse(plan.dispatch_release)

        complete = updater.plan_release(
            workspace_version="1.2.4",
            latest_release="1.2.3",
            merge_sha=sha,
            existing_tag_sha=sha,
            release_exists=True,
        )
        self.assertFalse(complete.create_tag)
        self.assertFalse(complete.dispatch_release)

        retry = updater.plan_release(
            workspace_version="1.2.4",
            latest_release="1.2.3",
            merge_sha=sha,
            existing_tag_sha=sha,
            release_exists=False,
        )
        self.assertFalse(retry.create_tag)
        self.assertTrue(retry.dispatch_release)

        first = updater.plan_release(
            workspace_version="1.2.4",
            latest_release="1.2.3",
            merge_sha=sha,
            existing_tag_sha=None,
            release_exists=False,
        )
        self.assertTrue(first.create_tag)
        self.assertTrue(first.dispatch_release)

        with self.assertRaisesRegex(updater.UpdateError, "different commit"):
            updater.plan_release(
                workspace_version="1.2.4",
                latest_release="1.2.3",
                merge_sha=sha,
                existing_tag_sha="b" * 40,
                release_exists=False,
            )

        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory, "github-output")
            with contextlib.redirect_stdout(io.StringIO()):
                result = updater.main(
                    [
                        "plan-release",
                        "--workspace-version",
                        "1.2.4",
                        "--latest-release",
                        "v1.2.3",
                        "--merge-sha",
                        sha,
                        "--existing-tag-sha",
                        sha,
                        "--release-missing",
                        "--github-output",
                        str(output),
                    ]
                )
            self.assertEqual(result, 0)
            self.assertEqual(
                output.read_text(),
                "tag=v1.2.4\ncreate_tag=false\ndispatch_release=true\n",
            )

    def test_only_the_exact_merged_updater_pr_qualifies_for_release(self):
        updater = load_updater()
        sha = "a" * 40
        pull_request = {
            "merged": True,
            "labels": [{"name": "managed-tool-update"}],
            "base": {"ref": "main"},
            "merge_commit_sha": sha,
        }

        self.assertTrue(updater.qualifies_for_release(pull_request, sha))

        rejected = {
            "unmerged": {"merged": False},
            "unlabelled": {"labels": []},
            "wrong base": {"base": {"ref": "release"}},
            "SHA mismatch": {"merge_commit_sha": "b" * 40},
        }
        for reason, replacement in rejected.items():
            with self.subTest(reason=reason):
                candidate = copy.deepcopy(pull_request)
                candidate.update(replacement)
                self.assertFalse(updater.qualifies_for_release(candidate, sha))

        with tempfile.TemporaryDirectory() as directory:
            pull_requests = pathlib.Path(directory, "pulls.json")
            output = pathlib.Path(directory, "github-output")
            pull_requests.write_text(json.dumps([{"merged": False}, pull_request]))
            with contextlib.redirect_stdout(io.StringIO()):
                result = updater.main(
                    [
                        "qualify-pr",
                        "--pull-requests",
                        str(pull_requests),
                        "--merge-sha",
                        sha,
                        "--github-output",
                        str(output),
                    ]
                )
            self.assertEqual(result, 0)
            self.assertEqual(output.read_text(), "eligible=true\n")


if __name__ == "__main__":
    unittest.main()
