using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Thermal;

// This harness links the production transport, but never constructs the real firmware adapter.
// Each scenario owns one in-memory firmware instance and drains all delayed tasks before returning.
internal static class FaultMatrixTests
{
    private static int _scenarios;
    private static readonly CpuPowerEnvelope Baseline = new(70, 125);
    private static readonly CpuPowerEnvelope Restricted = new(65, 100);
    private static readonly CpuPowerEnvelope Minimum = new(50, 60);

    public static async Task<int> RunAsync()
    {
        await PreviousOwnedValueSurvivesFailedAttemptAsync();
        await RestorationInterleavingsAsync();
        await ExternalTemperatureCannotBeOverwrittenAsync();
        await ModeChangeBetweenPhysicalWritesAsync();
        await ExternalChangesDuringPhysicalWritesAsync();
        await ConcurrentStopsShareRestorationFailureAsync();
        await DiscoveryBoundaryMatrixAsync();
        await PhysicalFailureMatrixAsync();
        await ReadbackMismatchMatrixAsync();
        await IgnoredAndClampedSetterMatrixAsync();
        await CoordinatorAdmissionAsync();
        await LateCompletionMatrixAsync();
        await ObservedCapabilityGridAsync();
        await DeterministicRandomTracesAsync();
        await DeterministicFaultTracesAsync();
        Console.WriteLine($"PASS: {_scenarios} detailed fault-matrix scenarios (fake firmware only)");
        return _scenarios;
    }

    private static void Require(bool condition, string context)
    {
        if (!condition) throw new Exception("Fault matrix: " + context);
    }

    private static async Task<Exception> MustFailAsync(Func<Task> action, string context)
    {
        try { await action(); }
        catch (Exception ex) { return ex; }
        throw new Exception("Expected failure: " + context);
    }

    private static async Task<(ScriptedFirmware Firmware, ThermalCpuTransport Transport)> CreateAsync(bool enable = true,
        CpuPowerEnvelope? baseline = null)
    {
        var firmware = new ScriptedFirmware();
        if (baseline is not null)
        {
            firmware.Values[CpuThermalWriteCoordinator.LongPowerId] = checked((int)baseline.LongWatts);
            firmware.Values[CpuThermalWriteCoordinator.ShortPowerId] = checked((int)baseline.ShortWatts);
        }
        var transport = await ThermalCpuTransport.DiscoverAsync(firmware);
        if (enable) await transport.EnableAsync(88);
        return (firmware, transport);
    }

    private static async Task PreviousOwnedValueSurvivesFailedAttemptAsync()
    {
        foreach (var field in new[] { CpuThermalWriteCoordinator.LongPowerId, CpuThermalWriteCoordinator.ShortPowerId,
            CpuThermalWriteCoordinator.TemperatureId })
        {
            var (firmware, transport) = await CreateAsync();
            await transport.WriteAsync(Restricted);
            firmware.ResetTrace();
            firmware.Fault = operation => operation.Kind == 'W' && operation.Id == field ? Fault.Before : Fault.None;
            await MustFailAsync(() => field == CpuThermalWriteCoordinator.TemperatureId
                ? transport.SetTemperatureLimitAsync(85) : transport.WriteAsync(Minimum), "failure before next owned field write");
            firmware.Fault = null;
            await transport.RestoreIfOwnedAsync();
            Require(firmware.Power == Baseline && firmware.Temperature == 100,
                $"fail-before on {field:X8} must still restore the previously verified owned value");
            _scenarios++;
        }
    }

    private static async Task RestorationInterleavingsAsync()
    {
        {
            var (firmware, transport) = await CreateAsync();
            await transport.WriteAsync(Minimum);
            firmware.After = operation =>
            {
                if (operation.Kind == 'W' && operation.Id == CpuThermalWriteCoordinator.TemperatureId)
                    firmware.Values[CpuThermalWriteCoordinator.LongPowerId] = 69;
            };
            await MustFailAsync(() => transport.RestoreIfOwnedAsync(), "external power changes during final temperature restoration");
            Require(!transport.IsEnabled && transport.LastRestorationError is not null && firmware.Power == new CpuPowerEnvelope(69, 125),
                "restoration detects and preserves a field changed during its final setter");
            _scenarios++;
        }

        {
            var (firmware, transport) = await CreateAsync();
            await transport.WriteAsync(Minimum);
            var changed = false;
            firmware.After = operation =>
            {
                if (changed || operation.Kind != 'R' || operation.Id != CpuThermalWriteCoordinator.TemperatureId) return;
                changed = true;
                firmware.Values[CpuThermalWriteCoordinator.TemperatureId] = 89;
            };
            var temperatureWritesBefore = firmware.Writes.Count(write => write.Id == CpuThermalWriteCoordinator.TemperatureId);
            await MustFailAsync(() => transport.RestoreIfOwnedAsync(), "external temperature changes after the first restoration getter");
            Require(!transport.IsEnabled && transport.LastRestorationError is not null && firmware.Temperature == 89
                && firmware.Writes.Count(write => write.Id == CpuThermalWriteCoordinator.TemperatureId) == temperatureWritesBefore,
                "restoration rechecks temperature ownership before its final setter");
            _scenarios++;
        }
    }

