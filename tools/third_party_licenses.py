"""RsEditor.exe に組み込まれる依存クレートのライセンス一覧を作る。

使い方: python tools/third_party_licenses.py <出力ファイル>

cargo metadata から Windows (x86_64-pc-windows-msvc) 向けの通常依存
(ビルド用・テスト用の依存は除く) をたどり、各クレートのソースに含まれる
LICENSE / COPYING / NOTICE などのファイルの本文をまとめて書き出す。
"""

import json
import os
import subprocess
import sys

TARGET = "x86_64-pc-windows-msvc"
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense", "copyright")


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: third_party_licenses.py <output>")
    out_path = sys.argv[1]

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--filter-platform", TARGET],
            check=True,
            capture_output=True,
        ).stdout
    )
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]

    # 通常依存 (kind == null) だけをたどる
    seen, stack = set(), [root]
    while stack:
        cur = stack.pop()
        if cur in seen:
            continue
        seen.add(cur)
        for dep in nodes[cur]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    seen.discard(root)

    crates = sorted((pkgs[i] for i in seen), key=lambda p: (p["name"].lower(), p["version"]))

    lines = [
        "RsEditor - サードパーティライセンス / Third-party licenses",
        "",
        "RsEditor.exe には以下のオープンソースソフトウェアが含まれています。",
        "This program includes the following open source software.",
        "",
    ]
    for p in crates:
        lines.append(f"  {p['name']} {p['version']}  ({p.get('license') or 'see below'})")
    lines.append("")

    # 本文が同じライセンスは 1 回だけ書き、使っているクレートを列挙する
    texts = {}  # 本文 -> [(クレート名, ファイル名)]
    missing = []
    for p in crates:
        crate_dir = os.path.dirname(p["manifest_path"])
        files = []
        for sub in ("", "fonts"):  # fonts/ は epaint_default_fonts の同梱フォントのライセンス
            d = os.path.join(crate_dir, sub)
            if not os.path.isdir(d):
                continue
            for f in sorted(os.listdir(d)):
                full = os.path.join(d, f)
                low = f.lower()
                if not os.path.isfile(full):
                    continue
                if low.startswith(LICENSE_PREFIXES) or (sub == "fonts" and low.endswith(".txt")):
                    files.append((os.path.join(sub, f) if sub else f, full))
        if not files:
            missing.append(p)
        for rel, full in files:
            with open(full, encoding="utf-8", errors="replace") as fh:
                body = fh.read().strip()
            texts.setdefault(body, []).append(f"{p['name']} {p['version']} ({rel})")

    if missing:
        lines.append("次のクレートはライセンス本文のファイルを同梱していないため、")
        lines.append("記載の SPDX ライセンスの標準の条文 (下記に同じ本文があります) が適用されます。")
        lines.append("The following crates do not ship license files; the standard text of")
        lines.append("the listed SPDX license (included below) applies.")
        for p in missing:
            lines.append(f"  {p['name']} {p['version']}  ({p.get('license')})")
        lines.append("")

    for body, users in texts.items():
        lines.append("=" * 78)
        lines.append("Used by:")
        lines.extend("  " + u for u in users)
        lines.append("=" * 78)
        lines.append(body)
        lines.append("")

    with open(out_path, "w", encoding="utf-8", newline="\r\n") as fh:
        fh.write("\n".join(lines) + "\n")

    print(f"{len(crates)} crates -> {out_path}")
    if missing:
        print("license text not bundled in crate:", ", ".join(p["name"] for p in missing))


if __name__ == "__main__":
    main()
