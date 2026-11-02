using System;
using System.Collections.Concurrent;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers.Sensors;

public static class UiRefreshStressRegressionTests
{
    public static async Task<int> RunAsync()
    {
        var checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new Exception(name); checks++; }
        static TaskCompletionSource Signal() => new(TaskCreationOptions.RunContinuationsAsynchronously);
        static HardwareSensorSnapshot Snapshot(float value) => new HardwareSensorSnapshot().WithSample(SensorItem.CpuTemperature, value, 1);

        // All refresh work is a blocked synthetic delegate. No WPF window, provider,
        // WMI transport or physical sensor is constructed by this pressure test.
        for (var repetition = 0; repetition < 12; repetition++)
        {
            var firstEntered = Signal();
            var firstRelease = Signal();
            var firstFinished = Signal();
            var finalEntered = Signal();
            var finalRelease = Signal();
            var finalFinished = Signal();
            var seen = new ConcurrentQueue<float>();
            var active = 0;
            var maximumActive = 0;
            var oldIsCurrent = true;
            long now = 0;
            var refresh = new LatestSensorRefresh(async (snapshot, isCurrent) =>
            {
                var current = Interlocked.Increment(ref active);
                int observed;
                do { observed = Volatile.Read(ref maximumActive); }
                while (current > observed && Interlocked.CompareExchange(ref maximumActive, current, observed) != observed);
                var value = snapshot[SensorItem.CpuTemperature];
                seen.Enqueue(value);
                try
                {
                    if (value == 1)
                    {
                        firstEntered.TrySetResult();
                        await firstRelease.Task;
                        oldIsCurrent = isCurrent();
                    }
                    else
                    {
                        finalEntered.TrySetResult();
                        await finalRelease.Task;
                    }
                }
                finally
                {
                    Interlocked.Decrement(ref active);
                    if (value == 1) firstFinished.TrySetResult(); else finalFinished.TrySetResult();
                }
            }, clock: () => Volatile.Read(ref now));
            refresh.Offer(Snapshot(-1)); // Disabled instances must not start a worker.
            refresh.Activate(TimeSpan.FromMilliseconds(100));
            refresh.Offer(Snapshot(1));
            await firstEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
            await Task.WhenAll(Enumerable.Range(0, 8).Select(worker => Task.Run(() =>
            {
                for (var iteration = 0; iteration < 150; iteration++)
                {
                    if (iteration % 3 == 0) refresh.Deactivate();
                    refresh.Activate(TimeSpan.FromMilliseconds(100 + worker));
                    refresh.Offer(Snapshot(worker * 1000 + iteration + 2));
                }
            })));
            refresh.Deactivate();
            refresh.Activate(TimeSpan.FromMilliseconds(100));
            refresh.Offer(Snapshot(99999));
            Check(active == 1 && !finalEntered.Task.IsCompleted,
                "Concurrent hide/show and interval changes retain the blocked callback's ownership");
            firstRelease.TrySetResult();
            await firstFinished.Task.WaitAsync(TimeSpan.FromSeconds(5));
            await finalEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
            Check(!oldIsCurrent && maximumActive == 1 && seen.SequenceEqual(new[] { 1f, 99999f }),
                "All intermediate generations coalesce into one latest refresh without admitting stale results");
            refresh.Deactivate();
            refresh.Offer(Snapshot(100000));
            finalRelease.TrySetResult();
            await finalFinished.Task.WaitAsync(TimeSpan.FromSeconds(5));
            Check(active == 0 && seen.Count == 2,
                "Final deactivation drains the callback and discards disabled offers");
        }

        foreach (var invalid in new[] { TimeSpan.Zero, TimeSpan.FromTicks(-1), TimeSpan.FromMilliseconds((double)int.MaxValue + 1) })
        {
            var invalidRefresh = new LatestSensorRefresh((_, _) => Task.CompletedTask);
            var rejected = false;
            try { invalidRefresh.Activate(invalid); } catch (ArgumentOutOfRangeException) { rejected = true; }
            Check(rejected, "The production refresh gate rejects zero, negative and overflow intervals");
        }

        // Clock-controlled cadence demonstrates minimum 100 ms coalescing even
        // when a caller supplies a smaller positive interval.
        long clock = 1000;
        var began = Signal(); var completed = Signal(); var last = Signal();
        var firstGate = Signal();
        var count = 0;
        var minimumInterval = new LatestSensorRefresh(async (_, _) =>
        {
            if (Interlocked.Increment(ref count) == 1)
            {
                began.TrySetResult(); await firstGate.Task; completed.TrySetResult();
            }
            else last.TrySetResult();
        }, clock: () => Volatile.Read(ref clock));
        minimumInterval.Activate(TimeSpan.FromTicks(1));
        minimumInterval.Offer(Snapshot(1));
        await began.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Volatile.Write(ref clock, 1099);
        minimumInterval.Offer(Snapshot(2));
        firstGate.TrySetResult();
        await completed.Task.WaitAsync(TimeSpan.FromSeconds(5));
        await Task.Delay(20);
        Check(count == 1, "A submillisecond interval cannot bypass the production 100 ms floor");
        Volatile.Write(ref clock, 1100);
        minimumInterval.Offer(Snapshot(3));
        await last.Task.WaitAsync(TimeSpan.FromSeconds(5));
        minimumInterval.Deactivate();
        Check(count == 2, "The pending latest snapshot resumes exactly when the minimum interval becomes due");
        Console.WriteLine($"PASS: {checks} production UI-refresh pressure checks (12 repetitions; 14,400 concurrent lifecycle/offer cycles; fake clock and callbacks only)");
        return checks;
    }
}
