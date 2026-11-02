using System;
using System.Collections.Generic;

namespace LenovoLegionToolkit.Lib;

public sealed record HardwareSensorSnapshot
{
    private static readonly int ItemCount = Enum.GetValues<SensorItem>().Length;
    private readonly float[] _values;

    private readonly long[] _sampleTicks;

    public long GetSampleTick(SensorItem item) => _sampleTicks[(int)item];

    public bool IsFresh(SensorItem item, long now, long maxAgeMilliseconds) =>
        float.IsFinite(this[item]) && this[item] >= 0 && maxAgeMilliseconds >= 0
        && now >= GetSampleTick(item) && now - GetSampleTick(item) <= maxAgeMilliseconds;

    public HardwareSensorSnapshot WithSample(SensorItem item, float value, long tick)
    {
        var copy = WithValue(item, value);
        copy._sampleTicks[(int)item] = tick;
        return copy;
    }

    public HardwareSensorSnapshot()
    {
        _values = new float[ItemCount];
        _sampleTicks = new long[ItemCount];
        for (int i = 0; i < _values.Length; i++)
            _values[i] = -1;
    }

    private HardwareSensorSnapshot(float[] values, long[] ticks)
    {
        _values = values;
        _sampleTicks = ticks;
    }

    private HardwareSensorSnapshot(HardwareSensorSnapshot source)
    {
        _values = (float[])source._values.Clone();
        _sampleTicks = (long[])source._sampleTicks.Clone();
    }

    private HardwareSensorSnapshot Copy() => new((float[])_values.Clone(), (long[])_sampleTicks.Clone());

    public HardwareSensorSnapshot WithSamples(IReadOnlyDictionary<SensorItem, float> values, long tick)
    {
        var copy = Copy();
        foreach (var (item, value) in values)
        {
            copy._values[(int)item] = value;
            copy._sampleTicks[(int)item] = tick;
        }
        return copy;
    }

    public float this[SensorItem item]
    {
        get => _values[(int)item];
        init => _values[(int)item] = value;
    }

    public float GetValue(SensorItem item) => _values[(int)item];

    public HardwareSensorSnapshot WithValue(SensorItem item, float value)
    {
        var copy = Copy();
        copy._values[(int)item] = value;
        return copy;
    }

    public HardwareSensorSnapshot WithValues(IReadOnlyDictionary<SensorItem, float> values)
    {
        var copy = Copy();
        foreach (var (item, val) in values)
            copy._values[(int)item] = val;
        return copy;
    }
}
