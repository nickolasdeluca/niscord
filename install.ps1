<#
.SYNOPSIS
    Installs (or updates) Niscord from the latest GitHub release.

.DESCRIPTION
    Downloads niscord.exe from the latest release, checks it against the
    release's SHA256SUMS.txt, installs it for the current user (no admin
    rights needed) and adds a Start Menu shortcut. Niscord keeps itself up
    to date afterwards.

    One-liner:
        irm https://raw.githubusercontent.com/nickolasdeluca/niscord/main/install.ps1 | iex

    With your group's server filled in:
        & ([scriptblock]::Create((irm https://raw.githubusercontent.com/nickolasdeluca/niscord/main/install.ps1))) -Server wss://your.server

.PARAMETER Server
    Server address to save in Niscord's settings (e.g. wss://niscord.example.com).

.PARAMETER Desktop
    Also put a shortcut on the desktop.

.PARAMETER NoLaunch
    Don't start Niscord when done.

.PARAMETER InstallDir
    Where to install (default: %LOCALAPPDATA%\Programs\Niscord).

.PARAMETER Uninstall
    Remove Niscord and its shortcuts (settings and logs are kept).
#>
[CmdletBinding()]
param(
    [string]$Server,
    [switch]$Desktop,
    [switch]$NoLaunch,
    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\Niscord'),
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'nickolasdeluca/niscord'
$Exe = Join-Path $InstallDir 'niscord.exe'
$StartMenuLink = Join-Path ([Environment]::GetFolderPath('Programs')) 'Niscord.lnk'
$DesktopLink = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Niscord.lnk'
$SettingsFile = Join-Path $env:APPDATA 'Niscord\settings.json'

function Write-Step($Text) { Write-Host "==> $Text" -ForegroundColor Cyan }

function Stop-IfRunning {
    $running = Get-Process niscord -ErrorAction SilentlyContinue |
        Where-Object { $_.Path -and ($_.Path -ieq $Exe) }
    if ($running) {
        throw "Niscord is running. Close it (this would end any stream) and run the installer again."
    }
}

function New-Shortcut($Path) {
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($Path)
    $link.TargetPath = $Exe
    $link.WorkingDirectory = $InstallDir
    $link.IconLocation = "$Exe,0"
    $link.Description = 'Niscord: screen sharing with friends'
    $link.Save()
}

if ($Uninstall) {
    Stop-IfRunning
    Write-Step 'Removing Niscord'
    foreach ($path in @($StartMenuLink, $DesktopLink)) {
        if (Test-Path $path) { Remove-Item $path -Force }
    }
    if (Test-Path $InstallDir) { Remove-Item $InstallDir -Recurse -Force }
    Write-Host "Niscord was removed. Settings and logs are still in $(Split-Path $SettingsFile)."
    return
}

Write-Step 'Finding the latest release'
$release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ 'User-Agent' = 'Niscord-installer' }
$exeAsset = $release.assets | Where-Object name -eq 'niscord.exe'
$sumsAsset = $release.assets | Where-Object name -eq 'SHA256SUMS.txt'
if (-not $exeAsset -or -not $sumsAsset) {
    throw "The latest release ($($release.tag_name)) has no niscord.exe or SHA256SUMS.txt."
}
$version = $release.tag_name -replace '^v', ''
Write-Host "Niscord $version"

Write-Step 'Downloading'
$temp = Join-Path ([IO.Path]::GetTempPath()) "niscord-$version-$PID.exe"
try {
    Invoke-WebRequest $exeAsset.browser_download_url -OutFile $temp -UseBasicParsing
    $sums = (Invoke-WebRequest $sumsAsset.browser_download_url -UseBasicParsing).Content
    if ($sums -is [byte[]]) { $sums = [Text.Encoding]::UTF8.GetString($sums) }
    $expected = ($sums -split "`n" | Where-Object { $_ -match '^\s*([0-9a-fA-F]{64})\s+\*?niscord\.exe\s*$' } |
        ForEach-Object { $Matches[1] } | Select-Object -First 1)
    if (-not $expected) { throw 'SHA256SUMS.txt has no entry for niscord.exe.' }
    $actual = (Get-FileHash $temp -Algorithm SHA256).Hash
    if ($actual -ne $expected.ToUpperInvariant()) {
        throw "The download doesn't match the release checksum (got $actual, expected $expected)."
    }

    Write-Step "Installing to $InstallDir"
    Stop-IfRunning
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Move-Item $temp $Exe -Force
}
finally {
    if (Test-Path $temp) { Remove-Item $temp -Force }
}

New-Shortcut $StartMenuLink
if ($Desktop) { New-Shortcut $DesktopLink }

if ($Server) {
    Write-Step 'Saving the server address'
    New-Item -ItemType Directory -Force -Path (Split-Path $SettingsFile) | Out-Null
    $settings = [ordered]@{}
    if (Test-Path $SettingsFile) {
        $existing = Get-Content $SettingsFile -Raw | ConvertFrom-Json
        foreach ($property in $existing.PSObject.Properties) { $settings[$property.Name] = $property.Value }
    }
    $settings['server_url'] = $Server
    # Niscord reads UTF-8 without a byte order mark.
    [IO.File]::WriteAllText($SettingsFile, ($settings | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
}

Write-Host ''
Write-Host "Niscord $version is installed. Find it in the Start Menu; it updates itself from now on." -ForegroundColor Green
Write-Host 'The first time you share or watch, Windows Firewall asks about Niscord: choose Allow.'
if (-not $NoLaunch) { Start-Process $Exe }