    private static async Task ExternalTemperatureCannotBeOverwrittenAsync()
    {
        foreach (var field in new[] { CpuThermalWriteCoordinator.TemperatureId, CpuThermalWriteCoordinator.LongPowerId,
            CpuThermalWriteCoordinator.ShortPowerId })
        {
            var (firmware, transport) = await CreateAsync();
            firmware.Values[field] = field == CpuThermalWriteCoordinator.TemperatureId ? 89
                : field == CpuThermalWriteCoordinator.LongPowerId ? 69 : 124;
            var before = firmware.Writes.Count;
            await MustFailAsync(() => transport.SetTemperatureLimitAsync(85), "external field changed before a temperature setter");
            Require(firmware.Writes.Count == before, "a temperature setter must verify all owned fields before mutation");
            await transport.RestoreIfOwnedAsync();
            _scenarios++;
        }
    }

    private static async Task ModeChangeBetweenPhysicalWritesAsync()
    {
        var (firmware, transport) = await CreateAsync();
        firmware.After = operation =>
        {
            if (operation.Kind == 'W' && operation.Id == CpuThermalWriteCoordinator.LongPowerId) firmware.Mode = 3;
        };
        var before = firmware.Writes.Count;
        await MustFailAsync(() => transport.WriteAsync(Minimum), "mode changes after the first physical power write");
        Require(firmware.Writes.Count == before + 1 && firmware.Writes[^1].Id == CpuThermalWriteCoordinator.LongPowerId,
            "a later field must not be written after the physical mode has changed");
        firmware.After = null;
        await transport.RestoreIfOwnedAsync();
        _scenarios++;
    }

    private static async Task ConcurrentStopsShareRestorationFailureAsync()
    {
        var (firmware, transport) = await CreateAsync();
        await transport.WriteAsync(Minimum);
        var entered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        firmware.BeforeAsync = async operation =>
        {
            if (operation.Kind != 'W') return;
            entered.TrySetResult();
            await release.Task;
        };
        firmware.Fault = operation => operation.Kind == 'W' ? Fault.Before : Fault.None;
        var first = transport.RestoreIfOwnedAsync();
        await entered.Task.WaitAsync(TimeSpan.FromSeconds(2));
        var second = transport.RestoreIfOwnedAsync();
        release.TrySetResult();
        await MustFailAsync(() => first, "first stop sees restoration failure");
        await MustFailAsync(() => second, "concurrent stop cannot erase the first stop's failure");
        Require(!transport.IsEnabled && transport.LastRestorationError is not null, "failed concurrent stop releases lease and latches error");
        _scenarios++;
    }

