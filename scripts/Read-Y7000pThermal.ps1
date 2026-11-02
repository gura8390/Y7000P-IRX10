[CmdletBinding()]
param([Alias('OutFile')][string]$OutputPath = (Join-Path $PSScriptRoot '..\..\verification\hardware-readonly.json'))

# Read-only firmware diagnostics. No Set* WMI method, mode switch or load test is used.
$ErrorActionPreference = 'Stop'
function Read-Section([scriptblock]$Read) {
    try { return @{ Status = 'Read'; Data = (& $Read) } }
    catch { return @{ Status = 'Unavailable'; Error = $_.Exception.Message } }
}

$principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
$report = [ordered]@{
    CollectedAt = (Get-Date).ToString('o')
    ReadOnly = $true
    Elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    Identity = Read-Section {
        $computer = Get-CimInstance Win32_ComputerSystem -OperationTimeoutSec 3
        $bios = Get-CimInstance Win32_BIOS -OperationTimeoutSec 3
        $cpu = Get-CimInstance Win32_Processor -OperationTimeoutSec 3
        @{ Manufacturer = $computer.Manufacturer; Model = $computer.Model; SystemSku = $computer.SystemSKUNumber;
            Bios = $bios.SMBIOSBIOSVersion; Cpu = $cpu.Name; Cores = $cpu.NumberOfCores; LogicalProcessors = $cpu.NumberOfLogicalProcessors }
    }
    CpuRanges = Read-Section {
        @(Get-CimInstance -Namespace root/WMI -ClassName LENOVO_CAPABILITY_DATA_01 -OperationTimeoutSec 3 |
            Where-Object { ($_.IDs -band 0xFFFF0000) -in @(0x01010000, 0x01020000, 0x01040000) } |
            Select-Object @{N='IdHex';E={'0x{0:X8}' -f $_.IDs}},IDs,MinValue,MaxValue,Step,DefaultValue)
    }
    CpuDiscrete = Read-Section {
        @(Get-CimInstance -Namespace root/WMI -ClassName LENOVO_DISCRETE_DATA -OperationTimeoutSec 3 |
            Where-Object { ($_.IDs -band 0xFFFF0000) -in @(0x01010000, 0x01020000, 0x01040000) } |
            Select-Object @{N='IdHex';E={'0x{0:X8}' -f $_.IDs}},Value)
    }
    Mode = Read-Section {
        $zone = Get-CimInstance -Namespace root/WMI -ClassName LENOVO_GAMEZONE_DATA -OperationTimeoutSec 3 | Select-Object -First 1
        $mode = Invoke-CimMethod -InputObject $zone -MethodName GetSmartFanMode -OperationTimeoutSec 3
        @{ RawMode = $mode.Data; ReturnValue = $mode.ReturnValue }
    }
    CurrentCpuValues = Read-Section {
        $other = Get-CimInstance -Namespace root/WMI -ClassName LENOVO_OTHER_METHOD -OperationTimeoutSec 3 | Select-Object -First 1
        foreach ($id in @([uint32]0x01020000, [uint32]0x01010000, [uint32]0x01040000, [uint32]0x05040000)) {
            $value = Invoke-CimMethod -InputObject $other -MethodName GetFeatureValue -Arguments @{ IDs = $id } -OperationTimeoutSec 3
            @{ IdHex = ('0x{0:X8}' -f $id); Value = $value.value; ReturnValue = $value.ReturnValue }
        }
    }
    AggregateCpuTemperature = Read-Section {
        $zone = Get-CimInstance -Namespace root/WMI -ClassName LENOVO_GAMEZONE_DATA -OperationTimeoutSec 3 | Select-Object -First 1
        $value = Invoke-CimMethod -InputObject $zone -MethodName GetCPUTemp -OperationTimeoutSec 3
        @{ Source = 'LENOVO_GAMEZONE_DATA.GetCPUTemp'; Celsius = $value.Data; ReturnValue = $value.ReturnValue;
            Scope = 'Single firmware aggregate; cannot replace independently read package and core-maximum temperatures.' }
    }
    Controllers = Read-Section {
        @(Get-Process | Where-Object { $_.ProcessName -match 'Legion|Vantage|Lenovo|Lnv|Toolkit' } | Select-Object ProcessName,Id,Path)
    }
}

$absoluteOutput = [IO.Path]::GetFullPath($OutputPath)
[IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($absoluteOutput)) | Out-Null
$report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $absoluteOutput -Encoding utf8
Write-Output "Read-only hardware report: $absoluteOutput"
Write-Output "Elevated: $($report.Elevated); range status: $($report.CpuRanges.Status); current-value status: $($report.CurrentCpuValues.Status)"
