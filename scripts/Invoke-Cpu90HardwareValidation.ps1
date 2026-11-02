[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$ValidatorPath,
    [Parameter(Mandatory = $true)][string]$OutputDirectory,
    [ValidateRange(0, 2147483647)][int]$StopToolkitProcessId = 0,
    [switch]$UseCustomMode,
    [switch]$CpuLoad,
    [ValidateRange(1, 3600)][int]$Seconds = 60,
    [int]$Workers = 0
)

# Windows PowerShell 5.1. The caller owns elevation (RunAs); this script never
# launches the application, changes a Windows plan, installs a driver or writes
# CPU limits. Only an explicitly requested raw Custom-mode transition is allowed.
# Exit 3 means physical completion/process exit is unconfirmed: leave the
# validator and firmware mode alone, then inspect progress.json before recovery.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 2.0
$expectedInstalledToolkit = Join-Path $env:LOCALAPPDATA 'Programs\LenovoLegionToolkit\Lenovo Legion Toolkit.exe'
$script:progress = [ordered]@{
    SchemaVersion = 1; StartedAtUtc = [DateTime]::UtcNow.ToString('o'); Phase = 'Preflight'; State = 'Running'
    WrapperProcessId = $PID; RequestedSeconds = $Seconds; CpuLoadRequested = [bool]$CpuLoad
    UseCustomModeRequested = [bool]$UseCustomMode; RequestedWorkers = $Workers
    StopToolkitProcessId = $StopToolkitProcessId; WindowsPlanChangesAllowed = $false
    CpuLimitWritesByWrapper = $false; ValidatorStarted = $false; ValidatorExited = $false
    OriginalStateVerified = $false; CustomBaselineVerified = $false; Errors = @()
}
$script:physicalIoUnconfirmed = $false
$script:hardwarePhaseEntered = $false
$script:modeChangeVerified = $false
$script:validatorStarted = $false
$script:validatorExitObserved = $false
$script:validatorProcess = $null
$script:original = $null
$script:customBaseline = $null
$script:baseline = $null
$script:zone = $null
$script:other = $null
$script:methodReturnTypes = @{}
$script:progressPath = $null
$exitCode = 1

function Write-FlushedJson([string]$Path, $Value) {
    $json = $Value | ConvertTo-Json -Depth 16
    $temporary = $Path + '.' + [Guid]::NewGuid().ToString('N') + '.pending'
    try {
        $bytes = [Text.UTF8Encoding]::new($false).GetBytes($json)
        $stream = [IO.FileStream]::new($temporary, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
        try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush($true) }
        finally { $stream.Dispose() }
        for ($attempt = 0; $attempt -lt 10; $attempt++) {
            try {
        if ([IO.File]::Exists($Path)) { [IO.File]::Replace($temporary, $Path, $Path + '.previous') }
                else { [IO.File]::Move($temporary, $Path) }
                break
            }
            catch [IO.IOException] {
                if ($attempt -eq 9) { throw }
                Start-Sleep -Milliseconds 50
            }
        }
    }
    finally { if ([IO.File]::Exists($temporary)) { [IO.File]::Delete($temporary) } }
}

function Save-Progress([string]$Phase) {
    $script:progress['Phase'] = $Phase
    $script:progress['UpdatedAtUtc'] = [DateTime]::UtcNow.ToString('o')
    $script:progress['PhysicalIoUnconfirmed'] = $script:physicalIoUnconfirmed
    if ($script:progressPath) { Write-FlushedJson $script:progressPath $script:progress }
}

function Assert-AcPower {
    $status = [Cpu90Validation.NativePower]::ReadAcLineStatus()
    if ($status -ne 1) { throw "AC power is required; GetSystemPowerStatus ACLineStatus=$status." }
    return $status
}

function Get-PowerPlan {
    $plans = @(Get-CimInstance -Namespace 'root/cimv2/power' -ClassName Win32_PowerPlan -Filter 'IsActive=True' -OperationTimeoutSec 5)
    if ($plans.Count -ne 1) { throw "Expected one active Windows power plan; found $($plans.Count)." }
    $match = [regex]::Match([string]$plans[0].InstanceID, '\{([0-9a-fA-F-]{36})\}')
    if (!$match.Success) { throw 'The active Windows power-plan GUID could not be read.' }
    return [ordered]@{ Guid = ([Guid]$match.Groups[1].Value).ToString(); Name = [string]$plans[0].ElementName }
}

