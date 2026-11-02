using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.Thermal;
using LenovoLegionToolkit.Lib.Utils;

internal static class DetailedLifecycleScenarios
{
    private static TaskCompletionSource Signal() => new(TaskCreationOptions.RunContinuationsAsynchronously);
    private static readonly TimeSpan Deadline = TimeSpan.FromSeconds(3);

    public static async Task RunAsync(Action<bool, string> check, bool counterexamplesOnly = false)
    {
        var failures = new List<string>();
        async Task Scenario(string name, Func<Task> run)
        {
            var watch = Stopwatch.StartNew();
            try { await run().WaitAsync(TimeSpan.FromSeconds(15)); Console.WriteLine($"SCENARIO PASS {name} ({watch.ElapsedMilliseconds} ms)"); }
            catch (Exception ex) { failures.Add(name); Console.WriteLine($"SCENARIO FAIL {name}: {ex}"); }
        }

        await Scenario("hung-background-temperature-restart-admission", async () =>
        {
            var old = new ThermalCpuTransport { BlockTemperatureAfterInitial = true };
            var fresh = new ThermalCpuTransport();
            var discoveries = 0;
            ThermalCpuTransport.Discover = () => Task.FromResult(++discoveries == 1 ? old : fresh);
            var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
            try
            {
                await controller.StartAsync().WaitAsync(Deadline);
                await old.TemperatureReadEntered.Task.WaitAsync(Deadline);
                await controller.StopAsync().WaitAsync(Deadline);
                check(!controller.IsRunning && !old.OwnsFirmware, "hung temperature does not retain actuator ownership after Stop");
                var rejected = false;
                for (var attempt = 0; attempt < 12; attempt++)
                {
                    try { await controller.StartAsync().WaitAsync(Deadline); }
                    catch (InvalidOperationException) { rejected = true; }
                }
                check(rejected && discoveries == 1 && fresh.TemperatureReadCalls == 0 && !controller.IsRunning,
                    "repeated Start cannot create more getters while a stopped generation's temperature read remains pending");
                old.TemperatureFailure = new InvalidOperationException("late old-generation temperature failure");
                old.TemperatureReadRelease.TrySetResult();
                await old.TemperatureReadCompleted.Task.WaitAsync(Deadline);
                await WaitForRetainedTemperatureAsync(controller);
                await controller.StartAsync().WaitAsync(Deadline);
                check(controller.IsRunning && fresh.EnableCalls == 1 && discoveries == 2,
                    "restart is admitted after the previous temperature read physically settles, including failure");
                await controller.StopAsync().WaitAsync(Deadline);
                var stopped = controller.Status;
                await Task.Delay(25);
                check(controller.Status == stopped && !fresh.OwnsFirmware,
                    "observing an old temperature failure cannot replace the new generation's final status");
            }
            finally { old.TemperatureReadRelease.TrySetResult(); await controller.StopAsync().WaitAsync(Deadline); }
        });

        await Scenario("cancelled-initial-temperature-restart-admission", async () =>
        {
            var old = new ThermalCpuTransport { BlockInitialTemperature = true };
            var fresh = new ThermalCpuTransport();
            var discoveries = 0;
            ThermalCpuTransport.Discover = () => Task.FromResult(++discoveries == 1 ? old : fresh);
            var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
            using var cancellation = new CancellationTokenSource();
            try
            {
                var start = controller.StartAsync(cancellation.Token);
                await old.TemperatureReadEntered.Task.WaitAsync(Deadline);
                cancellation.Cancel();
                await ExpectAsync<OperationCanceledException>(start, check, "initial read cancellation returns without any physical enable");
                await controller.StopAsync().WaitAsync(Deadline);
                var rejected = false;
                for (var attempt = 0; attempt < 12; attempt++)
                {
                    try { await controller.StartAsync().WaitAsync(Deadline); }
                    catch (InvalidOperationException) { rejected = true; }
                }
                check(rejected && discoveries == 1 && old.EnableCalls == 0 && fresh.TemperatureReadCalls == 0,
                    "cancelled initial read remains admitted as the only physical getter until it settles");
                old.TemperatureReadRelease.TrySetResult();
                await old.TemperatureReadCompleted.Task.WaitAsync(Deadline);
                await WaitForRetainedTemperatureAsync(controller);
                await controller.StartAsync().WaitAsync(Deadline);
                check(controller.IsRunning && fresh.EnableCalls == 1, "initial-read cancellation permits a clean restart after physical settlement");
            }
            finally { old.TemperatureReadRelease.TrySetResult(); await controller.StopAsync().WaitAsync(Deadline); }
        });

        await Scenario("blocked-status-subscriber-does-not-block-control", async () =>
        {
            var callbackEntered = Signal();
            var callbackRelease = Signal();
            var stoppedPublished = Signal();
            var transport = new ThermalCpuTransport { BlockTemperatureAfterInitial = true };
            var blockedTemperatureTransport = transport;
            var callbackEntries = 0;
            ThermalCpuTransport.Discover = () => Task.FromResult(transport);
            var settings = FirmwareOnlySettings();
            var controller = new CpuThermalController(new SensorsGroupController(), settings);
            controller.StatusChanged += status =>
            {
                Interlocked.Increment(ref callbackEntries);
                if (!callbackEntered.Task.IsCompleted)
                {
                    callbackEntered.TrySetResult();
                    callbackRelease.Task.GetAwaiter().GetResult();
                }
                if (status.Contains("已停止", StringComparison.Ordinal)) stoppedPublished.TrySetResult();
            };
            try
            {
                var start = Task.Run(() => controller.StartAsync());
                await callbackEntered.Task.WaitAsync(Deadline);
                await start.WaitAsync(TimeSpan.FromMilliseconds(600));
                check(controller.IsRunning && !callbackRelease.Task.IsCompleted, "blocked subscriber does not hold startup or its lifecycle lock");
                await transport.MinimumPowerApplied.Task.WaitAsync(Deadline);
                check(transport.LastWrittenTarget == transport.Capability.GetMinimumEnvelope(), "watchdog reaches fail-closed minimum while status subscriber is blocked");
                await controller.StopAsync().WaitAsync(Deadline);
                check(!controller.IsRunning && !transport.OwnsFirmware && controller.Status.Contains("已停止", StringComparison.Ordinal),
                    "Stop and immediate public Status do not wait for a blocked notification consumer");
                blockedTemperatureTransport.TemperatureReadRelease.TrySetResult();
                await WaitForRetainedTemperatureAsync(controller);
                for (var generation = 0; generation < 20; generation++)
                {
                    transport = new ThermalCpuTransport();
                    await controller.StartAsync().WaitAsync(Deadline);
                    await controller.StopAsync().WaitAsync(Deadline);
                    await WaitForRetainedTemperatureAsync(controller);
                }
                check(callbackEntries == 1 && !transport.OwnsFirmware,
                    "twenty restarts reuse one blocked notification consumer instead of accumulating per-status tasks");
                callbackRelease.TrySetResult();
                await stoppedPublished.Task.WaitAsync(Deadline);
                check(callbackEntries == 2, "bounded notification backlog coalesces forty intervening lifecycle transitions into the latest stopped status");
            }
            finally { callbackRelease.TrySetResult(); blockedTemperatureTransport.TemperatureReadRelease.TrySetResult(); await controller.StopAsync().WaitAsync(Deadline); }
        });

        await Scenario("blocked-diagnostics-do-not-block-control", async () =>
        {
            var entered = Signal();
            var release = Signal();
            Log.TraceEntered = entered;
            Log.TraceRelease = release;
            var transport = new ThermalCpuTransport { BlockTemperatureAfterInitial = true };
            ThermalCpuTransport.Discover = () => Task.FromResult(transport);
            var controller = new CpuThermalController(new SensorsGroupController(), FirmwareOnlySettings());
            var firstAfterLog = new TaskCompletionSource<string>(TaskCreationOptions.RunContinuationsAsynchronously);
            controller.StatusChanged += status => firstAfterLog.TrySetResult(status);
            try
            {
                var start = Task.Run(() => controller.StartAsync());
                await entered.Task.WaitAsync(Deadline);
                await start.WaitAsync(TimeSpan.FromMilliseconds(600));
                check(controller.IsRunning && !release.Task.IsCompleted, "blocked logger does not hold startup or lifecycle lock");
                await transport.MinimumPowerApplied.Task.WaitAsync(Deadline);
                check(transport.LastWrittenTarget == transport.Capability.GetMinimumEnvelope(), "blocked logger cannot stop independent minimum-power control");
                await controller.StopAsync().WaitAsync(Deadline);
                check(!controller.IsRunning && !transport.OwnsFirmware && controller.Status.Contains("已停止", StringComparison.Ordinal),
                    "blocked diagnostics cannot delay restoration or wash away public stopped status");
                Log.TraceRelease = null; Log.TraceEntered = null; release.TrySetResult();
                check(await firstAfterLog.Task.WaitAsync(Deadline) == controller.Status,
                    "a released diagnostic stall publishes the current stopped status instead of an obsolete running status");
            }
            finally { Log.TraceRelease = null; Log.TraceEntered = null; release.TrySetResult(); transport.TemperatureReadRelease.TrySetResult(); await controller.StopAsync().WaitAsync(Deadline); }
        });

        await Scenario("queued-start-is-superseded-by-new-stop", async () =>
        {
            var transport = new ThermalCpuTransport();
            var discoveries = 0;
            ThermalCpuTransport.Discover = () => { discoveries++; return Task.FromResult(transport); };
            var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
            var gate = (SemaphoreSlim)typeof(CpuThermalController).GetField("_lifecycle",
                System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance)!.GetValue(controller)!;
            await gate.WaitAsync();
            var start = controller.StartAsync();
            var stop = controller.StopAsync();
            gate.Release();
            try
            {
                await ExpectAsync<InvalidOperationException>(start, check, "Start queued before a subsequent Stop is rejected as superseded");
                await stop.WaitAsync(Deadline);
                check(discoveries == 0 && transport.EnableCalls == 0 && !controller.IsRunning,
                    "a Stop superseding queued Start prevents any new physical acquisition");
            }
            finally { await controller.StopAsync().WaitAsync(Deadline); }
        });

        if (!counterexamplesOnly)
        {
            await Scenario("startup-failure-and-clean-restart-matrix", async () =>
            {
                foreach (var phase in new[] { "discovery", "temperature-exception", "temperature-invalid", "enable", "subscription", "configuration" })
                {
                    var failed = new ThermalCpuTransport();
                    var fresh = new ThermalCpuTransport();
                    var settings = new ApplicationSettings();
                    settings.Store.EnableHardwareSensors = true;
                    var sensors = new SensorsGroupController();
                    if (phase == "temperature-exception") failed.TemperatureFailure = new InvalidOperationException("initial temperature failure");
                    if (phase == "temperature-invalid") failed.TemperatureC = double.NaN;
                    if (phase == "enable") failed.EnableFailure = new InvalidOperationException("late partial enable failure");
                    if (phase == "subscription") sensors.SubscribeFailure = new InvalidOperationException("disposed hardware sensor host");
                    if (phase == "configuration") settings.Store.CpuThermalFirmwareLimitC = 89;
                    var discoveries = 0;
                    ThermalCpuTransport.Discover = () =>
                    {
                        discoveries++;
                        return phase == "discovery" ? Task.FromException<ThermalCpuTransport>(new InvalidOperationException("discovery failure")) : Task.FromResult(failed);
                    };
                    var controller = new CpuThermalController(sensors, settings);
                    await ExpectAsync<InvalidOperationException>(controller.StartAsync(), check, $"{phase}: startup failure propagates");
                    check(!controller.IsRunning && !failed.OwnsFirmware && sensors.Subscriptions == 0,
                        $"{phase}: failed startup leaves no loop, owner or hardware subscription");
                    check(failed.RestoreCalls == (phase is "discovery" or "configuration" ? 0 : 1),
                        $"{phase}: only discovered transports are restored exactly once");
                    check(phase != "configuration" || discoveries == 0, "invalid configuration never begins firmware discovery");
                    sensors.SubscribeFailure = null;
                    settings.Store.CpuThermalFirmwareLimitC = 88;
                    ThermalCpuTransport.Discover = () => Task.FromResult(fresh);
                    await controller.StartAsync().WaitAsync(Deadline);
                    check(controller.IsRunning && fresh.OwnsFirmware && sensors.Subscriptions == 1,
                        $"{phase}: a verified cleanup permits one clean restart");
                    await controller.StopAsync().WaitAsync(Deadline);
                    check(!fresh.OwnsFirmware && sensors.Subscriptions == 0, $"{phase}: restart stops without residual ownership");
                }
            });

            await Scenario("startup-and-restoration-double-failure-latches", async () =>
            {
                var transport = new ThermalCpuTransport
                {
                    EnableFailure = new InvalidOperationException("partial enable failed"),
                    RestoreFailure = new InvalidOperationException("cleanup readback failed")
                };
                var discoveries = 0;
                ThermalCpuTransport.Discover = () => { discoveries++; return Task.FromResult(transport); };
                var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
                AggregateException? aggregate = null;
                try { await controller.StartAsync().WaitAsync(Deadline); }
                catch (AggregateException ex) { aggregate = ex; }
                check(aggregate?.InnerExceptions.Count == 2, "failed startup retains both enable and restoration failures");
                check(!controller.IsRunning && transport.OwnsFirmware && controller.Status.Contains("未确认", StringComparison.Ordinal),
                    "unverified restoration keeps an explicit unknown-ownership state");
                for (var i = 0; i < 8; i++)
                {
                    await ExpectAsync<InvalidOperationException>(controller.StopAsync(), check, "latched startup restoration failure rejects another successful Stop");
                    await ExpectAsync<InvalidOperationException>(controller.StartAsync(), check, "latched startup restoration failure rejects reacquisition");
                }
                check(discoveries == 1 && transport.RestoreCalls == 1 && controller.Status.Contains("未确认", StringComparison.Ordinal),
                    "repeated calls do not wash the failure status or replay an unverified firmware restore");
                // Fake ownership is intentionally left unconfirmed to model the production latch.
            });

            await Scenario("cancelled-discovery-late-failure-does-not-acquire", async () =>
            {
                var discovery = new TaskCompletionSource<ThermalCpuTransport>(TaskCreationOptions.RunContinuationsAsynchronously);
                ThermalCpuTransport.Discover = () => discovery.Task;
                var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
                using var cancellation = new CancellationTokenSource();
                var start = controller.StartAsync(cancellation.Token);
                cancellation.Cancel();
                await ExpectAsync<OperationCanceledException>(start, check, "discovery cancellation is bounded before any lease exists");
                await controller.StopAsync().WaitAsync(Deadline);
                var status = controller.Status;
                discovery.TrySetException(new InvalidOperationException("late read-only discovery failure"));
                await Task.Delay(15);
                check(!controller.IsRunning && controller.Status == status, "late discovery failure cannot reenable control or replace final status");
                var fresh = new ThermalCpuTransport();
                ThermalCpuTransport.Discover = () => Task.FromResult(fresh);
                await controller.StartAsync().WaitAsync(Deadline);
                await controller.StopAsync().WaitAsync(Deadline);
                check(fresh.EnableCalls == 1 && fresh.RestoreCalls == 1 && !fresh.OwnsFirmware,
                    "a cancelled read-only discovery does not prevent a subsequent verified lifecycle");
            });

            await Scenario("queued-concurrent-starts-cancel-and-stop-during-enable", async () =>
            {
                var transport = new ThermalCpuTransport { BlockEnable = true, BlockRestore = true };
                var discoveries = 0;
                ThermalCpuTransport.Discover = () => { discoveries++; return Task.FromResult(transport); };
                var sensors = new SensorsGroupController();
                var settings = new ApplicationSettings();
                settings.Store.EnableHardwareSensors = true;
                var controller = new CpuThermalController(sensors, settings);
                var initial = controller.StartAsync();
                await transport.EnableEntered.Task.WaitAsync(Deadline);
                var queued = new List<Task>();
                using var cancellation = new CancellationTokenSource();
                for (var i = 0; i < 16; i++) queued.Add(controller.StartAsync(i % 2 == 0 ? cancellation.Token : default));
                cancellation.Cancel();
                var stop = controller.StopAsync();
                check(transport.OwnsFirmware && transport.EnableCalls == 1 && !stop.IsCompleted,
                    "queued Stop cannot discard an enable operation still in physical flight");
                transport.EnableRelease.TrySetResult();
                await initial.WaitAsync(Deadline);
                foreach (var task in queued)
                {
                    try { await task.WaitAsync(Deadline); throw new Exception("superseded queued Start unexpectedly succeeded"); }
                    catch (OperationCanceledException) { }
                    catch (InvalidOperationException) { }
                }
                check(discoveries == 1 && transport.EnableCalls == 1 && sensors.MaximumSubscriptions <= 1,
                    "queued Start cancellations and supersession create no duplicate acquisition or loop");
                await transport.RestoreEntered.Task.WaitAsync(Deadline);
                await ExpectAsync<InvalidOperationException>(controller.StartAsync(), check, "new Start is refused while previous physical restore is pending");
                check(ReferenceEquals(stop, controller.StopAsync()) && transport.RestoreCalls == 1,
                    "all Stop requests join the same delayed restoration");
                transport.RestoreRelease.TrySetResult();
                await stop.WaitAsync(Deadline);
                check(!controller.IsRunning && !transport.OwnsFirmware && sensors.Subscriptions == 0,
                    "cancelled and superseded startup waiters leave no owner after one cleanup");
            });

            await Scenario("late-power-io-settlement-matrix", async () =>
            {
                foreach (var phase in new[] { "read-success", "read-failure", "write-success", "write-failure" })
                {
                    var write = phase.StartsWith("write", StringComparison.Ordinal);
                    var fail = phase.EndsWith("failure", StringComparison.Ordinal);
                    var transport = new ThermalCpuTransport { BlockRead = !write, BlockWrite = write, TemperatureC = write ? 90 : 50 };
                    ThermalCpuTransport.Discover = () => Task.FromResult(transport);
                    var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
                    try
                    {
                        await controller.StartAsync().WaitAsync(Deadline);
                        await (write ? transport.WriteEntered.Task : transport.ReadEntered.Task).WaitAsync(Deadline);
                        await Task.Delay(75);
                        var stop = controller.StopAsync();
                        await transport.RestoreEntered.Task.WaitAsync(Deadline);
                        check(!stop.IsCompleted && transport.OwnsFirmware, $"{phase}: Stop retains physical ownership until timed-out I/O settles");
                        check(ReferenceEquals(stop, controller.StopAsync()), $"{phase}: pending Stop remains single-flight");
                        if (fail)
                        {
                            if (write) transport.WriteFailure = new InvalidOperationException("late write failed after timeout");
                            else transport.ReadFailure = new InvalidOperationException("late read failed after timeout");
                        }
                        transport.ReadRelease.TrySetResult();
                        transport.WriteRelease.TrySetResult();
                        await stop.WaitAsync(Deadline);
                        check(!controller.IsRunning && !transport.OwnsFirmware && transport.RestoreCalls == 1,
                            $"{phase}: physical settlement permits exactly one verified cleanup");
                        check(transport.MaximumPhysicalConcurrency == 1 && transport.WritesWithoutOwnership == 0,
                            $"{phase}: no physical concurrency or post-restoration write is admitted");
                        check(controller.Status.Contains("已停止", StringComparison.Ordinal), $"{phase}: late I/O cannot erase the final stopped status");
                    }
                    finally { transport.ReadRelease.TrySetResult(); transport.WriteRelease.TrySetResult(); await controller.StopAsync().WaitAsync(Deadline); }
                }
            });

            await Scenario("throwing-and-reentrant-status-subscribers", async () =>
            {
                var failedCallback = Signal();
                var stoppedCallback = Signal();
                var transport = new ThermalCpuTransport();
                ThermalCpuTransport.Discover = () => Task.FromResult(transport);
                var controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
                controller.StatusChanged += _ => { failedCallback.TrySetResult(); throw new InvalidOperationException("bad subscriber"); };
                controller.StatusChanged += status => { if (status.Contains("已停止", StringComparison.Ordinal)) stoppedCallback.TrySetResult(); };
                await controller.StartAsync().WaitAsync(Deadline);
                await failedCallback.Task.WaitAsync(Deadline);
                await controller.StopAsync().WaitAsync(Deadline);
                await stoppedCallback.Task.WaitAsync(Deadline);
                check(!transport.OwnsFirmware && !controller.IsRunning, "throwing status subscriber cannot bypass later subscribers or restoration");

                transport = new ThermalCpuTransport();
                ThermalCpuTransport.Discover = () => Task.FromResult(transport);
                controller = new CpuThermalController(new SensorsGroupController(), new ApplicationSettings());
                var stoppedReentrantly = new TaskCompletionSource<Task>(TaskCreationOptions.RunContinuationsAsynchronously);
                var requested = 0;
                controller.StatusChanged += _ =>
                {
                    if (Interlocked.Exchange(ref requested, 1) == 0) stoppedReentrantly.TrySetResult(controller.StopAsync());
                };
                await controller.StartAsync().WaitAsync(Deadline);
                var requestedStop = await stoppedReentrantly.Task.WaitAsync(Deadline);
                await requestedStop.WaitAsync(Deadline);
                check(!controller.IsRunning && transport.RestoreCalls == 1 && !transport.OwnsFirmware,
                    "a subscriber can request Stop without blocking lifecycle completion or starting duplicate restoration");
            });

            foreach (var seed in new[] { 0x1701, 0x7000, 0x1490, 0x5060, 0x90, 0x1102 })
            {
                await Scenario($"fixed-seed-concurrent-restarts-{seed:X}", () => StressAsync(seed, check));
            }
        }

        if (failures.Count != 0) throw new Exception($"{failures.Count} lifecycle scenario(s) failed: {string.Join(", ", failures)}");
    }

