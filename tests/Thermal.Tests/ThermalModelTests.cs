using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Thermal;

public static class ThermalModelTests
{
    public static async Task<int> RunAsync()
    {
        int scenarios = 0;
        void Check(bool condition, string name)
        {
            if (!condition) throw new InvalidOperationException(name);
            scenarios++;
        }
        var capability = new CpuPowerCapability(50, 135, 60, 195, 1);
        var baseline = new CpuPowerEnvelope(120, 185);
        long now = 10000;
        foreach (var sample in new (double Package, double Core, double Firmware)[]
            { (double.NaN, 100, 60), (99, double.NaN, 62), (double.NaN, 60, 89) })
        {
            var transport = new MemoryTransport(baseline);
            var supervisor = new ThermalControlSupervisor(transport, capability, baseline,
                () => (new(sample.Package, now), new(sample.Core, now)), () => now, () => (1, 1),
                new() { PredictionMilliseconds = 0 }, getFirmwareTemperature: () => new(sample.Firmware, now));
            var result = await supervisor.EvaluateAsync();
            var maximum = new[] { sample.Package, sample.Core, sample.Firmware }.Where(double.IsFinite).Max();
            Check(result.ObservedTemperatureC == maximum && result.Decision?.Phase == ThermalPhase.Hot
                && result.Target!.LongWatts < baseline.LongWatts && result.Source == ThermalTemperatureSource.FirmwareWithPartialHardware,
                "A fresh partial hardware hotspot must not be hidden by a cooler firmware aggregate");
        }

        var staleHardware = new ThermalControlSupervisor(new MemoryTransport(baseline), capability, baseline,
            () => (new(110, now - 3000), new(double.NaN, now)), () => now, () => (1, 1),
            new() { PredictionMilliseconds = 0 }, getFirmwareTemperature: () => new(60, now));
        var staleResult = await staleHardware.EvaluateAsync();
        Check(staleResult.Source == ThermalTemperatureSource.LenovoFirmwareAggregate && staleResult.ObservedTemperatureC == 60,
            "Expired partial hardware data cannot replace a fresh firmware reading");

        TemperatureReading package = new(double.NaN, now), core = new(100, now), firmware = new(60, now);
        var partialCooling = new ThermalControlSupervisor(new MemoryTransport(baseline), capability, baseline,
            () => (package, core), () => now, () => (1, 1), new() { PredictionMilliseconds = 0 },
            getFirmwareTemperature: () => firmware);
        var previous = await partialCooling.EvaluateAsync();
        for (int i = 1; i <= 120; i++)
        {
            now += 250;
            package = new(double.NaN, now); core = new(100, now); firmware = new(60, now);
            var next = await partialCooling.EvaluateAsync();
            if (next.Target!.LongWatts > previous.Target!.LongWatts || next.Target.ShortWatts > previous.Target.ShortWatts)
                throw new InvalidOperationException("Cool aggregate authorized recovery over a continuously hot partial hardware source");
            previous = next;
        }
        Check(previous.Decision?.Phase == ThermalPhase.Hot && previous.Decision.CoolSince is null,
            "Continuous known partial hotspot never authorizes recovery from a cooler aggregate");

        scenarios += EnumeratePowerGrids();
        scenarios += await SimulateLongTracesAsync();

        Console.WriteLine($"PASS: {scenarios} thermal model scenarios");
        return scenarios;
    }

    private static int EnumeratePowerGrids()
    {
        var random = new Random(0x509088);
        var steps = new[] { .1m, .2m, .25m, .5m, 1m, 2m, 3m, 5m };
        int cases = 0;
        for (int index = 0; index < 500; index++)
        {
            var step = steps[random.Next(steps.Length)];
            var minLong = random.Next(10, 40) / 2m;
            var minShort = random.Next(10, 40) / 2m;
            var longs = Enumerable.Range(0, random.Next(3, 15)).Select(i => minLong + i * step).ToArray();
            var shorts = Enumerable.Range(0, random.Next(3, 15)).Select(i => minShort + i * step).ToArray();
            var capability = new CpuPowerCapability((double)minLong, (double)longs[^1], (double)minShort, (double)shorts[^1], (double)step);
            var legal = (from longWatts in longs from shortWatts in shorts where longWatts <= shortWatts
                select new CpuPowerEnvelope((double)longWatts, (double)shortWatts)).ToArray();
            if (legal.Length == 0) continue;
            foreach (var target in legal)
                if (!capability.IsAligned(target) || capability.RoundDown(target) != target)
                    throw new InvalidOperationException($"Grid roundtrip failed at seed index {index}: {target}");
            for (int test = 0; test < 12; test++)
            {
                var maxLong = Math.Min(longs[^1], shorts[^1]);
                var longCeiling = minLong + (maxLong - minLong) * random.Next(1001) / 1000m;
                var shortFloor = Math.Max(minShort, longCeiling);
                var shortCeiling = shortFloor + (shorts[^1] - shortFloor) * random.Next(1001) / 1000m;
                var ceiling = new CpuPowerEnvelope((double)longCeiling, (double)shortCeiling);
                var expected = legal.Where(x => x.LongWatts <= ceiling.LongWatts && x.ShortWatts <= ceiling.ShortWatts)
                    .OrderByDescending(x => x.ShortWatts).ThenByDescending(x => x.LongWatts).FirstOrDefault();
                CpuPowerEnvelope? actual = null;
                try { actual = capability.RoundDown(ceiling); }
                catch (ArgumentException) when (expected is null) { }
                if (expected != actual)
                    throw new InvalidOperationException($"Brute-force grid disagrees at seed index {index}: ceiling={ceiling}, expected={expected}, actual={actual}");
                cases++;
            }
        }
        Console.WriteLine($"GRID MODEL: {cases} seeded ceilings, brute-force discrete feasible envelopes (not physical firmware)");
        return cases;
    }

