using System;

namespace LenovoLegionToolkit.Lib.Thermal;

public enum ThermalPhase { Normal, Warm, Hot, Recovering, SensorFault }
public readonly record struct TemperatureReading(double Celsius, long SampledAtMilliseconds)
{
    public bool IsFresh(long now, long maxAge) => double.IsFinite(Celsius) && Celsius > 0 && Celsius < 150
        && SampledAtMilliseconds >= 0 && now >= SampledAtMilliseconds && now - SampledAtMilliseconds <= maxAge;
}
public sealed record ThermalOptions
{
    public double TargetC { get; init; } = 90;
    public double WarmC { get; init; } = 87;
    public double RecoverC { get; init; } = 86;
    public long FreshnessMilliseconds { get; init; } = 2000;
    public long RecoveryMilliseconds { get; init; } = 20000;
    public long AdjustmentMilliseconds { get; init; } = 2000;
    public double MinimumFraction { get; init; } = 0.35;
    public double Reduction { get; init; } = 0.10;
    public double WarmReduction { get; init; } = 0.10;
    public double RecoveryStep { get; init; } = 0.02;
    public void Validate()
    {
        if (!double.IsFinite(TargetC) || !double.IsFinite(WarmC) || !double.IsFinite(RecoverC)
            || RecoverC <= 0 || RecoverC >= WarmC || WarmC >= TargetC || TargetC >= 150
            || FreshnessMilliseconds <= 0 || RecoveryMilliseconds <= 0 || AdjustmentMilliseconds <= 0
            || !double.IsFinite(MinimumFraction) || MinimumFraction <= 0 || MinimumFraction > 1
            || !double.IsFinite(Reduction) || Reduction <= 0 || Reduction > 1
            || !double.IsFinite(WarmReduction) || WarmReduction <= 0 || WarmReduction > 1
            || !double.IsFinite(RecoveryStep) || RecoveryStep <= 0 || RecoveryStep > 1)
            throw new ArgumentOutOfRangeException(nameof(ThermalOptions));
    }
}
public sealed record ThermalDecision(ThermalPhase Phase, double PowerFraction, bool RequestCooling,
    long? CoolSince, long LastAdjustment, long EvaluatedAt, string Reason);

// Pure policy: no WMI, wall clock, UI or persistent preset writes.
// Fraction is relative to a separately validated user baseline, never a firmware wattage claim.
public sealed class ThermalPolicy
{
    private readonly ThermalOptions _options;
    public ThermalPolicy(ThermalOptions? options = null)
    {
        _options = options ?? new();
        _options.Validate();
    }
    public ThermalDecision Evaluate(TemperatureReading package, TemperatureReading coreMax, long now,
        ThermalDecision? previous = null) => EvaluateCore(Math.Max(package.Celsius, coreMax.Celsius),
            package.IsFresh(now, _options.FreshnessMilliseconds) && coreMax.IsFresh(now, _options.FreshnessMilliseconds),
            now, previous, "package/core maximum");

    // Explicit single-source contract for a machine-validated firmware aggregate. This
    // reading is not represented as two independent package/core sensor measurements.
    public ThermalDecision EvaluateFirmware(TemperatureReading firmware, long now,
        ThermalDecision? previous = null) => EvaluateCore(firmware.Celsius,
            firmware.IsFresh(now, _options.FreshnessMilliseconds), now, previous, "Lenovo firmware aggregate");

    // The supervisor can supplement a valid firmware source with fresh partial hardware
    // telemetry. Keep the actual hottest reading; never fabricate a missing second sensor.
    internal ThermalDecision EvaluateFallbackHotspot(TemperatureReading hottest, long now,
        ThermalDecision? previous = null) => EvaluateCore(hottest.Celsius,
            hottest.IsFresh(now, _options.FreshnessMilliseconds), now, previous, "firmware plus available hardware hotspot");

    private ThermalDecision EvaluateCore(double temperature, bool readingsHealthy, long now,
        ThermalDecision? previous, string source)
    {
        bool validPrevious = previous is null || double.IsFinite(previous.PowerFraction)
            && previous.PowerFraction > 0 && previous.PowerFraction <= 1
            && previous.EvaluatedAt >= 0 && previous.LastAdjustment >= 0 && previous.LastAdjustment <= previous.EvaluatedAt
            && (previous.CoolSince is null || previous.CoolSince >= 0 && previous.CoolSince <= previous.EvaluatedAt);
        var fraction = validPrevious ? Math.Clamp(previous?.PowerFraction ?? 1, _options.MinimumFraction, 1) : _options.MinimumFraction;
        var last = validPrevious ? Math.Min(previous?.LastAdjustment ?? now, now) : now;
        bool healthy = readingsHealthy && validPrevious && (previous is null || now >= previous.EvaluatedAt);
        if (!healthy)
            return new(ThermalPhase.SensorFault, fraction, true, null, last, now,
                $"Temperature missing, stale or clock regressed ({source}); recovery inhibited.");
        if (temperature >= _options.TargetC)
        {
            if (previous is null || fraction == 1 || now - last >= _options.AdjustmentMilliseconds)
            {
                fraction = Math.Max(_options.MinimumFraction, fraction - _options.Reduction);
                last = now;
            }
            return new(ThermalPhase.Hot, fraction, true, null, last, now, "Target reached; tighten CPU power ceiling.");
        }
        if (temperature >= _options.WarmC)
        {
            // Preemptive restriction is applied once on entry from an unrestricted state.
            if (fraction == 1) { fraction = Math.Max(_options.MinimumFraction, 1 - _options.WarmReduction); last = now; }
            return new(ThermalPhase.Warm, fraction, true, null, last, now, "Approaching target; reserve thermal headroom.");
        }
        if (temperature > _options.RecoverC)
            return new(ThermalPhase.Recovering, fraction, fraction < 1, null, last, now, "Hold restriction inside hysteresis band.");
        var coolSince = previous?.CoolSince ?? now;
        if (now - coolSince >= _options.RecoveryMilliseconds && now - last >= _options.AdjustmentMilliseconds)
        {
            fraction = Math.Min(1, fraction + _options.RecoveryStep);
            last = now;
        }
        return new(fraction >= 1 ? ThermalPhase.Normal : ThermalPhase.Recovering, fraction, fraction < 1,
            coolSince, last, now, fraction >= 1 ? "No thermal restriction." : "Sustained cooling; recover slowly.");
    }
}
