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

.PARAMETER PadToMax
    Pad a private copy of the payload to the supported 8 MiB limit, exercising
    the relocation boundary without changing the input image.

.PARAMETER Expect
    Fail unless this literal text appears in the captured target transcript.

.EXAMPLE
    .\scripts\chainboot-qemu.ps1
    .\scripts\chainboot-qemu.ps1 -Payload multicore
    .\scripts\chainboot-qemu.ps1 -Payload .\kernel8.img -RunSeconds 15
    .\scripts\chainboot-qemu.ps1 -Payload multicore -PadToMax -Expect 'All 4 cores online'
#>
param(
    [string]$Payload = 'hello-uart',
    [int]$Port = 4444,
    [int]$RunSeconds = 8,
    [string]$LogPath,
    [switch]$PadToMax,
    [string]$Expect
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Push-Location $root
$paddedImage = $null
$createdPaddedImage = $false

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

    if ($PadToMax) {
        $bytes = [System.IO.File]::ReadAllBytes($image)
        if ($bytes.Length -gt 8MB) { throw 'Payload exceeds the 8 MiB loader limit.' }
        $paddedImage = Join-Path $root "target\chainboot-padded-$PID-$Port.img"
        $file = [System.IO.File]::Open($paddedImage, [System.IO.FileMode]::CreateNew)
        $createdPaddedImage = $true
        try {
            $file.Write($bytes, 0, $bytes.Length)
            $file.SetLength(8MB)
        } finally {
            $file.Dispose()
        }
        $image = $paddedImage
    }
    if (($PadToMax -or $Expect) -and -not $LogPath) {
        $LogPath = Join-Path $root "target\chainboot-qemu-$Port.log"
    }

    # ----------------------------------------------------------------------
    # QEMU. The FIRST -serial is the PL011 the kernel prints on; the second is
    # the AUX mini UART and is discarded.
    # ----------------------------------------------------------------------
    $qemuArgs = @(
        '-M', 'raspi3b',
        '-display', 'none',
        '-serial', "tcp:127.0.0.1:$Port,server=on,wait=on",
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
    if ($PadToMax -or $Expect) {
        $transcript = Get-Content -LiteralPath $LogPath -Raw
        if ($PadToMax -and $transcript -notmatch 'received\s*:\s*8388608\b') {
            throw 'The loader did not receive the complete 8 MiB regression payload.'
        }
        if ($Expect -and -not $transcript.Contains($Expect)) {
            throw "Expected target output was not observed: $Expect"
        }
    }
} finally {
    if ($qemu -and -not $qemu.HasExited) { Stop-Process -Id $qemu.Id -Force }
    if ($createdPaddedImage -and (Test-Path -LiteralPath $paddedImage)) {
        Remove-Item -LiteralPath $paddedImage
    }
    Pop-Location
}
