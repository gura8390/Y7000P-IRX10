using System;
using System.Threading;
using System.Threading.Tasks;

namespace LenovoLegionToolkit.Lib.Thermal;

public sealed record ThermalControlOptions
{
    public ThermalOptions Policy { get; init; } = new() { TargetC = 88, WarmC = 85, RecoverC = 82 };
    public TimeSpan CommandTimeout { get; init; } = TimeSpan.FromMilliseconds(1500);
    public long CommandLifetimeMilliseconds { get; init; } = 2000;
    public long ReadbackIntervalMilliseconds { get; init; } = 1000;
    public long PredictionMilliseconds { get; init; } = 1000;
    public double MaximumPredictionC { get; init; } = 5;

    public void Validate()
    {
        ArgumentNullException.ThrowIfNull(Policy);
        Policy.Validate();
        if (CommandTimeout <= TimeSpan.Zero || CommandTimeout.TotalMilliseconds > int.MaxValue
            || CommandLifetimeMilliseconds <= 0 || CommandLifetimeMilliseconds > int.MaxValue
            || ReadbackIntervalMilliseconds <= 0
            || PredictionMilliseconds < 0 || PredictionMilliseconds > Policy.FreshnessMilliseconds
            || !double.IsFinite(MaximumPredictionC) || MaximumPredictionC < 0 || MaximumPredictionC > 20)
            throw new ArgumentOutOfRangeException(nameof(ThermalControlOptions));
    }
}

public enum ThermalControlStatus { Holding, Verified, SensorFault, ActuatorFault, UnknownCompletion, Superseded, Busy }
public enum ThermalTemperatureSource { Unavailable, PackageAndCoreMaximum, LenovoFirmwareAggregate, FirmwareWithPartialHardware }
public sealed record ThermalControlResult(ThermalControlStatus Status, ThermalDecision? Decision,
    CpuPowerEnvelope? Target, ApplyResult? Apply, string Reason, double? ObservedTemperatureC = null, double PredictedRiseC = 0,
    ThermalTemperatureSource Source = ThermalTemperatureSource.Unavailable);

// This class performs one tick. Its owner supplies a scheduler and a verified, mode-specific
// baseline/capability. It never writes a fan, GPU limit, preset or persistent setting.
public sealed class ThermalControlSupervisor
{
    private readonly CpuPowerCapability _capability;
    private readonly CpuPowerEnvelope _baseline;
    private readonly CpuPowerEnvelope _minimum;
    private readonly Func<(TemperatureReading Package, TemperatureReading CoreMax)> _getTemperatures;
    private readonly Func<TemperatureReading>? _getFirmwareTemperature;
    private readonly Func<long> _now;
    private readonly Func<(long Policy, long Mode)> _epochs;
    private readonly (long Policy, long Mode) _ownerEpoch;
    private readonly ThermalControlOptions _options;
    private readonly ThermalPolicy _policy;
    private readonly HardwareCommandExecutor _executor;
    private readonly SemaphoreSlim _tickGate = new(1, 1);
    private ThermalDecision? _decision;
    private ThermalControlResult? _lastResult;
    private CpuPowerEnvelope _lastVerified;
    private CpuPowerEnvelope _recoveryBudget;
    private CpuPowerEnvelope? _faultCeiling;
    private bool _actuatorFault;
    private long _lastRecovery;
    private long _lastVerification;
    private double? _previousTemperature;
    private long _previousTemperatureTick;
    private double _predictedRise;
    private ThermalTemperatureSource _previousSource;
    private int _previousPartialHardwareMask;

    public ThermalControlResult? LastResult => Volatile.Read(ref _lastResult);
    public ThermalDecision? LastDecision => LastResult?.Decision;
    public bool HasActuatorFault => Volatile.Read(ref _actuatorFault);

    public ThermalControlSupervisor(ICpuPowerTransport transport, CpuPowerCapability capability, CpuPowerEnvelope verifiedBaseline,
        Func<(TemperatureReading Package, TemperatureReading CoreMax)> getTemperatures, Func<long> now,
        Func<(long Policy, long Mode)> epochs, ThermalControlOptions? options = null, HardwareCommandExecutor? executor = null,
        Func<TemperatureReading>? getFirmwareTemperature = null)
    {
        ArgumentNullException.ThrowIfNull(transport);
        ArgumentNullException.ThrowIfNull(capability);
        ArgumentNullException.ThrowIfNull(getTemperatures);
        ArgumentNullException.ThrowIfNull(now);
        ArgumentNullException.ThrowIfNull(epochs);
        if (!capability.IsAligned(verifiedBaseline)) throw new ArgumentException("Baseline must be verified and aligned.", nameof(verifiedBaseline));
        _capability = capability;
        _baseline = verifiedBaseline;
        _minimum = capability.GetMinimumEnvelope();
        _getTemperatures = getTemperatures;
        _getFirmwareTemperature = getFirmwareTemperature;
        _now = now;
        _epochs = epochs;
        _ownerEpoch = epochs();
        _options = options ?? new();
        _options.Validate();
        _policy = new(_options.Policy);
        _executor = executor ?? new(transport, capability, now, epochs);
        _lastVerified = verifiedBaseline;
        _recoveryBudget = verifiedBaseline;
        _lastRecovery = now();
        _lastVerification = _lastRecovery;
    }

