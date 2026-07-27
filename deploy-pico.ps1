<#
.SYNOPSIS
    Copy the firmware in firmware/ onto a Pico running MicroPython.

.DESCRIPTION
    Talks to MicroPython's raw REPL over the USB serial port, so it needs
    nothing installed - no Python, no mpremote, no Thonny. Files are sent
    base64-encoded, which keeps binary content and newlines intact.

    The port is only used if it belongs to a Raspberry Pi USB device
    (VID 2E8A), so a wrong -Port cannot spray data at some other device.

.PARAMETER Port
    Serial port of the Pico. Autodetected when omitted.

.PARAMETER Source
    Directory whose contents are copied to the device root. Defaults to the
    firmware/ directory next to this script - everything in there goes to the
    device verbatim, which is why this script lives outside it.

.PARAMETER List
    List what is currently on the device and exit without writing anything.

.PARAMETER NoReset
    Leave the Pico at the REPL instead of soft-resetting into the new main.py.
    Note that a Pico left this way is not running anything - use -Reset to
    start it again.

.PARAMETER Reset
    Only soft-reset the device, so main.py starts again. Useful after a
    -NoReset deploy, or after anything else that dropped it to the REPL.

.EXAMPLE
    .\deploy-pico.ps1
    .\deploy-pico.ps1 -List
    .\deploy-pico.ps1 -Reset
    .\deploy-pico.ps1 -Port COM7
#>
[CmdletBinding()]
param(
    [string]$Port,
    [string]$Source = (Join-Path $PSScriptRoot 'firmware'),
    [switch]$List,
    [switch]$NoReset,
    [switch]$Reset
)

$ErrorActionPreference = 'Stop'

# Raspberry Pi Trading / Raspberry Pi Ltd - RP2040 and RP2350 both use this.
$PICO_VID = 'VID_2E8A'

# Control characters of the raw REPL protocol.
$CTRL_A = [char]1   # enter raw REPL
$CTRL_B = [char]2   # leave raw REPL
$CTRL_C = [char]3   # interrupt whatever is running
$CTRL_D = [char]4   # execute / soft reset

# ---------------------------------------------------------------- discovery

function Find-PicoPorts {
    Get-CimInstance Win32_PnPEntity -ErrorAction SilentlyContinue |
        Where-Object { $_.PNPDeviceID -like "USB\$PICO_VID*" -and $_.Name -match '\((COM\d+)\)' } |
        ForEach-Object {
            if ($_.Name -match '\((COM\d+)\)') {
                [PSCustomObject]@{ Port = $Matches[1]; Device = $_.PNPDeviceID }
            }
        }
}

function Resolve-PicoPort([string]$requested) {
    $picos = @(Find-PicoPorts)
    if ($requested) {
        $match = $picos | Where-Object { $_.Port -ieq $requested }
        if ($match) { return $match[0] }
        $present = @(Find-PicoPorts | ForEach-Object { $_.Port })
        $hint = if ($present) { "Raspberry Pi ports present: $($present -join ', ')" }
                else { 'no Raspberry Pi serial ports are present' }
        throw "$requested is not a Raspberry Pi USB device ($hint). Refusing to write to it."
    }
    if (-not $picos) {
        throw 'No Pico found. Check it is plugged in and running MicroPython.'
    }
    return $picos[0]
}

function Assert-PortFree([string]$port) {
    $holder = Get-Process hwinfo-pico-bridge-tray, hwinfo-pico-bridge -ErrorAction SilentlyContinue
    if ($holder) {
        throw ("The bridge is running and holds $port. Pause it first " +
               '(right-click the tray icon -> Pause), then run this again. ' +
               'Its "Update Pico" item does all of this for you.')
    }
}

# ------------------------------------------------------------- raw REPL