    private static async Task ExternalChangesDuringPhysicalWritesAsync()
    {
        foreach (var field in new[] { CpuThermalWriteCoordinator.LongPowerId, CpuThermalWriteCoordinator.ShortPowerId,
            CpuThermalWriteCoordinator.TemperatureId })
        {
            var (firmware, transport) = await CreateAsync();
            var changed = false;
            firmware.After = operation =>
            {
                if (changed || operation.Kind != 'W') return;
                changed = true;
                firmware.Values[field] = field == CpuThermalWriteCoordinator.LongPowerId ? 51
                    : field == CpuThermalWriteCoordinator.ShortPowerId ? 126 : 89;
            };
            var before = firmware.Writes.Count;
            await MustFailAsync(() => transport.WriteAsync(Minimum), "external field changes after the first power setter");
            Require(firmware.Writes.Count == before + 1, "no second power setter follows an observable intervening external change");
            firmware.After = null;
            try { await transport.RestoreIfOwnedAsync(); }
            catch (InvalidOperationException) { Require(transport.LastRestorationError is not null, "coupled restoration conflict is explicit"); }
            Require(firmware.Values[field] == (field == CpuThermalWriteCoordinator.LongPowerId ? 51
                : field == CpuThermalWriteCoordinator.ShortPowerId ? 126 : 89), "stop preserves the external field");
            _scenarios++;
        }

        {
            var (firmware, transport) = await CreateAsync();
            firmware.After = operation =>
            {
                if (operation.Kind == 'W' && operation.Id == CpuThermalWriteCoordinator.TemperatureId)
                    firmware.Values[CpuThermalWriteCoordinator.LongPowerId] = 69;
            };
            await MustFailAsync(() => transport.SetTemperatureLimitAsync(85), "power changes during temperature setter");
            firmware.After = null;
            await transport.RestoreIfOwnedAsync();
            Require(firmware.Power == new CpuPowerEnvelope(69, 125) && firmware.Temperature == 100,
                "temperature setter detects intervening power change and preserves it on stop");
            _scenarios++;
        }
    }

    private static async Task DiscoveryBoundaryMatrixAsync()
    {
        var probe = new ScriptedFirmware();
        await ThermalCpuTransport.DiscoverAsync(probe);
        foreach (var step in probe.Operations.ToArray())
        foreach (var fault in new[] { Fault.Before, Fault.After })
        {
            var firmware = new ScriptedFirmware();
            firmware.Fault = operation => operation.Number == step.Number ? fault : Fault.None;
            await MustFailAsync(async () => { await ThermalCpuTransport.DiscoverAsync(firmware); }, "discovery read failure " + step);
            Require(firmware.Writes.Count == 0 && firmware.FaultHits == 1, "failed discovery remains read-only");
            firmware.Fault = null;
            var fresh = await ThermalCpuTransport.DiscoverAsync(firmware);
            await fresh.EnableAsync();
            await fresh.RestoreIfOwnedAsync();
            _scenarios++;
        }

        var modeChanging = new ScriptedFirmware();
        modeChanging.After = operation =>
        {
            if (operation.Kind == 'R' && operation.Id == CpuThermalWriteCoordinator.TemperatureId) modeChanging.Mode = 3;
        };
        await MustFailAsync(async () => { await ThermalCpuTransport.DiscoverAsync(modeChanging); }, "mode changes during discovery baseline getters");
        Require(modeChanging.Writes.Count == 0, "mode drift during discovery never changes firmware");
        _scenarios++;
    }

    private enum Phase { Enable, Tighten, Recover, SetTemperature, Restore }

    private static async Task PrepareAsync(ScriptedFirmware firmware, ThermalCpuTransport transport, Phase phase)
    {
        if (phase == Phase.Enable) return;
        await transport.EnableAsync(88);
        if (phase is Phase.Tighten or Phase.Restore) await transport.WriteAsync(Restricted);
        if (phase == Phase.Recover) await transport.WriteAsync(Minimum);
    }

    private static Task ExecuteAsync(ThermalCpuTransport transport, Phase phase) => phase switch
    {
        Phase.Enable => transport.EnableAsync(88),
        Phase.Tighten => transport.WriteAsync(Minimum),
        Phase.Recover => transport.WriteAsync(Restricted),
        Phase.SetTemperature => transport.SetTemperatureLimitAsync(85),
        Phase.Restore => transport.RestoreIfOwnedAsync(),
        _ => throw new ArgumentOutOfRangeException(nameof(phase))
    };

