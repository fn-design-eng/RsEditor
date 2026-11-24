# RsEditor の配布アーカイブ (zip / 7z) を作る。
#
# 使い方 (リポジトリのルートで):  pwsh tools\package.ps1
#
# 1. cargo build --release --target x86_64-pc-windows-msvc --locked を行い、
#    cargo が報告した exe (compiler-artifact の executable) を検査する
#    (exe は x64 の PE で、バージョン情報が Cargo.toml の version と一致することを確認する)
# 2. 実行ごとの一時フォルダ (dist\.work-*) に RsEditor\ を作って配布するファイルを集める
#    (フォルダ名・exe 名にはバージョンを付けない。更新時は上書きしてもらう)
# 3. 同じ一時フォルダで zip / 7z を作り、両方を 7z t で検査して中身を確認する
# 4. すべての検査に通ってから、両方を dist\RsEditor-v<version>-windows-x64.zip / .7z に置き換え、
#    最後にルートの RsEditor.exe を更新する (途中で失敗したら以前のものを元に戻す。
#    戻せなかった場合は退避先 dist\.work-* を残して場所を表示する)
#
# -DistDir は出力先 (既定は dist)。テスト用。

param(
    [string]$DistDir = ''
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$Target = 'x86_64-pc-windows-msvc'

$sevenZip = Join-Path $env:ProgramFiles '7-Zip\7z.exe'
if (-not (Test-Path $sevenZip)) { throw "7-Zip が見つかりません: $sevenZip" }

# Cargo.toml の version
$version = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "version: $version"

# 1. ビルド (ターゲットと Cargo.lock を固定し、できた exe のパスは cargo の出力から得る)
$cargoOut = & cargo build --release --target $Target --locked --message-format=json-render-diagnostics
if ($LASTEXITCODE -ne 0) { throw 'cargo build --release に失敗しました' }
$exePath = $null
foreach ($line in $cargoOut) {
    if (-not ($line -is [string]) -or -not $line.StartsWith('{')) { continue }
    $msg = $line | ConvertFrom-Json
    if ($msg.reason -eq 'compiler-artifact' -and $msg.executable -and
        $msg.target.name -eq 'rs_editor' -and ($msg.target.kind -contains 'bin')) {
        $exePath = $msg.executable
    }
}
if (-not $exePath -or -not (Test-Path $exePath)) {
    throw 'cargo の出力から rs_editor の実行ファイルを特定できませんでした'
}
Write-Host "built: $exePath"

# exe の検査: PE の Machine が x64 (0x8664) であること
$bytes = [System.IO.File]::ReadAllBytes($exePath)
if ($bytes.Length -lt 0x40 -or $bytes[0] -ne 0x4D -or $bytes[1] -ne 0x5A) { throw "PE ファイルではありません: $exePath" }
$peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
if ($peOffset -lt 0 -or $peOffset + 6 -gt $bytes.Length -or
    $bytes[$peOffset] -ne 0x50 -or $bytes[$peOffset + 1] -ne 0x45 -or
    $bytes[$peOffset + 2] -ne 0 -or $bytes[$peOffset + 3] -ne 0) {
    throw "PE ヘッダーが不正です: $exePath"
}
$machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)
if ($machine -ne 0x8664) { throw ('exe が x64 ではありません (Machine = 0x{0:X4}): {1}' -f $machine, $exePath) }
# バージョン情報 (build.rs の winresource が Cargo.toml の version をそのまま FileVersion / ProductVersion に入れる)
$vi = (Get-Item $exePath).VersionInfo
if ($vi.FileVersion -ne $version -or $vi.ProductVersion -ne $version) {
    throw "exe のバージョン情報 (FileVersion=$($vi.FileVersion), ProductVersion=$($vi.ProductVersion)) が Cargo.toml の version ($version) と一致しません"
}

