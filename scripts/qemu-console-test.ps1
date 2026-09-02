<#
.SYNOPSIS
    Boots the kernel in QEMU and drives its console programmatically.

.DESCRIPTION
    QEMU's Windows stdio backend rejects a redirected stdin, so this script
    uses `-serial pipe:<name>` instead and connects to the resulting
    \\.\pipe\<name> as a client for a duplex channel to the kernel's PL011.

    Everything the kernel prints is echoed to this window and captured to
    -LogPath.

.PARAMETER Commands
    Console keystrokes to send, in order, once boot output goes quiet.

.PARAMETER SettleMs
    How long the console must be silent before the next command is sent.

.PARAMETER TimeoutSec
    Hard limit on the whole run.

.PARAMETER LogPath
    Where to write the transcript.

.PARAMETER SdImage
    Raw disk image to attach as the SD card, created at -SdSizeMb if absent.

.PARAMETER SdSizeMb
    Size, in MiB, of an SD image this script has to create. Default 64.

.PARAMETER Screenshot
    Path to write a PPM screendump once all commands are sent, via the QEMU
    monitor over a loopback socket.

.PARAMETER MonitorPort
    Loopback port for the monitor socket used by -Screenshot.

.EXAMPLE
    .\scripts\qemu-console-test.ps1 -Commands 'h','m','s','t','c','e'

.EXAMPLE
    .\scripts\qemu-console-test.ps1 -Commands 'd' -LogPath abort.log

.EXAMPLE
    .\scripts\qemu-console-test.ps1 -Commands 'f','g' -Screenshot card.ppm
#>
[CmdletBinding()]
param(
    [string[]]$Commands = @(),
    [int]$SettleMs = 1200,
    [int]$TimeoutSec = 60,
    [string]$LogPath = 'console-test.log',
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release',
    [string]$SdImage,
    [int]$SdSizeMb = 64,
    [string]$Screenshot,
    [int]$MonitorPort = 55555,
    [ValidateSet('shell', 'hello-uart', 'multicore', 'async-tasks', 'heap', 'display', 'sdcard', 'gpio', 'bitbang', 'buses', 'async-bus')]
    [string]$Example = 'shell',
    [switch]$BusDevices
)

$ErrorActionPreference = 'Stop'
$env:PATH = [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $env:PATH

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$kernel = Join-Path $root "target\aarch64-unknown-none\$Profile\examples\$Example"
if (-not (Test-Path $kernel)) {
    throw "example '$Example' not found at $kernel; run 'cargo build --$Profile --example $Example' first"
}

$pipeName = "cogware-bcm2837-$PID"

$qemuArgs = @(
    '-M', 'raspi3b'
    '-serial', "pipe:$pipeName"
    '-serial', 'null'
    '-display', 'none'
    '-kernel', $kernel
)

# QEMU 9.0+ models SPI0 and the BSC controllers for real, so virtual
# peripherals can attach. Needed by the `buses` example.
if ($Example -in @('buses', 'async-bus') -or $BusDevices) {
    $qemuArgs += @(
        '-device', 'ds1338,address=0x68,bus=i2c-bus.1'
        '-device', 'tmp105,address=0x49,bus=i2c-bus.1'
        '-device', 'sst25vf016b,bus=spi'
    )
}

if ($Screenshot) {
    if (-not [System.IO.Path]::IsPathRooted($Screenshot)) {
        $Screenshot = Join-Path $root $Screenshot
    }
    # A loopback socket, not a named pipe: QEMU on Windows allows only one
    # `pipe:` chardev, already used by serial. QEMU 11+ also requires
    # `-object monitor-hmp`; `-monitor`/`-mon` produce a monitor that never
    # answers.
    $qemuArgs += @(
        '-chardev', "socket,id=mon0,host=127.0.0.1,port=$MonitorPort,server=on,wait=off"
        '-object', 'monitor-hmp,id=hmp0,chardev=mon0'
    )
}

if ($SdImage) {
    if (-not [System.IO.Path]::IsPathRooted($SdImage)) {
        $SdImage = Join-Path $root $SdImage
    }
    if (-not (Test-Path $SdImage)) {
        & qemu-img create -f raw $SdImage "${SdSizeMb}M" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'qemu-img failed' }
    }
    $qemuArgs += @('-drive', "file=$SdImage,if=sd,format=raw")
}