function Invoke-FirmwareMethod($Instance, [string]$Method, [hashtable]$Arguments) {
    $className = [string]$Instance.CimSystemProperties.ClassName
    $key = $className + '.' + $Method
    try {
        if (!$script:methodReturnTypes.ContainsKey($key)) {
            $class = Get-CimClass -Namespace 'root/WMI' -ClassName $className -OperationTimeoutSec 5
            $declaration = $class.CimClassMethods[$Method]
            if ($null -eq $declaration) { throw "Firmware method metadata missing: $key." }
            $script:methodReturnTypes[$key] = [string]$declaration.ReturnType
        }
        $call = @{ InputObject = $Instance; MethodName = $Method; OperationTimeoutSec = 5; ErrorAction = 'Stop' }
        if ($Arguments -and $Arguments.Count) { $call['Arguments'] = $Arguments }
        $result = Invoke-CimMethod @call
        $returnProperty = $result.PSObject.Properties['ReturnValue']
        if ($null -ne $returnProperty -and $null -ne $returnProperty.Value) {
            $returnValue = $returnProperty.Value
            $returnType = $script:methodReturnTypes[$key]
            if ($returnType -eq 'Boolean' -or $returnValue -is [bool]) {
                if (!$returnValue) { throw "$key returned Boolean false." }
            }
            elseif ($returnType -match '^(U?Int(8|16|32|64)|SInt(8|16|32|64))$') {
                if ([Convert]::ToInt64($returnValue) -ne 0) { throw "$key returned code $returnValue ($returnType)." }
            }
            else { throw "Unsupported firmware return convention: $key, $returnType, $returnValue." }
        }
        return $result
    }
    catch {
        # A CIM timeout/error does not prove that a vendor operation stopped.
        if ($script:hardwarePhaseEntered -or $Method -eq 'SetSmartFanMode') { $script:physicalIoUnconfirmed = $true }
        throw
    }
}

function Read-RawMode {
    $result = Invoke-FirmwareMethod $script:zone 'GetSmartFanMode' @{}
    if ($null -eq $result.PSObject.Properties['Data'] -or $null -eq $result.Data) { throw 'GetSmartFanMode did not return Data.' }
    return [Convert]::ToInt32($result.Data)
}

function Read-CpuLimits {
    $values = [ordered]@{}
    foreach ($field in @(
        @{ Name = 'LongWatts'; Id = [uint32]0x01020000 },
        @{ Name = 'ShortWatts'; Id = [uint32]0x01010000 },
        @{ Name = 'TemperatureC'; Id = [uint32]0x01040000 }
    )) {
        $result = Invoke-FirmwareMethod $script:other 'GetFeatureValue' @{ IDs = $field.Id }
        if ($null -eq $result.PSObject.Properties['value'] -or $null -eq $result.value) { throw "No value returned for $($field.Name)." }
        $value = [Convert]::ToInt32($result.value)
        if ($value -le 0) { throw "Invalid CPU baseline $($field.Name)=$value." }
        $values[$field.Name] = $value
    }
    return $values
}

function Read-Snapshot {
    $firstMode = Read-RawMode
    $plan = Get-PowerPlan
    $limits = Read-CpuLimits
    $lastMode = Read-RawMode
    $lastPlan = Get-PowerPlan
    if ($firstMode -ne $lastMode -or $plan.Guid -ne $lastPlan.Guid) { throw 'Mode or Windows power plan changed during snapshot collection.' }
    return [ordered]@{ CapturedAtUtc = [DateTime]::UtcNow.ToString('o'); RawMode = $lastMode; WindowsPowerPlan = $lastPlan; CpuLimits = $limits }
}

function Assert-LimitsEqual($Expected, $Actual, [string]$Label) {
    foreach ($field in @('LongWatts', 'ShortWatts', 'TemperatureC')) {
        if ($Expected[$field] -ne $Actual[$field]) { throw "$Label mismatch: $field expected $($Expected[$field]), observed $($Actual[$field]). No CPU limit was written by this wrapper." }
    }
}

