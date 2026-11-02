using System;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.Thermal;

var checks = 0;
void Check(bool condition, string name)
{
    if (!condition) throw new Exception(name);
    checks++;
}
async Task Expect<T>(Task task, string name) where T : Exception
{
    try { await task.WaitAsync(TimeSpan.FromSeconds(3)); }
    catch (T) { Check(true, name); return; }
    throw new Exception(name);
}

if (args.Length > 0 && args[0] == "--counterexamples")
{
    await DetailedLifecycleScenarios.RunAsync(Check, counterexamplesOnly: true);
    Console.WriteLine($"PASS: {checks} lifecycle counterexample checks; fake host only, no hardware I/O");
    return;
}

// Cancellation ends the caller's wait, but never abandons an in-flight physical enable.
{
    var transport = new ThermalCpuTransport { BlockEnable = true };
    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
    var sensors = new SensorsGroupController();
    var controller = new CpuThermalController(sensors, new ApplicationSettings());
    using var cancellation = new CancellationTokenSource();
    var start = controller.StartAsync(cancellation.Token);
    await transport.EnableEntered.Task.WaitAsync(TimeSpan.FromSeconds(3));
    cancellation.Cancel();
    Check(!start.IsCompleted && transport.OwnsFirmware, "cancelled startup retains ownership until physical enable settles");
    transport.EnableRelease.TrySetResult();
    await Expect<OperationCanceledException>(start, "cancelled startup reports cancellation after cleanup");
    Check(transport.RestoreCalls == 1 && !transport.OwnsFirmware, "cancelled late enable is restored exactly once");
    Check(!controller.IsRunning && sensors.Subscriptions == 0 && transport.ReadCalls == 0 && transport.TemperatureReadCalls == 1,
        "cancelled startup performs only the initial read-only temperature validation and leaves no control loop or subscription");
}

// Multiple Stop calls join the same physical cleanup instead of issuing concurrent restores.
{
    var transport = new ThermalCpuTransport { BlockRestore = true };
    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
    var sensors = new SensorsGroupController();
    var settings = new ApplicationSettings();
    settings.Store.EnableHardwareSensors = true;
    var controller = new CpuThermalController(sensors, settings);
    await controller.StartAsync();
    Check(sensors.Subscriptions == 1, "optional hardware monitoring receives an owned sampling subscription when enabled");
    var first = controller.StopAsync();
    await transport.RestoreEntered.Task.WaitAsync(TimeSpan.FromSeconds(3));
    var second = controller.StopAsync();
    Check(ReferenceEquals(first, second), "concurrent stops reuse the same cleanup task");
    Check(transport.RestoreCalls == 1 && transport.OwnsFirmware, "pending restore retains firmware ownership");
    transport.RestoreRelease.TrySetResult();
    await first.WaitAsync(TimeSpan.FromSeconds(3));
    Check(!controller.IsRunning && !transport.OwnsFirmware && sensors.Subscriptions == 0, "verified stop cleans ownership and subscription");
}

// A stalled read cannot create an unbounded verifier queue or release ownership on UI timeout.
{
    var transport = new ThermalCpuTransport { BlockRead = true };
    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
    var sensors = new SensorsGroupController();
    var controller = new CpuThermalController(sensors, new ApplicationSettings());
    await controller.StartAsync();
    await transport.ReadEntered.Task.WaitAsync(TimeSpan.FromSeconds(3));
    await Task.Delay(350);
    Check(transport.ReadCalls == 1 && transport.VerifyCalls == 0, "stalled read stays single-flight across subsequent watchdog ticks");
    var stop = controller.StopAsync();
    await Expect<TimeoutException>(stop.WaitAsync(TimeSpan.FromMilliseconds(50)), "caller may bound its wait on pending stop");
    Check(!stop.IsCompleted && transport.OwnsFirmware, "caller timeout does not abandon physical ownership");
    Check(ReferenceEquals(stop, controller.StopAsync()), "repeated stop during I/O stall joins existing restoration");
    transport.ReadRelease.TrySetResult();
    await stop.WaitAsync(TimeSpan.FromSeconds(3));
    Check(transport.MaximumPhysicalConcurrency == 1 && !transport.OwnsFirmware && !controller.IsRunning,
        "late physical completion permits one serialized restoration");
}

// Once restoration fails, neither another Stop nor a new Start may report a verified reset.
{
    var transport = new ThermalCpuTransport { RestoreFailure = new InvalidOperationException("fake restoration readback mismatch") };
    var discoveries = 0;
    ThermalCpuTransport.Discover = () => { discoveries++; return Task.FromResult(transport); };
    var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
    await controller.StartAsync();
    await Expect<InvalidOperationException>(controller.StopAsync(), "restoration failure propagates");
    Check(!controller.IsRunning && controller.Status.Contains("未确认", StringComparison.Ordinal), "failed restoration leaves an explicit unconfirmed status");
    await Expect<InvalidOperationException>(controller.StopAsync(), "second stop cannot wash away an unconfirmed restoration");
    await Expect<InvalidOperationException>(controller.StartAsync(), "restart is rejected after unconfirmed restoration");
    Check(transport.RestoreCalls == 1 && discoveries == 1, "latched restoration fault does not silently reacquire or fake successful cleanup");
}

