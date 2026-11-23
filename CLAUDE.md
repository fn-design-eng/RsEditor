# RsEditor

Rust (eframe/egui) 製のタブ付きテキストエディタ。文字コード変換・検索/置換・行番号表示を備える。

- ビルド: `cargo build --release` → `target\release\rs_editor.exe` を `RsEditor.exe` としてコピー
- テスト: `cargo test`

## バージョン管理ルール（必ず守ること）

- GitHub で管理する。管理開始時点の版を **v1.0.0** とする。
- バージョンは `Cargo.toml` の `version` で管理し、Git タグは `v` を付ける（例: `v1.0.1`）。
- 次のバージョンを開発するときは、開発版 **v1.0.1-dev1** から始める。
  - 開発版の番号は `-dev1`, `-dev2`, … と進める。
  - 開発が完了した時点で正式版 **v1.0.1** をリリースする。
  - それ以降も同じ流れ（例: v1.0.2-dev1 → … → v1.0.2）。
- **ユーザーから明示的な指示があるまで、次の操作は行わない:**
  - `git commit`
  - 正式版のリリース（タグ付け、GitHub Release の作成、push を含む）
- 実行ファイル `RsEditor.exe` と `dist/` はリポジトリに含めない（`.gitignore` 済み）。
- リリース時は `pwsh tools\package.ps1` で配布アーカイブを作り、GitHub Release に添付する。
  - 添付するのは `dist\RsEditor-v<version>-windows-x64.zip` と `.7z` の 2 つ（中身は同じ）。
  - アーカイブを解凍すると `RsEditor\` フォルダが 1 つでき、中に `RsEditor.exe`・`LICENSE.txt`・`THIRD_PARTY_LICENSES.txt`・使い方ガイド・`samples\` が入る。
  - 利用者は更新時に `RsEditor` フォルダへ上書きするので、**フォルダ名と exe 名にはバージョンを付けない**（アーカイブのファイル名にだけ付ける）。

## モデルの使い分けルール（必ず守ること）

| モデル | 推論レベル | 担当 |
|---|---|---|
| Haiku 5.5 | 高 | ビルド、簡単な実装・調査 |
| Sonnet 5.5 | 中 | 中程度の実装、調査 |
| Opus 5.5 | 中 | メイン（全体の進行と判断） |

- ビルドや簡単な実装・調査は Haiku 5.5（高）、中程度の実装・調査は Sonnet 5.5（中）のサブエージェントに任せる。
- Opus 5.5 はメインのセッション専用とし、**サブエージェントでは使用しない**。
