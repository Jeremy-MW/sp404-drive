<#
.SYNOPSIS
    Builds SP-404 Drive's Windows installer: target\installer\SP-404-Drive-<version>-windows-x64-setup.exe.

.DESCRIPTION
    1. Builds the three programs (cargo build --release), with the C runtime linked in
       (.cargo\config.toml).
    2. Downloads WinFsp's installer, the version pinned below, once, into
       target\installer, and checks its SHA-256 every time.
    3. Writes the app icon (drawn by the tray app's own code) and the licence page.
    4. Compiles windows\installer.nsi with NSIS, in two passes: the first only makes
       Uninstall.exe, so it can be signed; the second makes the installer, carrying it.

    With -Sign, it signs the three programs, Uninstall.exe and the installer (sign.ps1),
    then checks the signatures of the files inside the finished installer. That's five
    signatures, so the token asks for its password five times. Without -Sign nothing is
    signed, and the installer isn't for release.

.PARAMETER SkipBuild
    Use the programs already in target\release.

.PARAMETER Sign
    Sign everything, for a release.
#>
[CmdletBinding()]
param([switch]$SkipBuild, [switch]$Sign)

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
$programs = foreach ($exe in 'sp404drive.exe', 'sp404drive-host.exe', 'sp404drive-tray.exe') {
    $path = Join-Path $bin $exe
    if (-not (Test-Path -LiteralPath $path)) { throw "Missing: $path" }
    $path
}
$signScript = Join-Path $PSScriptRoot 'sign.ps1'
if ($Sign) {
    # Before packaging: the installer carries these files as they are.
    & $signScript -Path $programs
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

Copyright $([char]0x00A9) 2026 polecatplugins.com.

SP-404 Drive is free software: you can redistribute it and/or modify it under the terms of version 3 of the GNU General Public License as published by the Free Software Foundation. It comes with no warranty. Source code: https://github.com/Jeremy-MW/sp404-drive

$gpl
"@

# Both passes get the same defines.
$staging = Join-Path ([IO.Path]::GetTempPath()) ("sp404drive-uninstaller-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $staging | Out-Null
$defines = @(
    '/V3', '/INPUTCHARSET', 'UTF8'
    "/DVersion=$version"
    "/DBinDir=$bin"
    "/DOutDir=$out"
    "/DWinFspMsi=$msi"
    "/DWinFspVersion=$($WinFsp.Version)"
    "/DIconFile=$icon"
    "/DLicenceFile=$licence"
    "/DNoticesFile=$PSScriptRoot\THIRD-PARTY-NOTICES.txt"
    "/DUninstallerStagingDir=$staging"
)
$nsi = Join-Path $PSScriptRoot 'installer.nsi'
try {
    # Pass 1: a throwaway installer that only writes Uninstall.exe into $staging. It asks
    # for administrator rights like the real one (the uninstaller takes the same level),
    # so it's run as invoker: all it does is write into the staging folder.
    & $MakeNsis '/DGenerateUninstallerOnly' @defines $nsi
    if ($LASTEXITCODE -ne 0) { throw "makensis (pass 1) failed ($LASTEXITCODE)" }
    $generator = Join-Path $staging 'GenerateUninstaller.exe'
    $env:__COMPAT_LAYER = 'RunAsInvoker'
    try {
        $run = Start-Process -FilePath $generator -ArgumentList '/S' -PassThru -Wait
    } finally {
        Remove-Item Env:\__COMPAT_LAYER
    }
    if ($run.ExitCode -ne 0) { throw "The uninstaller generator failed ($($run.ExitCode))" }
    $uninstaller = Join-Path $staging 'Uninstall.exe'
    if (-not (Test-Path -LiteralPath $uninstaller)) { throw "The generator didn't write $uninstaller" }

    # Now or never: once inside the installer, it can't be signed.
    if ($Sign) { & $signScript -Path $uninstaller }

    # Pass 2: the installer, carrying the uninstaller as an ordinary file.
    & $MakeNsis @defines $nsi
    if ($LASTEXITCODE -ne 0) { throw "makensis (pass 2) failed ($LASTEXITCODE)" }
} finally {
    Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
}

$setup = Join-Path $out "SP-404-Drive-$version-windows-x64-setup.exe"

if ($Sign) {
    & $signScript -Path $setup

    # Check the files inside the finished installer, not the ones that went in: an
    # unsigned uninstaller would only show when someone uninstalled ("Publisher: Unknown").
    $sevenZip = 'C:\Program Files\7-Zip\7z.exe'
    if (-not (Test-Path -LiteralPath $sevenZip)) {
        throw "7-Zip is needed to check the signatures inside the installer: $sevenZip"
    }
    $inspect = Join-Path ([IO.Path]::GetTempPath()) ("sp404drive-verify-" + [guid]::NewGuid())
    try {
        & $sevenZip x $setup "-o$inspect" -y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "7-Zip couldn't unpack the installer ($LASTEXITCODE)" }
        foreach ($name in 'Uninstall.exe', 'sp404drive.exe', 'sp404drive-host.exe', 'sp404drive-tray.exe') {
            $found = Get-ChildItem -LiteralPath $inspect -Recurse -File -Filter $name | Select-Object -First 1
            if (-not $found) { throw "The installer doesn't contain $name" }
            $status = (Get-AuthenticodeSignature -LiteralPath $found.FullName).Status
            if ($status -ne 'Valid') { throw "$name inside the installer isn't validly signed ($status)" }
            Write-Host "Inside the installer, $name is signed."
        }
    } finally {
        Remove-Item -LiteralPath $inspect -Recurse -Force -ErrorAction SilentlyContinue
    }
}

Write-Host ''
Write-Host "Built: $setup ($([math]::Round((Get-Item -LiteralPath $setup).Length / 1MB, 1)) MB)"
if (-not $Sign) { Write-Host 'Not signed: not for release.' }