    public async Task<ThermalControlResult> EvaluateAsync(CancellationToken token = default)
    {
        if (!await _tickGate.WaitAsync(0, token).ConfigureAwait(false))
            return new(ThermalControlStatus.Busy, LastDecision, null, null, "A control tick is already in progress.",
                Source: LastResult?.Source ?? ThermalTemperatureSource.Unavailable);
        try
        {
            var now = _now();
            if (_epochs() != _ownerEpoch)
                return Publish(new(ThermalControlStatus.Superseded, _decision, null, null,
                    "Mode or policy changed; acquire a new verified baseline before restarting control."));

            (TemperatureReading Package, TemperatureReading CoreMax) input;
            try { input = _getTemperatures(); }
            catch { input = (new(double.NaN, now), new(double.NaN, now)); }
            var packageFresh = input.Package.IsFresh(now, _options.Policy.FreshnessMilliseconds);
            var coreFresh = input.CoreMax.IsFresh(now, _options.Policy.FreshnessMilliseconds);
            var fresh = packageFresh && coreFresh;
            var source = ThermalTemperatureSource.PackageAndCoreMaximum;
            var firmware = new TemperatureReading(double.NaN, now);
            int partialHardwareMask = 0;
            if (!fresh && _getFirmwareTemperature is not null)
            {
                source = ThermalTemperatureSource.LenovoFirmwareAggregate;
                try { firmware = _getFirmwareTemperature(); }
                catch { firmware = new(double.NaN, now); }
                fresh = firmware.IsFresh(now, _options.Policy.FreshnessMilliseconds);
                if (fresh && (packageFresh || coreFresh))
                {
                    source = ThermalTemperatureSource.FirmwareWithPartialHardware;
                    partialHardwareMask = (packageFresh ? 1 : 0) | (coreFresh ? 2 : 0);
                }
            }
            double? observed = fresh ? Math.Max(input.Package.Celsius, input.CoreMax.Celsius) : null;
            var sampleTick = Math.Min(input.Package.SampledAtMilliseconds, input.CoreMax.SampledAtMilliseconds);
            var fallbackHotspot = firmware;
            if (source is ThermalTemperatureSource.LenovoFirmwareAggregate or ThermalTemperatureSource.FirmwareWithPartialHardware)
            {
                if (source == ThermalTemperatureSource.FirmwareWithPartialHardware)
                {
                    if (packageFresh && input.Package.Celsius > fallbackHotspot.Celsius) fallbackHotspot = input.Package;
                    if (coreFresh && input.CoreMax.Celsius > fallbackHotspot.Celsius) fallbackHotspot = input.CoreMax;
                }
                observed = fresh ? fallbackHotspot.Celsius : null;
                sampleTick = fallbackHotspot.SampledAtMilliseconds;
            }
            if (source != _previousSource || partialHardwareMask != _previousPartialHardwareMask)
            {
                _previousTemperature = null;
                _predictedRise = 0;
                if (_decision is not null) _decision = _decision with { CoolSince = null };
                _previousSource = source;
                _previousPartialHardwareMask = partialHardwareMask;
            }
            if (fresh)
            {
                if (_previousTemperature is { } previous && sampleTick > _previousTemperatureTick
                    && sampleTick - _previousTemperatureTick <= _options.Policy.FreshnessMilliseconds)
                    _predictedRise = Math.Clamp((observed!.Value - previous) * _options.PredictionMilliseconds
                        / (sampleTick - _previousTemperatureTick), 0, _options.MaximumPredictionC);
                else if (_previousTemperature is null || sampleTick < _previousTemperatureTick) _predictedRise = 0;
                if (_previousTemperature is null || sampleTick != _previousTemperatureTick)
                {
                    _previousTemperature = observed;
                    _previousTemperatureTick = sampleTick;
                }
            }
            else { _previousTemperature = null; _predictedRise = 0; }
            var prediction = _predictedRise;

            _decision = source switch
            {
                ThermalTemperatureSource.LenovoFirmwareAggregate =>
                    _policy.EvaluateFirmware(firmware with { Celsius = firmware.Celsius + prediction }, now, _decision),
                ThermalTemperatureSource.FirmwareWithPartialHardware =>
                    _policy.EvaluateFallbackHotspot(fallbackHotspot with { Celsius = fallbackHotspot.Celsius + prediction }, now, _decision),
                _ => _policy.Evaluate(input.Package with { Celsius = input.Package.Celsius + prediction },
                    input.CoreMax with { Celsius = input.CoreMax.Celsius + prediction }, now, _decision)
            };
            CpuPowerEnvelope desired;
            if (_decision.Phase == ThermalPhase.SensorFault)
            {
                desired = _minimum;
                _recoveryBudget = _minimum;
                _lastRecovery = now;
            }
            else
            {
                desired = _capability.RoundDown(new(
                    Math.Max(_minimum.LongWatts, _baseline.LongWatts * _decision.PowerFraction),
                    Math.Max(_minimum.ShortWatts, _baseline.ShortWatts * _decision.PowerFraction)));
                // Physical minimum can be lower than the fractional policy minimum. Preserve
                // a slow ramp from that minimum after telemetry returns instead of jumping up.
                _recoveryBudget = Min(_recoveryBudget, desired);
                if (_decision.CoolSince is { } coolSince && now - coolSince >= _options.Policy.RecoveryMilliseconds
                    && now - _lastRecovery >= _options.Policy.AdjustmentMilliseconds)
                {
                    _recoveryBudget = new(Math.Min(_baseline.LongWatts, _recoveryBudget.LongWatts + _baseline.LongWatts * _options.Policy.RecoveryStep),
                        Math.Min(_baseline.ShortWatts, _recoveryBudget.ShortWatts + _baseline.ShortWatts * _options.Policy.RecoveryStep));
                    _lastRecovery = now;
                }
                desired = _capability.RoundDown(Min(desired, _recoveryBudget));
            }
            if (_faultCeiling is not null) desired = _capability.RoundDown(Min(desired, _faultCeiling));

            var unchanged = desired == _lastVerified;
            if (unchanged && now >= _lastVerification && now - _lastVerification < _options.ReadbackIntervalMilliseconds)
                return Publish(new(_actuatorFault ? ThermalControlStatus.ActuatorFault
                    : _decision.Phase == ThermalPhase.SensorFault ? ThermalControlStatus.SensorFault : ThermalControlStatus.Holding,
                    _decision, desired, null, _actuatorFault ? "Actuator fault is latched; automatic relaxation is inhibited." : "Verified power target is unchanged.", observed, prediction, source));

            var expiresAt = now > long.MaxValue - _options.CommandLifetimeMilliseconds ? long.MaxValue : now + _options.CommandLifetimeMilliseconds;
            var command = PowerConstraintArbiter.Resolve(_baseline,
                [new(desired, _ownerEpoch.Policy, _ownerEpoch.Mode, expiresAt)], _capability, now,
                _ownerEpoch.Policy, _ownerEpoch.Mode, expiresAt);
            _lastVerification = now;
            var applied = unchanged
                ? await _executor.VerifyAsync(command, _options.CommandTimeout, token).ConfigureAwait(false)
                : await _executor.ApplyAsync(command, _options.CommandTimeout, token).ConfigureAwait(false);
            if (applied.Status == ApplyStatus.Verified)
            {
                _lastVerified = command.Target;
                return Publish(new(_actuatorFault ? ThermalControlStatus.ActuatorFault
                    : _decision.Phase == ThermalPhase.SensorFault ? ThermalControlStatus.SensorFault : ThermalControlStatus.Verified,
                    _decision, command.Target, applied, _actuatorFault ? "Stricter limit verified; fault remains latched until explicit restart." : applied.Reason, observed, prediction, source));
            }

            _actuatorFault = true;
            var failedCeiling = Min(_lastVerified, command.Target);
            _faultCeiling = _faultCeiling is null ? failedCeiling : Min(_faultCeiling, failedCeiling);
            _recoveryBudget = Min(_recoveryBudget, _faultCeiling);
            return Publish(new(applied.Status == ApplyStatus.UnknownCompletion ? ThermalControlStatus.UnknownCompletion : ThermalControlStatus.ActuatorFault,
                _decision, command.Target, applied, applied.Reason + " Automatic relaxation is inhibited until explicit restart.", observed, prediction, source));
        }
        finally { _tickGate.Release(); }
    }

    private ThermalControlResult Publish(ThermalControlResult result)
    {
        Volatile.Write(ref _lastResult, result);
        return result;
    }

    private static CpuPowerEnvelope Min(CpuPowerEnvelope first, CpuPowerEnvelope second) =>
        new(Math.Min(first.LongWatts, second.LongWatts), Math.Min(first.ShortWatts, second.ShortWatts));
}
