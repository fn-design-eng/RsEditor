"""RsEditor.exe に組み込まれる依存クレートのライセンス一覧を作る。

使い方: python tools/third_party_licenses.py <出力ファイル>

cargo metadata から Windows (x86_64-pc-windows-msvc) 向けの通常依存
(ビルド用・テスト用の依存は除く) をたどり、各クレートのソースに含まれる
ライセンス本文 (Cargo.toml の license_file、無ければ LICENSE / COPYING / NOTICE など)
をまとめて書き出す。クレート自身の本文と、同梱アセット (フォント) のライセンスは
別々に集める。ソースに本文が無いクレートは tools/licenses/ に保存した上流の本文を使い、
manifest.json の SHA-256 と版を検証する。解決できないクレートがあればエラー終了する。
"""

import hashlib
import json
import os
import subprocess
import sys

TARGET = "x86_64-pc-windows-msvc"
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense", "copyright")

# ソースにライセンス本文が同梱されていないクレートは、上流リポジトリの該当コミットから
# 取得した本文を tools/licenses/ に保存し、tools/licenses/manifest.json に
# 「クレート名 版」ごとのファイルと SHA-256 を記録してある。
# クレートの版が変わる (manifest に無い) とエラーになるので、依存を更新したら
# 上流の本文を取り直して manifest.json を更新すること。
LICENSES_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "licenses")


def load_manifest() -> dict:
    with open(os.path.join(LICENSES_DIR, "manifest.json"), encoding="utf-8") as fh:
        return json.load(fh)


def manifest_bodies(manifest: dict, key: str):
    """manifest のクレートの本文を [(相対パス, 本文)] で返す。無い / ハッシュ不一致なら例外。"""
    entry = manifest["crates"].get(key)
    if entry is None:
        raise ValueError("manifest.json に登録がありません (版が変わった可能性)")
    out = []
    for rel in entry["files"]:
        path = os.path.join(LICENSES_DIR, *rel.split("/"))
        if not os.path.isfile(path):
            raise ValueError(f"ファイルがありません: {rel}")
        with open(path, "rb") as fh:
            raw = fh.read()
        digest = hashlib.sha256(raw.replace(b"\r\n", b"\n")).hexdigest()
        if digest != manifest["sha256"].get(rel):
            raise ValueError(f"SHA-256 が一致しません: {rel}")
        text = raw.decode("utf-8", errors="replace").replace("\r\n", "\n").strip()
        out.append((rel, text))
    return out


def read_text(path: str) -> str:
    with open(path, encoding="utf-8", errors="replace") as fh:
        return fh.read().strip()


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: third_party_licenses.py <output>")
    out_path = sys.argv[1]

    # このプロジェクト自身の LICENSE も配布物に入るので、無ければ失敗させる
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    if not os.path.isfile(os.path.join(repo_root, "LICENSE")):
        sys.exit("error: プロジェクトの LICENSE が見つかりません: " + os.path.join(repo_root, "LICENSE"))

    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", TARGET],
            check=True,
            capture_output=True,
            cwd=repo_root,
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
    texts = {}  # 本文 -> [クレート名 (ファイル名)]
    pending = []  # 自身の本文が無く manifest の本文を使うクレート
    manifest = load_manifest()
    problems = []  # manifest の検証エラー
    for p in crates:
        crate_dir = os.path.dirname(p["manifest_path"])
        label = f"{p['name']} {p['version']}"

        # クレート自身のライセンス本文: license_file があればそれを優先し、
        # 無ければクレートのルートの LICENSE / COPYING / NOTICE などを集める
        own = []
        lf = p.get("license_file")
        if lf:
            full = os.path.join(crate_dir, lf)
            if os.path.isfile(full):
                own.append((lf, full))
        if not own:
            own = [
                (f, os.path.join(crate_dir, f))
                for f in sorted(os.listdir(crate_dir))
                if f.lower().startswith(LICENSE_PREFIXES)
                and os.path.isfile(os.path.join(crate_dir, f))
            ]
        own_bodies = [(rel, read_text(full)) for rel, full in own]
        own_bodies = [(rel, b) for rel, b in own_bodies if b]
        if not own_bodies:
            pending.append(p)

        # 同梱アセット (fonts/ は epaint_default_fonts の同梱フォント) のライセンスは
        # クレート自身のライセンスとは別に集める (これがあってもクレート自身の本文の代わりにはならない)
        assets = []
        d = os.path.join(crate_dir, "fonts")
        if os.path.isdir(d):
            for f in sorted(os.listdir(d)):
                full = os.path.join(d, f)
                if os.path.isfile(full) and f.lower().endswith(".txt"):
                    assets.append((os.path.join("fonts", f), read_text(full)))

        for rel, body in own_bodies + [(r, b) for r, b in assets if b]:
            texts.setdefault(body, []).append(f"{label} ({rel})")

    # 本文が同梱されていないクレート: manifest の上流本文 (SHA-256 を検証) を使う
    for p in pending:
        key = f"{p['name']} {p['version']}"
        try:
            for rel, body in manifest_bodies(manifest, key):
                texts.setdefault(body, []).append(f"{key} (上流 / upstream: licenses/{rel})")
        except ValueError as e:
            problems.append(f"{key}: {e}")

    if problems:
        sys.exit(
            "error: tools/licenses の検証に失敗しました / license manifest check failed:\n  "
            + ("\n  ").join(problems)
            + "\n  依存を更新した場合は、上流の本文を確認して tools/licenses/manifest.json を更新してください。"
        )

    for body, users in texts.items():
        lines.append("=" * 78)
        lines.append("Used by:")
        lines.extend("  " + u for u in users)
        lines.append("=" * 78)
        lines.append(body)
        lines.append("")

    with open(out_path, "w", encoding="utf-8", newline="\r\n") as fh:
        fh.write("\n".join(lines) + "\n")

    print(f"{len(crates)} crates, {len(texts)} distinct texts -> {out_path}")


if __name__ == "__main__":
    main()
