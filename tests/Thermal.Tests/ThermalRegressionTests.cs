using System;
using System.Diagnostics;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Thermal;

public static class ThermalRegressionTests
{
    public static async Task<int> RunAsync()
    {
        var checks = 0;
        void Check(bool condition, string name)
        {
            if (!condition) throw new Exception("Thermal regression: " + name);
            checks++;
        }

        var cap = new CpuPowerCapability(10, 100, 10, 150, 1);
        var decimalCap = new CpuPowerCapability(10, 100, 10, 150, .1);
        var decimalTarget = new CpuPowerEnvelope(10.2, 10.7);
        Check(decimalCap.RoundDown(decimalTarget) == decimalTarget, "decimal grid preserves aligned values");
        Check(decimalCap.IsAligned(decimalTarget), "decimal command accepted by executor validation");
        var rounded = decimalCap.RoundDown(new(10.26, 10.78));
        Check(rounded == new CpuPowerEnvelope(10.2, 10.7), "decimal floor respects ceilings");
        Check(decimalCap.RoundDown(rounded) == rounded, "decimal rounding is idempotent");
        Check(decimalCap.RoundDown(new(Math.BitDecrement(10.2), 10.7)).LongWatts <= Math.BitDecrement(10.2),
            "decimal conversion never rounds above a strict ceiling");
        var coupled = new CpuPowerCapability(11, 100, 10, 150, 4);
        Check(coupled.RoundDown(new(19, 19)) == new CpuPowerEnvelope(15, 18), "coupled grids find legal lower long limit");
        Check(!coupled.IsAligned(new(19, 19)), "unsupported exact grid rejected");
        Check(!new CpuPowerCapability(10, 100, 10, 150, double.NaN).Contains(new(20, 30)), "NaN step rejected");
        Check(!new CpuPowerCapability(10, 100, 10, 150, 1e-40).Contains(new(20, 30)), "unrepresentable step rejected");
        Check(!new CpuPowerCapability(10, double.MaxValue, 10, double.MaxValue, 1).Contains(new(20, 30)), "unrepresentable bound rejected");
        Check(!new CpuPowerCapability(1, 1e28, 1, 1e28, 1e-28).IsAligned(new(1e28, 1e28)), "overflowing grid rejected without exception");

        var strict = PowerConstraintArbiter.Resolve(new(80, 100),
            [new(new(60, 90), 2, 3, 800), new(new(70, 70), 2, 3, 700)], cap, 0, 2, 3, 900);
        Check(strict.Target == new CpuPowerEnvelope(60, 70) && strict.ExpiresAt == 700, "all constraints and earliest deadline respected");
        try
        {
            PowerConstraintArbiter.Resolve(new(80, 100), [new(new(60, 90), 2, 4, 800)], cap, 0, 2, 3, 900);
            Check(false, "old mode accepted");
        }
        catch (InvalidOperationException) { Check(true, "old mode rejected"); }

        var options = new ThermalOptions
        {
            TargetC = 88, WarmC = 85, RecoverC = 82, WarmReduction = .15,
            Reduction = .20, RecoveryStep = .01, RecoveryMilliseconds = 5000, AdjustmentMilliseconds = 2000,
        };
        var policy = new ThermalPolicy(options);
        ThermalDecision Eval(double temperature, long now, ThermalDecision? previous = null) =>
            policy.Evaluate(new(temperature, now), new(temperature, now), now, previous);
        var warm = Eval(85, 0);
        Check(warm.Phase == ThermalPhase.Warm && Math.Abs(warm.PowerFraction - .85) < .000001, "custom preemptive threshold and step");
        var firstHot = Eval(88, 2000, warm);
        Check(firstHot.Phase == ThermalPhase.Hot && Math.Abs(firstHot.PowerFraction - .65) < .000001, "custom hot threshold and step");
        var jitterWarm = Eval(87, 2500, firstHot);
        var jitterHot = Eval(88, 3000, jitterWarm);
        Check(jitterHot.PowerFraction == firstHot.PowerFraction, "threshold jitter cannot bypass adjustment interval");
        var tighter = Eval(88, 4000, jitterHot);
        Check(tighter.PowerFraction < jitterHot.PowerFraction, "reduction resumes on configured interval");
        var cooling = Eval(80, 5000, tighter);
        Check(Eval(80, 9999, cooling).PowerFraction == cooling.PowerFraction, "custom recovery dwell");
        Check(Math.Abs(Eval(80, 10000, cooling).PowerFraction - cooling.PowerFraction - .01) < .000001, "custom slow recovery step");
        var badState = firstHot with { PowerFraction = double.NaN };
        var badResult = Eval(80, 5000, badState);
        Check(badResult.Phase == ThermalPhase.SensorFault && badResult.PowerFraction == options.MinimumFraction,
            "invalid previous state fails closed without NaN propagation");
        var regressed = Eval(80, 1000, firstHot);
        Check(regressed.Phase == ThermalPhase.SensorFault && regressed.LastAdjustment <= regressed.EvaluatedAt,
            "regressed clock rebases adjustment timer");
        var newCool = Eval(80, 2000, regressed);
        Check(Eval(80, 7000, newCool).PowerFraction > newCool.PowerFraction, "recovery can resume after fresh post-regression dwell");
        try { _ = new ThermalPolicy(options with { WarmReduction = double.NaN }); Check(false, "NaN warm step accepted"); }
        catch (ArgumentOutOfRangeException) { Check(true, "invalid configurable warm step rejected"); }

        Check(policy.Evaluate(new(88, 0), new(double.NaN, 0), 0).Phase == ThermalPhase.SensorFault,
            "dual-source API still requires core maximum");
        Check(policy.Evaluate(new(double.NaN, 0), new(88, 0), 0).Phase == ThermalPhase.SensorFault,
            "dual-source API still requires package reading");
        var firmwareHot = policy.EvaluateFirmware(new(88, 0), 0);
        Check(firmwareHot.Phase == ThermalPhase.Hot && firmwareHot.PowerFraction < 1,
            "explicit firmware aggregate API controls from one genuine reading");
        foreach (var invalidFirmware in new[] { double.NaN, double.PositiveInfinity, 0, -1, 150 })
            Check(policy.EvaluateFirmware(new(invalidFirmware, 0), 0).Phase == ThermalPhase.SensorFault,
                "invalid firmware source fails closed");
        Check(policy.EvaluateFirmware(new(80, 0), 3000, firmwareHot).Phase == ThermalPhase.SensorFault,
            "firmware aggregate has its own freshness deadline");
        Check(policy.EvaluateFirmware(new(80, 1000), 999, firmwareHot).Phase == ThermalPhase.SensorFault,
            "future firmware timestamp rejected");
        var firmwareCool = policy.EvaluateFirmware(new(80, 1000), 1000, firmwareHot);
        Check(policy.EvaluateFirmware(new(80, 5999), 5999, firmwareCool).PowerFraction == firmwareCool.PowerFraction,
            "firmware aggregate recovery respects dwell");
        Check(policy.EvaluateFirmware(new(80, 6000), 6000, firmwareCool).PowerFraction > firmwareCool.PowerFraction,
            "firmware aggregate recovery resumes gradually after dwell");

        PowerCommand Command(CpuPowerEnvelope? target = null, long expiry = 100000) => new(target ?? new(50, 70), 1, 1, expiry);
        var fast = new TestTransport();
        var fastExecutor = new HardwareCommandExecutor(fast, cap, () => 0, () => (1, 1));
        Check((await fastExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Verified, "ordinary write/readback verified");
        Check((await fastExecutor.ApplyAsync(Command(new(50.5, 70)), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Rejected && fast.Writes == 1,
            "unaligned command rejected before write");
        using (var cancelled = new CancellationTokenSource())
        {
            cancelled.Cancel();
            try { await fastExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1), cancelled.Token); Check(false, "cancelled command accepted"); }
            catch (OperationCanceledException) { Check(fast.Writes == 1, "pre-cancelled command never writes"); }
        }

        var blocked = new TestTransport(blockWrite: true);
        var blockedExecutor = new HardwareCommandExecutor(blocked, cap, () => 0, () => (1, 1));
        Check((await blockedExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(30))).Status == ApplyStatus.UnknownCompletion,
            "active I/O timeout has unknown completion");
        var watch = Stopwatch.StartNew();
        var queuedTimeout = await blockedExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(40)).WaitAsync(TimeSpan.FromSeconds(2));
        Check(queuedTimeout.Status == ApplyStatus.Rejected && blocked.Writes == 1 && watch.Elapsed < TimeSpan.FromSeconds(1),
            "queued timeout is bounded and cannot overlap timed-out physical call");
        var deadlineWatch = Stopwatch.StartNew();
        Check((await blockedExecutor.ApplyAsync(Command(expiry: 30), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Rejected
            && deadlineWatch.Elapsed < TimeSpan.FromMilliseconds(500), "queued command deadline bounds admission");
        using (var cancelQueued = new CancellationTokenSource())
        {
            var pending = blockedExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1), cancelQueued.Token);
            cancelQueued.Cancel();
            try { await pending; Check(false, "queued cancellation ignored"); }
            catch (OperationCanceledException) { Check(blocked.Writes == 1, "queued cancellation does not write"); }
        }
        blocked.ReleaseWrite.TrySetResult();
        Check((await blockedExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Verified,
            "executor recovers after late physical completion");

        var cancelActiveTransport = new TestTransport(blockWrite: true);
        var cancelActiveExecutor = new HardwareCommandExecutor(cancelActiveTransport, cap, () => 0, () => (1, 1));
        using (var cancelActive = new CancellationTokenSource())
        {
            var active = cancelActiveExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1), cancelActive.Token);
            await cancelActiveTransport.WriteStarted.Task.WaitAsync(TimeSpan.FromSeconds(1));
            cancelActive.Cancel();
            Check((await active).Status == ApplyStatus.UnknownCompletion, "active cancellation reports uncertain physical state");
            Check((await cancelActiveExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(30))).Status == ApplyStatus.Rejected
                && cancelActiveTransport.Writes == 1, "active cancellation retains hardware ownership");
            cancelActiveTransport.ReleaseWrite.TrySetResult();
            Check((await cancelActiveExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Verified,
                "ownership released after cancelled call physically completes");
        }

        var readBlocked = new TestTransport(blockRead: true);
        var readExecutor = new HardwareCommandExecutor(readBlocked, cap, () => 0, () => (1, 1));
        Check((await readExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(30))).Status == ApplyStatus.UnknownCompletion,
            "readback is included in physical operation timeout");
        Check((await readExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(30))).Status == ApplyStatus.Rejected && readBlocked.Writes == 1,
            "readback retains exclusive ownership");
        readBlocked.ReleaseRead.TrySetResult();
        Check((await readExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Verified, "readback late completion releases ownership");

        foreach (var writeFault in new[] { true, false })
        {
            var faulty = new TestTransport { FailWrite = writeFault, FailRead = !writeFault };
            var executor = new HardwareCommandExecutor(faulty, cap, () => 0, () => (1, 1));
            Check((await executor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Failed, "transport failure is explicit");
            faulty.FailWrite = faulty.FailRead = false;
            Check((await executor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Verified, "transport failure does not leak gate");
        }
        var mismatch = new TestTransport { OverrideReadback = new(40, 60) };
        Check((await new HardwareCommandExecutor(mismatch, cap, () => 0, () => (1, 1))
            .ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Failed, "readback mismatch is never reported verified");

        long mode = 1;
        var epochTransport = new TestTransport(blockWrite: true);
        var epochExecutor = new HardwareCommandExecutor(epochTransport, cap, () => 0, () => (1, mode));
        var oldModeOperation = epochExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1));
        await epochTransport.WriteStarted.Task.WaitAsync(TimeSpan.FromSeconds(1));
        mode = 2;
        epochTransport.ReleaseWrite.TrySetResult();
        Check((await oldModeOperation).Status == ApplyStatus.Failed, "mode changes during I/O invalidate verification");
        Check((await epochExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Rejected && epochTransport.Writes == 1,
            "superseded mode rejected before write");

        using (var synchronousRelease = new ManualResetEventSlim())
        {
            var synchronous = new SynchronouslyBlockingTransport(synchronousRelease);
            var synchronousExecutor = new HardwareCommandExecutor(synchronous, cap, () => 0, () => (1, 1));
            try
            {
                Check((await synchronousExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(30))
                    .WaitAsync(TimeSpan.FromSeconds(2))).Status == ApplyStatus.UnknownCompletion,
                    "synchronous vendor prefix cannot block timeout handling on caller thread");
                Check((await synchronousExecutor.ApplyAsync(Command(), TimeSpan.FromMilliseconds(30))).Status == ApplyStatus.Rejected
                    && synchronous.Writes == 1, "synchronous stalled vendor call retains ownership");
            }
            finally { synchronousRelease.Set(); }
            Check((await synchronousExecutor.ApplyAsync(Command(), TimeSpan.FromSeconds(1))).Status == ApplyStatus.Verified,
                "synchronous vendor completion safely releases gate");
        }

        checks += await ThermalSupervisorTests.RunAsync();
        Console.WriteLine($"PASS: {checks} thermal regression checks");
        return checks;
    }

    private sealed class SynchronouslyBlockingTransport(ManualResetEventSlim release) : ICpuPowerTransport
    {
        public int Writes;
        private CpuPowerEnvelope _value = new(10, 10);
        public Task WriteAsync(CpuPowerEnvelope target)
        {
            Interlocked.Increment(ref Writes);
            release.Wait();
            _value = target;
            return Task.CompletedTask;
        }
        public Task<CpuPowerEnvelope> ReadAsync() => Task.FromResult(_value);
    }

    private sealed class TestTransport(bool blockWrite = false, bool blockRead = false) : ICpuPowerTransport
    {
        public readonly TaskCompletionSource ReleaseWrite = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource ReleaseRead = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public readonly TaskCompletionSource WriteStarted = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public int Writes;
        public bool FailWrite;
        public bool FailRead;
        public CpuPowerEnvelope? OverrideReadback;
        private CpuPowerEnvelope _value = new(10, 10);

        public async Task WriteAsync(CpuPowerEnvelope target)
        {
            Interlocked.Increment(ref Writes);
            WriteStarted.TrySetResult();
            if (FailWrite) throw new InvalidOperationException("write failed");
            if (blockWrite) await ReleaseWrite.Task;
            _value = target;
        }

        public async Task<CpuPowerEnvelope> ReadAsync()
        {
            if (FailRead) throw new InvalidOperationException("read failed");
            if (blockRead) await ReleaseRead.Task;
            return OverrideReadback ?? _value;
        }
    }
}
