<#
.SYNOPSIS
    Builds SP-404 Drive's Windows installer: target\installer\SP-404-Drive-<version>-windows-x64-setup.exe.

.DESCRIPTION
    1. Builds the three programs (cargo build --release), with the C runtime linked in
       (.cargo\config.toml).
    2. Downloads WinFsp's installer, the version pinned below, once, into
       target\installer, and checks its SHA-256 every time.
    3. Writes the app icon (drawn by the tray app's own code) and the licence page.
    4. Compiles windows\installer.nsi with NSIS.

    Nothing is signed yet.

.PARAMETER SkipBuild
    Use the programs already in target\release.
#>
[CmdletBinding()]
param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'

# WinFsp 2.1 ("WinFsp 2025"), the release the drive was built and tested with.
$WinFsp = @{
    Version = '2.1.25156'
    Url     = 'https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-2.1.25156.msi'
    Sha256  = '073a70e00f77423e34bed98b86e600def93393ba5822204fac57a29324db9f7a'
}
$MakeNsis = 'C:\Program Files (x86)\NSIS\makensis.exe'

$root = Split-Path -Parent $PSScriptRoot
$bin = Join-Path $root 'target\release'
$out = Join-Path $root 'target\installer'

if (-not (Test-Path -LiteralPath $MakeNsis)) {
    throw "NSIS is missing: $MakeNsis (winget install NSIS.NSIS)"
}
$version = (Select-String -LiteralPath (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "SP-404 Drive $version"

if (-not $SkipBuild) {
    # winfsp-sys needs libclang; Doxygen's copy works.
    if (-not $env:LIBCLANG_PATH -and (Test-Path 'C:\Program Files\doxygen\bin\libclang.dll')) {
        $env:LIBCLANG_PATH = 'C:\Program Files\doxygen\bin'
    }
    Push-Location $root
    try {
        cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
}
foreach ($exe in 'sp404drive.exe', 'sp404drive-host.exe', 'sp404drive-tray.exe') {
    if (-not (Test-Path -LiteralPath (Join-Path $bin $exe))) { throw "Missing: $bin\$exe" }
}

New-Item -ItemType Directory -Force -Path $out | Out-Null

$msi = Join-Path $out "winfsp-$($WinFsp.Version).msi"
if (-not (Test-Path -LiteralPath $msi)) {
    Write-Host "Downloading $($WinFsp.Url)"
    Invoke-WebRequest -Uri $WinFsp.Url -OutFile "$msi.part"
    Move-Item -LiteralPath "$msi.part" -Destination $msi
}
$hash = (Get-FileHash -LiteralPath $msi -Algorithm SHA256).Hash.ToLowerInvariant()
if ($hash -ne $WinFsp.Sha256) {
    Remove-Item -LiteralPath $msi
    throw "WinFsp's installer has the wrong SHA-256 ($hash); deleted. Run again to download it afresh."
}

$icon = Join-Path $out 'app.ico'
& (Join-Path $bin 'sp404drive-tray.exe') --write-icon $icon | Out-Null
if (-not (Test-Path -LiteralPath $icon)) { throw "The tray app didn't write $icon" }

# The licence page: the README's use-at-your-own-risk notice, then the GPL.
$readme = Get-Content -LiteralPath (Join-Path $root 'README.md') -Raw
$risk = [regex]::Match($readme, '> \*\*Use at your own risk\.\*\*(?:.|\n)*?(?=\r?\n\r?\n)').Value
if (-not $risk) { throw "README.md has no use-at-your-own-risk notice" }
$risk = ($risk -replace '(?m)^> ?', '' -replace '\*\*', '' -replace '\r?\n', ' ').Trim()
$licence = Join-Path $out 'LICENSE.txt'
$gpl = Get-Content -LiteralPath (Join-Path $root 'LICENSE') -Raw
Set-Content -LiteralPath $licence -Encoding utf8 -Value @"
$risk

"SP-404MKII" and "Roland" are trademarks of Roland Corporation. SP-404 Drive is not affiliated with or endorsed by Roland Corporation.

SP-404 Drive is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. Source code: https://github.com/Jeremy-MW/sp404-drive

$gpl
"@

& $MakeNsis /V3 /INPUTCHARSET UTF8 `
    "/DVersion=$version" `
    "/DBinDir=$bin" `
    "/DOutDir=$out" `
    "/DWinFspMsi=$msi" `
    "/DWinFspVersion=$($WinFsp.Version)" `
    "/DIconFile=$icon" `
    "/DLicenceFile=$licence" `
    "/DNoticesFile=$PSScriptRoot\THIRD-PARTY-NOTICES.txt" `
    (Join-Path $PSScriptRoot 'installer.nsi')
if ($LASTEXITCODE -ne 0) { throw "makensis failed ($LASTEXITCODE)" }

$setup = Join-Path $out "SP-404-Drive-$version-windows-x64-setup.exe"
Write-Host ''
Write-Host "Built: $setup ($([math]::Round((Get-Item -LiteralPath $setup).Length / 1MB, 1)) MB)"
