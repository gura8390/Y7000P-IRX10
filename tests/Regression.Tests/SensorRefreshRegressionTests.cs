using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;

public static class SensorRefreshRegressionTests
{
    public static async Task<int> RunAsync()
    {
        int checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new Exception(name); checks++; }
        static TaskCompletionSource Signal() => new(TaskCreationOptions.RunContinuationsAsynchronously);
        static HardwareSensorSnapshot Snapshot(float value) => new HardwareSensorSnapshot().WithSample(SensorItem.CpuTemperature, value, 1);

        var firstEntered = Signal();
        var secondEntered = Signal();
        var firstRelease = Signal();
        var secondRelease = Signal();
        var values = new List<float>();
        int active = 0, maximumActive = 0;
        long firstStart = 0, secondStart = 0;
        long now = 1000;
        var refresh = new LatestSensorRefresh(async (snapshot, isCurrent) =>
        {
            var count = Interlocked.Increment(ref active);
            maximumActive = Math.Max(maximumActive, count);
            lock (values) values.Add(snapshot[SensorItem.CpuTemperature]);
            try
            {
                if (snapshot[SensorItem.CpuTemperature] == 1)
                {
                    firstStart = Volatile.Read(ref now); firstEntered.TrySetResult();
                    await firstRelease.Task;
                }
                else
                {
                    secondStart = Volatile.Read(ref now); secondEntered.TrySetResult();
                    await secondRelease.Task;
                }
            }
            finally { Interlocked.Decrement(ref active); }
        }, clock: () => Volatile.Read(ref now));
        refresh.Activate(TimeSpan.FromMilliseconds(200));
        refresh.Offer(Snapshot(1));
        await firstEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        for (int i = 2; i <= 100; i++) refresh.Offer(Snapshot(i));
        Check(active == 1 && !secondEntered.Task.IsCompleted, "slow UI reads retain single-flight ownership despite a burst of CPU updates");
        firstRelease.TrySetResult();
        Volatile.Write(ref now, 1199); refresh.Offer(Snapshot(100));
        await Task.Delay(20);
        Check(!secondEntered.Task.IsCompleted, "a CPU notification before the UI deadline cannot start the next WMI batch");
        Volatile.Write(ref now, 1200); refresh.Offer(Snapshot(100));
        await secondEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Check(values.SequenceEqual(new[] { 1f, 100f }), "pending UI refresh retains only the latest snapshot");
        Check(maximumActive == 1, "UI async refreshes never overlap");
        Check(secondStart - firstStart == 200, "UI refresh cadence follows its own monotonic deadline rather than the CPU notification rate");
        refresh.Deactivate();
        secondRelease.TrySetResult();

        var oldEntered = Signal(); var oldRelease = Signal(); var replacementEntered = Signal();
        bool oldResultCurrent = true;
        int generationActive = 0, generationMaximum = 0;
        var generations = new LatestSensorRefresh(async (snapshot, isCurrent) =>
        {
            generationMaximum = Math.Max(generationMaximum, Interlocked.Increment(ref generationActive));
            try
            {
                if (snapshot[SensorItem.CpuTemperature] == 1)
                {
                    oldEntered.TrySetResult(); await oldRelease.Task;
                    oldResultCurrent = isCurrent();
                }
                else replacementEntered.TrySetResult();
            }
            finally { Interlocked.Decrement(ref generationActive); }
        });
        generations.Activate(TimeSpan.FromMilliseconds(100)); generations.Offer(Snapshot(1));
        await oldEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        generations.Deactivate(); generations.Activate(TimeSpan.FromMilliseconds(100)); generations.Offer(Snapshot(2));
        Check(!replacementEntered.Task.IsCompleted, "hide/show cannot bypass the previous in-flight WMI batch");
        oldRelease.TrySetResult();
        await replacementEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Check(!oldResultCurrent && generationMaximum == 1, "late results from a previous visibility generation cannot update the new UI");
        generations.Deactivate();

        var errorObserved = Signal(); var recovered = Signal();
        var errors = new LatestSensorRefresh((snapshot, _) =>
        {
            if (snapshot[SensorItem.CpuTemperature] == 1) throw new InvalidOperationException("fixture failure");
            recovered.TrySetResult(); return Task.CompletedTask;
        }, _ => { errorObserved.TrySetResult(); throw new InvalidOperationException("diagnostic fixture failure"); });
        errors.Activate(TimeSpan.FromMilliseconds(100)); errors.Offer(Snapshot(1));
        await errorObserved.Task.WaitAsync(TimeSpan.FromSeconds(5)); errors.Offer(Snapshot(2));
        await recovered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        Check(recovered.Task.IsCompleted, "callback and diagnostic failures do not stop future UI refreshes");
        errors.Deactivate();

        var monitor = new PowerMonitor();
        int resets = 0; monitor.ResetNeeded += () => resets++;
        Check(Enumerable.Range(0, 1000).All(_ => monitor.Read(50) == 50) && resets == 0,
            "stable legal 50W readings do not repeatedly reset all hardware");
        Check(monitor.Read(float.NaN) == -1 && monitor.Read(float.PositiveInfinity) == -1 && resets == 0,
            "non-finite power readings invalidate telemetry without an equality-based reset loop");
        Check(monitor.Read(0) == -1 && monitor.Read(-1) == -1, "missing power telemetry is unavailable");
        Check(monitor.Read(401) == -1 && resets == 1, "explicit impossible finite driver values retain reset notification");
        monitor.Reset();
        Check(monitor.Read(50) == 50, "a reset does not discard subsequent valid steady power");
        return checks;
    }
}