function Read-Until([System.IO.Ports.SerialPort]$sp, [string]$marker, [int]$timeoutMs = 10000) {
    $buffer = New-Object System.Text.StringBuilder
    $deadline = [DateTime]::UtcNow.AddMilliseconds($timeoutMs)
    while ([DateTime]::UtcNow -lt $deadline) {
        $chunk = $sp.ReadExisting()
        if ($chunk) {
            [void]$buffer.Append($chunk)
            if ($buffer.ToString().Contains($marker)) { return $buffer.ToString() }
        }
        else {
            Start-Sleep -Milliseconds 5
        }
    }
    throw "Timed out waiting for the Pico. Received so far: '$($buffer.ToString())'"
}

function Enter-RawRepl([System.IO.Ports.SerialPort]$sp) {
    # Retried, because a Pico still coming back from a soft reset discards
    # whatever is in its CDC buffer - the Ctrl-A simply vanishes, and waiting
    # longer never produces a banner. One that is merely running main.py
    # answers first time.
    for ($try = 1; $try -le 4; $try++) {
        try {
            # Two interrupts: the first breaks out of any sleep, the second
            # out of a loop that swallowed the first.
            $sp.Write($CTRL_C); Start-Sleep -Milliseconds 60
            $sp.Write($CTRL_C); Start-Sleep -Milliseconds 60
            $sp.DiscardInBuffer()
            $sp.Write($CTRL_A)
            [void](Read-Until $sp 'raw REPL; CTRL-B to exit' 2000)
            return
        }
        catch {
            if ($try -eq 4) {
                throw ("The device did not answer as MicroPython. Is it flashed, " +
                       'and is anything else holding the port?')
            }
            Start-Sleep -Milliseconds 400
        }
    }
}

function Exit-RawRepl([System.IO.Ports.SerialPort]$sp, [bool]$reset) {
    $sp.Write($CTRL_B)
    Start-Sleep -Milliseconds 60
    if ($reset) {
        $sp.DiscardInBuffer()
        $sp.Write($CTRL_D)   # soft reset, so main.py starts again
    }
}

# Runs a snippet on the device and returns its stdout. Globals persist between
# calls, so imports and open file handles carry over.
function Invoke-Pico([System.IO.Ports.SerialPort]$sp, [string]$code, [int]$timeoutMs = 15000) {
    $sp.Write($code)
    $sp.Write($CTRL_D)

    # The device answers: "OK" <stdout> \x04 <stderr> \x04 ">"
    $raw = Read-Until $sp "$CTRL_D>" $timeoutMs
    $body = $raw
    $ok = $body.IndexOf('OK')
    if ($ok -ge 0) { $body = $body.Substring($ok + 2) }

    $parts = $body.Split($CTRL_D)
    $stdout = if ($parts.Count -gt 0) { $parts[0] } else { '' }
    $stderr = if ($parts.Count -gt 1) { $parts[1] } else { '' }
    if ($stderr.Trim()) {
        throw "Pico reported an error:`n$($stderr.Trim())"
    }
    return $stdout
}

# ------------------------------------------------------------- transfer

$PRELUDE = @'
import gc
import os
try:
    import ubinascii as _b64
except ImportError:
    import binascii as _b64
# Whatever was running got interrupted mid-flight and its heap is still live;
# a transfer needs a few KB per chunk.
gc.collect()
def _mkdir(p):
    try:
        os.mkdir(p)
    except OSError:
        pass
def _walk(d):
    for name in os.listdir(d):
        p = d + '/' + name if d != '/' else '/' + name
        st = os.stat(p)
        if st[0] & 0x4000:
            print('DIR  ' + p)
            _walk(p)
        else:
            print('%8d %s' % (st[6], p))
def _forget():
    try:
        os.remove('.pico-deploy')
    except OSError:
        pass
'@