    private static async Task PhysicalFailureMatrixAsync()
    {
        foreach (var phase in Enum.GetValues<Phase>())
        {
            var (probe, probeTransport) = await CreateAsync(false);
            await PrepareAsync(probe, probeTransport, phase);
            probe.ResetTrace();
            await ExecuteAsync(probeTransport, phase);
            var steps = probe.Operations.ToArray();
            await probeTransport.RestoreIfOwnedAsync();
            foreach (var step in steps)
            foreach (var fault in new[] { Fault.Before, Fault.After })
            {
                var (firmware, transport) = await CreateAsync(false);
                await PrepareAsync(firmware, transport, phase);
                firmware.ResetTrace();
                firmware.Fault = operation => operation.Number == step.Number ? fault : Fault.None;
                await MustFailAsync(() => ExecuteAsync(transport, phase), $"{phase}/{step}/{fault}");
                Require(firmware.FaultHits == 1, $"{phase}/{step}/{fault} reached the selected boundary exactly once");
                firmware.Fault = null;
                if (phase == Phase.Restore)
                {
                    Require(!transport.IsEnabled && transport.LastRestorationError is not null, "restoration failures are never silently successful");
                    var temperatureSetter = steps.Single(operation => operation.Kind == 'W'
                        && operation.Id == CpuThermalWriteCoordinator.TemperatureId);
                    if (step.Number < temperatureSetter.Number || step.Number == temperatureSetter.Number && fault == Fault.Before)
                        Require(firmware.Temperature == 88, "power restoration must verify before temperature protection is relaxed");
                    await MustFailAsync(() => transport.RestoreIfOwnedAsync(), "repeated stop retains failed restoration");
                    await MustFailAsync(() => transport.EnableAsync(), "failed restoration prevents reuse of stale baseline");
                    // A completely new owner may discover the actual residual physical state.
                    var fresh = await ThermalCpuTransport.DiscoverAsync(firmware);
                    await fresh.EnableAsync(85);
                    await fresh.RestoreIfOwnedAsync();
                }
                else
                {
                    await transport.RestoreIfOwnedAsync();
                    Require(firmware.Power == Baseline && firmware.Temperature == 100,
                        $"{phase}/{step}/{fault} restores exactly after the one injected failure is removed");
                }
                Require(firmware.IllegalTransientWrites == 0 && firmware.Writes.All(w => w.Mode == 255),
                    "every successful or partially successful physical write kept legal power ordering and Custom mode");
                _scenarios++;
            }
            Console.WriteLine($"  boundary failures: {phase}, {steps.Length} I/O points x before/after");
        }
    }

    private static async Task ReadbackMismatchMatrixAsync()
    {
        foreach (var phase in new[] { Phase.Enable, Phase.Tighten, Phase.Recover, Phase.SetTemperature, Phase.Restore })
        {
            var (probe, probeTransport) = await CreateAsync(false);
            await PrepareAsync(probe, probeTransport, phase);
            probe.ResetTrace();
            await ExecuteAsync(probeTransport, phase);
            var steps = probe.Operations.ToArray();
            await probeTransport.RestoreIfOwnedAsync();
            // Inject false readback only after a setter for that same field. Preflight getters
            // cannot distinguish a plausible provider lie from a genuine external user change.
            foreach (var step in steps.Where(s => s.Kind == 'R'
                && steps.Any(w => w.Kind == 'W' && w.Id == s.Id && w.Number < s.Number)))
            {
                var (firmware, transport) = await CreateAsync(false);
                await PrepareAsync(firmware, transport, phase);
                firmware.ResetTrace();
                firmware.Fault = operation => operation.Number == step.Number ? Fault.Mismatch : Fault.None;
                await MustFailAsync(() => ExecuteAsync(transport, phase), $"readback mismatch {phase}/{step}");
                firmware.Fault = null;
                if (phase != Phase.Restore)
                {
                    await transport.RestoreIfOwnedAsync();
                    Require(firmware.Power == Baseline && firmware.Temperature == 100, "mismatched readback keeps enough ownership information for restoration");
                }
                else Require(transport.LastRestorationError is not null && !transport.IsEnabled, "restoration mismatch remains explicitly unverified");
                _scenarios++;
            }
        }
    }

