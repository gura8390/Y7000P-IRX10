[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$OutputFile,
    [Parameter(Mandatory)][ValidateSet('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')][string]$Target
)
$ErrorActionPreference = 'Stop'
$sourceRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$metadataText = & (Join-Path $PSScriptRoot 'cargo.ps1') metadata --locked --format-version 1 --filter-platform $Target
if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve the locked dependency graph.' }
$metadata = ($metadataText -join [Environment]::NewLine) | ConvertFrom-Json
$lines = [Collections.Generic.List[string]]::new()
$lines.Add('Jiks locked Cargo dependency licenses')
$lines.Add('Cargo.lock SHA-256: ' + (Get-FileHash -LiteralPath (Join-Path $sourceRoot 'Cargo.lock') -Algorithm SHA256).Hash.ToLowerInvariant())
$lines.Add('Target: ' + $Target)
$lines.Add('Generated from the exact downloaded crate sources; local cache paths are omitted.')
$count = 0
foreach ($package in ($metadata.packages | Where-Object { $_.source } | Sort-Object name, version)) {
    $directory = [IO.Path]::GetDirectoryName($package.manifest_path)
    $licenseFiles = @(Get-ChildItem -LiteralPath $directory -File | Where-Object { $_.Name -match '^(LICENSE|LICENCE|COPYING|NOTICE|COPYRIGHT)([._-].*)?$' })
    if (!$licenseFiles.Count -and $package.license_file) {
        $licensePath = [IO.Path]::GetFullPath((Join-Path $directory $package.license_file))
        if (!$licensePath.StartsWith($directory + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) { throw "License path escapes crate: $($package.name)" }
        $licenseFiles = @(Get-Item -LiteralPath $licensePath)
    }
    if (!$licenseFiles.Count) { throw "License text is absent; review before packaging: $($package.name) $($package.version)" }
    $lines.Add('')
    $lines.Add(('=' * 72))
    $lines.Add("$($package.name) $($package.version) | $($package.license)")
    if ($package.repository) { $lines.Add([string]$package.repository) }
    foreach ($file in $licenseFiles) {
        if ($file.Attributes -band [IO.FileAttributes]::ReparsePoint -or $file.Length -gt 1048576) { throw "Unsafe/oversized dependency license: $($package.name)" }
        $lines.Add("--- $($file.Name) ---")
        $lines.Add([IO.File]::ReadAllText($file.FullName))
    }
    $count++
}
$outputPath = [IO.Path]::GetFullPath($OutputFile)
[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($outputPath)) | Out-Null
[IO.File]::WriteAllText($outputPath, ($lines -join [Environment]::NewLine), [Text.UTF8Encoding]::new($false))
Write-Output "Collected license texts for $count locked Cargo packages."
