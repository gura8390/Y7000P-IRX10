using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Management;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.System.Management;
using LenovoLegionToolkit.Lib.Thermal;

namespace LenovoLegionToolkit.Lib.Controllers;

public sealed record ThermalMachineIdentity(string Manufacturer, string Model, string Bios, string Cpu);

/// <summary>Small firmware boundary, injectable for tests without issuing physical writes.</summary>
public interface IThermalCpuFirmware
{
    Task<ThermalMachineIdentity> GetIdentityAsync();
    Task<IReadOnlyList<RangeCapability>> GetCapabilitiesAsync();
    Task<IReadOnlyList<DiscreteCapability>> GetDiscreteCapabilitiesAsync();
    Task<int> GetModeAsync();
    Task<int> ReadAsync(uint id);
    Task WriteAsync(uint id, int value);
    void EnsureExclusiveProcess();
}

/// <summary>
/// CPU-only adapter for the owner's verified Y7000P IRX10 hardware identity.
/// Discovery is read-only; EnableAsync explicitly acquires ownership and sets the firmware temperature limit.
/// </summary>
public sealed class ThermalCpuTransport : ICpuPowerTransport
{
    private readonly IThermalCpuFirmware _firmware;
    private readonly RangeCapability _temperature;
    private CpuThermalWriteCoordinator.Lease? _lease;
    private CpuPowerEnvelope _ceiling;
    private int _temperatureCeiling;
    // A failed setter may have completed before reporting its error. Keep the previous
    // owned value AND the attempted value until a complete readback verifies the change.
    private readonly HashSet<int> _ownedLong = [];
    private readonly HashSet<int> _ownedShort = [];
    private readonly HashSet<int> _ownedTemperature = [];
    private readonly SemaphoreSlim _lifecycleGate = new(1, 1);
    private readonly SemaphoreSlim _temperatureReadGate = new(1, 1);

    public ThermalMachineIdentity Identity { get; }
    public CpuPowerCapability Capability { get; }
    public CpuPowerEnvelope Baseline { get; }
    public int OriginalTemperatureLimit { get; }
    public int TemperatureMinimum => _temperature.Min;
    public int TemperatureMaximum => _temperature.Max;
    public int TemperatureStep => _temperature.Step;
    public bool IsEnabled => _lease is not null;
    public bool IsSuperseded => _lease?.IsSuperseded ?? false;
    public Exception? LastRestorationError { get; private set; }
    public Exception? LastTemperatureReadError { get; private set; }

    private ThermalCpuTransport(IThermalCpuFirmware firmware, ThermalMachineIdentity identity,
        CpuPowerCapability capability, CpuPowerEnvelope baseline, RangeCapability temperature, int originalTemperature)
    {
        _firmware = firmware;
        Identity = identity;
        Capability = capability;
        Baseline = baseline;
        _ceiling = baseline;
        _temperature = temperature;
        OriginalTemperatureLimit = originalTemperature;
    }