// Repeated readback failures use the production bounded executor rather than parallel verifiers.
{
    var transport = new ThermalCpuTransport { ReadFailure = new InvalidOperationException("fake readback fault") };
    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
    var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
    await controller.StartAsync();
    await transport.ReadEntered.Task.WaitAsync(TimeSpan.FromSeconds(3));
    await Task.Delay(250);
    Check(transport.ReadCalls >= 1 && transport.MaximumPhysicalConcurrency == 1 && transport.VerifyCalls == 0,
        "readback faults retain single-flight execution without a parallel ownership verification queue");
    Check(controller.Status.Contains("故障", StringComparison.Ordinal), "readback fault is surfaced in controller status");
    await controller.StopAsync().WaitAsync(TimeSpan.FromSeconds(3));
}

// No optional hardware monitor is required, and a hung firmware-temperature read cannot block
// independent watchdog ticks, fail-closed power reduction, or the power restoration path.
{
    var transport = new ThermalCpuTransport { BlockTemperatureAfterInitial = true };
    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
    var sensors = new SensorsGroupController { HardwareTelemetryAvailable = false };
    var settings = new ApplicationSettings();
    settings.Store.EnableHardwareSensors = false;
    settings.Store.CpuThermalControl = settings.Store.CpuThermalControl with
    {
        Policy = new ThermalOptions { TargetC = 88, WarmC = 85, RecoverC = 82, FreshnessMilliseconds = 250 },
        PredictionMilliseconds = 100
    };
    var controller = new CpuThermalController(sensors, settings);
    var sensorFaultReported = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
    controller.StatusChanged += status =>
    {
        if (status.Contains("温度数据失效", StringComparison.Ordinal)) sensorFaultReported.TrySetResult();
    };
    await controller.StartAsync();
    Check(controller.IsRunning && sensors.Subscriptions == 0, "firmware-only mode starts without hardware-monitor subscription");
    await transport.TemperatureReadEntered.Task.WaitAsync(TimeSpan.FromSeconds(3));
    await transport.MinimumPowerApplied.Task.WaitAsync(TimeSpan.FromSeconds(3));
    await sensorFaultReported.Task.WaitAsync(TimeSpan.FromSeconds(3));
    Check(transport.TemperatureReadCalls == 2 && transport.MaximumTemperatureConcurrency == 1,
        "one initial temperature read and at most one hung background read are admitted");
    Check(transport.LastWrittenTarget == transport.Capability.GetMinimumEnvelope(),
        "stale firmware-only temperature still drives the production supervisor to minimum legal CPU power");
    await controller.StopAsync().WaitAsync(TimeSpan.FromSeconds(3));
    Check(!controller.IsRunning && !transport.OwnsFirmware && !transport.TemperatureReadCompleted.Task.IsCompleted,
        "pending temperature read does not block Stop or restoration and is observed separately");
    transport.TemperatureReadRelease.TrySetResult();
    await transport.TemperatureReadCompleted.Task.WaitAsync(TimeSpan.FromSeconds(3));
}

// A host diagnostic failure may not bypass either startup or physical restoration.
{
    LenovoLegionToolkit.Lib.Utils.Log.Failure = new System.IO.IOException("injected diagnostic boundary failure");
    try
    {
        var transport = new ThermalCpuTransport();
        ThermalCpuTransport.Discover = () => Task.FromResult(transport);
        var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
        await controller.StartAsync();
        Check(controller.IsRunning && transport.OwnsFirmware, "diagnostic failures do not abort verified control startup");
        await controller.StopAsync().WaitAsync(TimeSpan.FromSeconds(3));
        Check(transport.RestoreCalls == 1 && !transport.OwnsFirmware, "diagnostic failures cannot bypass restoration");
    }
    finally { LenovoLegionToolkit.Lib.Utils.Log.Failure = null; }
}

// Inject a crashed worker task while retaining the real lifecycle and firmware cleanup path.
{
    var transport = new ThermalCpuTransport();
    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
    var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
    await controller.StartAsync();
    var workerField = typeof(CpuThermalController).GetField("_loop", System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance)!;
    var originalWorker = (Task)workerField.GetValue(controller)!;
    workerField.SetValue(controller, Task.FromException(new InvalidOperationException("injected worker failure")));
    await Expect<InvalidOperationException>(controller.StartAsync(), "restart cannot silently reuse an unexpectedly stopped worker");
    Check(transport.OwnsFirmware && transport.RestoreCalls == 0,
        "rejected restart retains the original owner for explicit Stop cleanup");
    await controller.StopAsync().WaitAsync(TimeSpan.FromSeconds(3));
    await originalWorker.WaitAsync(TimeSpan.FromSeconds(3));
    Check(!controller.IsRunning && !transport.OwnsFirmware && transport.RestoreCalls == 1,
        "a crashed worker still drains and restores owned firmware");
    Check(controller.Status.Contains("恢复", StringComparison.Ordinal) && controller.Status.Contains("异常", StringComparison.Ordinal),
        "verified restoration and prior worker failure are reported distinctly");
}

await DetailedLifecycleScenarios.RunAsync(Check);
Console.WriteLine($"PASS: {checks} production thermal lifecycle checks; host firmware/sensors/settings/log are fakes, no hardware I/O");
