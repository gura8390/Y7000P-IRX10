using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Reflection;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Automation;
using LenovoLegionToolkit.Lib.Automation.Pipeline;
using LenovoLegionToolkit.Lib.Automation.Steps;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;
using LibreHardwareMonitor.Hardware;

using var isolatedTestHost = RegressionTestHost.CreateIsolated();
var checks = 0;
void Check(bool condition, string name)
{
    if (!condition) throw new Exception(name);
    checks++;
}

ISensor Sensor(string name, SensorType type, float? value)
{
    var proxy = DispatchProxy.Create<ISensor, PropertyProxy>();
    var state = (PropertyProxy)(object)proxy;
    state.Properties["Name"] = name;
    state.Properties["SensorType"] = type;
    state.Properties["Value"] = value;
    return proxy;
}

IHardware Hardware(string name, HardwareType type, params ISensor[] sensors)
{
    var proxy = DispatchProxy.Create<IHardware, PropertyProxy>();
    var state = (PropertyProxy)(object)proxy;
    state.Properties["Name"] = name;
    state.Properties["HardwareType"] = type;
    state.Properties["Sensors"] = sensors;
    return proxy;
}

var cpu = new CpuSensorProvider();
cpu.Discover([Hardware("CPU", HardwareType.Cpu,
    Sensor("CPU Package", SensorType.Temperature, 85),
    Sensor("Core Max", SensorType.Temperature, 88),
    Sensor("CPU Total", SensorType.Load, 60),
    Sensor("CPU Package", SensorType.Power, 50),
    Sensor("Core #1", SensorType.Voltage, 1.1f),
    Sensor("P-Core #1", SensorType.Clock, 4000),
    Sensor("E-Core #1", SensorType.Clock, 3000))]);
cpu.VoltageMode = CpuVoltageMode.Core;
cpu.VoltageCoreIndex = -1;
cpu.Read();
Check(cpu.Values[SensorItem.CpuVoltage] == -1, "negative voltage core index returns unavailable without aborting sampling");
Check(cpu.Values[SensorItem.CpuTemperature] == 85, "invalid voltage index does not discard temperature");
cpu.VoltageCoreIndex = 0;
cpu.Read();
Check(cpu.Values[SensorItem.CpuVoltage] == 1.1f, "valid voltage index still reads selected core");
Check(cpu.Values.Keys.All(cpu.ProvidedSensorItems.Contains), "CPU ownership includes every published OSD value");
Check(cpu.ProvidedSensorItems.Contains(SensorItem.CpuPMaxFrequency) && cpu.ProvidedSensorItems.Contains(SensorItem.CpuEAverageFrequency), "hybrid clock metrics have explicit ownership");
cpu.Discover([]);
Check(!cpu.IsAvailable && !cpu.IsHybrid && cpu.AvailableCoreCount == 0 && cpu.Values.Count == 0, "CPU rediscovery clears previous hardware and cached values");
cpu.Read();
Check(cpu.Values[SensorItem.CpuVoltage] == -1 && cpu.PackageTemperature is null && cpu.CoreMaxTemperature is null, "removed CPU cannot expose old voltage or thermal telemetry");

var memory = new MemorySensorProvider();
var usedMemory = Sensor("Memory Used", SensorType.Data, 4);
var availableMemory = Sensor("Memory Available", SensorType.Data, 12);
memory.Discover([Hardware("Total Memory", HardwareType.Memory,
    usedMemory, availableMemory)]);
memory.Read();
Check(memory.IsAvailable && memory.Values[SensorItem.MemoryUtilization] == 25, "memory fixture exercises production read logic");
((PropertyProxy)(object)availableMemory).Properties["Value"] = null;
((PropertyProxy)(object)usedMemory).Properties["Value"] = 2f;
memory.Read();
Check(memory.Values[SensorItem.MemoryTotal] == 16 && memory.Values[SensorItem.MemoryUtilization] == 12.5f,
    "transient missing available memory uses verified capacity rather than reporting 100 percent");
memory.Discover([]);
Check(!memory.IsAvailable && memory.Values.Count == 0, "memory rediscovery invalidates old samples");

var storage = new StorageSensorProvider();
storage.Discover([Hardware("Disk", HardwareType.Storage, Sensor("Temperature", SensorType.Temperature, 40))]);
storage.Read();
Check(storage.Values[SensorItem.Disk1Temperature] == 40, "storage fixture exercises production read logic");
storage.Discover([]);
Check(!storage.IsAvailable && storage.Values.Count == 0, "storage rediscovery invalidates old samples");

ISensorProvider[] providers = [cpu, new GpuSensorProvider(), memory, storage];
foreach (var firmwareItem in new[] { SensorItem.CpuFanSpeed, SensorItem.GpuFanSpeed, SensorItem.PchFanSpeed,
    SensorItem.PchTemperature, SensorItem.BatteryState, SensorItem.BatteryLevel })
{
    var scope = SensorSubscriptionScope.FromSensorItems([firmwareItem], providers);
    Check(scope == HardwareUpdateScope.Fans, $"firmware-only {firmwareItem} retains the dashboard refresh heartbeat");
}
Check(SensorSubscriptionScope.FromSensorItems([SensorItem.CpuUtilization, SensorItem.BatteryLevel], providers)
    == (HardwareUpdateScope.Cpu | HardwareUpdateScope.Fans), "hardware and firmware dependencies combine without polling unrelated GPU/storage");