    public static async Task<ThermalCpuTransport> DiscoverAsync(IThermalCpuFirmware? firmware = null)
    {
        firmware ??= new LenovoThermalCpuFirmware();
        var identity = await firmware.GetIdentityAsync().ConfigureAwait(false);
        if (!identity.Manufacturer.Equals("LENOVO", StringComparison.OrdinalIgnoreCase)
            || identity.Model != "83NN" || identity.Bios != "S9CN15WW"
            || !identity.Cpu.Contains("i9-14900HX", StringComparison.OrdinalIgnoreCase))
            throw new NotSupportedException($"Active CPU control is limited to Lenovo 83NN / S9CN15WW / i9-14900HX. Found {identity}.");

        var ranges = await firmware.GetCapabilitiesAsync().ConfigureAwait(false);
        var discrete = await firmware.GetDiscreteCapabilitiesAsync().ConfigureAwait(false);
        var longRange = FindRange(ranges, CapabilityID.CPULongTermPowerLimit);
        var shortRange = FindRange(ranges, CapabilityID.CPUShortTermPowerLimit);
        var temperatureRange = FindRange(ranges, CapabilityID.CPUTemperatureLimit);
        foreach (var range in new[] { longRange, shortRange, temperatureRange })
        {
            if (range.Min <= 0 || range.Max < range.Min || range.Step <= 0
                || discrete.Any(d => ((uint)d.Id & 0xFFFF00FF) == ((uint)range.Id & 0xFFFF00FF)))
                throw new NotSupportedException($"Unsupported firmware range for {range.Id}: min={range.Min}, max={range.Max}, step={range.Step}. Discrete limits need a dedicated adapter.");
        }
        if (longRange.Step != shortRange.Step)
            throw new NotSupportedException("PL1 and PL2 expose different steps; this firmware profile is not validated.");

        var mode = await firmware.GetModeAsync().ConfigureAwait(false);
        if (mode != 255)
            throw new InvalidOperationException($"CPU thermal discovery needs Custom mode (firmware 255); current mode is {mode}. No mode change was made.");
        var capability = new CpuPowerCapability(longRange.Min, longRange.Max, shortRange.Min, shortRange.Max, longRange.Step);
        var baseline = new CpuPowerEnvelope(
            await firmware.ReadAsync(CpuThermalWriteCoordinator.LongPowerId).ConfigureAwait(false),
            await firmware.ReadAsync(CpuThermalWriteCoordinator.ShortPowerId).ConfigureAwait(false));
        var originalTemperature = await firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
        if (!capability.Contains(baseline) || capability.RoundDown(baseline) != baseline
            || !InRange(temperatureRange, originalTemperature))
            throw new InvalidOperationException("Firmware baseline does not match its legal capability grid; active control was not enabled.");
        if (await firmware.GetModeAsync().ConfigureAwait(false) != 255)
            throw new InvalidOperationException("Firmware mode changed during CPU baseline discovery; no control was enabled.");
        return new ThermalCpuTransport(firmware, identity, capability, baseline, temperatureRange, originalTemperature);
    }

    private static RangeCapability FindRange(IReadOnlyList<RangeCapability> ranges, CapabilityID id)
    {
        // 0xFF is the custom-mode discovery row; never borrow a different mode's limits.
        var matches = ranges.Where(r => r.Id == id).ToArray();
        if (matches.Length != 1) throw new NotSupportedException($"Expected one custom-mode range for {id}; found {matches.Length}.");
        return matches[0];
    }

    private static bool InRange(RangeCapability range, int value) =>
        value >= range.Min && value <= range.Max && (value - range.Min) % range.Step == 0;

    public async Task EnableAsync(int firmwareTemperatureLimit = 88)
    {
        await _lifecycleGate.WaitAsync().ConfigureAwait(false);
        try { await EnableCoreAsync(firmwareTemperatureLimit).ConfigureAwait(false); }
        finally { _lifecycleGate.Release(); }
    }

