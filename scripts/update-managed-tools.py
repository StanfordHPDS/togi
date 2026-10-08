#!/usr/bin/env python3
"""Discover managed-tool releases and update their checked-in versions."""

import argparse
import dataclasses
import datetime
import json
import os
import pathlib
import re
import subprocess
import sys
import urllib.error
import urllib.parse
import urllib.request


class UpdateError(RuntimeError):
    pass


@dataclasses.dataclass(frozen=True)
class UpdateResult:
    changed: bool
    changed_files: tuple[str, ...]
    previous_release: str
    next_release: str


@dataclasses.dataclass(frozen=True)
class ReleasePlan:
    create_tag: bool
    dispatch_release: bool


PLATFORMS = (
    ("x86_64", "apple-darwin", "tar.gz"),
    ("aarch64", "apple-darwin", "tar.gz"),
    ("x86_64", "unknown-linux-gnu", "tar.gz"),
    ("aarch64", "unknown-linux-gnu", "tar.gz"),
    ("x86_64", "pc-windows-msvc", "zip"),
    ("aarch64", "pc-windows-msvc", "zip"),
)

TOOLS = {
    "air": {"source": "github", "project": "posit-dev/air", "checksum": "per-asset"},
    "deptry": {"source": "pypi", "project": "deptry"},
    "ruff": {"source": "github", "project": "astral-sh/ruff", "checksum": "per-asset"},
    "panache": {"source": "github", "project": "jolars/panache", "checksum": "SHA256SUMS"},
    "sqlfluff": {"source": "pypi", "project": "sqlfluff"},
    "uv": {"source": "github", "project": "astral-sh/uv", "checksum": "per-asset"},
}

FILES = (
    "Cargo.lock",
    "Cargo.toml",
    "README.md",
    "crates/togi-core/src/tools/versions.rs",
    "crates/togi/tests/format_lint.rs",
    "crates/togi/tests/tools.rs",
)

OPTIONAL_FILES = (
    "crates/togi-core/src/adapters/panache.rs",
    "docs/togi.toml.md",
)

RELEASE_DIFF_ALLOWLIST = frozenset(
    {
        "Cargo.lock",
        "Cargo.toml",
        "README.md",
        "docs/togi.toml.md",
        "crates/togi-core/src/adapters/panache.rs",
        "crates/togi-core/src/tools/versions.rs",
        "crates/togi/tests/format_lint.rs",
        "crates/togi/tests/tools.rs",
    }
)


def _version(value):
    match = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)", value.strip())
    if not match:
        raise UpdateError(f"invalid stable version: {value!r}")
    return tuple(int(part) for part in match.groups())


def _version_text(value):
    return ".".join(str(part) for part in _version(value))


def latest_github_version(releases):
    stable = []
    for release in releases:
        if release.get("draft") or release.get("prerelease"):
            continue
        try:
            stable.append((_version(release["tag_name"]), release))
        except (KeyError, UpdateError):
            continue
    if not stable:
        raise UpdateError("GitHub returned no stable semantic-version release")
    return ".".join(str(part) for part in max(stable, key=lambda item: item[0])[0])


def latest_pypi_version(project):
    try:
        releases = project["releases"]
    except (KeyError, TypeError) as error:
        raise UpdateError("PyPI response has no releases") from error
    stable = []
    for value, files in releases.items():
        if not files or all(file.get("yanked", False) for file in files):
            continue
        try:
            stable.append(_version(value))
        except UpdateError:
            continue
    if not stable:
        raise UpdateError("PyPI returned no stable semantic-version release")
    return ".".join(str(part) for part in max(stable))


def _archives(tool):
    return [f"{tool}-{arch}-{os_name}.{extension}" for arch, os_name, extension in PLATFORMS]


def validate_release_assets(tool, version, assets):
    _version(version)
    if tool not in TOOLS or TOOLS[tool]["source"] != "github":
        raise UpdateError(f"{tool} is not a managed GitHub release tool")
    available = set(assets)
    expected = _archives(tool)
    checksum = TOOLS[tool]["checksum"]
    expected.extend(
        ["SHA256SUMS"] if checksum == "SHA256SUMS" else [f"{name}.sha256" for name in expected]
    )
    missing = [name for name in expected if name not in available]
    if missing:
        raise UpdateError(f"{tool} {version} is missing release assets: {', '.join(missing)}")


