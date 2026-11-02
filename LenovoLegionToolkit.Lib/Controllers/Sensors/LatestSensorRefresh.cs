using System;
using System.Threading;
using System.Threading.Tasks;

namespace LenovoLegionToolkit.Lib.Controllers.Sensors;

// One consumer per UI subscriber, even across hide/show or interval changes. Slow
// asynchronous reads keep their ownership while later snapshots replace one pending value.
public sealed class LatestSensorRefresh(Func<HardwareSensorSnapshot, Func<bool>, Task> refresh,
    Action<Exception>? onError = null, Func<long>? clock = null)
{
    private long Now => clock?.Invoke() ?? Environment.TickCount64;
    private readonly object _gate = new();
    private readonly SemaphoreSlim _changed = new(0, 1);
    private bool _enabled;
    private bool _running;
    private bool _pending;
    private long _generation;
    private long _intervalMilliseconds;
    private long _nextRefresh;
    private HardwareSensorSnapshot? _latest;

    // Saved JSON can contain a non-finite or otherwise unusable interval. Callers
    // retain their model's default without rewriting the user's settings file.
    public static TimeSpan IntervalOrDefault(double seconds, TimeSpan defaultInterval)
    {
        if (defaultInterval <= TimeSpan.Zero || defaultInterval.TotalMilliseconds > int.MaxValue)
            throw new ArgumentOutOfRangeException(nameof(defaultInterval));
        if (!double.IsFinite(seconds) || seconds <= 0 || seconds > int.MaxValue / 1000d)
            return defaultInterval;
        var interval = TimeSpan.FromSeconds(seconds);
        return interval > TimeSpan.Zero && interval.TotalMilliseconds <= int.MaxValue ? interval : defaultInterval;
    }

    public void Activate(TimeSpan interval)
    {
        if (interval <= TimeSpan.Zero || interval.TotalMilliseconds > int.MaxValue)
            throw new ArgumentOutOfRangeException(nameof(interval));
        lock (_gate)
        {
            _enabled = true;
            _generation++;
            _pending = false;
            _latest = null;
            _intervalMilliseconds = Math.Max(100, (long)Math.Ceiling(interval.TotalMilliseconds));
            _nextRefresh = Now;
            SignalChanged();
        }
    }

    public void Deactivate()
    {
        lock (_gate)
        {
            _enabled = false;
            _pending = false;
            _latest = null;
            _generation++;
            SignalChanged();
        }
    }

    public void Offer(HardwareSensorSnapshot snapshot)
    {
        lock (_gate)
        {
            if (!_enabled) return;
            _latest = snapshot;
            _pending = true;
            SignalChanged();
            if (_running) return;
            _running = true;
            _ = Task.Run(RunAsync);
        }
    }

    private void SignalChanged()
    {
        if (_changed.CurrentCount == 0) _changed.Release();
    }

    private bool IsCurrent(long generation)
    {
        lock (_gate) return _enabled && generation == _generation;
    }

    private async Task RunAsync()
    {
        while (true)
        {
            HardwareSensorSnapshot snapshot;
            long generation;
            long wait;
            lock (_gate)
            {
                if (!_enabled || !_pending)
                {
                    _running = false;
                    return;
                }
                wait = Math.Max(0, _nextRefresh - Now);
                snapshot = _latest!;
                generation = _generation;
                if (wait == 0)
                {
                    _pending = false;
                    _nextRefresh = Now + _intervalMilliseconds;
                }
            }
            if (wait != 0)
            {
                await _changed.WaitAsync(TimeSpan.FromMilliseconds(wait)).ConfigureAwait(false);
                continue;
            }
            try { await refresh(snapshot, () => IsCurrent(generation)).ConfigureAwait(false); }
            catch (Exception ex)
            {
                try { onError?.Invoke(ex); } catch { /* Diagnostic failures cannot stop future refreshes. */ }
            }
        }
    }
}