$qemu = Start-Process -PassThru -NoNewWindow -FilePath 'qemu-system-aarch64' -ArgumentList $qemuArgs

$client = $null
$writer = $null
$transcript = [System.Text.StringBuilder]::new()

try {
    # QEMU creates the pipe server as it initialises the chardev; retry until
    # it exists.
    $client = [System.IO.Pipes.NamedPipeClientStream]::new(
        '.', $pipeName,
        [System.IO.Pipes.PipeDirection]::InOut,
        [System.IO.Pipes.PipeOptions]::Asynchronous)

    $connected = $false
    foreach ($attempt in 1..50) {
        try { $client.Connect(200); $connected = $true; break } catch { Start-Sleep -Milliseconds 100 }
    }
    if (-not $connected) { throw "could not connect to \\.\pipe\$pipeName" }

    $writer = [System.IO.StreamWriter]::new($client)
    $writer.AutoFlush = $true

    $buffer = [byte[]]::new(4096)
    $deadline = (Get-Date).AddSeconds($TimeoutSec)
    $queue = [System.Collections.Generic.Queue[string]]::new()
    $Commands | ForEach-Object { $queue.Enqueue($_) }
    $lastOutput = Get-Date

    # A pending read must survive a Wait() timeout: reissuing ReadAsync
    # abandons it, and the abandoned task can still complete into $buffer,
    # silently eating the first chunk after every idle period.
    $pending = $null

    while ((Get-Date) -lt $deadline) {
        if ($null -eq $pending) {
            $pending = $client.ReadAsync($buffer, 0, $buffer.Length)
        }

        if ($pending.Wait(200)) {
            $count = $pending.Result
            $pending = $null
            if ($count -le 0) { break }
            $text = [System.Text.Encoding]::ASCII.GetString($buffer, 0, $count)
            [void]$transcript.Append($text)
            Write-Host -NoNewline $text
            $lastOutput = Get-Date
            continue
        }

        # Console has gone quiet; send the next command if there is one.
        if ($queue.Count -gt 0 -and ((Get-Date) - $lastOutput).TotalMilliseconds -ge $SettleMs) {
            $command = $queue.Dequeue()
            Write-Host "`n<<< sending '$command' >>>" -ForegroundColor Yellow
            [void]$transcript.AppendLine("`n<<< sending '$command' >>>")
            $writer.Write($command)
            $lastOutput = Get-Date
        } elseif ($queue.Count -eq 0 -and ((Get-Date) - $lastOutput).TotalSeconds -ge 3) {
            break
        }
    }

    if ($Screenshot) {
        # HMP is a plain text protocol: connect, wait for the banner, write the
        # command, then give QEMU a moment to walk the framebuffer.
        $monitor = [System.Net.Sockets.TcpClient]::new('127.0.0.1', $MonitorPort)
        try {
            $stream = $monitor.GetStream()
            $monitorWriter = [System.IO.StreamWriter]::new($stream)
            $monitorWriter.AutoFlush = $true
            Start-Sleep -Milliseconds 500
            # Forward slashes: the monitor treats a backslash as an escape.
            $monitorWriter.Write("screendump $($Screenshot -replace '\\', '/')`n")
            Start-Sleep -Milliseconds 2000
            $monitorWriter.Dispose()
        } finally {
            $monitor.Close()
        }
    }
} finally {
    if ($writer) { $writer.Dispose() }
    if ($client) { $client.Dispose() }
    if ($qemu -and -not $qemu.HasExited) { Stop-Process -Id $qemu.Id -Force }
    $transcript.ToString() | Set-Content -Path $LogPath -Encoding ascii
    Write-Host ''
    Write-Host "transcript -> $LogPath" -ForegroundColor DarkGray
    if ($Screenshot -and (Test-Path $Screenshot)) {
        $size = (Get-Item $Screenshot).Length
        Write-Host "screendump -> $Screenshot ($size bytes)" -ForegroundColor DarkGray
    } elseif ($Screenshot) {
        Write-Host "screendump FAILED: no file at $Screenshot" -ForegroundColor Red
    }
}