def validate_checksum_contents(tool, version, assets, contents):
    validate_release_assets(tool, version, assets)
    archives = _archives(tool)
    entries = {}
    for line in contents.splitlines():
        parts = line.split()
        if len(parts) >= 2 and re.fullmatch(r"[0-9a-fA-F]{64}", parts[0]):
            entries[parts[-1].lstrip("*")] = parts[0]
        elif line.strip():
            raise UpdateError(f"invalid checksum line for {tool} {version}: {line!r}")
    missing = [name for name in archives if name not in entries]
    if missing:
        raise UpdateError(f"checksum contents are missing: {', '.join(missing)}")


def validate_single_checksum(archive, contents):
    parts = contents.strip().split()
    if not parts or not re.fullmatch(r"[0-9a-fA-F]{64}", parts[0]):
        raise UpdateError(f"invalid checksum contents for {archive}")
    if len(parts) == 1:
        return
    if len(parts) == 2 and parts[1].lstrip("*") == archive:
        return
    raise UpdateError(f"checksum for {archive} names a different archive")


def _replace_once(text, pattern, replacement, path):
    changed, count = re.subn(pattern, replacement, text, count=1, flags=re.MULTILINE)
    if count != 1:
        raise UpdateError(f"could not find the expected version mirror in {path}")
    return changed


def _current_versions(text):
    return {
        name.lower(): version
        for name, version in re.findall(r'^pub const ([A-Z]+): &str = "([^"]+)";', text, re.MULTILINE)
    }


def _update_package_block(text, package, old, new, path):
    pattern = rf'(\[\[package\]\]\nname = "{re.escape(package)}"\nversion = "){re.escape(old)}("\n)'
    return _replace_once(text, pattern, rf"\g<1>{new}\g<2>", path)


