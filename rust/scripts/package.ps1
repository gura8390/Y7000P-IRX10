[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Binary,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [Parameter(Mandatory)][string]$DependencyLicenses,
    [ValidatePattern('^[0-9A-Za-z][0-9A-Za-z._-]{0,63}$')][string]$Label = '0.1.0',
    [ValidateSet('debug', 'release')][string]$Configuration = 'release',
    [Parameter(Mandatory)][ValidatePattern('^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$')][string]$SourceRevision
)
$ErrorActionPreference = 'Stop'
$sourceRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$binaryPath = (Resolve-Path -LiteralPath $Binary).Path
$outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
if ((Get-Item -LiteralPath $binaryPath).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'The binary cannot be a reparse point.' }
if ([IO.Path]::GetFileName($binaryPath) -cne 'Jiks.exe') { throw 'Select the explicit Jiks.exe build artifact.' }
if ([Diagnostics.FileVersionInfo]::GetVersionInfo($binaryPath).FileDescription -like '*isolated benchmark fixture*') { throw 'An isolated benchmark fixture cannot be packaged as the application.' }
$file = [IO.File]::OpenRead($binaryPath)
try {
    $reader = [IO.BinaryReader]::new($file)
    if ($reader.ReadUInt16() -ne 0x5A4D) { throw 'The artifact is not a Windows PE executable.' }
    $file.Position = 0x3c
    $peOffset = $reader.ReadUInt32()
    if ($peOffset -gt $file.Length - 24) { throw 'Invalid PE header offset.' }
    $file.Position = $peOffset
    if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) { throw 'The artifact must be an x64 Windows PE executable.' }
} finally { $file.Dispose() }
if ($Configuration -eq 'debug' -and $Label -notmatch 'debug|fixture|preview|test') { throw 'Debug packages must be clearly labeled as a test/preview.' }
$expectedLevel = if ($Configuration -eq 'release') { 'requireAdministrator' } else { 'asInvoker' }
# Ignore the developer PATH: every static dependency must be Windows-owned or in the package directory.
$peAudit = & (Join-Path $PSScriptRoot 'verify-pe.ps1') -Binary $binaryPath -ExpectedExecutionLevel $expectedLevel
if (!(($peAudit | ConvertFrom-Json).verified)) { throw 'Static PE dependency audit did not pass.' }
[IO.Directory]::CreateDirectory($outputRoot) | Out-Null
if ((Get-Item -LiteralPath $outputRoot).Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'The output directory cannot be a reparse point.' }
$packageName = "拯救者工具箱-$Label-windows-x64"
$finalDirectory = Join-Path $outputRoot $packageName
$finalZip = Join-Path $outputRoot "$packageName.zip"
if ((Test-Path -LiteralPath $finalDirectory) -or (Test-Path -LiteralPath $finalZip)) { throw 'Package output already exists; select a new directory or label.' }
# This is an explicit allowlist. No recursive copy of a target/user directory.
$entries = @(
    @{ Source = $binaryPath; Name = '拯救者工具箱.exe' },
    @{ Source = (Join-Path $sourceRoot 'LICENSE'); Name = 'LICENSE' },
    @{ Source = (Join-Path $PSScriptRoot 'package-readme.txt'); Name = 'README.txt' },
    @{ Source = (Join-Path $sourceRoot 'THIRD_PARTY_NOTICES.md'); Name = 'THIRD_PARTY_NOTICES.md' },
    @{ Source = (Resolve-Path -LiteralPath $DependencyLicenses).Path; Name = 'DEPENDENCY_LICENSES.txt' },
    @{ Source = (Join-Path $sourceRoot 'crates/jiks-platform/resources/pawnio/COPYING'); Name = 'licenses/PawnIO-COPYING.txt' },
    @{ Source = (Join-Path $sourceRoot 'crates/jiks-platform/resources/pawnio/NOTICE.txt'); Name = 'licenses/PawnIO-NOTICE.txt' },
    @{ Source = (Join-Path $sourceRoot 'crates/jiks-platform/resources/presentmon-fps/LICENSE'); Name = 'licenses/PresentMonFps-LICENSE.txt' },
    @{ Source = (Join-Path $sourceRoot 'crates/jiks-platform/resources/presentmon-fps/NOTICE.txt'); Name = 'licenses/PresentMonFps-NOTICE.txt' }
)
$pawnRoot = Join-Path $sourceRoot 'crates/jiks-platform/resources/pawnio'
$pawnManifestPath = Join-Path $pawnRoot 'source/source-manifest.json'
$pawnManifest = Get-Content -LiteralPath $pawnManifestPath -Raw | ConvertFrom-Json
if ($pawnManifest.schema -ne 1 -or $pawnManifest.commit -cne '52a7e536dff3e53c96917a28caac5e0fa6510696' -or @($pawnManifest.files).Count -ne 14) { throw 'Unexpected PawnIO source manifest.' }
$sourceNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($source in $pawnManifest.files) {
    $relative = [string]$source.path
    if ($relative -notmatch '^[A-Za-z0-9_./-]+$' -or $relative -match '(^|/)\.\.?(/|$)' -or $relative.StartsWith('/') -or !$sourceNames.Add($relative)) { throw 'Unsafe or duplicate PawnIO source path.' }
    $path = Join-Path (Join-Path $pawnRoot 'source') $relative
    if ((Get-Item -LiteralPath $path).Length -ne $source.bytes -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $source.sha256) { throw "PawnIO source hash mismatch: $relative" }
    $entries += @{ Source = $path; Name = "licenses/PawnIO-source/$relative" }
}
$entries += @{ Source = $pawnManifestPath; Name = 'licenses/PawnIO-source/source-manifest.json' }
$entries += @{ Source = (Join-Path $pawnRoot 'SOURCE-README.txt'); Name = 'licenses/PawnIO-source/SOURCE-README.txt' }
$licenseRoot = Join-Path $sourceRoot 'licenses'
$licenseManifestPath = Join-Path $licenseRoot 'source-manifest.json'
$licenseManifest = Get-Content -LiteralPath $licenseManifestPath -Raw | ConvertFrom-Json
$adapterLicenses = @('LibreHardwareMonitor-MPL-2.0.txt', 'RAMSPDToolkit-MPL-2.0.txt', 'NvAPIWrapper-LGPL-3.0.txt')
if ($licenseManifest.schema -ne 1 -or @($licenseManifest.files).Count -ne $adapterLicenses.Count) { throw 'Unexpected native-adapter license manifest.' }
$licenseNames = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
foreach ($license in $licenseManifest.files) {
    if ($adapterLicenses -cnotcontains [string]$license.path -or !$licenseNames.Add([string]$license.path)) { throw 'Unexpected or duplicate native-adapter license name.' }
    $path = Join-Path $licenseRoot $license.path
    if ((Get-Item -LiteralPath $path).Length -ne $license.bytes -or (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() -cne $license.sha256) { throw 'Native-adapter license text hash mismatch.' }
    $entries += @{ Source = $path; Name = "licenses/$($license.path)" }
}
$entries += @{ Source = $licenseManifestPath; Name = 'licenses/Adapter-license-source-manifest.json' }
$entries += @{ Source = (Join-Path $licenseRoot 'NativeAdapters-NOTICE.txt'); Name = 'licenses/NativeAdapters-NOTICE.txt' }
# Corresponding source is copied only from the final frozen source tree, at packaging time.
foreach ($name in @('intel_sensors.rs', 'gpu_sensors.rs', 'intel_gpu.rs', 'memory_sensors.rs')) {
    $entries += @{ Source = (Join-Path $sourceRoot "crates/jiks-platform/src/$name"); Name = "licenses/NativeSensors-source/$name" }
}
foreach ($name in @('gpu.rs', 'gpu_status.rs', 'gpu_pstate.rs', 'nvapi_native.rs')) {
    $entries += @{ Source = (Join-Path $sourceRoot "crates/jiks-platform/src/$name"); Name = "licenses/NvAPI-protocol-source/$name" }
}
foreach ($entry in $entries) { if (!(Test-Path -LiteralPath $entry.Source -PathType Leaf)) { throw "Required package file is absent: $($entry.Name)" } }
$nonce = [Guid]::NewGuid().ToString('N')
$destination = Join-Path $outputRoot ".jiks-package-$nonce.pending"
$zipPath = Join-Path $outputRoot ".jiks-package-$nonce.zip.pending"
try {
[IO.Directory]::CreateDirectory($destination) | Out-Null
foreach ($entry in $entries) {
    $item = Get-Item -LiteralPath $entry.Source
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Refusing a reparse point in package allowlist: $($entry.Name)" }
    $path = Join-Path $destination $entry.Name
    [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($path)) | Out-Null
    [IO.File]::Copy($entry.Source, $path, $false)
}
$manifestFiles = foreach ($entry in $entries) {
    $path = Join-Path $destination $entry.Name
    [ordered]@{ path = $entry.Name; size = (Get-Item -LiteralPath $path).Length; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant() }
}
$manifest = [ordered]@{
    application = '拯救者工具箱'; label = $Label; configuration = $Configuration
    source_revision = $SourceRevision; status = 'release-artifact'
    hardware_acceptance = 'separate-explicit-device-verification'
    packaged_utc = [DateTime]::UtcNow.ToString('o'); files = @($manifestFiles)
    exclusions = @('user settings', 'logs', 'personal reports', 'fonts', 'PDBs', 'local build configuration')
}
[IO.File]::WriteAllText((Join-Path $destination 'package-manifest.json'), ($manifest | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
Add-Type -AssemblyName System.IO.Compression.FileSystem
[IO.Compression.ZipFile]::CreateFromDirectory($destination, $zipPath, [IO.Compression.CompressionLevel]::Optimal, $false)
$archive = [IO.Compression.ZipFile]::OpenRead($zipPath)
try {
    $expected = @($entries.Name) + 'package-manifest.json'
    $actual = @($archive.Entries | ForEach-Object { $_.FullName.Replace('\', '/') })
    if (@(Compare-Object ($expected | Sort-Object) ($actual | Sort-Object)).Count) { throw 'Archive entries differ from the package allowlist.' }
    foreach ($entry in $archive.Entries) {
        $stream = $entry.Open()
        try { $sha = [Security.Cryptography.SHA256]::Create(); $hash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '').ToLowerInvariant() } finally { $stream.Dispose(); if ($sha) { $sha.Dispose() } }
        $localHash = (Get-FileHash -LiteralPath (Join-Path $destination $entry.FullName) -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne $localHash) { throw "Archive content verification failed: $($entry.FullName)" }
    }
} finally { $archive.Dispose() }
[IO.Directory]::Move($destination, $finalDirectory)
[IO.File]::Move($zipPath, $finalZip)
$zipPath = $finalZip
$result = [ordered]@{ archive = [IO.Path]::GetFileName($zipPath); bytes = (Get-Item -LiteralPath $zipPath).Length; sha256 = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant(); files = $expected.Count; verified = $true }
[IO.File]::WriteAllText((Join-Path $outputRoot "$packageName.sha256.json"), ($result | ConvertTo-Json), [Text.UTF8Encoding]::new($false))
$result | ConvertTo-Json
} finally {
    # Delete only this invocation's verified, absolute staging targets.
    $prefix = $outputRoot.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    foreach ($staging in @($destination, (Join-Path $outputRoot ".jiks-package-$nonce.zip.pending"))) {
        $absolute = [IO.Path]::GetFullPath($staging)
        if (!$absolute.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase) -or [IO.Path]::GetFileName($absolute) -notlike ".jiks-package-$nonce*") { throw 'Refusing to clean an unverified package staging path.' }
        if (Test-Path -LiteralPath $absolute) { Remove-Item -LiteralPath $absolute -Recurse -Force }
    }
}
