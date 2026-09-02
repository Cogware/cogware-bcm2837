<#
.SYNOPSIS
    Pushes a kernel image to the CogWare chain-loader over a serial line or TCP.

.DESCRIPTION
    Speaks the same protocol as `minipush.rb` from the rust-raspberrypi-OS-tutorials
    project, so either tool drives either loader:

        target -> host   banner text                 printed as it arrives
        target -> host   0x03 0x03 0x03              "send me a kernel"
        host   -> target uint32 little-endian        payload length
        target -> host   "OK"                        length accepted
        host   -> target <length> bytes              the raw image
        both             free-running terminal       the payload's own console

    Two transports. `-Serial` is real hardware: a USB-TTL adapter on GPIO 14/15.
    `-Tcp` is QEMU, which is how this is tested without a board:

        qemu-system-aarch64 -M raspi3b -display none `
          -serial tcp::4444,server=on,wait=off -serial null `
          -kernel target\aarch64-unknown-none\release\examples\chainloader

.PARAMETER Serial
    COM port of a USB-TTL adapter, e.g. COM3.

.PARAMETER Tcp
    host:port of a QEMU serial socket, e.g. localhost:4444.

.PARAMETER Payload
    The raw image to send. Must be a flat binary from `cargo image`, not an ELF —
    the loader copies it verbatim to 0x80000 and branches there.

.PARAMETER Baud
    Serial line rate. Must match the kernel's, which is 115200.

.PARAMETER NoTerminal
    Exit after the push instead of becoming a terminal. Used by the automated
    test; with -RunSeconds it captures the payload's output first.

.PARAMETER RunSeconds
    With -NoTerminal, how long to keep reading the payload's output.

.PARAMETER LogPath
    Also write everything received to this file.

.EXAMPLE
    .\scripts\minipush.ps1 -Serial COM3 -Payload kernel8.img

.EXAMPLE
    .\scripts\minipush.ps1 -Tcp localhost:4444 -Payload kernel8.img
#>
[CmdletBinding(DefaultParameterSetName = 'Serial')]
param(
    [Parameter(ParameterSetName = 'Serial', Mandatory)]
    [string]$Serial,

    [Parameter(ParameterSetName = 'Tcp', Mandatory)]
    [string]$Tcp,

    [Parameter(Mandatory)]
    [string]$Payload,

    [int]$Baud = 115200,
    [switch]$NoTerminal,
    [int]$RunSeconds = 5,
    [string]$LogPath,
    [int]$HandshakeTimeoutSec = 30
)

$ErrorActionPreference = 'Stop'

# --------------------------------------------------------------------------
# Payload
# --------------------------------------------------------------------------

if (-not (Test-Path -LiteralPath $Payload)) {
    throw "Payload not found: $Payload"
}
$bytes = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $Payload))
if ($bytes.Length -eq 0) { throw 'Payload is empty.' }

# An ELF starts 7F 45 4C 46. The loader would copy the header to 0x80000 and
# branch into it, which fails in a way that looks like a hardware fault, so
# catch it here instead.
if ($bytes.Length -ge 4 -and $bytes[0] -eq 0x7F -and $bytes[1] -eq 0x45 -and
    $bytes[2] -eq 0x4C -and $bytes[3] -eq 0x46) {
    throw "$Payload is an ELF. The loader needs a flat image: cargo image --example <name> -- -O binary kernel8.img"
}

# CRC-32/ISO-HDLC, so the value printed here can be compared with the one the
# loader prints back.
#
# All arithmetic is int64 and explicitly masked: PowerShell parses a 32-bit hex
# literal as a *signed* Int32, so 0xFFFFFFFF is -1 and 0xEDB88320 is negative.
function Get-Crc32 {
    param([byte[]]$Data)
    $mask = 0xFFFFFFFFL
    $poly = 0xEDB88320L
    $table = New-Object 'System.Int64[]' 256
    for ($i = 0; $i -lt 256; $i++) {
        $c = [int64]$i
        for ($k = 0; $k -lt 8; $k++) {
            if ($c -band 1L) { $c = ($poly -bxor ($c -shr 1)) -band $mask }
            else { $c = ($c -shr 1) -band $mask }
        }
        $table[$i] = $c
    }
    $crc = $mask
    foreach ($b in $Data) {
        $crc = ($table[[int](($crc -bxor $b) -band 0xFFL)] -bxor ($crc -shr 8)) -band $mask
    }
    return ($crc -bxor $mask) -band $mask
}

$crc = Get-Crc32 $bytes

Write-Host ''
Write-Host 'CogWare minipush' -ForegroundColor Cyan
Write-Host ("  payload         : {0}" -f (Resolve-Path -LiteralPath $Payload))
Write-Host ("  size            : {0} bytes ({1:N1} KiB)" -f $bytes.Length, ($bytes.Length / 1KB))
Write-Host ("  crc32           : 0x{0:x8}" -f $crc)

# --------------------------------------------------------------------------
# Transport
# --------------------------------------------------------------------------

$port = $null
$client = $null

