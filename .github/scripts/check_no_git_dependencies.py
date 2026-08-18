#!/usr/bin/env python3

import re
import sys
import tomllib
from pathlib import Path


DEPENDENCY_SECTIONS = ("dependencies", "dev-dependencies", "build-dependencies")
UNPUBLISHED_GIT_DEPENDENCIES = {
    "voting-crypto-deps": "https://github.com/valargroup/voting-circuits",
}


def allowed_unpublished_dependency(name: str, dependency: dict, section: str) -> bool:
    expected_git = UNPUBLISHED_GIT_DEPENDENCIES.get(name)
    revision = dependency.get("rev", "")
    return (
        section == "workspace.dependencies"
        and dependency.get("git") == expected_git
        and isinstance(dependency.get("version"), str)
        and re.fullmatch(r"[0-9a-f]{40}", revision) is not None
    )


def git_dependencies(manifest: dict) -> list[str]:
    violations = []

    def inspect(table: dict, section: str) -> None:
        for name, dependency in table.items():
            if (
                isinstance(dependency, dict)
                and "git" in dependency
                and not allowed_unpublished_dependency(name, dependency, section)
            ):
                violations.append(f"{section}.{name}")

    for section in DEPENDENCY_SECTIONS:
        inspect(manifest.get(section, {}), section)

    inspect(manifest.get("workspace", {}).get("dependencies", {}), "workspace.dependencies")

    for target, target_manifest in manifest.get("target", {}).items():
        for section in DEPENDENCY_SECTIONS:
            inspect(target_manifest.get(section, {}), f"target.{target}.{section}")

    return violations


def main() -> int:
    violations = []

    for path in sorted(Path.cwd().rglob("Cargo.toml")):
        if ".git" in path.parts or "target" in path.parts:
            continue

        manifest = tomllib.loads(path.read_text(encoding="utf-8"))
        violations.extend(f"{path}: {dependency}" for dependency in git_dependencies(manifest))

    if violations:
        print(
            "Inline git dependencies are not allowed unless explicitly "
            "allowlisted as an unpublished package with an immutable revision.",
            file=sys.stderr,
        )
        print(
            "Use a version requirement and put source overrides in the root "
            "[patch.crates-io] table.",
            file=sys.stderr,
        )
        for violation in violations:
            print(f"- {violation}", file=sys.stderr)
        return 1

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
