# RsEditor

Rust (eframe/egui) 製のタブ付きテキストエディタです（Windows 向け）。

## 主な機能

- タブで複数のファイルを編集
- 文字コードの自動判定と変換（UTF-8 / UTF-16 / Shift_JIS / EUC-JP / ISO-2022-JP など）
- 検索・置換（置換は Ctrl+Z で取り消し可能）
- 行番号表示（折り返しオフでも行番号は固定表示）

## ダウンロード

[Releases](../../releases) から `RsEditor-v<バージョン>-windows-x64.zip`（または `.7z`）をダウンロードして解凍してください。
`RsEditor` フォルダができるので、中の `RsEditor.exe` を実行します。

更新するときは、新しいアーカイブを解凍して既存の `RsEditor` フォルダに上書きしてください。

## ビルド

```powershell
cargo build --release
cargo test
```

アイコンを変更するときは `assets/icon.svg` を基準に `tools\make_icon.py` を直して実行します。

配布アーカイブの作成（7-Zip と Python が必要）:

```powershell
pwsh tools\package.ps1
```

`dist\` に zip と 7z ができます。

## ライセンス

[MIT License](LICENSE)

使用しているライブラリのライセンスは、配布アーカイブ内の `THIRD_PARTY_LICENSES.txt` にまとめています（`tools\third_party_licenses.py` で生成）。
