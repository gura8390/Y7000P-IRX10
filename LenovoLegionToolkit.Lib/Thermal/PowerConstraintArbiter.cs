using System;
using System.Collections.Generic;

namespace LenovoLegionToolkit.Lib.Thermal;

public sealed record CpuPowerEnvelope(double LongWatts, double ShortWatts);
public sealed record PowerConstraint(CpuPowerEnvelope Ceiling, long PolicyEpoch, long ModeEpoch, long ExpiresAt);
public sealed record PowerCommand(CpuPowerEnvelope Target, long PolicyEpoch, long ModeEpoch, long ExpiresAt);
public sealed record CpuPowerCapability(double MinLong, double MaxLong, double MinShort, double MaxShort, double Step)
{
    // Decimal firmware grids avoid binary rounding drift and repeated one-step reductions.
    public bool Contains(CpuPowerEnvelope value) => value is not null
        && IsRepresentable(MinLong) && IsRepresentable(MaxLong)
        && IsRepresentable(MinShort) && IsRepresentable(MaxShort) && IsRepresentable(Step)
        && IsRepresentable(value.LongWatts) && IsRepresentable(value.ShortWatts)
        && Step > 0 && (decimal)Step > 0 && MinLong > 0 && MinShort > 0
        && MaxLong >= MinLong && MaxShort >= MinShort
        && value.LongWatts >= MinLong && value.LongWatts <= MaxLong
        && value.ShortWatts >= MinShort && value.ShortWatts <= MaxShort
        && value.LongWatts <= value.ShortWatts;
    public bool IsAligned(CpuPowerEnvelope value) => TryRoundDown(value, out var rounded) && value == rounded;

    public CpuPowerEnvelope RoundDown(CpuPowerEnvelope value) => TryRoundDown(value, out var rounded)
        ? rounded : throw new ArgumentException("No legal power envelope satisfies the supplied ceiling.", nameof(value));

    private bool TryRoundDown(CpuPowerEnvelope value, out CpuPowerEnvelope rounded)
    {
        rounded = null!;
        if (!Contains(value)) return false;
        try
        {
            var step = (decimal)Step;
            var minLong = (decimal)MinLong;
            var minShort = (decimal)MinShort;
            var shortWatts = minShort + decimal.Floor(((decimal)value.ShortWatts - minShort) / step) * step;
            if ((double)shortWatts > value.ShortWatts) shortWatts -= step;
            // Different grid origins can reverse Long <= Short after independent rounding.
            var longCeiling = Math.Min((decimal)value.LongWatts, shortWatts);
            if (longCeiling < minLong) return false;
            var longWatts = minLong + decimal.Floor((longCeiling - minLong) / step) * step;
            if ((double)longWatts > value.LongWatts) longWatts -= step;
            rounded = new((double)longWatts, (double)shortWatts);
            return Contains(rounded);
        }
        catch (OverflowException) { return false; }
    }

    public CpuPowerEnvelope GetMinimumEnvelope()
    {
        if (!Contains(new(MinLong, MaxShort)))
            throw new InvalidOperationException("Capability has no usable envelope.");
        var step = (decimal)Step;
        var minShort = (decimal)MinShort;
        var shortWatts = minShort + decimal.Ceiling((Math.Max((decimal)MinLong, minShort) - minShort) / step) * step;
        var minimum = new CpuPowerEnvelope(MinLong, (double)shortWatts);
        return IsAligned(minimum) ? minimum : throw new InvalidOperationException("Capability has no usable minimum envelope.");
    }

    private static bool IsRepresentable(double value)
    {
        if (!double.IsFinite(value)) return false;
        try { _ = (decimal)value; return true; }
        catch (OverflowException) { return false; }
    }
}
public static class PowerConstraintArbiter
{
    public static PowerCommand Resolve(CpuPowerEnvelope user, IEnumerable<PowerConstraint> constraints,
        CpuPowerCapability capability, long now, long policyEpoch, long modeEpoch, long expiresAt)
    {
        ArgumentNullException.ThrowIfNull(constraints);
        ArgumentNullException.ThrowIfNull(capability);
        if (!capability.Contains(user) || expiresAt <= now) throw new ArgumentException("Invalid baseline or deadline.");
        var target = user;
        foreach (var constraint in constraints)
        {
            ArgumentNullException.ThrowIfNull(constraint);
            if (constraint.ExpiresAt <= now || constraint.PolicyEpoch != policyEpoch || constraint.ModeEpoch != modeEpoch)
                throw new InvalidOperationException("Constraint is expired or belongs to another policy/mode; re-evaluation required.");
            if (!capability.Contains(constraint.Ceiling)) throw new ArgumentException("Unsupported constraint.");
            target = new(Math.Min(target.LongWatts, constraint.Ceiling.LongWatts),
                Math.Min(target.ShortWatts, constraint.Ceiling.ShortWatts));
            expiresAt = Math.Min(expiresAt, constraint.ExpiresAt);
        }
        target = capability.RoundDown(target);
        if (!capability.Contains(target)) throw new InvalidOperationException("No legal envelope satisfies all ceilings.");
        return new(target, policyEpoch, modeEpoch, expiresAt);
    }
}
