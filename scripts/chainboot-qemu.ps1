<#
.SYNOPSIS
    Boots the chain-loader under QEMU and pushes a payload into it.

.DESCRIPTION
    Emulates the hardware debug loop: build a payload, start the loader,
    push, watch it boot. Use to test loader/protocol changes without a board.

    QEMU's serial socket uses `wait=on`: with `wait=off` the guest starts
    immediately, and output before the pusher connects (including the
    request token) is lost, so the push times out.

.PARAMETER Payload
    Example name to chain-load, or a path to a flat image. An example is built
    and objcopy'd for you.

.PARAMETER Port
    TCP port for the emulated PL011.

.PARAMETER RunSeconds
    How long to capture the payload's output after the push.

.PARAMETER LogPath
    Write the whole transcript here as well.

.EXAMPLE
    .\scripts\chainboot-qemu.ps1
    .\scripts\chainboot-qemu.ps1 -Payload multicore
    .\scripts\chainboot-qemu.ps1 -Payload .\kernel8.img -RunSeconds 15
#>
param(
    [string]$Payload = 'hello-uart',
    [int]$Port = 4444,
    [int]$RunSeconds = 8,
    [string]$LogPath
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root

try {
    # ----------------------------------------------------------------------
    # Payload: an example name, or an image that already exists
    # ----------------------------------------------------------------------
    if (Test-Path -LiteralPath $Payload) {
        $image = (Resolve-Path -LiteralPath $Payload).Path
    } else {
        $image = Join-Path $root "$Payload.img"
        Write-Host "Building payload '$Payload'..." -ForegroundColor Cyan
        cargo objcopy --release --example $Payload -- -O binary $image
        if ($LASTEXITCODE -ne 0) { throw "Failed to build example '$Payload'." }
    }

    Write-Host 'Building the chain-loader...' -ForegroundColor Cyan
    cargo build --release --example chainloader
    if ($LASTEXITCODE -ne 0) { throw 'Failed to build the chain-loader.' }

    $loader = Join-Path $root 'target\aarch64-unknown-none\release\examples\chainloader'

    # ----------------------------------------------------------------------
    # QEMU. The FIRST -serial is the PL011 the kernel prints on; the second is
    # the AUX mini UART and is discarded.
    # ----------------------------------------------------------------------
    $qemuArgs = @(
        '-M', 'raspi3b',
        '-display', 'none',
        '-serial', "tcp::$Port,server=on,wait=on",
        '-serial', 'null',
        '-kernel', $loader
    )

    Write-Host "Starting QEMU (PL011 on tcp:$Port, waiting for the pusher)..." -ForegroundColor Cyan
    $qemu = Start-Process qemu-system-aarch64 -ArgumentList $qemuArgs -PassThru -WindowStyle Hidden

    # `wait=on` means the socket is listening before the guest runs, so a short
    # settle is enough; there is no output to miss.
    $deadline = (Get-Date).AddSeconds(10)
    while ((Get-Date) -lt $deadline) {
        if (Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue) { break }
        Start-Sleep -Milliseconds 200
    }

    $push = @{
        Tcp        = "localhost:$Port"
        Payload    = $image
        NoTerminal = $true
        RunSeconds = $RunSeconds
    }
    if ($LogPath) { $push.LogPath = $LogPath }

    & (Join-Path $PSScriptRoot 'minipush.ps1') @push
} finally {
    if ($qemu -and -not $qemu.HasExited) { Stop-Process -Id $qemu.Id -Force }
    Pop-Location
}
