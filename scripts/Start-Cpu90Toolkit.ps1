[CmdletBinding()]
param([string]$ReleaseDirectory = (Join-Path $PSScriptRoot '..\..\release-cpu90-r2'))
$ErrorActionPreference = 'Stop'
$releasePath = [IO.Path]::GetFullPath($ReleaseDirectory)
$application = Join-Path $releasePath 'app\Lenovo Legion Toolkit.exe'
if (!(Test-Path -LiteralPath $application -PathType Leaf)) { throw "Application not found: $application" }
if (Get-Process -Name 'Lenovo Legion Toolkit' -ErrorAction SilentlyContinue) {
    throw 'Exit the existing Lenovo Legion Toolkit from its tray menu before starting this build.'
}
$settingsPath = Join-Path $env:LOCALAPPDATA 'LenovoLegionToolkit'
if (Test-Path -LiteralPath $settingsPath) {
    $backupPath = Join-Path $releasePath ('backups\settings-' + (Get-Date -Format 'yyyyMMdd-HHmmss-fff'))
    New-Item -ItemType Directory -Path $backupPath | Out-Null
    foreach ($settingsFile in Get-ChildItem -LiteralPath $settingsPath -File -Force) {
        $copyPath = Join-Path $backupPath $settingsFile.Name
        Copy-Item -LiteralPath $settingsFile.FullName -Destination $copyPath
        if ((Get-FileHash -LiteralPath $copyPath).Hash -ne (Get-FileHash -LiteralPath $settingsFile.FullName).Hash) {
            throw "Settings backup verification failed: $($settingsFile.Name)"
        }
    }
}
# This is the interactive application; it must show its window for the user to control.
Start-Process -FilePath $application -WorkingDirectory (Split-Path -Parent $application) -Verb RunAs -WindowStyle Normal -ArgumentList @('--disable-update-checker', '--trace')
