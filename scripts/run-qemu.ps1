<#
.SYNOPSIS
    Boots a cogware-bcm2837 example in QEMU.

.DESCRIPTION
    Wraps the QEMU invocation used throughout development. The kernel console
    (PL011 / UART0) is attached to this window's stdio, so boot output appears
    here and keystrokes go to the kernel shell.

    Ctrl-A then X quits QEMU.

.PARAMETER Example
    Which example kernel to boot. Default 'shell', the full reference kernel.
    The others are single-subsystem demos: hello-uart, multicore, async-tasks,
    heap, display, sdcard.

.PARAMETER Gdb
    Start the GDB stub on :1234 and halt the CPU before the first instruction.
    Attach with:
        aarch64-none-elf-gdb target\aarch64-unknown-none\release\examples\shell `
            -ex "target remote :1234"

.PARAMETER Trace
    Log taken exceptions and interrupts to int.log. Useful for diagnosing
    aborts during early boot.

.PARAMETER Profile
    Cargo profile to boot: release (default) or debug.

.PARAMETER NoBuild
    Boot whatever is already built instead of rebuilding first.

.PARAMETER Display
    Open a graphics window showing the VideoCore framebuffer. Without it the
    kernel still boots and 'f'/'g' still work; there is nothing to display.

.PARAMETER SdImage
    Raw disk image to attach as the SD card, created at -SdSizeMb if absent.
    Without it the EMMC controller reports no card ('i' shows this).

.PARAMETER SdSizeMb
    Size, in MiB, of an SD image this script has to create. Default 64.

.PARAMETER AutoDraw
    Send the -Startup commands once boot goes quiet, then hand the keyboard
    back to you. Implies -Display. Only meaningful for the 'shell' example.

.PARAMETER Startup
    Commands -AutoDraw sends. Default: 'f' (bring up the display) then 'g'
    (draw the test card).

.PARAMETER MonitorPort
    With -AutoDraw, also expose the QEMU monitor on this loopback port so the
    running window can be screenshotted:
        (New-Object Net.Sockets.TcpClient '127.0.0.1',55556)
    0 (the default) leaves the monitor off.

.EXAMPLE
    .\scripts\run-qemu.ps1

.EXAMPLE
    .\scripts\run-qemu.ps1 -Example hello-uart

.EXAMPLE
    .\scripts\run-qemu.ps1 -Example display -Display

.EXAMPLE
    .\scripts\run-qemu.ps1 -Gdb

.EXAMPLE
    .\scripts\run-qemu.ps1 -Display -SdImage .\sd.img

.EXAMPLE
    .\scripts\run-qemu.ps1 -AutoDraw -SdImage .\sd.img
#>
[CmdletBinding()]
param(
    [switch]$Gdb,
    [switch]$Trace,
    [ValidateSet('release', 'debug')]
    [string]$Profile = 'release',
    [switch]$NoBuild,
    [switch]$Display,
    [string]$SdImage,
    [int]$SdSizeMb = 64,
    [switch]$AutoDraw,
    [string[]]$Startup = @('f', 'g'),
    [int]$MonitorPort = 0,
    [ValidateSet('shell', 'hello-uart', 'multicore', 'async-tasks', 'heap', 'display', 'sdcard', 'gpio', 'bitbang', 'buses', 'async-bus')]
    [string]$Example = 'shell',
    [switch]$BusDevices
)

$ErrorActionPreference = 'Stop'

# Agent shells inherit a stale PATH; re-read the persisted user PATH so that
# scoop's qemu and the ARM toolchain are always found.
$env:PATH = [Environment]::GetEnvironmentVariable('Path', 'User') + ';' + $env:PATH

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $NoBuild) {
    Write-Host "Building example '$Example'..." -ForegroundColor Cyan
    if ($Profile -eq 'release') {
        cargo build --release --example $Example
    }
    else {
        cargo build --example $Example
    }
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
}

$kernel = Join-Path $root "target\aarch64-unknown-none\$Profile\examples\$Example"
if (-not (Test-Path $kernel)) {
    throw "example '$Example' not built at $kernel (drop -NoBuild, or check the name)"
}

# -M raspi3b        BCM2837, 1 GiB, 4x Cortex-A53. Never pass -smp: this
#                   machine hard-codes min == max == 4.
# -serial mon:stdio serial0 is the PL011, the kernel console. 'mon:' puts the
#                   QEMU monitor on the same channel, so Ctrl-A X quits.
# -serial null      serial1 is the AUX mini UART; discarded.
# -display none     no graphics window unless -Display was asked for.
if ($AutoDraw) { $Display = $true }

# QEMU's Windows stdio backend cannot be driven programmatically, so -AutoDraw
# swaps the console onto a named pipe and this script relays it by hand.
$pipeName = "cogware-bcm2837-run-$PID"

$qemuArgs = @(
    '-M', 'raspi3b'
    '-serial', $(if ($AutoDraw) { "pipe:$pipeName" } else { 'mon:stdio' })
    '-serial', 'null'
    '-kernel', $kernel
)

# QEMU 9.0+ models SPI0 and the BSC controllers for real (not stubs), so
# virtual peripherals can attach. Needed by `buses`; harmless otherwise.
if ($Example -in @('buses', 'async-bus') -or $BusDevices) {
    $qemuArgs += @(
        '-device', 'ds1338,address=0x68,bus=i2c-bus.1'
        '-device', 'tmp105,address=0x49,bus=i2c-bus.1'
        '-device', 'sst25vf016b,bus=spi'
    )
}

if ($Display) {
    # Window stays blank until the kernel allocates a framebuffer ('f').
    # show-cursor has been a display sub-option since QEMU 8. The top-level
    # `-show-cursor` flag was removed in QEMU 11 and is a hard parse error.
    $qemuArgs += @('-display', 'default,show-cursor=on')
}
else {
    $qemuArgs += @('-display', 'none')
}

if ($SdImage) {
    if (-not [System.IO.Path]::IsPathRooted($SdImage)) {
        $SdImage = Join-Path $root $SdImage
    }
    if (-not (Test-Path $SdImage)) {
        Write-Host "Creating ${SdSizeMb}MiB SD image at $SdImage" -ForegroundColor Cyan
        & qemu-img create -f raw $SdImage "${SdSizeMb}M" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'qemu-img failed' }
    }
    # raspi3b wires if=sd to the EMMC controller; the image must be raw, not
    # qcow2.
    $qemuArgs += @('-drive', "file=$SdImage,if=sd,format=raw")
}

if ($AutoDraw -and $MonitorPort -gt 0) {
    # QEMU 11+ requires `-object monitor-hmp`; `-monitor`/`-mon` produce a
    # monitor that never answers. A socket also avoids clashing with the
    # serial named pipe: QEMU on Windows allows only one `pipe:` chardev.
    $qemuArgs += @(
        '-chardev', "socket,id=mon0,host=127.0.0.1,port=$MonitorPort,server=on,wait=off"
        '-object', 'monitor-hmp,id=hmp0,chardev=mon0'
    )
}

if ($Trace) { $qemuArgs += @('-d', 'int,guest_errors', '-D', (Join-Path $root 'int.log')) }
if ($Gdb) { $qemuArgs += @('-s', '-S') }

Write-Host ''
Write-Host "  kernel : $kernel" -ForegroundColor DarkGray
Write-Host "  qemu   : $((Get-Command qemu-system-aarch64).Source)" -ForegroundColor DarkGray
if ($Display) {
    Write-Host '  display: window open.' -ForegroundColor Yellow
}
if ($AutoDraw) {
    Write-Host "  startup: will send '$($Startup -join "', '")' once boot settles." -ForegroundColor Yellow
}
elseif ($Display) {
    Write-Host '  hint   : press f then g in this console to draw the test card.' -ForegroundColor Yellow
}
if ($SdImage) {
    Write-Host "  sd     : $SdImage" -ForegroundColor DarkGray
}
if ($Gdb) {
    Write-Host '  gdb    : stub on :1234, CPU halted. Attach, then `continue`.' -ForegroundColor Yellow
}
if ($AutoDraw) {
    Write-Host '  quit   : Ctrl-C, or close the QEMU window.' -ForegroundColor DarkGray
}
else {
    Write-Host '  quit   : Ctrl-A then X' -ForegroundColor DarkGray
}
Write-Host ''
Write-Host ('-' * 72) -ForegroundColor DarkGray

if (-not $AutoDraw) {
    & qemu-system-aarch64 @qemuArgs

    Write-Host ('-' * 72) -ForegroundColor DarkGray
    Write-Host "QEMU exited with $LASTEXITCODE." -ForegroundColor DarkGray
    return
}

# --- -AutoDraw: relay the kernel console by hand -----------------------------
#
# Connects to \\.\pipe\<name> as a client for a duplex channel to the PL011:
# reads are kernel output, writes are keystrokes.

$qemu = Start-Process -PassThru -NoNewWindow -FilePath 'qemu-system-aarch64' -ArgumentList $qemuArgs
$client = $null
$writer = $null

try {
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
    $queue = [System.Collections.Generic.Queue[string]]::new()
    $Startup | ForEach-Object { $queue.Enqueue($_) }
    $lastOutput = Get-Date

    # Keep a timed-out read rather than reissuing it: an abandoned ReadAsync
    # still completes into $buffer, and that data would be lost.
    $pending = $null

    while (-not $qemu.HasExited) {
        if ($null -eq $pending) {
            $pending = $client.ReadAsync($buffer, 0, $buffer.Length)
        }

        if ($pending.Wait(50)) {
            $count = $pending.Result
            $pending = $null
            if ($count -le 0) { break }
            Write-Host -NoNewline ([System.Text.Encoding]::ASCII.GetString($buffer, 0, $count))
            $lastOutput = Get-Date
            continue
        }

        if ($queue.Count -gt 0 -and ((Get-Date) - $lastOutput).TotalMilliseconds -ge 1200) {
            $command = $queue.Dequeue()
            Write-Host "`n<<< $command >>>" -ForegroundColor Yellow
            $writer.Write($command)
            $lastOutput = Get-Date
            continue
        }

        while ([Console]::KeyAvailable) {
            $key = [Console]::ReadKey($true)
            $writer.Write($key.KeyChar)
            $lastOutput = Get-Date
        }
    }
}
finally {
    if ($writer) { $writer.Dispose() }
    if ($client) { $client.Dispose() }
    if ($qemu -and -not $qemu.HasExited) { Stop-Process -Id $qemu.Id -Force }
    Write-Host ''
    Write-Host ('-' * 72) -ForegroundColor DarkGray
    Write-Host 'QEMU exited.' -ForegroundColor DarkGray
}