function Send-PicoFile {
    param(
        [System.IO.Ports.SerialPort]$sp,
        [string]$LocalPath,
        [string]$RemotePath,
        [int]$ChunkSize = 512
    )
    $bytes = [System.IO.File]::ReadAllBytes($LocalPath)
    Invoke-Pico $sp "_f=open('$RemotePath','wb')" | Out-Null
    try {
        for ($offset = 0; $offset -lt $bytes.Length; $offset += $ChunkSize) {
            $len = [Math]::Min($ChunkSize, $bytes.Length - $offset)
            $slice = New-Object byte[] $len
            [Array]::Copy($bytes, $offset, $slice, 0, $len)
            $encoded = [Convert]::ToBase64String($slice)
            Invoke-Pico $sp "_f.write(_b64.a2b_base64('$encoded'))" | Out-Null
        }
    }
    finally {
        Invoke-Pico $sp '_f.close()' | Out-Null
    }
    return $bytes.Length
}

# ------------------------------------------------------------------ main

$Source = (Resolve-Path $Source).Path
if (-not (Test-Path $Source)) { throw "Source directory not found: $Source" }

$target = Resolve-PicoPort $Port
Assert-PortFree $target.Port
Write-Host "Pico on $($target.Port)  [$($target.Device)]" -ForegroundColor Cyan

$sp = New-Object System.IO.Ports.SerialPort($target.Port, 115200, 'None', 8, 'One')
$sp.ReadTimeout = 2000
$sp.WriteTimeout = 5000
$sp.DtrEnable = $true
$sp.RtsEnable = $true
$sp.Open()

try {
    # Entering the raw REPL first puts the device in a known state whatever it
    # was doing; the finally block below is what actually resets it.
    Enter-RawRepl $sp

    if ($Reset) {
        Write-Host 'Soft-resetting; the display should restart.' -ForegroundColor Green
        return
    }

    Invoke-Pico $sp $PRELUDE | Out-Null

    if ($List) {
        Write-Host "`nOn the device:" -ForegroundColor Cyan
        (Invoke-Pico $sp "_walk('/')").Trim() -split "`r?`n" |
            Where-Object { $_ } | ForEach-Object { "  $_" }
        return
    }

    $files = Get-ChildItem -Recurse -File $Source
    if (-not $files) { throw "Nothing to deploy in $Source" }

    # The bridge's updater skips files matching the record it keeps on the
    # device. This script copies everything and does not maintain that record,
    # so drop it up front: left behind it would describe files this script has
    # since replaced, and a deploy that dies half way invalidates it too.
    Invoke-Pico $sp '_forget()' | Out-Null

    # Create directories parent-first so nested ones always have a parent.
    $dirs = Get-ChildItem -Recurse -Directory $Source |
        ForEach-Object { $_.FullName.Substring($Source.Length).TrimStart('\').Replace('\', '/') } |
        Sort-Object { ($_ -split '/').Count }
    foreach ($dir in $dirs) { Invoke-Pico $sp "_mkdir('$dir')" | Out-Null }

    Write-Host "Copying $($files.Count) files..." -ForegroundColor Cyan
    $total = 0
    $index = 0
    foreach ($file in $files) {
        $index++
        $remote = $file.FullName.Substring($Source.Length).TrimStart('\').Replace('\', '/')
        Write-Progress -Activity 'Deploying to Pico' -Status $remote `
            -PercentComplete (100 * $index / $files.Count)
        $written = Send-PicoFile -sp $sp -LocalPath $file.FullName -RemotePath $remote
        $total += $written
        "  {0,-46} {1,7:N0} B" -f $remote, $written
    }
    Write-Progress -Activity 'Deploying to Pico' -Completed

    Write-Host ("`nDone - {0:N0} bytes." -f $total) -ForegroundColor Green
    if (-not $NoReset) { Write-Host 'Soft-resetting; the display should restart.' }
}
finally {
    # Always hand the device back, even if a transfer failed part way.
    try { Exit-RawRepl $sp (-not $NoReset) } catch { }
    $sp.Close()
    $sp.Dispose()
}
