# Runs outside Flash so Windows can replace its executable.
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$exe = $env:FLASH_UPDATE_EXE
$package = $env:FLASH_UPDATE_PATH
$directory = [IO.Path]::GetDirectoryName($package)
$installDir = [IO.Path]::GetDirectoryName($exe)
$staged = $null
$payload = $null
$locked = $false
$mutex = $null
$icon = [Windows.Forms.MessageBoxIcon]::Information
try {
    $parent = Get-Process -Id ([int]$env:FLASH_UPDATE_PARENT) -ErrorAction SilentlyContinue
    if ($parent -and -not $parent.WaitForExit(30000)) {
        throw 'Flash is still closing. Open Flash and try again.'
    }

    $mutex = New-Object Threading.Mutex($false, 'Global\FlashApplicationUpdate')
    try {
        $locked = $mutex.WaitOne(0)
    } catch [Threading.AbandonedMutexException] {
        $locked = $true
    }
    if (-not $locked) { throw 'Another Flash update is in progress.' }
    if ([Diagnostics.FileVersionInfo]::GetVersionInfo($exe).ProductVersion -ne $env:FLASH_UPDATE_VERSION) {
        throw 'This copy of Flash changed while the update was being prepared.'
    }

    # Deny writes and deletion while Windows Installer reads the verified package.
    $payload = [IO.File]::Open($package, 'Open', 'Read', 'Read')
    if ((Get-FileHash -InputStream $payload -Algorithm SHA256).Hash -ne $env:FLASH_UPDATE_HASH) {
        throw 'The downloaded package changed after verification.'
    }
    $payload.Position = 0

    if ($env:FLASH_UPDATE_KIND -eq 'msi') {
        $msiexec = [IO.Path]::Combine($env:FLASH_UPDATE_SYSTEM, 'msiexec.exe')
        # Windows Installer expects a quoted path and a quoted directory property.
        $arguments = '/i "{0}" /passive /norestart INSTALLFOLDER="{1}"' -f $package, $installDir
        $result = Start-Process -FilePath $msiexec -ArgumentList $arguments -Wait -PassThru
        switch ($result.ExitCode) {
            0 { $message = 'Flash was updated. Open Flash to use the new version.' }
            3010 { $message = 'Flash was updated. Restart Windows before opening Flash again.' }
            default { throw "Windows Installer returned error $($result.ExitCode)." }
        }
    } else {
        # Copy to the same volume before replacing the original in one operation.
        $staged = [IO.Path]::Combine($installDir, "Flash-update-$([Guid]::NewGuid()).tmp")
        $file = [IO.File]::Open($staged, 'CreateNew', 'Write', 'None')
        try {
            $payload.CopyTo($file)
            $file.Flush($true)
        } finally {
            $file.Dispose()
        }
        [IO.File]::Replace($staged, $exe, [NullString]::Value)
        $message = 'Flash was updated. Open Flash to use the new version.'
    }
} catch {
    $message = "Could not apply the Flash update.`n`n$($_.Exception.Message)"
    $icon = [Windows.Forms.MessageBoxIcon]::Error
} finally {
    if ($payload) { $payload.Dispose() }
    if ($staged) { Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue }
    Remove-Item -LiteralPath $package -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $directory -ErrorAction SilentlyContinue
    if ($locked) { $mutex.ReleaseMutex() }
    if ($mutex) { $mutex.Dispose() }
}
[void][Windows.Forms.MessageBox]::Show($message, 'Flash update', 'OK', $icon)