    private async Task EnableCoreAsync(int firmwareTemperatureLimit)
    {
        if (LastRestorationError is not null)
            throw new InvalidOperationException("Previous CPU restoration was not verified. Discover a new firmware baseline before enabling control.", LastRestorationError);
        if (_lease is not null) throw new InvalidOperationException("CPU thermal transport is already enabled.");
        firmwareTemperatureLimit = Math.Min(firmwareTemperatureLimit, OriginalTemperatureLimit);
        ValidateTemperature(firmwareTemperatureLimit);
        _firmware.EnsureExclusiveProcess();
        await EnsureCustomModeAsync().ConfigureAwait(false);
        var lease = await CpuThermalWriteCoordinator.AcquireAsync(Baseline, firmwareTemperatureLimit).ConfigureAwait(false);
        _lease = lease;
        _ceiling = Baseline;
        _temperatureCeiling = firmwareTemperatureLimit;
        _ownedLong.Clear();
        _ownedShort.Clear();
        _ownedTemperature.Clear();
        try
        {
            if (await ReadEnvelopeAsync().ConfigureAwait(false) != Baseline
                || await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false) != OriginalTemperatureLimit)
                throw new InvalidOperationException("CPU firmware settings changed since discovery. Discover a fresh baseline before enabling control.");
            await SetTemperatureLimitAsync(firmwareTemperatureLimit).ConfigureAwait(false);
        }
        catch
        {
            await RestoreCoreAsync().ConfigureAwait(false);
            throw;
        }
    }

    public async Task<CpuPowerEnvelope> ReadAsync()
    {
        var lease = _lease;
        if (lease is null)
        {
            await EnsureCustomModeAsync().ConfigureAwait(false);
            var observed = await ReadEnvelopeAsync().ConfigureAwait(false);
            await EnsureCustomModeAsync().ConfigureAwait(false);
            return observed;
        }
        return await CpuThermalWriteCoordinator.ExecuteOwnerAsync(lease, _ceiling, _temperatureCeiling, async () =>
        {
            await EnsureCustomModeAsync().ConfigureAwait(false);
            var temperature = await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
            if (temperature != _temperatureCeiling)
                throw new InvalidOperationException($"CPU firmware temperature limit was changed externally: expected {_temperatureCeiling}, observed {temperature}.");
            var observed = await ReadEnvelopeAsync().ConfigureAwait(false);
            await EnsureCustomModeAsync().ConfigureAwait(false);
            return observed;
        }).ConfigureAwait(false);
    }

    private async Task<CpuPowerEnvelope> ReadEnvelopeAsync() => new(
        await _firmware.ReadAsync(CpuThermalWriteCoordinator.LongPowerId).ConfigureAwait(false),
        await _firmware.ReadAsync(CpuThermalWriteCoordinator.ShortPowerId).ConfigureAwait(false));

    /// <summary>A genuine single firmware aggregate; never presented as package/core-maximum dual telemetry.</summary>
    public async Task<TemperatureReading> ReadCpuTemperatureAsync()
    {
        // A stalled telemetry getter must not own the CPU actuator gate or accumulate more getters.
        if (!await _temperatureReadGate.WaitAsync(0).ConfigureAwait(false)) return new(double.NaN, 0);
        try
        {
            var requestedAt = Environment.TickCount64;
            var value = await _firmware.ReadAsync((uint)CapabilityID.CpuCurrentTemperature).ConfigureAwait(false);
            if (value <= 0 || value >= 150)
                throw new InvalidOperationException($"Invalid Lenovo firmware CPU aggregate temperature: {value}.");
            LastTemperatureReadError = null;
            return new(value, requestedAt);
        }
        catch (Exception ex)
        {
            LastTemperatureReadError = ex;
            throw;
        }
        finally { _temperatureReadGate.Release(); }
    }

    public async Task WriteAsync(CpuPowerEnvelope target)
    {
        if (!Capability.Contains(target) || Capability.RoundDown(target) != target
            || target.LongWatts > Baseline.LongWatts || target.ShortWatts > Baseline.ShortWatts)
            throw new ArgumentOutOfRangeException(nameof(target), "CPU targets must lie on the firmware grid and cannot exceed the verified user baseline.");
        var lease = _lease ?? throw new InvalidOperationException("Enable CPU thermal control before writing power limits.");
        await CpuThermalWriteCoordinator.ExecuteOwnerAsync(lease, target, _temperatureCeiling, async () =>
        {
            await EnsureCustomModeAsync().ConfigureAwait(false);
            var temperature = await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
            if (temperature != _temperatureCeiling)
                throw new InvalidOperationException($"CPU firmware temperature limit was changed externally: expected {_temperatureCeiling}, observed {temperature}. Power was not changed.");
            var current = await ReadEnvelopeAsync().ConfigureAwait(false);
            if (current != _ceiling)
                throw new InvalidOperationException($"CPU firmware power limits were changed externally: expected {_ceiling}, observed {current}. Power was not changed.");
            await WriteEnvelopeAsync(target, current, _temperatureCeiling).ConfigureAwait(false);
            var observed = await ReadEnvelopeAsync().ConfigureAwait(false);
            if (observed != target) throw new InvalidOperationException($"CPU power readback mismatch: requested {target}, observed {observed}.");
            if (await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false) != _temperatureCeiling)
                throw new InvalidOperationException("CPU firmware temperature limit changed during the power update.");
            await EnsureCustomModeAsync().ConfigureAwait(false);
            RememberVerifiedValue(_ownedLong, checked((int)target.LongWatts));
            RememberVerifiedValue(_ownedShort, checked((int)target.ShortWatts));
            _ceiling = target;
            return true;
        }).ConfigureAwait(false);
    }

    private async Task WriteEnvelopeAsync(CpuPowerEnvelope target, CpuPowerEnvelope current, int? expectedTemperature = null)
    {
        async Task VerifyCurrentAsync()
        {
            await EnsureCustomModeAsync().ConfigureAwait(false);
            if (await ReadEnvelopeAsync().ConfigureAwait(false) != current
                || expectedTemperature is { } expected
                && await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false) != expected)
                throw new InvalidOperationException("CPU firmware settings changed between physical writes; no further field was written.");
            await EnsureCustomModeAsync().ConfigureAwait(false);
        }

        async Task Long()
        {
            if (current.LongWatts == target.LongWatts) return;
            var value = checked((int)target.LongWatts);
            await VerifyCurrentAsync().ConfigureAwait(false);
            // Record intent before I/O: a firmware error may still have changed the field.
            _ownedLong.Add(value);
            await _firmware.WriteAsync(CpuThermalWriteCoordinator.LongPowerId, value).ConfigureAwait(false);
            current = current with { LongWatts = target.LongWatts };
        }
        async Task Short()
        {
            if (current.ShortWatts == target.ShortWatts) return;
            var value = checked((int)target.ShortWatts);
            await VerifyCurrentAsync().ConfigureAwait(false);
            _ownedShort.Add(value);
            await _firmware.WriteAsync(CpuThermalWriteCoordinator.ShortPowerId, value).ConfigureAwait(false);
            current = current with { ShortWatts = target.ShortWatts };
        }
        // Preserve PL1 <= PL2 even between the two physical writes.
        if (target.ShortWatts < current.LongWatts) { await Long().ConfigureAwait(false); await Short().ConfigureAwait(false); }
        else { await Short().ConfigureAwait(false); await Long().ConfigureAwait(false); }
    }

    private static void RememberVerifiedValue(HashSet<int> ownedValues, int value)
    {
        // An unchanged field that this owner never wrote still belongs to the user.
        if (ownedValues.Count == 0) return;
        ownedValues.Clear();
        ownedValues.Add(value);
    }

    public async Task SetTemperatureLimitAsync(int temperature)
    {
        ValidateTemperature(temperature);
        var lease = _lease ?? throw new InvalidOperationException("CPU thermal ownership has not been acquired.");
        await CpuThermalWriteCoordinator.ExecuteOwnerAsync(lease, _ceiling, temperature, async () =>
        {
            await EnsureCustomModeAsync().ConfigureAwait(false);
            var currentTemperature = await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
            var expectedTemperature = _ownedTemperature.Count == 0 ? OriginalTemperatureLimit : _temperatureCeiling;
            if (currentTemperature != expectedTemperature || await ReadEnvelopeAsync().ConfigureAwait(false) != _ceiling)
                throw new InvalidOperationException("CPU firmware settings changed externally. Temperature was not changed; discover a fresh baseline before restarting control.");
            await EnsureCustomModeAsync().ConfigureAwait(false);
            _ownedTemperature.Add(temperature);
            await _firmware.WriteAsync(CpuThermalWriteCoordinator.TemperatureId, temperature).ConfigureAwait(false);
            var observed = await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
            if (observed != temperature)
                throw new InvalidOperationException($"Firmware temperature-limit readback mismatch: requested {temperature}, observed {observed}.");
            if (await ReadEnvelopeAsync().ConfigureAwait(false) != _ceiling)
                throw new InvalidOperationException("CPU firmware power limits changed during the temperature update.");
            await EnsureCustomModeAsync().ConfigureAwait(false);
            RememberVerifiedValue(_ownedTemperature, temperature);
            _temperatureCeiling = temperature;
            return true;
        }).ConfigureAwait(false);
    }

    private void ValidateTemperature(int value)
    {
        if (value > 90 || value > OriginalTemperatureLimit || !InRange(_temperature, value))
            throw new ArgumentOutOfRangeException(nameof(value), $"CPU firmware temperature limit must be on the discovered {_temperature.Min}..{_temperature.Max} step {_temperature.Step} grid and at most 90°C.");
    }

    private async Task EnsureCustomModeAsync()
    {
        var mode = await _firmware.GetModeAsync().ConfigureAwait(false);
        if (mode != 255)
            throw new InvalidOperationException($"CPU thermal control requires Custom mode (firmware 255); current mode is {mode}. Select Custom mode first. No mode change was made.");
    }

    public async Task VerifyOwnershipAsync()
    {
        var lease = _lease ?? throw new InvalidOperationException("CPU thermal control is not enabled.");
        await CpuThermalWriteCoordinator.ExecuteOwnerAsync(lease, _ceiling, _temperatureCeiling, async () =>
        {
            await EnsureCustomModeAsync().ConfigureAwait(false);
            var temperature = await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
            if (temperature != _temperatureCeiling)
                throw new InvalidOperationException($"CPU firmware temperature limit was changed externally: expected {_temperatureCeiling}, observed {temperature}.");
            var power = await ReadEnvelopeAsync().ConfigureAwait(false);
            if (power != _ceiling)
                throw new InvalidOperationException($"CPU firmware power limits were changed externally: expected {_ceiling}, observed {power}.");
            await EnsureCustomModeAsync().ConfigureAwait(false);
            return true;
        }).ConfigureAwait(false);
    }

    public async Task RestoreIfOwnedAsync()
    {
        await _lifecycleGate.WaitAsync().ConfigureAwait(false);
        try { await RestoreCoreAsync().ConfigureAwait(false); }
        finally { _lifecycleGate.Release(); }
    }

    private async Task RestoreCoreAsync()
    {
        var lease = _lease;
        if (lease is null)
        {
            if (LastRestorationError is not null)
                throw new InvalidOperationException("Previous CPU restoration was not verified; this transport cannot report a successful stop. Discover a new firmware baseline.", LastRestorationError);
            return;
        }
        try
        {
            await CpuThermalWriteCoordinator.ReleaseAsync(lease, async () =>
            {
                // A changed mode or a changed field belongs to the user/firmware; do not overwrite it.
                if (await _firmware.GetModeAsync().ConfigureAwait(false) != 255) return;
                var current = await ReadEnvelopeAsync().ConfigureAwait(false);
                var restoreLong = !lease.UserChangedFields.Contains(CpuThermalWriteCoordinator.LongPowerId)
                    && _ownedLong.Contains(checked((int)current.LongWatts)) ? Baseline.LongWatts : current.LongWatts;
                var restoreShort = !lease.UserChangedFields.Contains(CpuThermalWriteCoordinator.ShortPowerId)
                    && _ownedShort.Contains(checked((int)current.ShortWatts)) ? Baseline.ShortWatts : current.ShortWatts;
                var restored = new CpuPowerEnvelope(restoreLong, restoreShort);
                if (!Capability.IsAligned(restored))
                    throw new InvalidOperationException("CPU baseline restoration conflicts with an externally changed power limit. Power and temperature settings were left unchanged; inspect the current CPU settings before restarting control.");
                await WriteEnvelopeAsync(restored, current).ConfigureAwait(false);
                if (await ReadEnvelopeAsync().ConfigureAwait(false) != restored)
                    throw new InvalidOperationException("CPU baseline restoration failed readback verification.");
                var temperature = await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false);
                if (!lease.UserChangedFields.Contains(CpuThermalWriteCoordinator.TemperatureId)
                    && _ownedTemperature.Contains(temperature))
                {
                    await EnsureCustomModeAsync().ConfigureAwait(false);
                    if (await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false) != temperature
                        || await ReadEnvelopeAsync().ConfigureAwait(false) != restored)
                        throw new InvalidOperationException("CPU firmware settings changed during restoration; the temperature limit was left unchanged.");
                    await EnsureCustomModeAsync().ConfigureAwait(false);
                    await _firmware.WriteAsync(CpuThermalWriteCoordinator.TemperatureId, OriginalTemperatureLimit).ConfigureAwait(false);
                    if (await _firmware.ReadAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false) != OriginalTemperatureLimit)
                        throw new InvalidOperationException("CPU temperature-limit restoration failed readback verification.");
                }
                if (await ReadEnvelopeAsync().ConfigureAwait(false) != restored)
                    throw new InvalidOperationException("CPU power limits changed during temperature-limit restoration.");
                await EnsureCustomModeAsync().ConfigureAwait(false);
            }).ConfigureAwait(false);
        }
        catch (Exception ex)
        {
            LastRestorationError = ex;
            throw;
        }
        finally { _lease = null; }
    }
}