# 2. 一時フォルダに配布フォルダを作る (以前のアーカイブには触れない)
if (-not $DistDir) { $DistDir = Join-Path $root 'dist' }
New-Item -ItemType Directory -Force $DistDir | Out-Null
$DistDir = (Resolve-Path $DistDir).Path
$work = Join-Path $DistDir (".work-" + [Guid]::NewGuid().ToString('N'))
$base = "RsEditor-v$version-windows-x64"
$keepWork = $false
try {
    $stage = Join-Path $work 'RsEditor'
    New-Item -ItemType Directory -Force $stage | Out-Null

    Copy-Item $exePath (Join-Path $stage 'RsEditor.exe')
    Copy-Item 'RsEditor_使い方ガイド.pptx' $stage
    Copy-Item 'samples' (Join-Path $stage 'samples') -Recurse
    if (-not (Test-Path 'LICENSE')) { throw 'LICENSE がありません' }
    Copy-Item 'LICENSE' (Join-Path $stage 'LICENSE.txt')
    python tools\third_party_licenses.py (Join-Path $stage 'THIRD_PARTY_LICENSES.txt')
    if ($LASTEXITCODE -ne 0) { throw 'THIRD_PARTY_LICENSES.txt の作成に失敗しました' }

    # 3. アーカイブ (一時フォルダで実行し、中身が RsEditor\... になるようにする)
    $sampleCount = (Get-ChildItem (Join-Path $stage 'samples') -Recurse -File | Measure-Object).Count
    Push-Location $work
    try {
        foreach ($ext in 'zip', '7z') {
            $file = "$base.$ext"
            & $sevenZip a "-t$ext" -mx=9 $file 'RsEditor' | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "$file の作成に失敗しました" }

            # 検査: アーカイブ自体の整合性と中身
            & $sevenZip t $file | Out-Null
            if ($LASTEXITCODE -ne 0) { throw "$file の検査 (7z t) に失敗しました" }
            $paths = @(& $sevenZip l -slt -ba $file |
                Where-Object { $_ -like 'Path = *' } |
                ForEach-Object { ($_.Substring(7)) -replace '\\', '/' })
            if ($paths.Count -eq 0) { throw "$file の中身を読み取れません" }
            $tops = @($paths | ForEach-Object { ($_ -split '/')[0] } | Select-Object -Unique)
            if ($tops.Count -ne 1 -or $tops[0] -cne 'RsEditor') {
                throw "$file のトップが RsEditor\ だけになっていません: $($tops -join ', ')"
            }
            foreach ($need in 'RsEditor/RsEditor.exe', 'RsEditor/LICENSE.txt', 'RsEditor/THIRD_PARTY_LICENSES.txt') {
                if ($paths -cnotcontains $need) { throw "$file に $need がありません" }
            }
            if (-not ($paths | Where-Object { $_ -like 'RsEditor/*.pptx' })) { throw "$file に使い方ガイド (pptx) がありません" }
            $samples = @($paths | Where-Object { $_ -like 'RsEditor/samples/*' })
            if (@($samples | Where-Object { $_ -match '\.[^/]+$' }).Count -lt $sampleCount) { throw "$file の samples\ のファイルが足りません" }
            Write-Host "verified: $file"
        }
    }
    finally {
        Pop-Location
    }

    # 4. 検査がすべて通ってから公開する: アーカイブを dist に移し、最後にルートの RsEditor.exe を更新する。
    #    途中で失敗したら以前のものを戻す。戻せなかった場合は $work (退避した以前のファイル) を残す。
    $backup = Join-Path $work 'backup'
    New-Item -ItemType Directory -Force $backup | Out-Null
    $items = @(
        foreach ($ext in 'zip', '7z') {
            [pscustomobject]@{ Name = "$base.$ext"; Src = (Join-Path $work "$base.$ext"); Dest = (Join-Path $DistDir "$base.$ext"); Had = $false; Done = $false }
        }
        [pscustomobject]@{ Name = 'RsEditor.exe'; Src = $exePath; Dest = (Join-Path $root 'RsEditor.exe'); Had = $false; Done = $false }
    )
    try {
        foreach ($it in $items) {
            $it.Had = Test-Path $it.Dest
            if ($it.Had) { Move-Item $it.Dest (Join-Path $backup $it.Name) -Force }
            if ($it.Name -eq 'RsEditor.exe') {
                # 同じフォルダの一時名へコピーし、コピーが終わってから正式な名前へ改名する。
                # 正式な名前へ直接コピーすると、途中 (ディスクの空きなど) で失敗したときに、
                # 中途半端な exe が正式な名前のまま残る (以前の exe が無い初回は、元に戻す対象にもならない)
                $tmpExe = "$($it.Dest).tmp-$([Guid]::NewGuid().ToString('N'))"
                try {
                    Copy-Item $it.Src $tmpExe -Force
                    Move-Item $tmpExe $it.Dest -Force
                }
                finally {
                    if (Test-Path $tmpExe) { Remove-Item $tmpExe -Force -ErrorAction SilentlyContinue }
                }
            }
            else { Move-Item $it.Src $it.Dest -Force }
            $it.Done = $true
        }
    }
    catch {
        $err = $_
        $restoreFailed = $false
        # 元に戻す: 新しく置いたものを消し、退避しておいた以前のものを戻す (1 つ失敗しても残りは続ける)
        foreach ($it in $items) {
            $old = Join-Path $backup $it.Name
            try {
                if ($it.Done -and (Test-Path $it.Dest)) { Remove-Item $it.Dest -Force }
                if ($it.Had -and (Test-Path $old)) { Move-Item $old $it.Dest -Force }
            }
            catch {
                $restoreFailed = $true
                Write-Host "復元に失敗しました: $($it.Dest) ($($_.Exception.Message))" -ForegroundColor Red
            }
        }
        if ($restoreFailed) {
            $keepWork = $true
            Write-Host "以前のファイルの退避先を残しました: $backup" -ForegroundColor Red
            Write-Host "  手作業でここから dist (と RsEditor.exe) に戻してください。" -ForegroundColor Red
        }
        throw $err
    }
    foreach ($ext in 'zip', '7z') { Write-Host "created: $(Join-Path $DistDir "$base.$ext")" }
    Write-Host "updated: $(Join-Path $root 'RsEditor.exe')"
}
finally {
    if (-not $keepWork -and (Test-Path $work)) { Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue }
}