def apply_updates(repo, latest, installed_on):
    repo = pathlib.Path(repo)
    unknown = set(latest) - set(TOOLS)
    if unknown:
        raise UpdateError(f"unknown managed tools: {', '.join(sorted(unknown))}")
    original = {}
    for relative in FILES:
        path = repo / relative
        try:
            original[relative] = path.read_text()
        except OSError as error:
            raise UpdateError(f"could not read {relative}: {error}") from error
    for relative in OPTIONAL_FILES:
        path = repo / relative
        if path.is_file():
            original[relative] = path.read_text()

    current = _current_versions(original["crates/togi-core/src/tools/versions.rs"])
    for name, candidate in latest.items():
        candidate_tuple = _version(candidate)
        if name not in current:
            raise UpdateError(f"could not find {name} in versions.rs")
        if candidate_tuple < _version(current[name]):
            raise UpdateError(f"refusing to downgrade {name} from {current[name]} to {candidate}")

    changed_tools = {name: _version_text(value) for name, value in latest.items() if _version_text(value) != current[name]}
    release_match = re.search(r'^version = "(\d+\.\d+\.\d+)"$', original["Cargo.toml"], re.MULTILINE)
    if not release_match:
        raise UpdateError("could not find workspace release version in Cargo.toml")
    previous_release = release_match.group(1)
    if not changed_tools:
        return UpdateResult(False, (), previous_release, previous_release)

    major, minor, patch = _version(previous_release)
    next_release = f"{major}.{minor}.{patch + 1}"
    updated = dict(original)

    for name, new in changed_tools.items():
        old = current[name]
        constant = name.upper()
        versions_path = "crates/togi-core/src/tools/versions.rs"
        updated[versions_path] = _replace_once(
            updated[versions_path],
            rf'^(pub const {constant}: &str = "){re.escape(old)}(";)$',
            rf"\g<1>{new}\g<2>",
            versions_path,
        )
        for relative in ("crates/togi/tests/tools.rs", "crates/togi/tests/format_lint.rs"):
            marker = rf'^(const {constant}_DEFAULT: &str = "){re.escape(old)}(";)$'
            if re.search(marker, updated[relative], re.MULTILINE):
                updated[relative] = _replace_once(updated[relative], marker, rf"\g<1>{new}\g<2>", relative)
            elif relative.endswith("tools.rs") or name != "uv":
                raise UpdateError(f"could not find the expected version mirror in {relative}")

        readme = updated["README.md"]
        list_pattern = rf'^({re.escape(name)}\s+){re.escape(old)}(\s+.+?installed )\d{{4}}-\d{{2}}-\d{{2}}$'
        readme = _replace_once(readme, list_pattern, rf"\g<1>{new}\g<2>{installed_on}", "README.md")
        readme = _replace_once(
            readme,
            rf'^(  {re.escape(name)} ){re.escape(old)}$',
            rf"\g<1>{new}",
            "README.md",
        )
        updated["README.md"] = readme
        if name == "panache" and OPTIONAL_FILES[0] in updated:
            updated[OPTIONAL_FILES[0]] = _replace_once(
                updated[OPTIONAL_FILES[0]],
                rf"(panache CLI contract \(verified against panache ){re.escape(old)}(\))",
                rf"\g<1>{new}\g<2>",
                OPTIONAL_FILES[0],
            )
        docs_path = "docs/togi.toml.md"
        if docs_path in updated:
            docs = updated[docs_path]
            docs = re.sub(
                rf'\b({re.escape(name)} = "){re.escape(old)}(")',
                rf"\g<1>{new}\g<2>",
                docs,
            )
            section = re.compile(
                rf'(\[tools\.{re.escape(name)}\][^\[]*?\nversion = "){re.escape(old)}(")',
                re.MULTILINE,
            )
            docs = section.sub(rf"\g<1>{new}\g<2>", docs)
            docs = re.sub(
                rf"(\b{re.escape(name)}\s+){re.escape(old)}\b",
                rf"\g<1>{new}",
                docs,
            )
            updated[docs_path] = docs

    cargo = updated["Cargo.toml"]
    cargo = _replace_once(
        cargo,
        rf'^(version = "){re.escape(previous_release)}("$)',
        rf"\g<1>{next_release}\g<2>",
        "Cargo.toml",
    )
    cargo = _replace_once(
        cargo,
        rf'^(togi-core = \{{ path = "crates/togi-core", version = "){re.escape(previous_release)}(" \}}$)',
        rf"\g<1>{next_release}\g<2>",
        "Cargo.toml",
    )
    updated["Cargo.toml"] = cargo
    updated["README.md"] = _replace_once(
        updated["README.md"],
        rf'^(togi ){re.escape(previous_release)}$',
        rf"\g<1>{next_release}",
        "README.md",
    )
    lock = updated["Cargo.lock"]
    lock = _update_package_block(lock, "togi", previous_release, next_release, "Cargo.lock")
    lock = _update_package_block(lock, "togi-core", previous_release, next_release, "Cargo.lock")
    updated["Cargo.lock"] = lock

    changed_files = tuple(
        relative for relative in original if updated[relative] != original[relative]
    )
    for relative in changed_files:
        (repo / relative).write_text(updated[relative])
    return UpdateResult(True, changed_files, previous_release, next_release)


def qualifies_for_release(pull_request, merge_sha):
    labels = {item.get("name") for item in pull_request.get("labels", []) if isinstance(item, dict)}
    return bool(
        (pull_request.get("merged") is True or pull_request.get("merged_at") is not None)
        and "managed-tool-update" in labels
        and pull_request.get("base", {}).get("ref") == "main"
        and pull_request.get("merge_commit_sha") == merge_sha
    )


def qualifying_pull_request(pull_requests, merge_sha):
    return next(
        (item for item in pull_requests if qualifies_for_release(item, merge_sha)),
        None,
    )


