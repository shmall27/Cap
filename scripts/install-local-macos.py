#!/usr/bin/env python3
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tomllib


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def require_cap_closed():
    result = subprocess.run(
        ["pgrep", "-f", r"^/Applications/Cap[.]app/Contents/MacOS/Cap($| )"],
        stdout=subprocess.DEVNULL,
    )
    if result.returncode != 1:
        raise SystemExit("Quit Cap before installing a local build.")


def signing_requirement(app):
    result = run("codesign", "-d", "-r-", str(app), capture_output=True, text=True)
    for line in (result.stdout + result.stderr).splitlines():
        if "designated => " in line:
            return line.split("designated => ", 1)[1]
    raise SystemExit(f"Could not read the signing requirement for {app}")


def main():
    parser = argparse.ArgumentParser(
        description="Build and install local Cap using the matching installed release's resources and sidecars."
    )
    parser.add_argument("--skip-build", action="store_true")
    parser.add_argument("--stage-only", action="store_true")
    args = parser.parse_args()
    if platform.system() != "Darwin":
        raise SystemExit("This installer requires macOS.")

    repo = Path(__file__).resolve().parents[1]
    installed = Path("/Applications/Cap.app")
    version = tomllib.loads((repo / "apps/desktop-gpui/Cargo.toml").read_text())["package"]["version"]
    info = plistlib.loads((installed / "Contents/Info.plist").read_bytes())
    if info["CFBundleShortVersionString"] != version:
        raise SystemExit(f"Install official Cap {version} first; its resources and sidecars must match.")

    target = {"arm64": "aarch64", "x86_64": "x86_64"}[platform.machine()] + "-apple-darwin"
    if not args.skip_build:
        run("node", "scripts/setup.js", cwd=repo)
        run("bash", "scripts/build-gpui-binary.sh", "release", target, cwd=repo)
    binary = repo / "apps/desktop-gpui/target" / target / "release/cap-gpui"
    if not binary.is_file():
        raise SystemExit(f"Build output is missing: {binary}")

    staged = repo / "target/local-macos/Cap.app"
    staged.parent.mkdir(parents=True, exist_ok=True)
    if staged.exists():
        shutil.rmtree(staged)
    run("ditto", str(installed), str(staged))
    shutil.copy2(binary, staged / "Contents/MacOS/Cap")
    revision = run("git", "rev-parse", "HEAD", cwd=repo, capture_output=True, text=True).stdout.strip()
    info["CFBundleGetInfoString"] = f"Cap {version}, local build {revision[:12]}"
    info["CapLocalBuildRevision"] = revision
    info["CapLocalBuildSourceSHA256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
    (staged / "Contents/Info.plist").write_bytes(plistlib.dumps(info))
    quarantine = subprocess.run(
        ["xattr", "-p", "com.apple.quarantine", str(staged)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if quarantine.returncode == 0:
        run("xattr", "-dr", "com.apple.quarantine", str(staged))
    run(
        "codesign", "--force", "--sign", "-", "--options", "runtime",
        "--entitlements", str(repo / "apps/desktop/src-tauri/Entitlements.plist"), str(staged),
    )
    run("codesign", "--verify", "--deep", "--strict", str(staged))
    if args.stage_only:
        print(f"Staged local build: {staged}")
        return

    require_cap_closed()
    signing_changed = signing_requirement(installed) != signing_requirement(staged)
    backup_root = Path.home() / "Library/Application Support/Cap Local Builds"
    backup = backup_root / datetime.datetime.now().strftime("install-%Y%m%d-%H%M%S-%f")
    backup.mkdir(parents=True)
    store = Path.home() / "Library/Application Support/so.cap.desktop/store"
    settings = json.loads(store.read_text()) if store.exists() else {}
    settings.setdefault("general_settings", {})["updateDownloadPolicy"] = "never"
    if store.exists():
        shutil.copy2(store, backup / "store")
    store.parent.mkdir(parents=True, exist_ok=True)
    temporary_store = store.with_name("store.local-install.tmp")
    temporary_store.write_text(json.dumps(settings, indent=2) + "\n")
    temporary_store.chmod(0o600)
    installed.rename(backup / "Cap.app")
    try:
        staged.rename(installed)
        temporary_store.replace(store)
    except BaseException:
        if installed.exists():
            installed.rename(staged)
        (backup / "Cap.app").rename(installed)
        temporary_store.unlink(missing_ok=True)
        raise
    (backup_root / "latest-backup.txt").write_text(str(backup) + "\n")
    print(f"Installed: {installed}\nBackup: {backup}\nAutomatic update checks: disabled")
    if signing_changed:
        run("tccutil", "reset", "All", info["CFBundleIdentifier"])
        print("Signing identity changed: cleared Cap's stale privacy grants.")
        print("Reopen Cap and grant Screen Recording and Accessibility access again.")


if __name__ == "__main__":
    main()
