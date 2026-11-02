using System;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Thermal;

internal static class ThermalSupervisorTests
{
    public static async Task<int> RunAsync()
    {
        var checks = 0;
        void Check(bool condition, string name)
        {
            if (!condition) throw new Exception("Supervisor regression: " + name);
            checks++;
        }

        var capability = new CpuPowerCapability(10, 100, 10, 150, 1);
        var baseline = new CpuPowerEnvelope(80, 100);
        var options = new ThermalControlOptions
        {
            PredictionMilliseconds = 0,
            Policy = new ThermalOptions { TargetC = 88, WarmC = 85, RecoverC = 82, RecoveryMilliseconds = 1000, AdjustmentMilliseconds = 100 },
            CommandTimeout = TimeSpan.FromMilliseconds(100),
            CommandLifetimeMilliseconds = 10000,
        };
        long now = 0;
        long sampleTick = 0;
        double temperature = 80;
        long mode = 1;
        (TemperatureReading Package, TemperatureReading CoreMax) ReadTemperatures() =>
            (new(temperature, sampleTick), new(temperature, sampleTick));
        void Sample(double value, long tick) { temperature = value; now = sampleTick = tick; }
        ThermalControlSupervisor Create(RecordingTransport transport, ThermalControlOptions? settings = null, HardwareCommandExecutor? executor = null) =>
            new(transport, capability, baseline, ReadTemperatures, () => now, () => (1, mode), settings ?? options, executor);

        var normalTransport = new RecordingTransport(baseline);
        var normal = Create(normalTransport);
        var normalResult = await normal.EvaluateAsync();
        Check(normalResult.Status == ThermalControlStatus.Holding && normalTransport.Writes == 0, "normal state avoids redundant writes");
        Sample(80, 1000);
        Check((await normal.EvaluateAsync()).Apply?.Status == ApplyStatus.Verified && normalTransport.Reads == 1
            && normalTransport.Writes == 0, "unchanged target receives periodic readback without rewriting limits");
        normalTransport.Value = new(90, 110);
        Sample(80, 2000);
        var interference = await normal.EvaluateAsync();
        Check(interference.Status == ThermalControlStatus.ActuatorFault && normal.HasActuatorFault
            && normalTransport.Writes == 0, "external power override invalidates controlled status");

        Sample(88, 0);
        var hotTransport = new RecordingTransport(baseline);
        var hot = Create(hotTransport);
        var restricted = await hot.EvaluateAsync();
        Check(restricted.Status == ThermalControlStatus.Verified && restricted.Target == new CpuPowerEnvelope(72, 90), "hot temperature maps relative fraction to legal CPU limits");
        Sample(double.NaN, 100);
        var sensorFault = await hot.EvaluateAsync();
        Check(sensorFault.Status == ThermalControlStatus.SensorFault && sensorFault.Target == new CpuPowerEnvelope(10, 10)
            && hotTransport.Value == new CpuPowerEnvelope(10, 10), "invalid telemetry requests physical capability minimum");
        Sample(80, 200);
        Check((await hot.EvaluateAsync()).Target == new CpuPowerEnvelope(10, 10), "fresh telemetry does not jump from physical minimum to fractional minimum");
        Sample(80, 1199);
        Check((await hot.EvaluateAsync()).Target == new CpuPowerEnvelope(10, 10), "physical minimum held through recovery dwell");
        Sample(80, 1200);
        var slowPhysicalRecovery = await hot.EvaluateAsync();
        Check(slowPhysicalRecovery.Target == new CpuPowerEnvelope(11, 12), "post-fault physical recovery follows configured slow ramp");
        Sample(80, 1300);
        Check((await hot.EvaluateAsync()).Target == new CpuPowerEnvelope(13, 14), "sub-step recovery budget accumulates across ticks");

        Sample(88, 0);
        var writeFailureTransport = new RecordingTransport(baseline) { FailWrite = true };
        var writeFailure = Create(writeFailureTransport);
        Check((await writeFailure.EvaluateAsync()).Status == ThermalControlStatus.ActuatorFault && writeFailure.HasActuatorFault,
            "write failure is explicit and latched");
        writeFailureTransport.FailWrite = false;
        Sample(80, 100);
        Check((await writeFailure.EvaluateAsync()).Target == new CpuPowerEnvelope(72, 90), "retry can verify stricter failed target");
        Sample(80, 1100);
        var latched = await writeFailure.EvaluateAsync();
        Check(latched.Target!.LongWatts <= 72 && latched.Target.ShortWatts <= 90
            && latched.Status == ThermalControlStatus.ActuatorFault, "successful retry does not automatically clear fault or relax limit");

        Sample(88, 0);
        var recoveryFailureTransport = new RecordingTransport(baseline);
        var recoveryFailure = Create(recoveryFailureTransport);
        await recoveryFailure.EvaluateAsync();
        Sample(80, 100);
        await recoveryFailure.EvaluateAsync();
        recoveryFailureTransport.FailWrite = true;
        Sample(80, 1100);
        Check((await recoveryFailure.EvaluateAsync()).Status == ThermalControlStatus.ActuatorFault, "failed recovery write is latched");
        recoveryFailureTransport.FailWrite = false;
        Sample(80, 1200);
        var heldAfterRecoveryFailure = await recoveryFailure.EvaluateAsync();
        Check(heldAfterRecoveryFailure.Target == new CpuPowerEnvelope(72, 90), "failed relaxation cannot raise the last verified ceiling on retry");

        Sample(80, 0);
        mode = 1;
        var modeTransport = new RecordingTransport(baseline);
        var owned = Create(modeTransport);
        mode = 2;
        Sample(100, 500);
        Check((await owned.EvaluateAsync()).Status == ThermalControlStatus.Superseded && modeTransport.Writes == 0,
            "changed mode requires new capability/baseline before any write");
        mode = 1;

        Sample(84, 0);
        var predictionTransport = new RecordingTransport(baseline);
        var predictive = Create(predictionTransport, options with { PredictionMilliseconds = 1000 });
        await predictive.EvaluateAsync();
        Sample(86, 1000);
        var predicted = await predictive.EvaluateAsync();
        Check(predicted.Decision?.Phase == ThermalPhase.Hot && predicted.PredictedRiseC == 2
            && predicted.ObservedTemperatureC == 86, "rising-temperature prediction reserves headroom before 88 degrees");
        now = 1050; // Same hardware sample, faster supervisor tick within adjustment interval.
        var cachedPrediction = await predictive.EvaluateAsync();
        Check(cachedPrediction.PredictedRiseC == 2 && cachedPrediction.Target == predicted.Target,
            "repeated cached samples neither amplify nor discard the measured rising trend");

        Sample(88, 0);
        var blockingTransport = new RecordingTransport(baseline, blockWrite: true);
        var blocking = Create(blockingTransport);
        var active = blocking.EvaluateAsync();
        Check((await blocking.EvaluateAsync()).Status == ThermalControlStatus.Busy, "overlapping ticks do not race policy or hardware");
        blockingTransport.ReleaseWrite.TrySetResult();
        Check((await active).Status == ThermalControlStatus.Verified && blockingTransport.Writes == 1, "single in-flight tick completes once");

        Sample(88, 0);
        var unknownTransport = new RecordingTransport(baseline, blockWrite: true);
        var sharedExecutor = new HardwareCommandExecutor(unknownTransport, capability, () => now, () => (1, mode));
        var unknownOptions = options with { CommandTimeout = TimeSpan.FromMilliseconds(30) };
        var unknown = Create(unknownTransport, unknownOptions, sharedExecutor);
        Check((await unknown.EvaluateAsync()).Status == ThermalControlStatus.UnknownCompletion && unknown.HasActuatorFault,
            "late write yields explicit unknown completion and fault latch");
        var restarted = Create(unknownTransport, unknownOptions, sharedExecutor);
        Check((await restarted.EvaluateAsync()).Status == ThermalControlStatus.ActuatorFault && unknownTransport.Writes == 1,
            "explicit restart sharing executor cannot overlap an older unknown physical operation");
        unknownTransport.ReleaseWrite.TrySetResult();

        Sample(80, 0);
        var throwingTelemetry = new ThermalControlSupervisor(new RecordingTransport(baseline), capability, baseline,
            () => throw new InvalidOperationException("telemetry unavailable"), () => now, () => (1, 1), options);
        Check((await throwingTelemetry.EvaluateAsync()).Target == new CpuPowerEnvelope(10, 10), "telemetry callback failure requests minimum instead of escaping loop");
        var offsetCap = new CpuPowerCapability(11, 100, 10, 150, 4);
        Check(offsetCap.GetMinimumEnvelope() == new CpuPowerEnvelope(11, 14), "minimum envelope obeys coupled grid origins");

        Sample(80, 0);
        TemperatureReading firmwareReading = new(88, 0);
        var firmwareTransport = new RecordingTransport(baseline);
        var firmwareSupervisor = new ThermalControlSupervisor(firmwareTransport, capability, baseline,
            () => (new(double.NaN, now), new(double.NaN, now)), () => now, () => (1, 1), options,
            getFirmwareTemperature: () => firmwareReading);
        var firmwareOnly = await firmwareSupervisor.EvaluateAsync();
        Check(firmwareOnly.Status == ThermalControlStatus.Verified && firmwareOnly.Target == new CpuPowerEnvelope(72, 90)
            && firmwareOnly.Source == ThermalTemperatureSource.LenovoFirmwareAggregate && firmwareOnly.ObservedTemperatureC == 88,
            "explicit genuine firmware fallback is identified and controls without fabricated package/core readings");
        now = 2100; // The cache still contains the original tick=0 reading.
        var staleFirmware = await firmwareSupervisor.EvaluateAsync();
        Check(staleFirmware.Status == ThermalControlStatus.SensorFault && staleFirmware.Target == new CpuPowerEnvelope(10, 10)
            && staleFirmware.ObservedTemperatureC is null && staleFirmware.Source == ThermalTemperatureSource.LenovoFirmwareAggregate,
            "stale firmware cache cannot refresh its own timestamp or prevent minimum restriction");
        now = 2200;
        firmwareReading = new(80, now);
        Check((await firmwareSupervisor.EvaluateAsync()).Target == new CpuPowerEnvelope(10, 10),
            "fresh firmware recovery holds physical minimum through dwell");
        now = 3200;
        firmwareReading = new(80, now);
        Check((await firmwareSupervisor.EvaluateAsync()).Target == new CpuPowerEnvelope(11, 12),
            "firmware fallback shares slow physical recovery ramp");
        now = 3300;
        firmwareReading = new(double.NaN, now);
        Check((await firmwareSupervisor.EvaluateAsync()).Target == new CpuPowerEnvelope(10, 10),
            "invalid firmware aggregate fails closed at physical minimum");

        Sample(80, 0);
        var noFallbackTransport = new RecordingTransport(baseline);
        var noFallback = new ThermalControlSupervisor(noFallbackTransport, capability, baseline,
            () => (new(80, now), new(double.NaN, now)), () => now, () => (1, 1), options);
        Check((await noFallback.EvaluateAsync()).Status == ThermalControlStatus.SensorFault,
            "supervisor without explicit firmware delegate preserves strict dual-source contract");
        var firmwareReads = 0;
        var preferredDual = new ThermalControlSupervisor(new RecordingTransport(baseline), capability, baseline,
            () => (new(80, now), new(90, now)), () => now, () => (1, 1), options,
            getFirmwareTemperature: () => { firmwareReads++; return new(60, now); });
        var dualResult = await preferredDual.EvaluateAsync();
        Check(dualResult.Source == ThermalTemperatureSource.PackageAndCoreMaximum && dualResult.ObservedTemperatureC == 90
            && firmwareReads == 0, "fresh dual-source hotspot remains preferred over optional firmware aggregate");

        Sample(80, 0);
        var firmwarePredictReading = new TemperatureReading(84, 0);
        var firmwarePredict = new ThermalControlSupervisor(new RecordingTransport(baseline), capability, baseline,
            () => (new(double.NaN, now), new(double.NaN, now)), () => now, () => (1, 1),
            options with { PredictionMilliseconds = 1000 }, getFirmwareTemperature: () => firmwarePredictReading);
        await firmwarePredict.EvaluateAsync();
        now = 1000;
        firmwarePredictReading = new(86, now);
        var firmwarePrediction = await firmwarePredict.EvaluateAsync();
        Check(firmwarePrediction.PredictedRiseC == 2 && firmwarePrediction.Decision?.Phase == ThermalPhase.Hot,
            "firmware aggregate uses independent timestamped rising-temperature prediction");

        Sample(80, 0);
        var dualAvailable = true;
        var switching = new ThermalControlSupervisor(new RecordingTransport(baseline), capability, baseline,
            () => dualAvailable ? (new(80, now), new(80, now)) : (new(double.NaN, now), new(double.NaN, now)),
            () => now, () => (1, 1), options with { PredictionMilliseconds = 1000 },
            getFirmwareTemperature: () => new(84, now));
        await switching.EvaluateAsync();
        now = 500;
        dualAvailable = false;
        var sourceSwitch = await switching.EvaluateAsync();
        Check(sourceSwitch.Source == ThermalTemperatureSource.LenovoFirmwareAggregate && sourceSwitch.PredictedRiseC == 0
            && sourceSwitch.Decision?.CoolSince is null, "source change resets prediction and continuous cooling evidence");

        Sample(80, 0);
        var firmwareException = new ThermalControlSupervisor(new RecordingTransport(baseline), capability, baseline,
            () => (new(double.NaN, now), new(double.NaN, now)), () => now, () => (1, 1), options,
            getFirmwareTemperature: () => throw new InvalidOperationException("firmware unavailable"));
        Check((await firmwareException.EvaluateAsync()).Target == new CpuPowerEnvelope(10, 10),
            "firmware delegate failure requests minimum and does not escape control tick");

        Console.WriteLine($"PASS: {checks} supervisor regression checks");
        return checks;
    }

    private sealed class RecordingTransport(CpuPowerEnvelope initial, bool blockWrite = false) : ICpuPowerTransport
    {
        public CpuPowerEnvelope Value = initial;
        public int Writes;
        public int Reads;
        public bool FailWrite;
        public readonly TaskCompletionSource ReleaseWrite = new(TaskCreationOptions.RunContinuationsAsynchronously);

        public async Task WriteAsync(CpuPowerEnvelope target)
        {
            Writes++;
            if (FailWrite) throw new InvalidOperationException("write failed");
            if (blockWrite) await ReleaseWrite.Task;
            Value = target;
        }

        public Task<CpuPowerEnvelope> ReadAsync()
        {
            Reads++;
            return Task.FromResult(Value);
        }
    }
}
