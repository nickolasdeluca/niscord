<#
.SYNOPSIS
    Frees disk space taken by Niscord's build output (target\).

.DESCRIPTION
    Cargo never deletes old build output by itself, so target\ keeps growing:
    every test binary, incremental cache and superseded dependency build stays.
    Everything removed here is rebuilt on demand.

    By default this removes the debug build (target\debug: tests and
    `cargo run`) and the release build's incremental cache, but keeps the
    compiled release dependencies, so the next release build stays quick, and
    keeps target\release\niscord.exe.

        scripts\clean.ps1            # the usual clean-up
        scripts\clean.ps1 -Release   # also old release builds (keeps the exes)
        scripts\clean.ps1 -All       # everything (like `cargo clean`)
        scripts\clean.ps1 -WhatIf    # just show what would go, and its size

.PARAMETER Release
    Also clear target
elease, keeping niscord.exe and niscord-server.exe.
    Cargo keeps every old build of every dependency: each compiler update or
    change of build flags adds a full set (several GB) that is never used
    again. The next release build recompiles the dependencies once.

.PARAMETER All
    Remove all of target\, including the built executables.
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [switch]$Release,
    [switch]$All
)

$ErrorActionPreference = 'Stop'
$target = Join-Path (Split-Path $PSScriptRoot -Parent) 'target'

function Get-Size($Path) {
    if (-not (Test-Path $Path)) { return 0 }
    $sum = (Get-ChildItem $Path -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object Length -Sum).Sum
    if ($sum) { [long]$sum } else { 0 }
}

function Format-Size([long]$Bytes) {
    if ($Bytes -ge 1GB) { '{0:N1} GB' -f ($Bytes / 1GB) } else { '{0:N0} MB' -f ($Bytes / 1MB) }
}

if (-not (Test-Path $target)) {
    Write-Host 'Nothing to clean: there is no target folder.'
    return
}

if ($All) {
    $paths = @($target)
}
else {
    $paths = @(
        (Join-Path $target 'debug'),
        (Join-Path $target 'release\incremental'),
        # Leftovers of builds into a side folder, e.g. while niscord.exe was running.
        (Join-Path $target 'preview')
    )
}

$before = Get-Size $target
$freed = 0

# The built programs survive -Release: set them aside while the folder goes.
$keep = @()
$releaseDir = Join-Path $target 'release'
if ($Release -and -not $All -and (Test-Path $releaseDir)) {
    $paths += $releaseDir
    $keep = @('niscord.exe', 'niscord-server.exe') | ForEach-Object { Join-Path $releaseDir $_ } | Where-Object { Test-Path $_ }
}
$stash = Join-Path ([IO.Path]::GetTempPath()) "niscord-clean-$PID"
if ($keep -and -not $WhatIfPreference) {
    New-Item -ItemType Directory -Force $stash | Out-Null
    $keep | ForEach-Object { Copy-Item $_ $stash }
}
foreach ($path in $paths) {
    if (-not (Test-Path $path)) { continue }
    $size = Get-Size $path
    $label = $path.Substring((Split-Path $target -Parent).Length + 1)
    if ($PSCmdlet.ShouldProcess("$label ($(Format-Size $size))", 'Remove')) {
        try {
            Remove-Item $path -Recurse -Force
            $freed += $size
            Write-Host ("Removed {0,-28} {1,10}" -f $label, (Format-Size $size))
        }
        catch {
            # Usually a running niscord.exe or test holding a file open.
            Write-Warning "Couldn't remove all of ${label}: $($_.Exception.Message)"
            $freed += $size - (Get-Size $path)
        }
    }
}

if ($keep -and -not $WhatIfPreference) {
    New-Item -ItemType Directory -Force $releaseDir | Out-Null
    Get-ChildItem $stash | ForEach-Object {
        $kept = Join-Path $releaseDir $_.Name
        if (-not (Test-Path $kept)) { Move-Item $_.FullName $kept }
        $freed -= $_.Length
    }
    Remove-Item $stash -Recurse -Force
    Write-Host ("Kept    {0}" -f (($keep | ForEach-Object { Split-Path $_ -Leaf }) -join ', '))
}

if (-not $WhatIfPreference) {
    Write-Host ''
    Write-Host ("Freed {0}. target\ was {1}, now {2}." -f (Format-Size $freed), (Format-Size $before), (Format-Size (Get-Size $target))) -ForegroundColor Green
}
