[CmdletBinding()]
param(
    [string]$Version = $env:WATCHCAT_VERSION,
    [string]$InstallDir = $env:WATCHCAT_INSTALL_DIR,
    [string]$Repository = $(if ($env:WATCHCAT_REPOSITORY) { $env:WATCHCAT_REPOSITORY } else { "hx-w/watchcat" }),
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"
if (-not $Version) { $Version = "latest" }
if (-not $InstallDir) { $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\watchcat\bin" }

if (-not [Environment]::Is64BitOperatingSystem) {
    throw "Watchcat requires a 64-bit Windows installation."
}
$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
if ($architecture -ne "X64") {
    throw "Unsupported Windows architecture: $architecture"
}
$target = "x86_64-pc-windows-msvc"

if ($Version -eq "latest") {
    $release = Invoke-RestMethod -Headers @{ "User-Agent" = "watchcat-installer" } -Uri "https://api.github.com/repos/$Repository/releases/latest"
    $Version = $release.tag_name
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+$') { throw "Invalid release version: $Version" }

$archive = "watchcat-$target.zip"
$baseUrl = if ($env:WATCHCAT_RELEASE_BASE_URL) { $env:WATCHCAT_RELEASE_BASE_URL } else { "https://github.com/$Repository/releases/download/$Version" }
if ($baseUrl -notmatch '^https://' -and $baseUrl -notmatch '^http://(127\.0\.0\.1|localhost):') {
    throw "Release base URL must use HTTPS (localhost HTTP is allowed for tests)."
}
$destination = Join-Path $InstallDir "watchcat.exe"
if ($DryRun) {
    Write-Output "Would install $Version for $target to $destination"
    exit 0
}

$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("watchcat-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    $archivePath = Join-Path $temporary $archive
    $checksumsPath = Join-Path $temporary "SHA256SUMS"
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$archive" -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/SHA256SUMS" -OutFile $checksumsPath

    $escapedArchive = [regex]::Escape($archive)
    $checksumLine = Get-Content $checksumsPath | Where-Object { $_ -match "^([0-9a-fA-F]{64})\s+$escapedArchive$" } | Select-Object -First 1
    if (-not $checksumLine) { throw "Checksum is missing for $archive" }
    $expected = ($checksumLine -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { throw "Checksum verification failed" }

    Expand-Archive -Path $archivePath -DestinationPath $temporary
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $source = Join-Path $temporary "watchcat-$target\watchcat.exe"
    $staged = Join-Path $InstallDir (".watchcat." + [guid]::NewGuid() + ".tmp")
    $backup = Join-Path $temporary "watchcat.exe.backup"
    $hadExisting = Test-Path $destination
    if ($hadExisting) { Copy-Item $destination $backup }
    try {
        Copy-Item $source $staged
        Move-Item -Force $staged $destination
        $actualVersion = & $destination --version
        $expectedVersion = "watchcat " + $Version.Substring(1)
        if ($actualVersion -ne $expectedVersion) {
            throw "Expected '$expectedVersion', got '$actualVersion'"
        }
    }
    catch {
        if ($hadExisting -and (Test-Path $backup)) {
            Copy-Item -Force $backup $destination
        }
        elseif (Test-Path $destination) {
            Remove-Item -Force $destination
        }
        throw
    }
    finally {
        Remove-Item -Force -ErrorAction SilentlyContinue $staged
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $InstallDir) {
        $newPath = (@($entries) + $InstallDir) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Output "Added $InstallDir to the user PATH. Open a new terminal to use it."
    }
    Write-Output "Installed Watchcat $Version to $destination"
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $temporary
}
