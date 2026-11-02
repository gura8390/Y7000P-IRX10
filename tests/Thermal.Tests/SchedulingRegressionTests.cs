using System;
using System.Collections.Generic;
using System.Diagnostics;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers.Sensors;

public static class SchedulingRegressionTests
{
    public static int Run()
    {
        int checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new Exception(name); checks++; }
        var planner = new SensorUpdateScheduler();
        var cpu = new object();
        var disk = new object();
        planner.Set(cpu, TimeSpan.FromMilliseconds(100), HardwareUpdateScope.Cpu, 0);
        planner.Set(disk, TimeSpan.FromSeconds(60), HardwareUpdateScope.Storage, 0);
        Check(planner.TakeDue(0) == (HardwareUpdateScope.Cpu | HardwareUpdateScope.Storage), "Initial subscriptions due");
        Check(planner.TakeDue(99) == HardwareUpdateScope.None, "No premature sample");
        Check(planner.NextDelay(35).TotalMilliseconds == 65, "I/O time does not shift deadline");
        int diskSamples = 1;
        for (int time = 100; time <= 60000; time += 100)
            if ((planner.TakeDue(time) & HardwareUpdateScope.Storage) != 0) diskSamples++;
        Check(diskSamples == 2, "Slow storage is independent of fast CPU (2 instead of 601)");
        Check(planner.TakeDue(180015) == (HardwareUpdateScope.Cpu | HardwareUpdateScope.Storage), "Resume coalesces missed periods");
        Check(planner.TakeDue(180015) == HardwareUpdateScope.None, "No catch-up storm");
        Check(planner.NextDelay(180015).TotalMilliseconds == 85, "Original cadence maintained");
        planner.Set(cpu, TimeSpan.FromSeconds(2), HardwareUpdateScope.Memory, 180015);
        Check(planner.TakeDue(180015) == HardwareUpdateScope.Memory, "Subscription can replace interval and scope");
        Check(planner.Remove(cpu) && !planner.Remove(cpu), "Idempotent stop");
        planner.Clear();
        Check(planner.Count == 0, "All subscriptions removed");
        Check(!SensorUpdateScheduler.Covers(HardwareUpdateScope.Cpu, HardwareUpdateScope.All), "CPU cannot satisfy All");
        Check(SensorUpdateScheduler.Covers(HardwareUpdateScope.All, HardwareUpdateScope.Cpu), "All covers CPU");
        foreach (var invalid in new[] { TimeSpan.Zero, TimeSpan.FromDays(30) })
        {
            try { planner.Set(cpu, invalid, HardwareUpdateScope.Cpu, 0); throw new Exception("Invalid interval accepted"); }
            catch (ArgumentOutOfRangeException) { checks++; }
        }
        var source = new HardwareSensorSnapshot().WithSample(SensorItem.CpuTemperature, 75, 10);
        var values = new Dictionary<SensorItem, float> { [SensorItem.MemoryUtilization] = 40, [SensorItem.CpuPower] = 55 };
        var result = source.WithSamples(values, 100);
        Check(result.GetSampleTick(SensorItem.CpuTemperature) == 10, "Batch preserves unrelated freshness");
        Check(result.GetSampleTick(SensorItem.CpuPower) == 100 && result[SensorItem.CpuPower] == 55, "Batch records tick and value");
        Check(source[SensorItem.CpuPower] == -1, "Batch keeps source immutable");
        Benchmark();
        return checks;
    }

    private static void Benchmark()
    {
        // Identical values and snapshots, comparing the old per-metric merge with a batch.
        var values = new Dictionary<SensorItem, float>();
        foreach (var item in Enum.GetValues<SensorItem>()) values[item] = 50;
        const int iterations = 30000;
        HardwareSensorSnapshot snapshot = new();
        void Individual() { for (int i = 0; i < iterations; i++) foreach (var pair in values) snapshot = snapshot.WithSample(pair.Key, pair.Value, i); }
        void Batch() { for (int i = 0; i < iterations; i++) snapshot = snapshot.WithSamples(values, i); }
        Individual(); Batch();
        (long bytes, double ms) Measure(Action run)
        {
            var bytes = GC.GetAllocatedBytesForCurrentThread();
            var timer = Stopwatch.StartNew(); run(); timer.Stop();
            return (GC.GetAllocatedBytesForCurrentThread() - bytes, timer.Elapsed.TotalMilliseconds);
        }
        var before = Measure(Individual);
        var after = Measure(Batch);
        if (after.bytes >= before.bytes) throw new Exception("Batch merge did not reduce allocations");
        Console.WriteLine($"BENCH snapshot ({values.Count} metrics, {iterations} iterations): individual={before.bytes}B/{before.ms:F2}ms; batch={after.bytes}B/{after.ms:F2}ms; allocation reduction={1d - (double)after.bytes / before.bytes:P1}");
        GC.KeepAlive(snapshot);
    }
}
