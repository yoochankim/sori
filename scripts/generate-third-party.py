#!/usr/bin/env python3
"""Generate notices for the dependencies in the shipped macOS binaries."""

import json
import pathlib
import subprocess


ROOT = pathlib.Path(__file__).resolve().parent.parent
TARGET = "aarch64-apple-darwin"
OUTPUT = ROOT / "THIRD_PARTY_NOTICES.txt"

# Some crates intentionally omit their workspace-level license file from the
# published archive. Pin overrides to the exact released version so dependency
# upgrades fail until their authoritative license source is reviewed again.
LICENSE_OVERRIDES = {
    ("block2", "0.6.2"): ("objc2", "madsmtm/objc2@b4167b582b2f75f9a1be75495c41b765344fd03c"),
    ("dispatch2", "0.3.1"): ("objc2", "madsmtm/objc2@8852b424193ca41602281b3d7540d7c8ed51e49a"),
    ("objc2", "0.6.4"): ("objc2", "madsmtm/objc2@8852b424193ca41602281b3d7540d7c8ed51e49a"),
    ("objc2-encode", "4.1.0"): ("objc2", "madsmtm/objc2@8d214f5477365ffcbcbb7de058c86ed9a518efb7"),
    ("objc2-audio-toolbox", "0.3.2"): ("objc2", "madsmtm/objc2@7b1abfd750a2cacaea71d6a56ecfb83cb7de560b"),
    ("objc2-core-audio", "0.3.2"): ("objc2", "madsmtm/objc2@7b1abfd750a2cacaea71d6a56ecfb83cb7de560b"),
    ("objc2-core-audio-types", "0.3.2"): ("objc2", "madsmtm/objc2@7b1abfd750a2cacaea71d6a56ecfb83cb7de560b"),
    ("objc2-core-foundation", "0.3.2"): ("objc2", "madsmtm/objc2@7b1abfd750a2cacaea71d6a56ecfb83cb7de560b"),
    ("objc2-foundation", "0.3.2"): ("objc2", "madsmtm/objc2@7b1abfd750a2cacaea71d6a56ecfb83cb7de560b"),
    ("cidre", "0.15.2"): ("cidre", "yury/cidre@b3818713679f4088673d37f7f93423887929367d"),
    ("cidre-macros", "0.5.0"): ("cidre", "yury/cidre@ca495f14cfb976344eb7349d6a6d1cd24834a9d1"),
    ("dasp_sample", "0.11.0"): ("dasp", "RustAudio/dasp@97c3bb9b2363c0b46ac1633858bf1054fd02a980"),
}


def metadata():
    raw = subprocess.check_output(
        [
            "cargo",
            "metadata",
            "--format-version",
            "1",
            "--filter-platform",
            TARGET,
            "--locked",
        ],
        cwd=ROOT,
    )
    return json.loads(raw)


def reachable_packages(data):
    root = next(package for package in data["packages"] if package["name"] == "sori-app")
    nodes = {node["id"]: node for node in data["resolve"]["nodes"]}
    reachable = set()
    pending = [root["id"]]
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        reachable.add(package_id)
        pending.extend(dependency["pkg"] for dependency in nodes[package_id]["deps"])
    return sorted(
        (
            package
            for package in data["packages"]
            if package["id"] in reachable and package["id"] != root["id"]
        ),
        key=lambda package: (package["name"], package["version"]),
    )


def license_files(package):
    directory = pathlib.Path(package["manifest_path"]).parent
    prefixes = ("LICENSE", "COPYING", "NOTICE")
    packaged = sorted(
        path
        for path in directory.iterdir()
        if path.is_file() and path.name.upper().startswith(prefixes)
    )
    if packaged:
        return packaged, "published Cargo package"
    key = (package["name"], package["version"])
    override = LICENSE_OVERRIDES.get(key)
    if override is None:
        return [], ""
    directory_name, source = override
    directory = ROOT / "third-party-licenses" / directory_name
    files = sorted(path for path in directory.iterdir() if path.is_file())
    return files, source


def main():
    sections = [
        "Sori third-party notices",
        "Generated from Cargo.lock for aarch64-apple-darwin.",
    ]
    for package in reachable_packages(metadata()):
        license_expression = package.get("license") or "UNKNOWN"
        files, license_source = license_files(package)
        if not files:
            raise SystemExit(
                f"No bundled license text for {package['name']} {package['version']} "
                f"({license_expression})"
            )
        texts = [f"[{path.name}]\n{path.read_text(errors='replace').strip()}" for path in files]
        sections.append(
            "\n".join(
                [
                    "=" * 78,
                    f"{package['name']} {package['version']}",
                    f"License: {license_expression}",
                    f"License source: {license_source}",
                    f"Repository: {package.get('repository') or package.get('homepage') or 'not listed'}",
                    "",
                    "\n\n".join(texts),
                ]
            )
        )
    OUTPUT.write_text("\n\n".join(sections) + "\n")


if __name__ == "__main__":
    main()