Check(SensorSubscriptionScope.FromSensorItems([SensorItem.GpuTemperatures], providers) == HardwareUpdateScope.Gpu,
    "display temperature group expands its GPU sampling dependency");
Check(!providers.Any(provider => provider.ProvidedSensorItems.Contains(SensorItem.GpuTemperatures)),
    "display group is not introduced as a fabricated snapshot metric");
Check(SensorSubscriptionScope.FromSensorItems([], providers) == HardwareUpdateScope.None, "empty dashboard still has no subscription");
var firmwareScheduler = new SensorUpdateScheduler();
firmwareScheduler.Set(new object(), TimeSpan.FromSeconds(1), HardwareUpdateScope.Fans, 0);
Check(firmwareScheduler.TakeDue(0) == HardwareUpdateScope.Fans
    && firmwareScheduler.TakeDue(999) == HardwareUpdateScope.None
    && firmwareScheduler.TakeDue(1000) == HardwareUpdateScope.Fans, "firmware-only heartbeat obeys the same independent subscription deadline");

var runMethod = typeof(AutomationPipeline).GetMethod("RunAsync", BindingFlags.Instance | BindingFlags.NonPublic)!;
Task Run(AutomationPipeline root, params AutomationPipeline[] others) =>
    (Task)runMethod.Invoke(root, [others.ToList(), CancellationToken.None])!;
async Task Reject(AutomationPipeline root, string name, params AutomationPipeline[] others)
{
    try { await Run(root, others); }
    catch (InvalidOperationException)
    {
        Check(true, name);
        return;
    }
    throw new Exception(name);
}

var counter = new CounterStep();
var a = new AutomationPipeline("A");
var b = new AutomationPipeline("B");
a.Steps.Add(counter);
a.Steps.Add(new QuickActionAutomationStep(b.Id));
b.Steps.Add(new QuickActionAutomationStep(a.Id));
await Reject(a, "root cycle is detected even when caller excludes root", b);
Check(counter.Count == 0, "root cycle is rejected before any step executes");

var direct = new AutomationPipeline("direct");
direct.Steps.Add(new QuickActionAutomationStep(direct.Id));
await Reject(direct, "direct self-reference is rejected");
Check(direct.GetValidationWarnings().Single().Contains("cycle", StringComparison.OrdinalIgnoreCase), "validation reports cycle as a warning");

var c = new AutomationPipeline("C");
b.Steps.Clear();
b.Steps.Add(new QuickActionAutomationStep(c.Id));
c.Steps.Add(new QuickActionAutomationStep(b.Id));
await Reject(a, "non-root cycle is detected", b, c);
Check(counter.Count == 0, "non-root cycle is rejected before any step executes");

var leaf = new AutomationPipeline("leaf");
leaf.Steps.Add(counter);
var reuse = new AutomationPipeline("reuse");
reuse.Steps.Add(new QuickActionAutomationStep(leaf.Id));
reuse.Steps.Add(new QuickActionAutomationStep(leaf.Id));
await Run(reuse, leaf);
Check(counter.Count == 2, "DAG may reuse a quick action after leaving its active path");

// An iterative expansion must handle deep valid chains without recursive stack overflow.
var chain = Enumerable.Range(0, 2000).Select(i => new AutomationPipeline($"chain-{i}")).ToArray();
for (var i = 0; i < chain.Length - 1; i++) chain[i].Steps.Add(new QuickActionAutomationStep(chain[i + 1].Id));
chain[^1].Steps.Add(counter);
await Run(chain[0], chain.Skip(1).ToArray());
Check(counter.Count == 3, "deep acyclic references execute without recursion");

var excessive = new AutomationPipeline("excessive");
for (var i = 0; i < 10001; i++) excessive.Steps.Add(counter);
await Reject(excessive, "excessive expansion is rejected before execution");
Check(counter.Count == 3, "expansion budget fails before side effects");

var original = new HardwareSensorSnapshot().WithSample(SensorItem.CpuTemperature, 85, 100);
var cloned = original with { };
Check(!ReferenceEquals(original, cloned) && cloned[SensorItem.CpuTemperature] == 85
    && cloned.GetSampleTick(SensorItem.CpuTemperature) == 100, "record copy preserves values and acquisition timestamp");
var changed = cloned.WithSample(SensorItem.CpuTemperature, 95, 200);
Check(original[SensorItem.CpuTemperature] == 85 && original.GetSampleTick(SensorItem.CpuTemperature) == 100
    && cloned[SensorItem.CpuTemperature] == 85 && cloned.GetSampleTick(SensorItem.CpuTemperature) == 100
    && changed[SensorItem.CpuTemperature] == 95 && changed.GetSampleTick(SensorItem.CpuTemperature) == 200,
    "updating a record copy through the supported API preserves its source value and timestamp");