    private static async Task LateCompletionMatrixAsync()
    {
        foreach (var kind in new[] { 'W', 'R' })
        foreach (var afterSideEffect in new[] { false, true })
        foreach (var failAfter in new[] { false, true })
        foreach (var cancel in new[] { false, true })
        {
            var (firmware, transport) = await CreateAsync();
            var entered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            var delayed = false;
            Func<Operation, Task> pause = async operation =>
            {
                if (delayed || operation.Kind != kind) return;
                delayed = true;
                entered.TrySetResult();
                await release.Task;
                if (failAfter) throw new InvalidOperationException("Late physical failure");
            };
            if (afterSideEffect) firmware.AfterAsync = pause;
            else firmware.BeforeAsync = pause;
            var executor = new HardwareCommandExecutor(transport, transport.Capability, () => 100, () => (1, 1));
            using var cancellation = new CancellationTokenSource();
            var apply = executor.ApplyAsync(new(Minimum, 1, 1, 10000),
                cancel ? TimeSpan.FromSeconds(2) : TimeSpan.FromMilliseconds(40), cancellation.Token);
            await entered.Task.WaitAsync(TimeSpan.FromSeconds(2));
            if (cancel) cancellation.Cancel();
            Require((await apply).Status == ApplyStatus.UnknownCompletion, "late physical I/O reports unknown completion");
            var queued = executor.ApplyAsync(new(Restricted, 1, 1, 10000), TimeSpan.FromMilliseconds(40));
            var stop = transport.RestoreIfOwnedAsync();
            Require((await queued).Status == ApplyStatus.Rejected, "a timed-out operation retains admission while a later command expires");
            Require(!stop.IsCompleted && transport.IsEnabled, "stop waits for the physical call to drain");
            release.TrySetResult();
            await stop.WaitAsync(TimeSpan.FromSeconds(2));
            Require(!transport.IsEnabled && firmware.Power == Baseline && firmware.Temperature == 100,
                "late completion/failure drains before verified restoration");
            firmware.BeforeAsync = null;
            firmware.AfterAsync = null;
            // Ensure the executor continuation has also relinquished its own gate.
            var drained = await executor.VerifyAsync(new(Baseline, 1, 1, 10000), TimeSpan.FromSeconds(2));
            Require(drained.Status == ApplyStatus.Verified, "late executor continuation released its gate after drain");
            _scenarios++;
        }
    }

    private static async Task IgnoredAndClampedSetterMatrixAsync()
    {
        foreach (var phase in Enum.GetValues<Phase>())
        {
            var (probe, probeTransport) = await CreateAsync(false);
            await PrepareAsync(probe, probeTransport, phase);
            probe.ResetTrace();
            await ExecuteAsync(probeTransport, phase);
            var setters = probe.Operations.Where(operation => operation.Kind == 'W').ToArray();
            await probeTransport.RestoreIfOwnedAsync();
            foreach (var step in setters)
            foreach (var fault in new[] { Fault.IgnoreWrite, Fault.ClampWrite })
            {
                var (firmware, transport) = await CreateAsync(false);
                await PrepareAsync(firmware, transport, phase);
                firmware.ResetTrace();
                firmware.Fault = operation => operation.Number == step.Number ? fault : Fault.None;
                await MustFailAsync(() => ExecuteAsync(transport, phase), $"{phase}/{step}/{fault} cannot verify a different physical target");
                Require(firmware.FaultHits == 1, "the selected physical setter was ignored or clamped exactly once");
                firmware.Fault = null;
                if (phase == Phase.Restore)
                    Require(!transport.IsEnabled && transport.LastRestorationError is not null, "ignored/clamped restoration stays unverified");
                else
                {
                    try { await transport.RestoreIfOwnedAsync(); }
                    catch (InvalidOperationException)
                    {
                        Require(transport.LastRestorationError is not null,
                            "a clamped field conflicting with baseline restoration is explicitly unverified");
                    }
                    // An unsolicited clamped value is indistinguishable from a concurrent
                    // external change. Stop preserves it unless it matches an owned value.
                    Require(!transport.IsEnabled && firmware.IllegalTransientWrites == 0,
                        "ignored/clamped setter drains and never creates an invalid coupled restoration");
                }
                _scenarios++;
            }
        }
    }

