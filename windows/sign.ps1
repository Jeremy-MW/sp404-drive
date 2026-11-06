<#
.SYNOPSIS
    Authenticode-signs files with a code-signing certificate on a hardware token, then
    verifies every signature.

.DESCRIPTION
    Used by build-installer.ps1 -Sign. Any failure (token absent, certificate not found,
    password dialog cancelled, timestamp server unreachable, signing or verification
    failed, file missing) throws and stops the build, so nothing goes on to be packaged
    unsigned.

    The token's password is typed into the token's own prompt, once per signature. This
    script never asks for it, passes it, stores it or logs it.

.PARAMETER Path
    The files to sign. Each must exist.

.PARAMETER CertificateSubject
    Matched against the certificate's subject, to pick the right certificate (the Sectigo
    certificate on the SafeNet token, by default).

.PARAMETER SignToolPath
    An explicit signtool.exe, tried before the search.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string[]]$Path,
    [string]$CertificateSubject = 'Jeremy Murray-Wakefield',
    [string]$SignToolPath,
    [string]$TimestampUrl = 'http://timestamp.sectigo.com'
)

$ErrorActionPreference = 'Stop'

$WindowsKitsBin = 'C:\Program Files (x86)\Windows Kits\10\bin'

# The explicit path, then the newest x64 signtool in the Windows SDK. The x86 build can't
# drive a 64-bit token's CSP reliably.
function Find-SignTool([string]$Explicit) {
    if ($Explicit) {
        if (-not (Test-Path -LiteralPath $Explicit -PathType Leaf)) {
            throw "signtool not found at $Explicit"
        }
        return (Resolve-Path -LiteralPath $Explicit).Path
    }
    if (Test-Path -LiteralPath $WindowsKitsBin) {
        # Sorted as versions, so 10.0.9 doesn't come above 10.0.28000.
        $found = Get-ChildItem -LiteralPath $WindowsKitsBin -Directory |
            Where-Object { $_.Name -as [version] } |
            Sort-Object { [version]$_.Name } -Descending |
            ForEach-Object { Join-Path $_.FullName 'x64\signtool.exe' } |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Select-Object -First 1
        if ($found) { return $found }
    }
    throw "signtool not found under $WindowsKitsBin; install the Windows SDK's signing tools or pass -SignToolPath."
}

function Invoke-SignTool([string]$Exe, [string[]]$Arguments) {
    & $Exe @Arguments 2>&1 | ForEach-Object { Write-Host "    $_" }
    return $LASTEXITCODE
}

# Check every file before the first password prompt.
$signTool = Find-SignTool $SignToolPath
$files = foreach ($p in $Path) {
    if (-not (Test-Path -LiteralPath $p -PathType Leaf)) { throw "Missing: $p" }
    (Resolve-Path -LiteralPath $p).Path
}

Write-Host "signtool: $signTool"
Write-Host "Signing $($files.Count) file(s); the token asks for its password once per file."

foreach ($file in $files) {
    Write-Host "Signing: $file"
    # No /as: appending would pile up signatures on a re-run. A plain sign replaces.
    $code = Invoke-SignTool $signTool @(
        'sign', '/a', '/n', $CertificateSubject,
        '/fd', 'SHA256', '/tr', $TimestampUrl, '/td', 'SHA256', '/v', $file)
    if ($code -ne 0) {
        throw "Signing failed for $file (signtool exit code $code). Is the token in, and was the password entered? Is $TimestampUrl reachable?"
    }
}

foreach ($file in $files) {
    Write-Host "Verifying: $file"
    # /pa: the Authenticode policy (the default driver policy rejects an application).
    $code = Invoke-SignTool $signTool @('verify', '/pa', '/v', $file)
    if ($code -ne 0) { throw "Signature verification failed for $file (exit code $code)." }
}

Write-Host "Signed and verified $($files.Count) file(s)."