var slowMerge = original.WithSamples(new Dictionary<SensorItem, float> { [SensorItem.CpuTemperature] = 85 }, 100);
Check(!slowMerge.IsFresh(SensorItem.CpuTemperature, 3101, 2000),
    "late merge cannot freshen a conservatively timestamped acquisition");

var scheduler = new SensorUpdateScheduler();
scheduler.Set(new object(), TimeSpan.FromMilliseconds(100), HardwareUpdateScope.Cpu, 0);
scheduler.Set(new object(), TimeSpan.FromSeconds(60), HardwareUpdateScope.Storage, 0);
var cpuUpdates = 0;
var storageUpdates = 0;
for (var time = 0; time <= 60000; time += 100)
{
    var due = scheduler.TakeDue(time);
    if ((due & HardwareUpdateScope.Cpu) != 0) cpuUpdates++;
    if ((due & HardwareUpdateScope.Storage) != 0) storageUpdates++;
}
Check(cpuUpdates == 601 && storageUpdates == 2, "production scheduler honors independent CPU/storage deadlines");
Console.WriteLine($"SCHEDULER: simulated 0..60000ms, CPU100ms={cpuUpdates}, storage60s={storageUpdates}; former union/min-interval storage=601");

var allSamples = Enum.GetValues<SensorItem>().ToDictionary(item => item, item => (float)(int)item + 1);
Check(allSamples.Count == 32, "snapshot allocation benchmark covers the complete production enum (32 metrics)");
var empty = new HardwareSensorSnapshot();
HardwareSensorSnapshot PerMetric()
{
    var snapshot = empty;
    foreach (var (item, value) in allSamples) snapshot = snapshot.WithSample(item, value, 100);
    return snapshot;
}
HardwareSensorSnapshot Batch() => empty.WithSamples(allSamples, 100);
var metricSnapshot = PerMetric();
var batchSnapshot = Batch();
Check(allSamples.Keys.All(item => metricSnapshot[item] == batchSnapshot[item] && metricSnapshot.GetSampleTick(item) == batchSnapshot.GetSampleTick(item)),
    "batch and per-metric updates produce identical values and timestamps for all 32 metrics");

const int benchmarkIterations = 10000;
(long Allocated, double Milliseconds) Measure(Func<HardwareSensorSnapshot> sample)
{
    for (var i = 0; i < 256; i++) GC.KeepAlive(sample());
    var allocatedBefore = GC.GetAllocatedBytesForCurrentThread();
    var started = Stopwatch.GetTimestamp();
    for (var i = 0; i < benchmarkIterations; i++) GC.KeepAlive(sample());
    return (GC.GetAllocatedBytesForCurrentThread() - allocatedBefore, Stopwatch.GetElapsedTime(started).TotalMilliseconds);
}
var perMetricMeasurement = Measure(PerMetric);
var batchMeasurement = Measure(Batch);
Check(batchMeasurement.Allocated < perMetricMeasurement.Allocated, "batch update allocates less than repeated per-metric immutable copies");
Console.WriteLine($"SNAPSHOT BENCHMARK: {benchmarkIterations} updates x {allSamples.Count} production metrics (microbenchmark; not hardware/system acceptance)");
Console.WriteLine($"  per-metric: {perMetricMeasurement.Allocated / benchmarkIterations:N0} bytes/update, {perMetricMeasurement.Milliseconds:F2} ms total");
Console.WriteLine($"  batch:      {batchMeasurement.Allocated / benchmarkIterations:N0} bytes/update, {batchMeasurement.Milliseconds:F2} ms total");
Console.WriteLine($"  allocation reduction: {(1d - (double)batchMeasurement.Allocated / perMetricMeasurement.Allocated) * 100:F2}%; elapsed time is observational, no timing threshold asserted");

checks += await SensorRefreshRegressionTests.RunAsync();
checks += AppFlagRegressionTests.Run();
checks += ConfigurationRegressionTests.Run();
checks += LegacySettingsRegressionTests.Run();
checks += await AtomicSettingsRegressionTests.RunAsync();
checks += await UiRefreshStressRegressionTests.RunAsync();
checks += IoCInitializationRegressionTests.Run();
Console.WriteLine($"PASS: {checks} production provider/automation regression checks (fake sensors, no hardware I/O)");

public class PropertyProxy : DispatchProxy
{
    public Dictionary<string, object?> Properties { get; } = [];
    protected override object? Invoke(MethodInfo? targetMethod, object?[]? args)
    {
        if (targetMethod?.Name.StartsWith("get_", StringComparison.Ordinal) == true && Properties.TryGetValue(targetMethod.Name[4..], out var value))
            return value;
        throw new InvalidOperationException($"Unexpected fake sensor call: {targetMethod?.Name}");
    }
}

sealed class CounterStep : IAutomationStep
{
    public int Count { get; private set; }
    public Task<bool> IsSupportedAsync() => Task.FromResult(true);
    public Task RunAsync(AutomationContext context, AutomationEnvironment environment, CancellationToken token)
    {
        Count++;
        return Task.CompletedTask;
    }
    public IAutomationStep DeepCopy() => new CounterStep();
}