def validate_diff(repo, merge_sha):
    if not re.fullmatch(r"[0-9a-fA-F]{40}", merge_sha):
        raise UpdateError(f"invalid merge commit SHA: {merge_sha!r}")
    try:
        completed = subprocess.run(
            ["git", "diff", "--name-only", "-z", f"{merge_sha}^1", merge_sha],
            cwd=repo,
            check=True,
            capture_output=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise UpdateError(f"could not inspect merge commit {merge_sha}") from error
    try:
        names = tuple(
            name.decode("utf-8")
            for name in completed.stdout.split(b"\0")
            if name
        )
    except UnicodeDecodeError as error:
        raise UpdateError("merge contains a path that is not valid UTF-8") from error
    unexpected = sorted(set(names) - RELEASE_DIFF_ALLOWLIST)
    if unexpected:
        raise UpdateError(
            "managed-tool update changes paths outside the release allowlist: "
            + ", ".join(unexpected)
        )
    return names


def plan_release(workspace_version, latest_release, merge_sha, existing_tag_sha, release_exists):
    workspace = _version(workspace_version)
    latest = _version(latest_release)
    if workspace <= latest:
        return ReleasePlan(False, False)
    if existing_tag_sha is not None and existing_tag_sha != merge_sha:
        raise UpdateError("release tag points at a different commit")
    if release_exists:
        return ReleasePlan(False, False)
    return ReleasePlan(existing_tag_sha is None, True)


def _workspace_version(repo):
    cargo = pathlib.Path(repo, "Cargo.toml").read_text()
    match = re.search(r'^version = "(\d+\.\d+\.\d+)"$', cargo, re.MULTILINE)
    if not match:
        raise UpdateError("could not find workspace release version in Cargo.toml")
    return match.group(1)


def _existing_tag_sha(repo, tag):
    completed = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"refs/tags/{tag}^{{commit}}"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    if completed.returncode == 1:
        return None
    if completed.returncode != 0:
        raise UpdateError(f"could not inspect local tag {tag}")
    return completed.stdout.strip()


def _release_exists(tag):
    repository = os.environ.get("GITHUB_REPOSITORY")
    if not repository:
        raise UpdateError("GITHUB_REPOSITORY is required to query release state")
    try:
        _request_json(f"https://api.github.com/repos/{repository}/releases/tags/{tag}")
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return False
        raise
    return True


def _write_github_output(path, values):
    if path is None:
        return
    with pathlib.Path(path).open("a") as output:
        for name, value in values.items():
            if isinstance(value, bool):
                value = str(value).lower()
            output.write(f"{name}={value}\n")


def _request_json(url):
    headers = {"Accept": "application/vnd.github+json", "User-Agent": "togi-tool-updater"}
    request = urllib.request.Request(url, headers=headers)
    token = os.environ.get("GITHUB_TOKEN")
    parsed = urllib.parse.urlsplit(url)
    if token and parsed.scheme == "https" and parsed.hostname == "api.github.com":
        # Unredirected headers are deliberately omitted from every redirected
        # request, preventing credentials from crossing origins.
        request.add_unredirected_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def _request_text(url):
    request = urllib.request.Request(url, headers={"User-Agent": "togi-tool-updater"})
    with urllib.request.urlopen(request) as response:
        return response.read().decode()


def _validate_upstream_version(name, version):
    metadata = TOOLS[name]
    _version(version)
    if metadata["source"] == "pypi":
        payload = _request_json(
            f"https://pypi.org/pypi/{metadata['project']}/{version}/json"
        )
        published = payload.get("info", {}).get("version")
        files = payload.get("urls", [])
        if published is None or _version_text(published) != version:
            raise UpdateError(f"PyPI did not return {name} {version}")
        if not files or all(file.get("yanked", False) for file in files):
            raise UpdateError(f"PyPI has no usable files for {name} {version}")
        return

    release = None
    for tag in (version, f"v{version}"):
        try:
            candidate = _request_json(
                f"https://api.github.com/repos/{metadata['project']}/releases/tags/{tag}"
            )
        except urllib.error.HTTPError as error:
            if error.code == 404:
                continue
            raise
        if candidate.get("draft") or candidate.get("prerelease"):
            continue
        try:
            if _version_text(candidate["tag_name"]) == version:
                release = candidate
                break
        except (KeyError, UpdateError):
            continue
    if release is None:
        raise UpdateError(f"GitHub did not return a stable release for {name} {version}")

    assets = {item["name"]: item["browser_download_url"] for item in release.get("assets", [])}
    validate_release_assets(name, version, assets)
    if metadata["checksum"] == "SHA256SUMS":
        validate_checksum_contents(name, version, assets, _request_text(assets["SHA256SUMS"]))
    else:
        for archive in _archives(name):
            validate_single_checksum(archive, _request_text(assets[f"{archive}.sha256"]))


def validate_pinned_versions(repo):
    versions_path = pathlib.Path(repo, "crates/togi-core/src/tools/versions.rs")
    versions = _current_versions(versions_path.read_text())
    missing = sorted(set(TOOLS) - set(versions))
    if missing:
        raise UpdateError(f"missing managed-tool pins: {', '.join(missing)}")
    for name in TOOLS:
        _validate_upstream_version(name, versions[name])
    return versions


def discover_versions():
    versions = {}
    for name, metadata in TOOLS.items():
        if metadata["source"] == "pypi":
            payload = _request_json(f"https://pypi.org/pypi/{metadata['project']}/json")
            versions[name] = latest_pypi_version(payload)
            continue
        releases = _request_json(f"https://api.github.com/repos/{metadata['project']}/releases?per_page=100")
        version = latest_github_version(releases)
        release = None
        for item in releases:
            if item.get("draft") or item.get("prerelease"):
                continue
            try:
                matches = _version_text(item["tag_name"]) == version
            except (KeyError, UpdateError):
                continue
            if matches:
                release = item
                break
        if release is None:
            raise UpdateError(f"could not find GitHub metadata for {name} {version}")
        assets = {item["name"]: item["browser_download_url"] for item in release.get("assets", [])}
        validate_release_assets(name, version, assets)
        if metadata["checksum"] == "SHA256SUMS":
            validate_checksum_contents(name, version, assets, _request_text(assets["SHA256SUMS"]))
        else:
            for archive in _archives(name):
                text = _request_text(assets[f"{archive}.sha256"])
                validate_single_checksum(archive, text)
        versions[name] = version
    return versions


def _json_result(value):
    print(json.dumps(dataclasses.asdict(value), sort_keys=True))


def main(argv=None):
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    update = commands.add_parser("update")
    update.add_argument("--repo", type=pathlib.Path, default=pathlib.Path("."))
    update.add_argument("--installed-on", default=datetime.date.today().isoformat())
    validate_pins = commands.add_parser("validate-pins")
    validate_pins.add_argument("--repo", type=pathlib.Path, default=pathlib.Path("."))
    qualify = commands.add_parser("qualify-pr")
    qualify_input = qualify.add_mutually_exclusive_group(required=True)
    qualify_input.add_argument("--pull-request")
    qualify_input.add_argument("--pull-requests", type=pathlib.Path)
    qualify.add_argument("--merge-sha", required=True)
    qualify.add_argument("--github-output", type=pathlib.Path)
    validate = commands.add_parser("validate-diff")
    validate.add_argument("--repo", type=pathlib.Path, default=pathlib.Path("."))
    validate.add_argument("--merge-sha", required=True)
    plan = commands.add_parser("plan-release")
    plan.add_argument("--repo", type=pathlib.Path, default=pathlib.Path("."))
    plan.add_argument("--workspace-version")
    plan.add_argument("--latest-release", required=True)
    plan.add_argument("--merge-sha", required=True)
    plan.add_argument("--existing-tag-sha")
    release_state = plan.add_mutually_exclusive_group()
    release_state.add_argument("--release-exists", action="store_true", default=None)
    release_state.add_argument("--release-missing", action="store_false", dest="release_exists")
    plan.add_argument("--github-output", type=pathlib.Path)
    args = parser.parse_args(argv)

    if args.command == "update":
        _json_result(apply_updates(args.repo, discover_versions(), args.installed_on))
    elif args.command == "validate-pins":
        print(json.dumps(validate_pinned_versions(args.repo), sort_keys=True))
    elif args.command == "qualify-pr":
        if args.pull_requests is not None:
            candidates = json.loads(args.pull_requests.read_text())
            if not isinstance(candidates, list):
                raise UpdateError("--pull-requests must contain a JSON list")
        else:
            if args.pull_request.lstrip().startswith("{"):
                raw = args.pull_request
            else:
                raw = pathlib.Path(args.pull_request).read_text()
            candidates = [json.loads(raw)]
        eligible = qualifying_pull_request(candidates, args.merge_sha) is not None
        _write_github_output(args.github_output, {"eligible": eligible})
        print(json.dumps({"eligible": eligible}))
    elif args.command == "validate-diff":
        names = validate_diff(args.repo, args.merge_sha)
        print(json.dumps({"valid": True, "changed_files": names}, sort_keys=True))
    else:
        workspace_version = args.workspace_version or _workspace_version(args.repo)
        tag = f"v{workspace_version}"
        existing_tag_sha = args.existing_tag_sha
        if existing_tag_sha is None:
            existing_tag_sha = _existing_tag_sha(args.repo, tag)
        release_exists = (
            args.release_exists
            if args.release_exists is not None
            else _release_exists(tag)
        )
        result = plan_release(
            workspace_version,
            args.latest_release,
            args.merge_sha,
            existing_tag_sha,
            release_exists,
        )
        values = {"tag": tag, **dataclasses.asdict(result)}
        _write_github_output(args.github_output, values)
        print(json.dumps(values, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, UpdateError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