function Assert-SnapshotEqual($Expected, $Actual, [string]$Label) {
    if ($Expected.RawMode -ne $Actual.RawMode) { throw "$Label mode mismatch: expected $($Expected.RawMode), observed $($Actual.RawMode)." }
    if ($Expected.WindowsPowerPlan.Guid -ne $Actual.WindowsPowerPlan.Guid) { throw "$Label Windows power plan changed; the wrapper will not modify the plan." }
    Assert-LimitsEqual $Expected.CpuLimits $Actual.CpuLimits $Label
}

function Set-RawModeVerified([int]$Mode, [string]$Phase) {
    $null = Assert-AcPower
    Save-Progress $Phase
    $script:hardwarePhaseEntered = $true
    $null = Invoke-FirmwareMethod $script:zone 'SetSmartFanMode' @{ Data = [uint32]$Mode }
    $modeDeadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        $observed = Read-RawMode
        if ($observed -eq $Mode) { return }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $modeDeadline)
    $script:physicalIoUnconfirmed = $true
    throw "Raw mode transition did not settle: requested $Mode, observed $observed. No further recovery write was attempted."
}

function Read-ValidatorReport([string]$Path) {
    if (![IO.File]::Exists($Path)) { return $null }
    # Validator reporting is best effort and may be observed midway through a write.
    try { return (Get-Content -LiteralPath $Path -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop) }
    catch { return $null }
}

function Report-Value($Report, [string]$Name) {
    if ($null -eq $Report) { return $null }
    $property = $Report.PSObject.Properties[$Name]
    if ($null -eq $property) { return $null }
    return $property.Value
}