    private static ApplicationSettings FirmwareOnlySettings()
    {
        var settings = new ApplicationSettings();
        settings.Store.CpuThermalControl = settings.Store.CpuThermalControl with
        {
            Policy = new ThermalOptions { TargetC = 88, WarmC = 85, RecoverC = 82, FreshnessMilliseconds = 180 },
            PredictionMilliseconds = 100
        };
        return settings;
    }

    private static async Task ExpectAsync<T>(Task task, Action<bool, string> check, string message) where T : Exception
    {
        try { await task.WaitAsync(Deadline); }
        catch (T) { check(true, message); return; }
        throw new Exception(message);
    }

    private static async Task WaitForRetainedTemperatureAsync(CpuThermalController controller)
    {
        // The fake's physical completion signal precedes completion of Task.Run's
        // unwrapped Task; wait for the exact retained task before asserting admission.
        var field = typeof(CpuThermalController).GetField("_pendingFirmwareRead",
            System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance);
        if (field?.GetValue(controller) is not Task read) return;
        try { await read.WaitAsync(Deadline); }
        catch (InvalidOperationException) { }
    }

    private static async Task StressAsync(int seed, Action<bool, string> check)
    {
        const int rounds = 20;
        const int operations = 24;
        var random = new Random(seed);
        var transports = new List<ThermalCpuTransport>();
        var liveOwners = 0;
        var maximumOwners = 0;
        var sensors = new SensorsGroupController();
        var settings = new ApplicationSettings();
        settings.Store.EnableHardwareSensors = true;
        var controller = new CpuThermalController(sensors, settings);
        ThermalCpuTransport.Discover = () =>
        {
            var transport = new ThermalCpuTransport
            {
                OwnershipChanged = acquired =>
                {
                    var owners = Interlocked.Add(ref liveOwners, acquired ? 1 : -1);
                    maximumOwners = Math.Max(maximumOwners, owners);
                }
            };
            transports.Add(transport); // The production lifecycle semaphore serializes discovery.
            return Task.FromResult(transport);
        };
        for (var round = 0; round < rounds; round++)
        {
            var tasks = new List<Task>();
            for (var operation = 0; operation < operations; operation++)
            {
                var start = random.Next(3) != 0;
                var delay = random.Next(0, 12);
                var cancel = random.Next(4) == 0;
                tasks.Add(Task.Run(async () =>
                {
                    using var cancellation = new CancellationTokenSource();
                    if (cancel) cancellation.CancelAfter(TimeSpan.FromMilliseconds(1 + delay / 2));
                    await Task.Delay(delay);
                    try
                    {
                        if (start) await controller.StartAsync(cancellation.Token).WaitAsync(Deadline);
                        else await controller.StopAsync().WaitAsync(Deadline);
                    }
                    catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { }
                    catch (InvalidOperationException ex) when (ex.Message.Contains("停止", StringComparison.Ordinal)
                        || ex.Message.Contains("温度读取仍未返回", StringComparison.Ordinal)) { }
                }));
            }
            await Task.WhenAll(tasks).WaitAsync(Deadline);
            await controller.StopAsync().WaitAsync(Deadline);
            await WaitForRetainedTemperatureAsync(controller);
            check(!controller.IsRunning && liveOwners == 0 && sensors.Subscriptions == 0,
                $"seed {seed:X} round {round}: bounded concurrent start/stop/cancel ends with no owner or subscription");
            check(maximumOwners <= 1 && sensors.MaximumSubscriptions <= 1,
                $"seed {seed:X} round {round}: no overlapping firmware owner or duplicate loop subscription");
            var allRestored = true;
            var allPhysicalSafe = true;
            foreach (var transport in transports)
            {
                // Cancellation may arrive after read-only discovery but before Enable.
                allRestored &= !transport.OwnsFirmware && transport.EnableCalls is 0 or 1 && transport.RestoreCalls == 1;
                allPhysicalSafe &= transport.MaximumPhysicalConcurrency <= 1 && transport.MaximumTemperatureConcurrency <= 1
                    && transport.WritesWithoutOwnership == 0;
            }
            check(allRestored, $"seed {seed:X} round {round}: every acquired generation restores exactly once");
            check(allPhysicalSafe, $"seed {seed:X} round {round}: no overlapping physical/getter operation or write without ownership");
        }
        check(transports.Count > 1, $"seed {seed:X}: the same production Controller performs repeated generations");
        Console.WriteLine($"STRESS seed=0x{seed:X} rounds={rounds} operations={rounds * operations} generations={transports.Count} maxOwners={maximumOwners} maxSubscriptions={sensors.MaximumSubscriptions}");
    }
}