    private static async Task CoordinatorAdmissionAsync()
    {
        var (firstFirmware, first) = await CreateAsync();
        var (secondFirmware, second) = await CreateAsync(false);
        await MustFailAsync(() => second.EnableAsync(), "a second independent transport cannot acquire the CPU owner");
        Require(first.IsEnabled && !second.IsEnabled && secondFirmware.Writes.Count == 0,
            "rejected second owner neither writes nor releases the first owner's lease");
        await first.WriteAsync(Minimum);
        await first.RestoreIfOwnedAsync();
        await second.EnableAsync();
        await second.RestoreIfOwnedAsync();
        Require(firstFirmware.Power == Baseline && secondFirmware.Power == Baseline,
            "ownership is reusable by a different transport only after verified stop");
        _scenarios++;

        var (firmware, transport) = await CreateAsync();
        var entered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var release = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var pauseOnce = false;
        firmware.BeforeAsync = async operation =>
        {
            if (operation.Kind != 'W' || pauseOnce) return;
            pauseOnce = true;
            entered.TrySetResult();
            await release.Task;
        };
        var ownerWrite = transport.WriteAsync(Minimum);
        await entered.Task.WaitAsync(TimeSpan.FromSeconds(2));
        var userCalls = 0;
        var userWrite = CpuThermalWriteCoordinator.WriteAsync(CpuThermalWriteCoordinator.LongPowerId, 100,
            (_, _) => { userCalls++; return Task.CompletedTask; });
        var gpuCalls = 0;
        await CpuThermalWriteCoordinator.WriteAsync(0x02020000, 200,
            (_, _) => { gpuCalls++; return Task.CompletedTask; }).WaitAsync(TimeSpan.FromSeconds(2));
        Require(gpuCalls == 1 && !userWrite.IsCompleted && userCalls == 0,
            "a stalled CPU owner serializes CPU mutations while unrelated GPU work bypasses this gate");
        release.TrySetResult();
        await ownerWrite;
        await MustFailAsync(() => userWrite, "queued CPU setter checks current ownership after the pending operation drains");
        Require(userCalls == 0, "queued rejected CPU mutation never calls hardware");
        await transport.RestoreIfOwnedAsync();
        _scenarios++;

        await MustFailAsync(() => CpuThermalWriteCoordinator.WriteAsync(CpuThermalWriteCoordinator.LongPowerId, 70,
            (_, _) => throw new InvalidOperationException("user setter failed")), "an unowned legacy setter may itself fail");
        (firmware, transport) = await CreateAsync();
        await transport.RestoreIfOwnedAsync();
        Require(!transport.IsEnabled, "a throwing legacy callback releases the shared CPU gate");
        _scenarios++;
    }

    private static async Task DeterministicRandomTracesAsync()
    {
        const int seed = 0x700083;
        var random = new Random(seed);
        for (var trace = 0; trace < 40; trace++)
        {
            var (firmware, transport) = await CreateAsync();
            for (var tick = 0; tick < 60; tick++)
            {
                var longWatts = random.Next(50, 71);
                var shortWatts = random.Next(Math.Max(60, longWatts), 126);
                var target = new CpuPowerEnvelope(longWatts, shortWatts);
                await transport.WriteAsync(target);
                Require(firmware.Power == target && await transport.ReadAsync() == target,
                    $"seed {seed}/trace {trace}/tick {tick}: physical target and verified readback agree");
                if (tick % 7 == 0) await transport.SetTemperatureLimitAsync(random.Next(85, 91));
                if (tick % 11 == 0)
                {
                    var writes = firmware.Writes.Count;
                    await MustFailAsync(() => CpuThermalWriteCoordinator.WriteAsync(
                        CpuThermalWriteCoordinator.LongPowerId, 135, firmware.WriteAsync, firmware.ReadAsync), "random legacy mutation cannot bypass active owner");
                    Require(firmware.Writes.Count == writes, "rejected random legacy mutation never calls physical setter");
                }
            }
            await transport.RestoreIfOwnedAsync();
            Require(firmware.Power == Baseline && firmware.Temperature == 100 && firmware.IllegalTransientWrites == 0,
                $"seed {seed}/trace {trace}: exact baseline restoration and all intermediate PL1 <= PL2");
            _scenarios++;
        }
        Console.WriteLine($"  deterministic random seed: {seed}; 40 traces x 60 target changes on observed hardware grids");
    }

