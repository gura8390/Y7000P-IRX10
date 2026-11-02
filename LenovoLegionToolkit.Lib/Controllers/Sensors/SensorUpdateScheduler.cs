using System;
using System.Collections.Generic;

namespace LenovoLegionToolkit.Lib.Controllers.Sensors;

// Monotonic, deterministic deadlines. The controller serializes access to this planner.
public sealed class SensorUpdateScheduler
{
    private sealed class Subscription(long interval, HardwareUpdateScope scope, long due)
    {
        public long Interval { get; } = interval;
        public HardwareUpdateScope Scope { get; } = scope;
        public long Due { get; set; } = due;
    }
    private readonly Dictionary<object, Subscription> _subscriptions = [];
    public int Count => _subscriptions.Count;

    public void Set(object owner, TimeSpan interval, HardwareUpdateScope scope, long now)
    {
        ArgumentNullException.ThrowIfNull(owner);
        if (interval <= TimeSpan.Zero || interval.TotalMilliseconds > int.MaxValue)
            throw new ArgumentOutOfRangeException(nameof(interval));
        if (scope == HardwareUpdateScope.None || (scope & ~HardwareUpdateScope.All) != 0)
            throw new ArgumentOutOfRangeException(nameof(scope));
        var milliseconds = Math.Max(100, (long)Math.Ceiling(interval.TotalMilliseconds));
        _subscriptions[owner] = new(milliseconds, scope, now);
    }

    public bool Remove(object owner) => _subscriptions.Remove(owner);
    public void Clear() => _subscriptions.Clear();

    public HardwareUpdateScope TakeDue(long now)
    {
        var scope = HardwareUpdateScope.None;
        foreach (var subscription in _subscriptions.Values)
        {
            if (now < subscription.Due) continue;
            scope |= subscription.Scope;
            var periods = (now - subscription.Due) / subscription.Interval + 1;
            subscription.Due += periods * subscription.Interval;
        }
        return scope;
    }

    public TimeSpan NextDelay(long now)
    {
        long delay = int.MaxValue;
        foreach (var entry in _subscriptions.Values)
            delay = Math.Min(delay, Math.Max(0, entry.Due - now));
        return TimeSpan.FromMilliseconds(delay);
    }

    public static bool Covers(HardwareUpdateScope sampled, HardwareUpdateScope requested) =>
        (sampled & requested) == requested;
}
