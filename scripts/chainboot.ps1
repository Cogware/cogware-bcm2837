<#
.SYNOPSIS
    Builds an example and chain-loads it onto the board over UART.

.DESCRIPTION
    The Windows counterpart of scripts/chainboot.sh. Compiles the current
    sources, converts the ELF into the flat binary the chain-loader expects,
    and pushes it to the loader already running on the Pi.

    ORDER MATTERS. Start this script first, then power-cycle the Pi. The
    loader sends three 0x03 bytes on boot; if nothing is listening then, the
    push waits forever.

    The push needs exclusive access to the serial port; close any terminal
    attached to it before running.

.PARAMETER Example
    Example to build and push. Defaults to 'shell', the full reference kernel.

.PARAMETER Serial
    COM port of the USB-TTL adapter, e.g. COM3. Required unless -BuildOnly.

.PARAMETER Baud
    Serial line rate. Must match the kernel's, which is 115200.

.PARAMETER Profile
    Cargo profile to build: release (default) or debug.

.PARAMETER BuildOnly
    Build and objcopy, but do not push. Prints the .img path.

.PARAMETER List
    List the examples this crate defines, then exit.

.EXAMPLE
    .\scripts\chainboot.ps1 -Serial COM3

.EXAMPLE
    .\scripts\chainboot.ps1 multicore -Serial COM3

.EXAMPLE
    .\scripts\chainboot.ps1 hello-uart -BuildOnly
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string]$Example = 'shell',
    [string]$Serial,
    [int]$Baud = 115200,
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release',
    [switch]$BuildOnly,
    [switch]$List
)

$ErrorActionPreference = 'Stop'

# Agent shells inherit a stale PATH; re-read the persisted user PATH so that
# scoop's toolchain is always found.
$env:PATH = [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $env:PATH

$root = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $root 'cogware-bcm2837\Cargo.toml'

# ---------------------------------------------------------------------------
# Required features vary per example (see Cargo.toml); read them at run time
# rather than hardcoding a list that would rot.
# ---------------------------------------------------------------------------

function Get-ExampleBlocks {
    $text = Get-Content -Raw -LiteralPath $manifest
    $parts = $text -split '\[\[example\]\]'
    return $parts[1..($parts.Count - 1)]
}

function Get-ExampleNames {
    foreach ($block in Get-ExampleBlocks) {
        if ($block -match 'name\s*=\s*"([^"]+)"') { $Matches[1] }
    }
}

function Get-RequiredFeatures {
    param([string]$Name)
    foreach ($block in Get-ExampleBlocks) {
        if ($block -match 'name\s*=\s*"([^"]+)"' -and $Matches[1] -eq $Name) {
            if ($block -match 'required-features\s*=\s*\[([^\]]*)\]') {
                $inner = $Matches[1]
                return (([regex]::Matches($inner, '"([^"]+)"') |
                    ForEach-Object { $_.Groups[1].Value }) -join ',')
            }
            return ''
        }
    }
    return $null
}

if ($List) {
    Get-ExampleNames
    return
}

$features = Get-RequiredFeatures -Name $Example
if ($null -eq $features) {
    Write-Host "chainboot: no [[example]] named '$Example' in the crate manifest." -ForegroundColor Red
    Write-Host ''
    Write-Host 'Available examples:'
    Get-ExampleNames | ForEach-Object { Write-Host "  $_" }
    exit 1
}

if (-not $BuildOnly -and -not $Serial) {
    throw 'chainboot: -Serial is required (e.g. -Serial COM3), or pass -BuildOnly.'
}

# --- build ------------------------------------------------------------------
Write-Host "==> building '$Example' [$Profile] features: $(if ($features) { $features } else { 'none' })" -ForegroundColor Cyan

$cargoArgs = @('build', '--manifest-path', $manifest, '--example', $Example)
if ($Profile -eq 'release') { $cargoArgs += '--release' }
if ($features) { $cargoArgs += @('--features', $features) }

& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

$elf = Join-Path $root "target\aarch64-unknown-none\$Profile\examples\$Example"
if (-not (Test-Path $elf)) { throw "expected ELF not found at $elf" }

# --- flat binary ------------------------------------------------------------
# The ELF keeps its full DWARF for JTAG; objcopy -O binary emits only the
# loadable sections, which is what the chain-loader copies to 0x80000.
$objcopy = $null
foreach ($candidate in @('rust-objcopy', 'llvm-objcopy', 'aarch64-linux-gnu-objcopy')) {
    if (Get-Command $candidate -ErrorAction SilentlyContinue) { $objcopy = $candidate; break }
}
if (-not $objcopy) {
    throw 'chainboot: no objcopy found (tried rust-objcopy, llvm-objcopy, aarch64-linux-gnu-objcopy)'
}

$img = Join-Path $root "target\aarch64-unknown-none\$Profile\examples\$Example.img"
& $objcopy -O binary $elf $img
if ($LASTEXITCODE -ne 0) { throw 'objcopy failed' }

$size = (Get-Item $img).Length
Write-Host "==> $Example.img  $size bytes  (via $objcopy)" -ForegroundColor Cyan

if ($BuildOnly) {
    Write-Output $img
    return
}

# --- push -------------------------------------------------------------------
Write-Host '==> pushing. Power-cycle the Pi now.' -ForegroundColor Yellow
& (Join-Path $PSScriptRoot 'minipush.ps1') -Serial $Serial -Payload $img -Baud $Baud