    private static async Task ObservedCapabilityGridAsync()
    {
        // Walk every legal integral envelope, including values above the current user's
        // baseline, by simulating an independently discovered maximum baseline first.
        var (firmware, transport) = await CreateAsync(baseline: new(135, 195));
        var envelopes = 0;
        for (var longWatts = 50; longWatts <= 135; longWatts++)
        for (var shortWatts = Math.Max(60, longWatts); shortWatts <= 195; shortWatts++)
        {
            var target = new CpuPowerEnvelope(longWatts, shortWatts);
            await transport.WriteAsync(target);
            Require(firmware.Power == target, "the observed step-1 grid accepts each legal envelope exactly");
            envelopes++;
        }
        foreach (var invalid in new[] { new CpuPowerEnvelope(49, 60), new CpuPowerEnvelope(50, 59),
            new CpuPowerEnvelope(136, 195), new CpuPowerEnvelope(135, 196), new CpuPowerEnvelope(61, 60),
            new CpuPowerEnvelope(50.5, 60), new CpuPowerEnvelope(50, 60.5), new CpuPowerEnvelope(double.NaN, 100),
            new CpuPowerEnvelope(50, double.PositiveInfinity) })
        {
            var before = firmware.Writes.Count;
            await MustFailAsync(() => transport.WriteAsync(invalid), "invalid observed capability boundary");
            Require(firmware.Writes.Count == before, "invalid power targets never reach firmware");
        }
        await transport.RestoreIfOwnedAsync();
        Require(firmware.Power == new CpuPowerEnvelope(135, 195) && firmware.Temperature == 100
            && firmware.IllegalTransientWrites == 0, "the complete grid walk restores its maximum baseline without illegal transient ordering");
        _scenarios++;

        for (var originalTemperature = 85; originalTemperature <= 100; originalTemperature++)
        {
            firmware = new ScriptedFirmware();
            firmware.Values[CpuThermalWriteCoordinator.TemperatureId] = originalTemperature;
            transport = await ThermalCpuTransport.DiscoverAsync(firmware);
            await transport.EnableAsync(88);
            Require(firmware.Temperature == Math.Min(88, originalTemperature), "enable preserves stricter original temperature on observed grid");
            for (var temperature = 84; temperature <= 101; temperature++)
            {
                if (temperature >= 85 && temperature <= Math.Min(90, originalTemperature))
                {
                    await transport.SetTemperatureLimitAsync(temperature);
                    Require(firmware.Temperature == temperature, "legal observed firmware temperature is applied exactly");
                }
                else
                {
                    var before = firmware.Writes.Count;
                    await MustFailAsync(() => transport.SetTemperatureLimitAsync(temperature), "temperature outside active safety/baseline/grid bounds");
                    Require(firmware.Writes.Count == before, "invalid temperature does not call physical setter");
                }
            }
            await transport.RestoreIfOwnedAsync();
            Require(firmware.Temperature == originalTemperature, "each observed original temperature restores exactly");
            _scenarios++;
        }
        Console.WriteLine($"  observed hardware grid: {envelopes} legal power envelopes; every original temperature 85..100 step 1");
    }

    private static async Task DeterministicFaultTracesAsync()
    {
        const int seed = 0x83FF700;
        var random = new Random(seed);
        var injected = 0;
        for (var trace = 0; trace < 24; trace++)
        {
            var (firmware, transport) = await CreateAsync();
            for (var tick = 0; tick < 50; tick++)
            {
                firmware.ResetTrace();
                var operationNumber = random.Next(1, 26);
                var fault = random.Next(2) == 0 ? Fault.Before : Fault.After;
                var inject = random.Next(3) != 0;
                if (inject) firmware.Fault = operation => operation.Number == operationNumber ? fault : Fault.None;
                Exception? error = null;
                try
                {
                    if (random.Next(4) == 0) await transport.SetTemperatureLimitAsync(random.Next(85, 91));
                    else
                    {
                        var longWatts = random.Next(50, 71);
                        await transport.WriteAsync(new(longWatts, random.Next(Math.Max(60, longWatts), 126)));
                    }
                }
                catch (InvalidOperationException ex) { error = ex; }
                firmware.Fault = null;
                Require(firmware.FaultHits == 0 ? error is null : error is not null,
                    $"seed {seed}/trace {trace}/tick {tick}: only the selected physical failure rejects an otherwise legal request");
                if (firmware.FaultHits != 0)
                {
                    injected++;
                    await transport.RestoreIfOwnedAsync();
                    Require(firmware.Power == Baseline && firmware.Temperature == 100,
                        $"seed {seed}/trace {trace}/tick {tick}: arbitrary failed trace restores both old and attempted owned values");
                    await transport.EnableAsync(88);
                }
            }
            await transport.RestoreIfOwnedAsync();
            Require(firmware.Power == Baseline && firmware.Temperature == 100 && firmware.IllegalTransientWrites == 0,
                $"seed {seed}/trace {trace}: random faults preserve ordering and exact restoration");
            _scenarios++;
        }
        Console.WriteLine($"  deterministic fault seed: {seed}; 24 traces x 50 changes, {injected} injected I/O failures");
    }

    private enum Fault { None, Before, After, Mismatch, IgnoreWrite, ClampWrite }
    private readonly record struct Operation(int Number, char Kind, uint Id)
    {
        public override string ToString() => $"{Number}:{Kind}:{Id:X8}";
    }