try {
    $outputRoot = [IO.Path]::GetFullPath($OutputDirectory)
    [IO.Directory]::CreateDirectory($outputRoot) | Out-Null
    $candidateProgressPath = Join-Path $outputRoot 'progress.json'
    $baselinePath = Join-Path $outputRoot 'baseline.json'
    $reportPath = Join-Path $outputRoot 'validator-report.json'
    foreach ($existing in @($candidateProgressPath, $baselinePath, $reportPath, (Join-Path $outputRoot 'custom-baseline.json'), (Join-Path $outputRoot 'validator.stdout.log'), (Join-Path $outputRoot 'validator.stderr.log'))) {
        if ([IO.File]::Exists($existing)) { throw "Use a fresh output directory; an existing evidence file will not be overwritten: $existing" }
    }
    $script:progressPath = $candidateProgressPath
    Save-Progress 'Preflight'
    $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run this script in an already elevated Windows PowerShell host.' }
    $validator = [IO.Path]::GetFullPath($ValidatorPath)
    if (![IO.File]::Exists($validator) -or [IO.Path]::GetFileName($validator) -cne 'Y7000p.ThermalValidation.exe') { throw "ValidatorPath must name the existing Y7000p.ThermalValidation.exe: $validator" }
    $script:progress['ValidatorPath'] = $validator
    $script:progress['ValidatorSha256'] = (Get-FileHash -LiteralPath $validator -Algorithm SHA256).Hash
    $computer = Get-CimInstance Win32_ComputerSystem -OperationTimeoutSec 5
    $bios = Get-CimInstance Win32_BIOS -OperationTimeoutSec 5
    $cpus = @(Get-CimInstance Win32_Processor -OperationTimeoutSec 5)
    if ($cpus.Count -ne 1 -or ([string]$computer.Manufacturer).Trim() -ine 'LENOVO' -or ([string]$computer.Model).Trim() -cne '83NN' -or ([string]$bios.SMBIOSBIOSVersion).Trim() -cne 'S9CN15WW' -or ([string]$cpus[0].Name).IndexOf('i9-14900HX', [StringComparison]::OrdinalIgnoreCase) -lt 0) {
        throw 'Hardware identity must be LENOVO / 83NN / S9CN15WW / i9-14900HX. No hardware action was performed.'
    }
    $identity = [ordered]@{ Manufacturer = ([string]$computer.Manufacturer).Trim(); Model = ([string]$computer.Model).Trim(); Bios = ([string]$bios.SMBIOSBIOSVersion).Trim(); Cpu = ([string]$cpus[0].Name).Trim(); LogicalProcessors = [int]$cpus[0].NumberOfLogicalProcessors }
    if ($Workers -lt 0 -or $Workers -gt $identity.LogicalProcessors -or ($PSBoundParameters.ContainsKey('Workers') -and $Workers -eq 0)) { throw 'Workers, when supplied, must be 1..logical processor count.' }
    if ($null -eq ('Cpu90Validation.NativePower' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
namespace Cpu90Validation {
    public static class NativePower {
        [StructLayout(LayoutKind.Sequential)] private struct PowerStatus {
            public byte AcLineStatus, BatteryFlag, BatteryLifePercent, SystemStatusFlag;
            public uint BatteryLifeTime, BatteryFullLifeTime;
        }
        [DllImport("kernel32.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)] private static extern bool GetSystemPowerStatus(out PowerStatus status);
        public static byte ReadAcLineStatus() {
            PowerStatus status;
            if (!GetSystemPowerStatus(out status)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return status.AcLineStatus;
        }
    }
}
'@ | Out-Null
    }
    $null = Assert-AcPower
    $zones = @(Get-CimInstance -Namespace 'root/WMI' -ClassName LENOVO_GAMEZONE_DATA -OperationTimeoutSec 5)
    $others = @(Get-CimInstance -Namespace 'root/WMI' -ClassName LENOVO_OTHER_METHOD -OperationTimeoutSec 5)
    if ($zones.Count -ne 1 -or $others.Count -ne 1) { throw 'Expected one firmware mode instance and one feature-value instance.' }
    $script:zone = $zones[0]; $script:other = $others[0]
    $script:original = Read-Snapshot
    $script:baseline = [ordered]@{ CapturedAtUtc = [DateTime]::UtcNow.ToString('o'); Identity = $identity; AcLineStatus = 1; Original = $script:original; MethodReturnTypes = $script:methodReturnTypes; ExpectedInstalledToolkitPath = $expectedInstalledToolkit }
    Write-FlushedJson $baselinePath $script:baseline
    $script:progress['BaselinePath'] = $baselinePath
    $script:progress['Original'] = $script:original
    Save-Progress 'BaselineFlushedBeforeAnyStopOrWrite'
    if ($script:original.RawMode -ne 255 -and !$UseCustomMode) { throw "Current raw mode is $($script:original.RawMode); active validation requires explicit -UseCustomMode or an already selected Custom mode." }

    if ($StopToolkitProcessId -ne 0) {
        $old = Get-Process -Id $StopToolkitProcessId -ErrorAction Stop
        try {
            $actualPath = [IO.Path]::GetFullPath($old.Path)
            if (![string]::Equals($actualPath, [IO.Path]::GetFullPath($expectedInstalledToolkit), [StringComparison]::OrdinalIgnoreCase)) { throw "Refusing to stop PID ${StopToolkitProcessId}: executable path is not the verified installed Toolkit path ($actualPath)." }
            $null = $old.Handle
            $script:progress['StoppedToolkit'] = [ordered]@{ ProcessId = $old.Id; StartTimeUtc = $old.StartTime.ToUniversalTime().ToString('o'); Path = $actualPath }
            Save-Progress 'StoppingOnlyVerifiedInstalledToolkitProcess'
            Stop-Process -InputObject $old -Force -ErrorAction Stop
            if (!$old.WaitForExit(5000)) { throw 'The verified Toolkit process has not exited; no mode switch or validation was started.' }
        }
        finally { $old.Dispose() }
    }
    $controllers = @(Get-Process -Name 'Lenovo Legion Toolkit', 'LenovoVantage', 'LegionSpace', 'LegionZone' -ErrorAction SilentlyContinue)
    try { if ($controllers.Count) { throw "Another firmware controller remains running (PIDs $($controllers.Id -join ', ')); it was not stopped." } }
    finally { foreach ($controller in $controllers) { $controller.Dispose() } }
    $afterStop = Read-Snapshot
    $script:progress['AfterToolkitStop'] = $afterStop
    Assert-SnapshotEqual $script:original $afterStop 'State after Toolkit stop'
    if ($script:original.RawMode -ne 255) {
        Set-RawModeVerified 255 'SwitchingExplicitlyRequestedCustomMode'
        $script:modeChangeVerified = $true
    }
    $script:customBaseline = Read-Snapshot
    if ($script:customBaseline.RawMode -ne 255) { throw 'Custom mode was not confirmed before starting the validator.' }
    if ($script:customBaseline.WindowsPowerPlan.Guid -ne $script:original.WindowsPowerPlan.Guid) { throw 'Windows power plan changed before validation; this wrapper will not change it.' }
    $script:baseline['CustomBaseline'] = $script:customBaseline
    Write-FlushedJson $baselinePath $script:baseline
    Write-FlushedJson (Join-Path $outputRoot 'custom-baseline.json') $script:customBaseline
    $script:progress['CustomBaseline'] = $script:customBaseline
    $null = Assert-AcPower
    # Start-Process joins ArgumentList on Windows PowerShell. Only the output file
    # can contain spaces; Windows filenames cannot contain a literal double quote.
    if ($reportPath.Contains('"')) { throw 'The output path contains an invalid quote.' }
    $arguments = @('--active', '--seconds', [string]$Seconds, '--output', ('"' + $reportPath + '"'))
    if ($CpuLoad) { $arguments += '--cpu-load' }
    if ($Workers -gt 0) { $arguments += @('--workers', [string]$Workers) }
    $script:progress['ValidatorArguments'] = $arguments
    $script:progress['ValidatorReportPath'] = $reportPath
    $script:progress['StdoutPath'] = Join-Path $outputRoot 'validator.stdout.log'
    $script:progress['StderrPath'] = Join-Path $outputRoot 'validator.stderr.log'
    Save-Progress 'StartingSingleValidator'
    $script:hardwarePhaseEntered = $true
    $script:validatorProcess = Start-Process -FilePath $validator -WorkingDirectory ([IO.Path]::GetDirectoryName($validator)) -ArgumentList ($arguments -join ' ') -WindowStyle Hidden -RedirectStandardOutput $script:progress['StdoutPath'] -RedirectStandardError $script:progress['StderrPath'] -PassThru
    $script:validatorStarted = $true
    $null = $script:validatorProcess.Handle
    $script:progress['ValidatorStarted'] = $true
    $script:progress['ValidatorProcessId'] = $script:validatorProcess.Id
    $script:progress['ValidatorStartTimeUtc'] = $script:validatorProcess.StartTime.ToUniversalTime().ToString('o')
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds + 180)
    $script:progress['MonitorDeadlineUtc'] = $deadline.ToString('o')
    while ($true) {
        $script:validatorProcess.Refresh()
        if ($script:validatorProcess.HasExited) { $script:validatorExitObserved = $true; break }
        $liveReport = Read-ValidatorReport $reportPath
        $script:progress['ValidatorPhase'] = Report-Value $liveReport 'Phase'
        $script:progress['ValidatorRestorationStatus'] = Report-Value $liveReport 'RestorationStatus'
        if ([DateTime]::UtcNow -ge $deadline) {
            $script:physicalIoUnconfirmed = $true
            $script:progress['State'] = 'Waiting'
            $script:progress['FollowUp'] = 'Validator is still alive beyond the bounded monitor window. Do not kill it, switch mode or write CPU settings. Match PID and StartTimeUtc, wait for physical I/O/process exit, then verify its restoration and both saved baselines before recovering the original mode.'
            $exitCode = 3
            Save-Progress 'WaitingForValidatorCompletionNoRecoveryAttempted'
            break
        }
        Save-Progress 'MonitoringValidator'
        Start-Sleep -Seconds 2
    }
    if ($script:validatorExitObserved) {
        $null = $script:validatorProcess.WaitForExit(5000)
        $script:progress['ValidatorExited'] = $true
        $script:progress['ValidatorExitCode'] = $script:validatorProcess.ExitCode
        $finalReport = $null
        for ($attempt = 0; $attempt -lt 10; $attempt++) {
            $candidate = Read-ValidatorReport $reportPath
            if ($null -ne (Report-Value $candidate 'EndedAtUtc')) { $finalReport = $candidate; break }
            Start-Sleep -Milliseconds 200
        }
        $customAfter = Read-Snapshot
        $script:progress['AfterValidator'] = $customAfter
        Assert-SnapshotEqual $script:customBaseline $customAfter 'Validator custom-mode restoration'
        $script:progress['CustomBaselineVerified'] = $true
        $restoration = Report-Value $finalReport 'RestorationStatus'
        $script:progress['ValidatorFinalReport'] = $finalReport
        if ($null -eq $finalReport -or $restoration -notin @('VerifiedOrNoFieldStillOwned', 'NotEnabled')) { throw 'Validator exited without a terminal, verified restoration report; the firmware mode will not be overwritten.' }
        if ($script:modeChangeVerified) { Set-RawModeVerified $script:original.RawMode 'RestoringOriginalRawModeAfterVerifiedCustomLimits' }
        $finalSnapshot = Read-Snapshot
        $script:progress['Final'] = $finalSnapshot
        Assert-SnapshotEqual $script:original $finalSnapshot 'Final original-state restoration'
        $script:progress['OriginalStateVerified'] = $true
        $phase = Report-Value $finalReport 'Phase'
        $loadStopped = Report-Value $finalReport 'LoadStopped'
        if ($script:validatorProcess.ExitCode -eq 0 -and $phase -eq 'Completed' -and $loadStopped -eq $true -and (Report-Value $finalReport 'Sampled90CCheckPassed') -eq $true) {
            $exitCode = 0; $script:progress['State'] = 'Completed'
        }
        else { $exitCode = 1; $script:progress['State'] = 'Failed'; $script:progress['Errors'] += 'Validator acceptance failed or was incomplete; the original mode, CPU limits and unchanged Windows plan were independently verified.' }
        Save-Progress 'FinishedWithIndependentReadback'
    }
}
catch {
    $script:progress['Errors'] += $_.Exception.ToString()
    # Before a validator exists there is no thermal lease. A completed and
    # verified mode transition can be undone; any unconfirmed CIM operation
    # or live/failed validator blocks this recovery path.
    if ($script:modeChangeVerified -and !$script:validatorStarted -and !$script:physicalIoUnconfirmed) {
        try {
            if ((Read-RawMode) -ne 255) { throw 'Mode changed externally before pre-validator recovery; it was left untouched.' }
            Set-RawModeVerified $script:original.RawMode 'RecoveringOriginalModeBeforeValidatorStart'
            $recovered = Read-Snapshot
            $script:progress['Final'] = $recovered
            Assert-SnapshotEqual $script:original $recovered 'Pre-validator original-state recovery'
            $script:progress['OriginalStateVerified'] = $true
        }
        catch { $script:progress['Errors'] += $_.Exception.ToString() }
    }
    if ($script:physicalIoUnconfirmed -or ($script:validatorStarted -and !$script:validatorExitObserved)) {
        $exitCode = 3; $script:progress['State'] = 'Waiting'
        $script:progress['FollowUp'] = 'Physical completion or validator exit is unconfirmed. No validator was killed and no blind CPU/mode recovery was attempted. Inspect the saved baseline, PID/StartTimeUtc and terminal validator report before any further hardware writes.'
    }
    else { $exitCode = 1; $script:progress['State'] = 'Failed' }
    try { Save-Progress 'FailedOrWaitingSeeErrors' } catch { Write-Error -ErrorAction Continue ("Could not persist final progress: " + $_.Exception.Message) }
    Write-Error -ErrorAction Continue ($script:progress['Errors'] -join [Environment]::NewLine)
}
finally {
    $script:progress['WrapperEndedAtUtc'] = [DateTime]::UtcNow.ToString('o')
    $script:progress['WrapperExitCode'] = $exitCode
    try { if ($script:progressPath) { Write-FlushedJson $script:progressPath $script:progress } }
    catch { Write-Error -ErrorAction Continue ("Could not flush final progress: " + $_.Exception.Message); if ($exitCode -eq 0) { $exitCode = 1 } }
    if ($null -ne $script:validatorProcess -and $script:validatorExitObserved) { $script:validatorProcess.Dispose() }
}
exit $exitCode