internal sealed class LenovoThermalCpuFirmware : IThermalCpuFirmware
{
    public Task<ThermalMachineIdentity> GetIdentityAsync() => Task.Run(() =>
        new ThermalMachineIdentity(ReadProperty("Win32_ComputerSystem", "Manufacturer"),
            ReadProperty("Win32_ComputerSystem", "Model"), ReadProperty("Win32_BIOS", "SMBIOSBIOSVersion"),
            ReadProperty("Win32_Processor", "Name")));

    private static string ReadProperty(string type, string property)
    {
        using var searcher = new ManagementObjectSearcher("root\\CIMV2", $"SELECT {property} FROM {type}");
        using var results = searcher.Get();
        foreach (ManagementObject item in results)
        {
            using (item) return item[property]?.ToString()?.Trim() ?? string.Empty;
        }
        throw new InvalidOperationException($"No {type} identity data was returned.");
    }

    public Task<IReadOnlyList<RangeCapability>> GetCapabilitiesAsync() => Task.Run<IReadOnlyList<RangeCapability>>(async () =>
        (await WMI.LenovoCapabilityData01.ReadAsync().ConfigureAwait(false)).ToArray());
    public Task<IReadOnlyList<DiscreteCapability>> GetDiscreteCapabilitiesAsync() => Task.Run<IReadOnlyList<DiscreteCapability>>(async () =>
        (await WMI.LenovoDiscreteData.ReadAsync().ConfigureAwait(false)).ToArray());
    public Task<int> GetModeAsync() => Task.Run(WMI.LenovoGameZoneData.GetSmartFanModeAsync);
    public Task<int> ReadAsync(uint id) => Task.Run(() => WMI.LenovoOtherMethod.GetFeatureValueAsync(id));
    public Task WriteAsync(uint id, int value) => Task.Run(() => WMI.LenovoOtherMethod.SetFeatureValueUncoordinatedAsync(id, value));

    public void EnsureExclusiveProcess()
    {
        foreach (var name in new[] { "Lenovo Legion Toolkit", "LenovoVantage", "LegionSpace", "LegionZone" })
        {
            foreach (var process in Process.GetProcessesByName(name))
            {
                using (process)
                {
                    if (process.Id != Environment.ProcessId)
                        throw new InvalidOperationException($"Another firmware controller is running: {name} (PID {process.Id}). Close it before starting active CPU control.");
                }
            }
        }
    }
}
