# RsEditor の配布アーカイブ (zip / 7z) を作る。
#
# 使い方 (リポジトリのルートで):  pwsh tools\package.ps1
#
# 1. cargo build --release を行い、target\release\rs_editor.exe を RsEditor.exe としてコピー
# 2. dist\RsEditor\ に配布するファイルを集める
#    (フォルダ名・exe 名にはバージョンを付けない。更新時は上書きしてもらう)
# 3. dist\RsEditor-v<version>-windows-x64.zip / .7z を作る
#    (解凍すると RsEditor フォルダが 1 つできる)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$sevenZip = Join-Path $env:ProgramFiles '7-Zip\7z.exe'
if (-not (Test-Path $sevenZip)) { throw "7-Zip が見つかりません: $sevenZip" }

# Cargo.toml の version
$version = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "version: $version"

# 1. ビルド
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release に失敗しました' }
Copy-Item 'target\release\rs_editor.exe' 'RsEditor.exe' -Force

# 2. 配布フォルダ
$dist = Join-Path $root 'dist'
$stage = Join-Path $dist 'RsEditor'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force $stage | Out-Null

Copy-Item 'RsEditor.exe' $stage
Copy-Item 'RsEditor_使い方ガイド.pptx' $stage
Copy-Item 'samples' (Join-Path $stage 'samples') -Recurse
if (Test-Path 'LICENSE') { Copy-Item 'LICENSE' (Join-Path $stage 'LICENSE.txt') }
python tools\third_party_licenses.py (Join-Path $stage 'THIRD_PARTY_LICENSES.txt')
if ($LASTEXITCODE -ne 0) { throw 'THIRD_PARTY_LICENSES.txt の作成に失敗しました' }

# 3. アーカイブ (dist で実行し、中身が RsEditor\... になるようにする)
$base = "RsEditor-v$version-windows-x64"
Push-Location $dist
try {
    foreach ($ext in 'zip', '7z') {
        $file = "$base.$ext"
        if (Test-Path $file) { Remove-Item $file -Force }
        & $sevenZip a "-t$ext" -mx=9 $file 'RsEditor' | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "$file の作成に失敗しました" }
        Write-Host "created: dist\$file"
    }
}
finally {
    Pop-Location
}
