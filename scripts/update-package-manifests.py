#!/usr/bin/env python3
"""Update Homebrew (Formula + Cask) and WinGet manifests from built release assets."""

from __future__ import annotations

import hashlib
import pathlib
import re
import sys


def sha256_file(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def replace_first(text: str, pattern: str, replacement: str) -> str:
    new_text, count = re.subn(pattern, replacement, text, count=1, flags=re.MULTILINE)
    if count == 0:
        raise RuntimeError(f"Pattern not found: {pattern}")
    return new_text


def main() -> int:
    if len(sys.argv) < 3:
        print("Usage: update-package-manifests.py <version-tag> <assets-dir>", file=sys.stderr)
        return 1

    tag = sys.argv[1]
    version = tag.lstrip("v")
    assets_dir = pathlib.Path(sys.argv[2])
    root = pathlib.Path(__file__).resolve().parent.parent

    macos_tar = assets_dir / f"clop-rs-v{version}-macos-arm64.tar.gz"
    macos_pkg = assets_dir / f"clop-rs-v{version}-macos-arm64.pkg"
    win_zip = assets_dir / f"clop-rs-v{version}-windows-x64.zip"

    # Generate SHA256SUMS.txt for all release assets
    checksum_lines = []
    for asset in sorted(assets_dir.iterdir()):
        if asset.is_file() and asset.name != "SHA256SUMS.txt":
            checksum_lines.append(f"{sha256_file(asset)}  {asset.name}")
    (assets_dir / "SHA256SUMS.txt").write_text("\n".join(checksum_lines) + "\n", encoding="utf-8")

    # 1. Update Formula/clop-rs.rb
    formula_path = root / "Formula" / "clop-rs.rb"
    if formula_path.exists() and macos_tar.exists():
        tar_sha = sha256_file(macos_tar)
        content = formula_path.read_text(encoding="utf-8")
        content = replace_first(content, r'^  version "[^"]+"', f'  version "{version}"')
        content = replace_first(content, r'^      sha256 "[^"]+"', f'      sha256 "{tar_sha}"')
        formula_path.write_text(content, encoding="utf-8")
        print(f"Updated {formula_path} (v{version}, sha256={tar_sha[:12]}...)")

    # 2. Update Casks/clop-rs.rb
    cask_path = root / "Casks" / "clop-rs.rb"
    if cask_path.exists() and macos_pkg.exists():
        pkg_sha = sha256_file(macos_pkg)
        content = cask_path.read_text(encoding="utf-8")
        content = replace_first(content, r'^  version "[^"]+"', f'  version "{version}"')
        content = replace_first(content, r'^  sha256 "[^"]+"', f'  sha256 "{pkg_sha}"')
        cask_path.write_text(content, encoding="utf-8")
        print(f"Updated {cask_path} (v{version}, sha256={pkg_sha[:12]}...)")

    # 3. Update WinGet manifests
    winget_dir = root / "packaging" / "winget"
    if winget_dir.exists() and win_zip.exists():
        zip_sha = sha256_file(win_zip).upper()

        ver_file = winget_dir / "HamidRezaAshkiyan.clop-rs.yaml"
        if ver_file.exists():
            txt = ver_file.read_text(encoding="utf-8")
            txt = replace_first(txt, r"^PackageVersion: .*$", f"PackageVersion: {version}")
            ver_file.write_text(txt, encoding="utf-8")

        loc_file = winget_dir / "HamidRezaAshkiyan.clop-rs.locale.en-US.yaml"
        if loc_file.exists():
            txt = loc_file.read_text(encoding="utf-8")
            txt = replace_first(txt, r"^PackageVersion: .*$", f"PackageVersion: {version}")
            txt = replace_first(
                txt,
                r"^ReleaseNotesUrl: .*$",
                f"ReleaseNotesUrl: https://github.com/HamidRezaAshkiyan/clop-rs/releases/tag/v{version}",
            )
            loc_file.write_text(txt, encoding="utf-8")

        inst_file = winget_dir / "HamidRezaAshkiyan.clop-rs.installer.yaml"
        if inst_file.exists():
            txt = inst_file.read_text(encoding="utf-8")
            txt = replace_first(txt, r"^PackageVersion: .*$", f"PackageVersion: {version}")
            txt = replace_first(
                txt,
                r"^    InstallerUrl: .*$",
                f"    InstallerUrl: https://github.com/HamidRezaAshkiyan/clop-rs/releases/download/v{version}/clop-rs-v{version}-windows-x64.zip",
            )
            txt = replace_first(txt, r"^    InstallerSha256: .*$", f"    InstallerSha256: {zip_sha}")
            inst_file.write_text(txt, encoding="utf-8")

        # Also copy updated WinGet manifests into release assets
        for yf in winget_dir.glob("*.yaml"):
            (assets_dir / yf.name).write_bytes(yf.read_bytes())

        print(f"Updated WinGet manifests (v{version}, sha256={zip_sha[:12]}...)")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