    private sealed class ScriptedFirmware : IThermalCpuFirmware
    {
        public readonly Dictionary<uint, int> Values = new()
        {
            [CpuThermalWriteCoordinator.LongPowerId] = 70,
            [CpuThermalWriteCoordinator.ShortPowerId] = 125,
            [CpuThermalWriteCoordinator.TemperatureId] = 100,
            [0x05040000] = 55
        };
        public readonly List<Operation> Operations = [];
        public readonly List<(uint Id, int Value, int Mode)> Writes = [];
        public Func<Operation, Fault>? Fault;
        public Action<Operation>? After;
        public Func<Operation, Task>? BeforeAsync;
        public Func<Operation, Task>? AfterAsync;
        public int Mode = 255;
        public int FaultHits;
        public int IllegalTransientWrites;
        public CpuPowerEnvelope Power => new(Values[CpuThermalWriteCoordinator.LongPowerId], Values[CpuThermalWriteCoordinator.ShortPowerId]);
        public int Temperature => Values[CpuThermalWriteCoordinator.TemperatureId];

        public void ResetTrace() { Operations.Clear(); FaultHits = 0; }
        private async Task<(Operation Operation, Fault Fault)> StartAsync(char kind, uint id)
        {
            var operation = new Operation(Operations.Count + 1, kind, id);
            Operations.Add(operation);
            if (BeforeAsync is { } wait) await wait(operation);
            var fault = Fault?.Invoke(operation) ?? FaultMatrixTests.Fault.None;
            if (fault != FaultMatrixTests.Fault.None) FaultHits++;
            if (fault == FaultMatrixTests.Fault.Before) throw new InvalidOperationException("Injected fail-before " + operation);
            return (operation, fault);
        }

        private async Task FinishAsync(Operation operation, Fault fault)
        {
            After?.Invoke(operation);
            if (AfterAsync is { } wait) await wait(operation);
            if (fault == FaultMatrixTests.Fault.After) throw new InvalidOperationException("Injected fail-after " + operation);
        }

        public async Task<ThermalMachineIdentity> GetIdentityAsync()
        {
            var (operation, fault) = await StartAsync('I', 0);
            await FinishAsync(operation, fault);
            return new("LENOVO", "83NN", "S9CN15WW", "Intel Core i9-14900HX");
        }
        public async Task<IReadOnlyList<RangeCapability>> GetCapabilitiesAsync()
        {
            var (operation, fault) = await StartAsync('C', 0);
            await FinishAsync(operation, fault);
            return [new((CapabilityID)0x01020300, 120, 50, 135, 1), new(CapabilityID.CPULongTermPowerLimit, 70, 50, 135, 1),
            new((CapabilityID)0x01010300, 185, 60, 195, 1), new(CapabilityID.CPUShortTermPowerLimit, 125, 60, 195, 1),
            new((CapabilityID)0x01040300, 100, 85, 100, 1), new(CapabilityID.CPUTemperatureLimit, 100, 85, 100, 1)];
        }
        public async Task<IReadOnlyList<DiscreteCapability>> GetDiscreteCapabilitiesAsync()
        {
            var (operation, fault) = await StartAsync('D', 0);
            await FinishAsync(operation, fault);
            return [];
        }
        public async Task<int> GetModeAsync()
        {
            var (operation, fault) = await StartAsync('M', 0);
            var mode = Mode;
            await FinishAsync(operation, fault);
            return mode;
        }
        public async Task<int> ReadAsync(uint id)
        {
            var (operation, fault) = await StartAsync('R', id);
            var value = Values[id];
            await FinishAsync(operation, fault);
            return fault == FaultMatrixTests.Fault.Mismatch ? value + 1 : value;
        }
        public async Task WriteAsync(uint id, int value)
        {
            var (operation, fault) = await StartAsync('W', id);
            Writes.Add((id, value, Mode));
            if (fault != FaultMatrixTests.Fault.IgnoreWrite)
                Values[id] = fault == FaultMatrixTests.Fault.ClampWrite ? value + 1 : value;
            if (Power.LongWatts > Power.ShortWatts)
            {
                IllegalTransientWrites++;
                throw new InvalidOperationException("Invalid transient PL1 > PL2");
            }
            await FinishAsync(operation, fault);
        }
        public void EnsureExclusiveProcess() { }
    }
}
