using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Thermal;

namespace LenovoLegionToolkit.Lib.Controllers;

/// <summary>One in-process owner for the three CPU fields used by active thermal control.</summary>
public static class CpuThermalWriteCoordinator
{
    public const uint LongPowerId = 0x01020000;
    public const uint ShortPowerId = 0x01010000;
    public const uint TemperatureId = 0x01040000;

    private static readonly SemaphoreSlim Gate = new(1, 1);
    private static Lease? _owner;

    public sealed class Lease
    {
        internal CpuPowerEnvelope Ceiling;
        internal int TemperatureCeiling;
        internal readonly HashSet<uint> UserChangedFields = [];
        private int _superseded;
        public bool IsSuperseded => Volatile.Read(ref _superseded) != 0;
        internal void MarkUserChanged(uint field)
        {
            UserChangedFields.Add(field);
            Volatile.Write(ref _superseded, 1);
        }
        internal Lease(CpuPowerEnvelope ceiling, int temperatureCeiling)
        {
            Ceiling = ceiling;
            TemperatureCeiling = temperatureCeiling;
        }
    }

    public static async Task<Lease> AcquireAsync(CpuPowerEnvelope ceiling, int temperatureCeiling)
    {
        Validate(ceiling, temperatureCeiling);
        await Gate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (_owner is not null)
                throw new InvalidOperationException("Another CPU thermal controller already owns the firmware fields.");
            return _owner = new Lease(ceiling, temperatureCeiling);
        }
        finally { Gate.Release(); }
    }

    // The operation uses the raw firmware adapter, avoiding recursive acquisition through the public WMI API.
    internal static async Task<T> ExecuteOwnerAsync<T>(Lease lease, CpuPowerEnvelope ceiling,
        int temperatureCeiling, Func<Task<T>> operation)
    {
        Validate(ceiling, temperatureCeiling);
        await Gate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (!ReferenceEquals(_owner, lease))
                throw new InvalidOperationException("CPU thermal ownership has ended.");
            if (lease.IsSuperseded)
                throw new InvalidOperationException("CPU thermal control was superseded by a user/automation CPU setting change. Discover a new baseline before resuming.");
            lease.Ceiling = ceiling;
            lease.TemperatureCeiling = temperatureCeiling;
            return await operation().ConfigureAwait(false);
        }
        finally { Gate.Release(); }
    }

    internal static async Task ReleaseAsync(Lease lease, Func<Task>? restore = null)
    {
        await Gate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (!ReferenceEquals(_owner, lease)) return;
            try
            {
                if (restore is not null) await restore().ConfigureAwait(false);
            }
            finally { _owner = null; }
        }
        finally { Gate.Release(); }
    }

    /// <summary>Legacy UI, automation and CLI setters pass through this same gate.</summary>
    public static async Task WriteAsync(uint id, int requested, Func<uint, int, Task> write,
        Func<uint, Task<int>>? read = null)
    {
        // The byte at 8..15 selects the mode in capability discovery, not in the active-value API.
        var field = id & 0xFFFF00FF;
        if ((field & 0xFF000000) != 0x01000000)
        {
            await write(id, requested).ConfigureAwait(false);
            return;
        }

        await Gate.WaitAsync().ConfigureAwait(false);
        try
        {
            RejectUserWriteWhileOwned();
            // Never silently substitute a thermal ceiling for the requested user budget.
            // The caller can stop control, apply the requested value, then acquire a fresh baseline.
            await write(id, requested).ConfigureAwait(false);
        }
        finally { Gate.Release(); }
    }

    internal static async Task ExecuteUserCpuWriteAsync(Func<Task> write)
    {
        await Gate.WaitAsync().ConfigureAwait(false);
        try
        {
            RejectUserWriteWhileOwned();
            await write().ConfigureAwait(false);
        }
        finally { Gate.Release(); }
    }

    private static void RejectUserWriteWhileOwned()
    {
        if (_owner is not null)
            throw new InvalidOperationException("CPU thermal control currently owns CPU settings. Stop CPU thermal control before changing a CPU power, temperature, or overclocking setting, apply the new setting, then enable control again to verify the new baseline. No CPU setting was changed.");
    }

    private static void Validate(CpuPowerEnvelope ceiling, int temperature)
    {
        if (!double.IsFinite(ceiling.LongWatts) || !double.IsFinite(ceiling.ShortWatts)
            || ceiling.LongWatts <= 0 || ceiling.LongWatts > ceiling.ShortWatts
            || ceiling.ShortWatts > int.MaxValue || ceiling.LongWatts != Math.Truncate(ceiling.LongWatts)
            || ceiling.ShortWatts != Math.Truncate(ceiling.ShortWatts) || temperature <= 0 || temperature > 90)
            throw new ArgumentOutOfRangeException(nameof(ceiling), "Thermal ceilings must be positive integral firmware values, with temperature at most 90°C.");
    }
}