if ($PSCmdlet.ParameterSetName -eq 'Serial') {
    Write-Host ("  transport       : {0} at {1} baud" -f $Serial, $Baud)
    try {
        $port = New-Object System.IO.Ports.SerialPort $Serial, $Baud, 'None', 8, 'One'
    } catch {
        throw "System.IO.Ports is unavailable. On PowerShell 7 run: Install-Module -Name Microsoft.PowerShell.ConsoleGuiTools; or use Windows PowerShell 5.1. ($_)"
    }
    $port.ReadTimeout = 200
    $port.WriteTimeout = 5000
    $port.Open()
    $stream = $port.BaseStream
} else {
    $parts = $Tcp -split ':'
    if ($parts.Count -ne 2) { throw "-Tcp wants host:port, got '$Tcp'" }
    Write-Host ("  transport       : tcp {0}:{1}" -f $parts[0], $parts[1])
    $client = New-Object System.Net.Sockets.TcpClient
    $client.Connect($parts[0], [int]$parts[1])
    $client.NoDelay = $true
    $stream = $client.GetStream()
    $stream.ReadTimeout = 200
    $stream.WriteTimeout = 5000
}

$log = $null
if ($LogPath) { $log = [System.IO.StreamWriter]::new($LogPath, $false) }

function Write-Received {
    param([string]$Text)
    [Console]::Out.Write($Text)
    if ($script:log) { $script:log.Write($Text) }
}

# Reads one byte, or $null if nothing arrived before the read timeout.
function Read-Byte {
    try { return $stream.ReadByte() } catch [System.IO.IOException] { return $null }
}

try {
    # ----------------------------------------------------------------------
    # 1. Wait for three consecutive ETX
    # ----------------------------------------------------------------------
    Write-Host ''
    Write-Host 'Waiting for the loader. Power or reset the target now.' -ForegroundColor Yellow
    Write-Host ''

    $deadline = (Get-Date).AddSeconds($HandshakeTimeoutSec)
    $tokens = 0
    while ($tokens -lt 3) {
        if ((Get-Date) -gt $deadline) {
            throw "No request token after $HandshakeTimeoutSec s. Is the chain-loader running, and is this the PL011 (the FIRST -serial on raspi3b)?"
        }
        $b = Read-Byte
        if ($null -eq $b -or $b -lt 0) { continue }
        if ($b -eq 3) {
            $tokens++
        } else {
            # Any other byte resets the count, so ETX inside banner text cannot
            # be mistaken for the token.
            $tokens = 0
            Write-Received ([char]$b)
        }
    }

    # ----------------------------------------------------------------------
    # 2. Length, 3. "OK"
    # ----------------------------------------------------------------------
    Write-Host ''
    Write-Host ('  -> length       : {0}' -f $bytes.Length) -ForegroundColor Green
    $stream.Write([BitConverter]::GetBytes([uint32]$bytes.Length), 0, 4)
    $stream.Flush()

    $reply = ''
    $deadline = (Get-Date).AddSeconds(10)
    while ($reply.Length -lt 2) {
        if ((Get-Date) -gt $deadline) { throw "Loader never acknowledged the length (got '$reply')." }
        $b = Read-Byte
        if ($null -ne $b -and $b -ge 0) { $reply += [char]$b }
    }
    if ($reply -ne 'OK') { throw "Loader rejected the length: '$reply'" }
    Write-Host '  <- OK' -ForegroundColor Green

    # ----------------------------------------------------------------------
    # 4. Payload
    # ----------------------------------------------------------------------
    $chunk = 512
    $sent = 0
    $started = Get-Date
    while ($sent -lt $bytes.Length) {
        $take = [Math]::Min($chunk, $bytes.Length - $sent)
        $stream.Write($bytes, $sent, $take)
        $sent += $take
        if (($sent % 16384) -lt $chunk -or $sent -eq $bytes.Length) {
            $pct = [int](100 * $sent / $bytes.Length)
            Write-Progress -Activity 'Pushing kernel' -Status "$sent / $($bytes.Length) bytes" -PercentComplete $pct
        }
    }
    $stream.Flush()
    Write-Progress -Activity 'Pushing kernel' -Completed
    $elapsed = ((Get-Date) - $started).TotalSeconds
    Write-Host ('  -> sent         : {0} bytes in {1:N2} s ({2:N1} KiB/s)' -f `
            $bytes.Length, $elapsed, ($bytes.Length / 1KB / [Math]::Max($elapsed, 0.001))) -ForegroundColor Green
    Write-Host ''
    Write-Host '--- payload output ---------------------------------------------' -ForegroundColor DarkGray

    # ----------------------------------------------------------------------
    # 5. Terminal
    # ----------------------------------------------------------------------
    if ($NoTerminal) {
        $stop = (Get-Date).AddSeconds($RunSeconds)
        while ((Get-Date) -lt $stop) {
            $b = Read-Byte
            if ($null -ne $b -and $b -ge 0) { Write-Received ([char]$b) }
        }
    } else {
        Write-Host '(Ctrl+C to quit)' -ForegroundColor DarkGray
        while ($true) {
            $b = Read-Byte
            if ($null -ne $b -and $b -ge 0) { Write-Received ([char]$b) }
            if ([Console]::KeyAvailable) {
                $key = [Console]::ReadKey($true)
                $out = [byte][char]$key.KeyChar
                if ($key.Key -eq 'Enter') { $out = 13 }
                $stream.WriteByte($out)
                $stream.Flush()
            }
        }
    }
} finally {
    Write-Host ''
    if ($log) { $log.Flush(); $log.Close() }
    if ($port -and $port.IsOpen) { $port.Close() }
    if ($client) { $client.Close() }
    if ($LogPath) { Write-Host "transcript -> $LogPath" -ForegroundColor DarkGray }
}
