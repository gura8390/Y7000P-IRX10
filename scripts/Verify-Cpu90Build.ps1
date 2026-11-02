[CmdletBinding()]
param(
    [string]$DotNet9 = (Join-Path $PSScriptRoot '..\..\tools\dotnet\dotnet.exe'),
    [string]$SystemDotNet = (Join-Path $env:ProgramFiles 'dotnet\dotnet.exe'),
    [string]$LogDirectory = (Join-Path $PSScriptRoot '..\..\verification')
)
$ErrorActionPreference = 'Stop'
if (!(Test-Path -LiteralPath $DotNet9)) { throw 'Supply -DotNet9 with a .NET 9 SDK dotnet.exe path.' }
if (!(Test-Path -LiteralPath $SystemDotNet)) { throw 'Supply -SystemDotNet with a .NET host that has the .NET 8 SDK/runtime for the isolated tests.' }
New-Item -ItemType Directory -Path $LogDirectory -Force | Out-Null
$sourceRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
function Invoke-Check([string]$HostPath, [string[]]$Arguments, [string]$Name) {
    $logPath = Join-Path $LogDirectory ($Name + '.log')
    & $HostPath @Arguments *> $logPath
    if ($LASTEXITCODE -ne 0) { Get-Content -LiteralPath $logPath -Tail 25; throw "Check failed: $Name" }
    Write-Output "PASS $Name ($logPath)"
}
Push-Location -LiteralPath $sourceRoot
try {
    Invoke-Check $DotNet9 @('build','LenovoLegionToolkit.WPF/LenovoLegionToolkit.WPF.csproj','-c','Release','-p:RestoreLockedMode=true') 'wpf-build'
    Invoke-Check $DotNet9 @('run','--project','tests/Thermal.Tests/Thermal.Tests.csproj','-c','Release') 'thermal'
    Invoke-Check $DotNet9 @('run','--project','tests/Regression.Tests/Regression.Tests.csproj','-c','Release') 'production-regressions'
    Invoke-Check $DotNet9 @('run','--project','tests/AutomationStartup.Tests/AutomationStartup.Tests.csproj','-c','Release') 'startup-regressions'
    Invoke-Check $SystemDotNet @('run','--project','tests/Firmware.Tests/Firmware.Tests.csproj','-c','Release') 'firmware'
    Invoke-Check $SystemDotNet @('run','--project','tests/ThermalLifecycle.Tests/ThermalLifecycle.Tests.csproj','-c','Release') 'lifecycle'
    Invoke-Check $DotNet9 @('run','--project','tools/ThermalValidation/ThermalValidation.csproj','-c','Release','--','--self-test') 'validator-self-tests'
    Invoke-Check $DotNet9 @('build','tests/WpfIsolation.Tests/WpfIsolation.Tests.csproj','-c','Release','-p:Platform=x64','-p:RestoreLockedMode=true') 'offscreen-wpf-build'
    # Execute only the fixture DLL. It rejects elevated execution, uses temporary
    # settings and never creates an HWND or runs the production application.
    $isolatedHost = Join-Path $sourceRoot 'BuildLLT\bin\x64\Release\net9.0-windows10.0.26100.0\win-x64\WpfIsolation.IsolatedHost.dll'
    Invoke-Check $DotNet9 @($isolatedHost,'--isolated-host') 'offscreen-wpf'
    $reportLine = Get-Content -LiteralPath (Join-Path $LogDirectory 'offscreen-wpf.log') | Where-Object { $_.StartsWith('Report: ') } | Select-Object -Last 1
    if (!$reportLine) { throw 'The isolated WPF fixture did not provide its report.' }
    $fixtureReport = [IO.Path]::GetFullPath($reportLine.Substring(8))
    $fixtureParent = [IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetTempPath()) 'LLT-WpfIsolation')) + [IO.Path]::DirectorySeparatorChar
    if (!$fixtureReport.StartsWith($fixtureParent, [StringComparison]::OrdinalIgnoreCase)) { throw 'Unexpected isolated WPF report destination.' }
    Copy-Item -LiteralPath $fixtureReport -Destination (Join-Path $LogDirectory 'offscreen-report.json')
} finally { Pop-Location }