    private static async Task<int> SimulateLongTracesAsync()
    {
        const int profileCount = 8;
        const int ticksPerProfile = 14400; // One simulated hour per profile, 250ms cadence.
        var capability = new CpuPowerCapability(50, 135, 60, 195, 1);
        var baseline = new CpuPowerEnvelope(120, 185);
        long evaluated = 0;
        for (int profile = 0; profile < profileCount; profile++)
        {
            long now = 10000;
            long mode = 1;
            TemperatureReading package = new(65, now), core = new(67, now), firmware = new(64, now);
            var options = new ThermalControlOptions { PredictionMilliseconds = 0 };
            var transport = new MemoryTransport(baseline);
            var supervisor = new ThermalControlSupervisor(transport, capability, baseline, () => (package, core),
                () => now, () => (1, mode), options, getFirmwareTemperature: () => firmware);
            CpuPowerEnvelope last = baseline;
            CpuPowerEnvelope? faultCeiling = null;
            long? coolSince = null;
            ThermalTemperatureSource lastSource = ThermalTemperatureSource.Unavailable;
            long previousNow = now;
            int writesAtSupersession = -1;
            for (int tick = 0; tick < ticksPerProfile; tick++)
            {
                now += 250;
                double temperature = profile switch
                {
                    0 => 65 + 4 * Math.Sin(tick / 80d),
                    1 => tick % 1600 < 800 ? 80 + (tick % 800) / 40d : 75,
                    2 => 100,
                    3 => tick % 600 < 120 ? 96 : 75,
                    4 => 70,
                    5 => tick % 700 < 200 ? 94 : 78,
                    6 => 65,
                    _ => 68 + 18 * Math.Sin(tick / 100d)
                };
                package = new(temperature, now); core = new(temperature + 1, now); firmware = new(temperature - 2, now);
                if (profile == 3 && tick % 400 < 20)
                    package = core = firmware = new(double.NaN, now);
                if (profile == 4)
                {
                    package = new(double.NaN, now);
                    if (tick % 800 < 400) core = new(100, now);
                    else core = new(double.NaN, now);
                    firmware = new(60, now);
                }
                if (profile == 5 && tick % 500 == 0) package = core = firmware = new(70, now - 3000);
                if (profile == 6 && tick == 400) transport.FailReads = true;
                if (profile == 6 && tick == 420) transport.FailReads = false;
                if (profile == 7 && tick == 700) { mode = 2; writesAtSupersession = transport.Writes; }

                var result = await supervisor.EvaluateAsync();
                evaluated++;
                if (result.Status == ThermalControlStatus.Superseded)
                {
                    if (mode != 2 || transport.Writes != writesAtSupersession)
                        throw new InvalidOperationException($"Superseded model wrote hardware at profile={profile}, tick={tick}");
                    continue;
                }
                var target = result.Target ?? throw new InvalidOperationException("An admitted model tick lost its target");
                if (!capability.IsAligned(target) || target.LongWatts > baseline.LongWatts || target.ShortWatts > baseline.ShortWatts)
                    throw new InvalidOperationException($"Illegal model target at profile={profile}, tick={tick}: {target}");
                if (result.Source != lastSource || now < previousNow) coolSince = null;
                bool healthyCool = result.ObservedTemperatureC is { } observed && observed <= options.Policy.RecoverC
                    && result.Decision?.Phase != ThermalPhase.SensorFault;
                coolSince = healthyCool ? coolSince ?? now : null;
                bool raised = target.LongWatts > last.LongWatts || target.ShortWatts > last.ShortWatts;
                if (raised && (coolSince is null || now - coolSince < options.Policy.RecoveryMilliseconds))
                    throw new InvalidOperationException($"Premature recovery at profile={profile}, tick={tick}");
                if (result.Decision?.Phase == ThermalPhase.SensorFault && target != capability.GetMinimumEnvelope())
                    throw new InvalidOperationException($"Invalid data did not request physical minimum at profile={profile}, tick={tick}");
                if (result.ObservedTemperatureC >= options.Policy.TargetC && raised)
                    throw new InvalidOperationException($"Hot telemetry raised power at profile={profile}, tick={tick}");
                if (supervisor.HasActuatorFault)
                {
                    faultCeiling ??= target;
                    if (target.LongWatts > faultCeiling.LongWatts || target.ShortWatts > faultCeiling.ShortWatts)
                        throw new InvalidOperationException($"Actuator fault relaxed automatically at profile={profile}, tick={tick}");
                    faultCeiling = new(Math.Min(faultCeiling.LongWatts, target.LongWatts), Math.Min(faultCeiling.ShortWatts, target.ShortWatts));
                }
                last = target; lastSource = result.Source; previousNow = now;
            }
        }
        Console.WriteLine($"TRACE MODEL: {profileCount} synthetic telemetry profiles x 1 simulated hour; {evaluated} ticks, legal bounds/freshness/recovery/fault/supersession invariants");
        return profileCount;
    }

    private sealed class MemoryTransport(CpuPowerEnvelope initial) : ICpuPowerTransport
    {
        public CpuPowerEnvelope Value = initial;
        public int Writes;
        public bool FailReads { get; set; }
        public Task WriteAsync(CpuPowerEnvelope target) { Writes++; Value = target; return Task.CompletedTask; }
        public Task<CpuPowerEnvelope> ReadAsync() => FailReads
            ? Task.FromException<CpuPowerEnvelope>(new InvalidOperationException("injected readback failure")) : Task.FromResult(Value);
    }
}
