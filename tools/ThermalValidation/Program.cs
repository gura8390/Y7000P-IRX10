using System.Diagnostics;
using System.Globalization;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading.Channels;
using System.ServiceProcess;
using Microsoft.Win32;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;
using LenovoLegionToolkit.Lib.System.Management;
using LenovoLegionToolkit.Lib.Thermal;
using LibreHardwareMonitor.Hardware;

return await ValidationApplication.RunAsync(args);

internal sealed record ValidationOptions(bool Active, bool CpuLoad, int Seconds, int Workers, string OutputPath)
{
    public static ValidationOptions? Parse(string[] args)
    {
        bool active = false, load = false;
        int seconds = 60, workers = Environment.ProcessorCount;
        string output = Path.Combine(FindRepositoryRoot(), "..", "verification", $"thermal-{DateTime.Now:yyyyMMdd-HHmmss}.json");
        for (int i = 0; i < args.Length; i++)
        {
            switch (args[i])
            {
                case "--help" or "-h":
                    Console.WriteLine("Y7000p.ThermalValidation [--output PATH] [--active --seconds 60 [--cpu-load] [--workers N]]");
                    Console.WriteLine("Default: read-only firmware discovery. Active mode requires Custom mode, no other LLT instance, and elevation.");
                    Console.WriteLine("--cpu-load explicitly starts a bounded, low-priority scalar CPU workload only after control/readback is verified.");
                    return null;
                case "--active": active = true; break;
                case "--cpu-load": load = true; break;
                case "--seconds": seconds = int.Parse(Next(), CultureInfo.InvariantCulture); break;
                case "--workers": workers = int.Parse(Next(), CultureInfo.InvariantCulture); break;
                case "--output" or "--out": output = Next(); break;
                default: throw new ArgumentException($"Unknown argument: {args[i]}");
            }
            string Next() => ++i < args.Length ? args[i] : throw new ArgumentException("Missing argument value.");
        }
        if (load && !active) throw new ArgumentException("--cpu-load requires explicit --active.");
        if (seconds is < 1 or > 3600 || workers < 1 || workers > Environment.ProcessorCount)
            throw new ArgumentOutOfRangeException(nameof(args), "Seconds must be 1..3600 and workers 1..logical processor count.");
        return new(active, load, seconds, workers, Path.GetFullPath(output));
    }

    private static string FindRepositoryRoot()
    {
        foreach (var start in new[] { Environment.CurrentDirectory, AppContext.BaseDirectory })
            for (var directory = new DirectoryInfo(start); directory is not null; directory = directory.Parent)
                if (Directory.Exists(Path.Combine(directory.FullName, "LenovoLegionToolkit.Lib"))) return directory.FullName;
        return AppContext.BaseDirectory;
    }
}

internal static class ValidationApplication
{
    private static readonly JsonSerializerOptions Json = new() { WriteIndented = true, Converters = { new JsonStringEnumConverter() } };
    private static readonly JsonSerializerOptions JsonLine = new() { Converters = { new JsonStringEnumConverter() } };

    public static async Task<int> RunAsync(string[] args)
    {
        if (args is ["--self-test"]) return await ValidatorSelfTests.RunAsync().ConfigureAwait(false);
        ValidationOptions? options;
        try { options = ValidationOptions.Parse(args); }
        catch (Exception ex) { Console.Error.WriteLine(ex.Message); return 2; }
        if (options is null) return 0;
        Directory.CreateDirectory(Path.GetDirectoryName(options.OutputPath)!);
        var samplesPath = Path.ChangeExtension(options.OutputPath, ".samples.jsonl");
        using var samples = new StreamWriter(samplesPath, false, new UTF8Encoding(false));
        var report = new Dictionary<string, object?>
        {
            ["StartedAtUtc"] = DateTimeOffset.UtcNow,
            ["Options"] = options,
            ["ReadOnly"] = !options.Active,
            ["Phase"] = "ReadOnlyDiscovery",
            ["TelemetryPath"] = samplesPath,
            ["Workload"] = options.CpuLoad ? "Low-priority scalar square-root loops; work units are not a general CPU benchmark" : "No generated load",
            ["ControlVerified"] = false,
            ["RestorationStatus"] = "NotEnabled"
        };
        void Save() => File.WriteAllText(options.OutputPath, JsonSerializer.Serialize(report, Json), new UTF8Encoding(false));
        async Task DrainAsync(Task task, string phase)
        {
            report["Phase"] = phase;
            Save();
            while (!task.IsCompleted)
            {
                var finished = await Task.WhenAny(task, Task.Delay(5000)).ConfigureAwait(false);
                if (finished == task) break;
                report["WaitingSinceUtc"] = DateTimeOffset.UtcNow;
                report["Phase"] = phase + ":WaitingForPhysicalIo";
                Save();
                Console.Error.WriteLine("Waiting for physical I/O; ownership has not been released.");
            }
            await task.ConfigureAwait(false);
            report.Remove("WaitingSinceUtc");
        }

        using var cancellation = new CancellationTokenSource();
        ConsoleCancelEventHandler cancelHandler = (_, e) => { e.Cancel = true; cancellation.Cancel(); };
        Console.CancelKeyPress += cancelHandler;
        ThermalCpuTransport? transport = null;
        IThermalTelemetrySampler? sampler = null;
        CpuLoad? load = null;
        CpuLoadDeadline? loadDeadline = null;
        Exception? failure = null;
        var stats = new TemperatureStatistics();
        var timer = new Stopwatch();
        TimeSpan? cpuTimeAtStart = null;
        var telemetryQueue = Channel.CreateBounded<CpuTelemetry>(new BoundedChannelOptions(256)
        { SingleReader = true, SingleWriter = true, FullMode = BoundedChannelFullMode.DropOldest });
        var loadGuard = new LoadTemperatureGuard(() => Volatile.Read(ref load)?.RequestStop());
        void OnSample(CpuTelemetry telemetry)
        {
            if (stats.Observe(telemetry)) loadGuard.Observe(telemetry);
            telemetryQueue.Writer.TryWrite(telemetry);
        }
        async Task WriteTelemetryAsync(ThermalControlResult? control)
        {
            while (telemetryQueue.Reader.TryRead(out var telemetry))
                await samples.WriteLineAsync(JsonSerializer.Serialize(new
                {
                    LoggedAtUtc = DateTimeOffset.UtcNow, ElapsedMilliseconds = timer.ElapsedMilliseconds,
                    Telemetry = telemetry, ControlAtLogTime = control, WorkUnits = load?.CompletedWorkUnits ?? 0
                }, JsonLine)).ConfigureAwait(false);
        }
        try
        {
            Save();
            var controllers = Process.GetProcessesByName("Lenovo Legion Toolkit");
            try { report["OtherToolkitProcessIds"] = controllers.Where(p => p.Id != Environment.ProcessId).Select(p => p.Id).ToArray(); }
            finally { foreach (var process in controllers) process.Dispose(); }
            var pawnIo = PawnIoPrerequisite.Read();
            report["PawnIoPrerequisite"] = pawnIo;
            Save();
            if (options.Active && ((int[])report["OtherToolkitProcessIds"]!).Length != 0)
                throw new InvalidOperationException("Another Lenovo Legion Toolkit instance is running. Close it before active validation; this tool does not close applications.");
            var firmware = new LenovoFirmwareProbe();
            report["Identity"] = await firmware.ReadIdentityAsync().ConfigureAwait(false);
            report["FirmwareAggregateCpuTemperature"] = await firmware.ReadAggregateTemperatureAsync().ConfigureAwait(false);
            var mode = await WMI.LenovoGameZoneData.GetSmartFanModeAsync().ConfigureAwait(false);
            report["InitialRawMode"] = mode;
            report["Capabilities"] = (await WMI.LenovoCapabilityData01.ReadAsync().ConfigureAwait(false))
                .Where(r => r.Id is CapabilityID.CPULongTermPowerLimit or CapabilityID.CPUShortTermPowerLimit or CapabilityID.CPUTemperatureLimit)
                .Select(r => new { Id = $"0x{(uint)r.Id:X8}", r.Min, r.Max, r.Step, r.DefaultValue }).ToArray();
            report["InitialCpuLimits"] = new
            {
                LongWatts = await WMI.LenovoOtherMethod.GetFeatureValueAsync(CpuThermalWriteCoordinator.LongPowerId).ConfigureAwait(false),
                ShortWatts = await WMI.LenovoOtherMethod.GetFeatureValueAsync(CpuThermalWriteCoordinator.ShortPowerId).ConfigureAwait(false),
                TemperatureC = await WMI.LenovoOtherMethod.GetFeatureValueAsync(CpuThermalWriteCoordinator.TemperatureId).ConfigureAwait(false)
            };
            Save();
            Console.WriteLine(JsonSerializer.Serialize(report, Json));
            if (!options.Active)
            {
                report["Outcome"] = mode == 255 ? "ReadOnlyDiscoveryCompleted" : "ReadOnlyDiscoveryCompleted:SelectCustomModeBeforeActiveControl";
                return 0;
            }
            if (((int[])report["OtherToolkitProcessIds"]!).Length != 0)
                throw new InvalidOperationException("Another Lenovo Legion Toolkit instance is running. Close it before active validation; this tool does not close applications.");
            if (mode != 255)
                throw new InvalidOperationException($"Active validation requires Custom mode (255); current mode is {mode}. Select Custom mode manually first; no mode change was made.");
            cancellation.Token.ThrowIfCancellationRequested();
            transport = await ThermalCpuTransport.DiscoverAsync().ConfigureAwait(false);
            report["VerifiedBaseline"] = transport.Baseline;
            report["TransportCapability"] = transport.Capability;
            report["OriginalTemperatureLimitC"] = transport.OriginalTemperatureLimit;
            report["Phase"] = "PreparingCpuOnlySensors";
            Save();
            sampler = pawnIo.Ready ? new CpuOnlySampler(OnSample) : new FirmwareTelemetrySampler(transport, OnSample);
            report["TelemetrySource"] = pawnIo.Ready ? "PackageAndCoreMaximum" : "LenovoFirmwareAggregate";
            report["TelemetryScope"] = pawnIo.Ready ? "CPU-only LibreHardwareMonitor hardware; GPU monitoring remains disabled."
                : "Single Lenovo firmware aggregate; package/core hotspot temperatures are unavailable and remain null. No driver was installed.";
            await sampler.StartAsync().WaitAsync(TimeSpan.FromSeconds(10), cancellation.Token).ConfigureAwait(false);
            var initialTelemetry = sampler.Latest;
            if (!initialTelemetry.IsFresh(Environment.TickCount64))
                throw new InvalidOperationException("Selected CPU telemetry source is invalid or stale; no active control or load was enabled.");
            report["Phase"] = "EnablingFirmware88C";
            Save();
            await transport.EnableAsync(88).ConfigureAwait(false);
            report["RestorationStatus"] = "Required";
            var executor = new HardwareCommandExecutor(transport, transport.Capability, () => Environment.TickCount64, () => (1L, 1L));
            long now = Environment.TickCount64;
            var verified = await executor.VerifyAsync(new(transport.Baseline, 1, 1, now + 3000), TimeSpan.FromSeconds(2), cancellation.Token).ConfigureAwait(false);
            if (verified.Status != ApplyStatus.Verified) throw new InvalidOperationException("Initial CPU/temperature readback did not verify: " + verified.Reason);
            var supervisor = new ThermalControlSupervisor(transport, transport.Capability, transport.Baseline,
                () => sampler.Latest.Readings, () => Environment.TickCount64, () => (1L, 1L), executor: executor,
                getFirmwareTemperature: () => sampler.Latest.FirmwareReading);
            var initialControl = await supervisor.EvaluateAsync(cancellation.Token).ConfigureAwait(false);
            if (initialControl.Status is ThermalControlStatus.ActuatorFault or ThermalControlStatus.UnknownCompletion or ThermalControlStatus.SensorFault)
                throw new InvalidOperationException("Initial control tick is not healthy: " + initialControl.Reason);
            if (options.CpuLoad && initialControl.ObservedTemperatureC is >= 85)
                throw new InvalidOperationException("CPU is already at least 85°C before generated load. Cool the current workload before starting a load validation.");
            report["ControlVerified"] = true;
            report["Phase"] = options.CpuLoad ? "ActiveControlWithCpuLoad" : "ActiveControlObservation";
            Save();
            cpuTimeAtStart = ReadProcessCpuTime();
            if (options.CpuLoad) load = new CpuLoad(options.Workers);
            timer.Start();
            stats.StartWindow();
            if (options.CpuLoad)
            {
                loadDeadline = new CpuLoadDeadline(load!.RequestStop);
                // This timer owns the load deadline. Report I/O, logging and a stalled
                // control tick must never keep the generated CPU workload alive.
                loadDeadline.Arm(TimeSpan.FromSeconds(options.Seconds));
                load.Start(cancellation.Token);
            }
            long lastSaved = 0;
            using var ticks = new PeriodicTimer(TimeSpan.FromMilliseconds(250));
            while (timer.Elapsed.TotalSeconds < options.Seconds)
            {
                cancellation.Token.ThrowIfCancellationRequested();
                var measured = stats.Snapshot();
                var thermalAbort = measured.MaximumC >= 91 || measured.CurrentOver90Milliseconds >= 1000;
                if (measured.Over90Samples > 0)
                {
                    report["AnySamplesOver90"] = true;
                    report["TemperatureAcceptance"] = "Failed: at least one observed CPU sample exceeded 90°C";
                    if (thermalAbort && load is not null) await load.StopAsync().ConfigureAwait(false);
                }
                if (loadGuard.Reason is not null && load is not null) await load.StopAsync().ConfigureAwait(false);
                var control = await supervisor.EvaluateAsync(cancellation.Token).ConfigureAwait(false);
                await WriteTelemetryAsync(control).ConfigureAwait(false);
                if (sampler.Error is { } sensorError) throw new InvalidOperationException("CPU sampler failed.", sensorError);
                if (control.Status is ThermalControlStatus.ActuatorFault or ThermalControlStatus.UnknownCompletion or ThermalControlStatus.Superseded or ThermalControlStatus.SensorFault)
                    throw new InvalidOperationException("Control fault; stopping generated load: " + control.Reason);
                if (thermalAbort || loadGuard.Reason is not null)
                    throw new InvalidOperationException("CPU thermal acceptance failed: generated load stopped at 91°C or one sampled second above 90°C.");
                if (timer.ElapsedMilliseconds - lastSaved >= 1000)
                {
                    report["Statistics"] = stats.Snapshot();
                    report["ElapsedSeconds"] = timer.Elapsed.TotalSeconds;
                    report["WorkUnits"] = load?.CompletedWorkUnits ?? 0;
                    Save();
                    await samples.FlushAsync().ConfigureAwait(false);
                    lastSaved = timer.ElapsedMilliseconds;
                }
                await ticks.WaitForNextTickAsync(cancellation.Token).ConfigureAwait(false);
            }
            report["Outcome"] = options.CpuLoad ? "BoundedCpuLoadRunCompleted" : "ActiveObservationCompletedWithoutGeneratedLoad";
        }
        catch (Exception ex)
        {
            failure = ex;
            report["Outcome"] = ex is OperationCanceledException ? "Cancelled" : "Failed";
            report["Error"] = ex.ToString();
            Console.Error.WriteLine(ex.Message);
        }
        finally
        {
            // No startup sample, restored limit or post-run external workload may
            // contaminate this bounded control run's acceptance statistics.
            var measured = stats.StopWindow();
            timer.Stop();
            cancellation.Cancel();
            // Always remove generated load before touching restoration or waiting on vendor I/O.
            try { if (load is not null) await DrainAsync(load.StopAsync(), "StoppingCpuLoad").ConfigureAwait(false); }
            catch (Exception ex) { failure ??= ex; report["LoadStopError"] = ex.ToString(); }
            report["LoadStopped"] = load is null || load.IsStopped;
            report["LoadDeadlineReached"] = loadDeadline?.Expired ?? false;
            report["LoadDeadlineStopRequestedAfterSeconds"] = loadDeadline?.StopRequestedAfterSeconds;
            report["ElapsedSeconds"] = timer.Elapsed.TotalSeconds;
            report["Statistics"] = measured;
            report["WorkUnits"] = load?.CompletedWorkUnits ?? 0;
            report["WorkUnitsPerSecond"] = timer.Elapsed.TotalSeconds > 0 ? (load?.CompletedWorkUnits ?? 0) / timer.Elapsed.TotalSeconds : 0;
            var processCpuSeconds = cpuTimeAtStart.HasValue ? (ReadProcessCpuTime() - cpuTimeAtStart.Value).TotalSeconds : 0;
            var saturation = timer.Elapsed.TotalSeconds > 0 ? processCpuSeconds / timer.Elapsed.TotalSeconds / Environment.ProcessorCount : 0;
            report["ProcessCpuSecondsDuringRun"] = processCpuSeconds;
            report["LogicalProcessorCount"] = Environment.ProcessorCount;
            report["ProcessCpuSaturationPercent"] = saturation * 100;
            report["CompletedRequestedDuration"] = timer.Elapsed.TotalSeconds >= options.Seconds;
            report["HighCpuLoadEvidence"] = options.CpuLoad && timer.Elapsed.TotalSeconds >= options.Seconds
                && options.Workers == Environment.ProcessorCount && saturation >= 0.90;
            try
            {
                if (transport is not null)
                {
                    await DrainAsync(transport.RestoreIfOwnedAsync(), "RestoringOwnedFirmwareFields").ConfigureAwait(false);
                    report["RestorationStatus"] = "VerifiedOrNoFieldStillOwned";
                }
            }
            catch (Exception ex) { failure ??= ex; report["RestorationStatus"] = "FailedUnverified"; report["RestorationError"] = ex.ToString(); }
            try { if (sampler is not null) await DrainAsync(sampler.StopAsync(), "ClosingCpuOnlySensors").ConfigureAwait(false); }
            catch (Exception ex) { failure ??= ex; report["SensorCloseError"] = ex.ToString(); }
            report["LoadGuardReason"] = loadGuard.Reason;
            await WriteTelemetryAsync(null).ConfigureAwait(false);
            await samples.FlushAsync().ConfigureAwait(false);
            report["EndedAtUtc"] = DateTimeOffset.UtcNow;
            report["Phase"] = failure is null ? "Completed" : "Failed";
            if (failure is not null && !Equals(report.GetValueOrDefault("Outcome"), "Cancelled")) report["Outcome"] = "Failed";
            report["Sampled90CCheckPassed"] = options.Active && failure is null && measured.ValidSamples > 0 && measured.Over90Samples == 0;
            report["AnySamplesOver90"] = measured.Over90Samples != 0;
            report["TemperatureAcceptance"] = !options.Active ? "NotRunReadOnly" : failure is not null ? "FailedOrIncomplete"
                : measured.ValidSamples == 0 ? "NoValidSamples" : measured.Over90Samples > 0 ? "FailedObservedSampleOver90C" : "PassedForObservedSamplesOnly";
            report["AcceptanceScope"] = "Sampled result for this bounded run and workload only; no claim about unobserved temperature peaks or other workloads.";
            Save();
            Console.CancelKeyPress -= cancelHandler;
            // Dispose synchronizes with an executing deadline callback before the
            // callback's target CancellationTokenSource can be disposed.
            loadDeadline?.Dispose();
            load?.Dispose();
            Console.WriteLine("Validation report: " + options.OutputPath);
        }
        return failure is null ? 0 : 1;
    }

    private static TimeSpan ReadProcessCpuTime()
    {
        using var process = Process.GetCurrentProcess();
        return process.TotalProcessorTime;
    }
}

internal sealed record CpuTelemetry(long SampledAtMilliseconds, double? PackageC, double? CoreMaximumC,
    double? CpuPowerWatts, double? UtilizationPercent, double? FrequencyMhz,
    string Source = "PackageAndCoreMaximum", double? FirmwareAggregateC = null)
{
    [JsonIgnore] public (TemperatureReading Package, TemperatureReading CoreMax) Readings =>
        (new(PackageC ?? double.NaN, SampledAtMilliseconds), new(CoreMaximumC ?? double.NaN, SampledAtMilliseconds));
    [JsonIgnore] public TemperatureReading FirmwareReading => new(FirmwareAggregateC ?? double.NaN, SampledAtMilliseconds);
    public bool IsFresh(long now) => Source == "LenovoFirmwareAggregate" ? FirmwareReading.IsFresh(now, 2000)
        : Readings.Package.IsFresh(now, 2000) && Readings.CoreMax.IsFresh(now, 2000);
}

internal interface IThermalTelemetrySampler
{
    CpuTelemetry Latest { get; }
    Exception? Error { get; }
    Task StartAsync();
    Task StopAsync();
}

internal sealed class CpuOnlySampler(Action<CpuTelemetry>? onSample = null) : IThermalTelemetrySampler
{
    private readonly Computer _computer = new()
    {
        IsCpuEnabled = true, IsGpuEnabled = false, IsMemoryEnabled = false, IsStorageEnabled = false,
        IsMotherboardEnabled = false, IsControllerEnabled = false, IsNetworkEnabled = false
    };
    private readonly CpuSensorProvider _provider = new();
    private readonly CancellationTokenSource _cts = new();
    private CpuTelemetry _latest = new(0, null, null, null, null, null);
    private Exception? _error;
    private Task? _loop;
    private Task? _startup;
    public CpuTelemetry Latest => Volatile.Read(ref _latest);
    public Exception? Error => Volatile.Read(ref _error);

    public Task StartAsync() => _startup ??= StartCoreAsync();

    private async Task StartCoreAsync()
    {
        await Task.Run(() =>
        {
            _computer.Open();
            foreach (var hardware in _computer.Hardware) Update(hardware);
            _provider.Discover(_computer.Hardware.ToArray());
            if (!_provider.IsAvailable) throw new InvalidOperationException("LibreHardwareMonitor exposed no CPU hardware.");
            Capture();
        }).ConfigureAwait(false);
        if (_cts.IsCancellationRequested) return;
        _loop = Task.Run(async () =>
        {
            try
            {
                using var ticks = new PeriodicTimer(TimeSpan.FromMilliseconds(250));
                while (await ticks.WaitForNextTickAsync(_cts.Token).ConfigureAwait(false))
                {
                    foreach (var hardware in _computer.Hardware) Update(hardware);
                    Capture();
                }
            }
            catch (OperationCanceledException) when (_cts.IsCancellationRequested) { }
            catch (Exception ex)
            {
                Volatile.Write(ref _error, ex);
                var invalid = new CpuTelemetry(Environment.TickCount64, null, null, null, null, null);
                Volatile.Write(ref _latest, invalid);
                onSample?.Invoke(invalid);
            }
        });
    }

    private void Capture()
    {
        _provider.Read();
        double? Sensor(SensorItem item) => _provider.Values.TryGetValue(item, out var value) && float.IsFinite(value) && value >= 0 ? value : null;
        static double? Temperature(float? value) => value is > 0 and < 150 && float.IsFinite(value.Value) ? value : null;
        var sample = new CpuTelemetry(Environment.TickCount64, Temperature(_provider.PackageTemperature), Temperature(_provider.CoreMaxTemperature),
            Sensor(SensorItem.CpuPower), Sensor(SensorItem.CpuUtilization), Sensor(SensorItem.CpuFrequency));
        Volatile.Write(ref _latest, sample);
        onSample?.Invoke(sample);
    }

    private static void Update(IHardware hardware)
    {
        hardware.Update();
        foreach (var sub in hardware.SubHardware) Update(sub);
    }

    public async Task StopAsync()
    {
        _cts.Cancel();
        // A timed-out Open/initial sample still owns the hardware. Never Close concurrently with it.
        if (_startup is not null)
            try { await _startup.ConfigureAwait(false); } catch (Exception ex) { Volatile.Write(ref _error, ex); }
        if (_loop is not null) await _loop.ConfigureAwait(false);
        await Task.Run(_computer.Close).ConfigureAwait(false);
        _cts.Dispose();
    }
}

internal sealed class FirmwareTelemetrySampler(ThermalCpuTransport transport, Action<CpuTelemetry>? onSample = null) : IThermalTelemetrySampler
{
    private readonly CancellationTokenSource _cts = new();
    private CpuTelemetry _latest = new(0, null, null, null, null, null, "LenovoFirmwareAggregate");
    private Exception? _error;
    private Task? _startup;
    private Task? _loop;
    public CpuTelemetry Latest => Volatile.Read(ref _latest);
    public Exception? Error => Volatile.Read(ref _error);
    public Task StartAsync() => _startup ??= StartCoreAsync();
    private async Task CaptureAsync()
    {
        var reading = await transport.ReadCpuTemperatureAsync().ConfigureAwait(false);
        var sample = new CpuTelemetry(reading.SampledAtMilliseconds, null, null, null, null, null,
            "LenovoFirmwareAggregate", double.IsFinite(reading.Celsius) ? reading.Celsius : null);
        Volatile.Write(ref _latest, sample);
        onSample?.Invoke(sample);
    }
    private async Task StartCoreAsync()
    {
        await CaptureAsync().ConfigureAwait(false);
        if (_cts.IsCancellationRequested) return;
        _loop = Task.Run(async () =>
        {
            try
            {
                using var ticks = new PeriodicTimer(TimeSpan.FromMilliseconds(250));
                while (await ticks.WaitForNextTickAsync(_cts.Token).ConfigureAwait(false)) await CaptureAsync().ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (_cts.IsCancellationRequested) { }
            catch (Exception ex)
            {
                Volatile.Write(ref _error, ex);
                var invalid = new CpuTelemetry(0, null, null, null, null, null, "LenovoFirmwareAggregate");
                Volatile.Write(ref _latest, invalid);
                onSample?.Invoke(invalid);
            }
        });
    }
    public async Task StopAsync()
    {
        _cts.Cancel();
        if (_startup is not null)
            try { await _startup.ConfigureAwait(false); } catch (Exception ex) { Volatile.Write(ref _error, ex); }
        if (_loop is not null) await _loop.ConfigureAwait(false);
        _cts.Dispose();
    }
}

internal sealed record TemperatureStatisticsSnapshot(int ValidSamples, int Over90Samples, double? MaximumC,
    long EstimatedMillisecondsOver90, long CurrentOver90Milliseconds,
    long? WindowStartedAtMilliseconds, long? WindowStoppedAtMilliseconds)
{
    public string TimeEstimateMethod => "Sample-and-hold between distinct fresh samples inside the verified active measurement window; gaps over 2s are excluded.";
}

internal sealed class TemperatureStatistics
{
    private readonly object _gate = new();
    private bool _measuring;
    private long? _startedAt;
    private long? _stoppedAt;
    private int _validSamples;
    private int _over90Samples;
    private double? _maximumC;
    private long _estimatedMillisecondsOver90;
    private long _currentOver90Milliseconds;
    private long _lastTick;
    private bool _wasOver90;

    public void StartWindow(long? atMilliseconds = null)
    {
        lock (_gate)
        {
            _startedAt = atMilliseconds ?? Environment.TickCount64;
            _stoppedAt = null;
            _validSamples = _over90Samples = 0;
            _maximumC = null;
            _estimatedMillisecondsOver90 = _currentOver90Milliseconds = _lastTick = 0;
            _wasOver90 = false;
            _measuring = true;
        }
    }

    public TemperatureStatisticsSnapshot StopWindow()
    {
        lock (_gate)
        {
            if (_measuring) _stoppedAt = Environment.TickCount64;
            _measuring = false;
            return SnapshotCore();
        }
    }

    public TemperatureStatisticsSnapshot Snapshot()
    {
        lock (_gate) return SnapshotCore();
    }

    private TemperatureStatisticsSnapshot SnapshotCore() => new(_validSamples, _over90Samples, _maximumC,
        _estimatedMillisecondsOver90, _currentOver90Milliseconds, _startedAt, _stoppedAt);

    // The bool controls the load guard too: warm-up/restoration callbacks must not
    // stop a later run. Invalid telemetry DURING the window still trips its guard.
    public bool Observe(CpuTelemetry telemetry)
    {
        lock (_gate)
        {
            if (!_measuring) return false;
            if (!telemetry.IsFresh(Environment.TickCount64)) return true;
            if (telemetry.SampledAtMilliseconds < _startedAt) return false;
            if (telemetry.SampledAtMilliseconds == _lastTick) return true;
            var temperature = telemetry.Source == "LenovoFirmwareAggregate" ? telemetry.FirmwareAggregateC!.Value
                : Math.Max(telemetry.PackageC!.Value, telemetry.CoreMaximumC!.Value);
            if (_lastTick > 0 && telemetry.SampledAtMilliseconds >= _lastTick && telemetry.SampledAtMilliseconds - _lastTick <= 2000 && _wasOver90)
            {
                var elapsed = telemetry.SampledAtMilliseconds - _lastTick;
                _estimatedMillisecondsOver90 += elapsed;
                _currentOver90Milliseconds += elapsed;
            }
            _lastTick = telemetry.SampledAtMilliseconds;
            _wasOver90 = temperature > 90;
            if (!_wasOver90) _currentOver90Milliseconds = 0;
            _validSamples++;
            if (_wasOver90) _over90Samples++;
            _maximumC = Math.Max(_maximumC ?? temperature, temperature);
            return true;
        }
    }
}

internal sealed class CpuLoad(int workers) : IDisposable
{
    private readonly CancellationTokenSource _cts = new();
    private readonly List<Thread> _threads = [];
    private CancellationTokenRegistration _registration;
    private long _completed;
    private double _checksum;
    public long CompletedWorkUnits => Interlocked.Read(ref _completed);
    public bool IsStopped => _threads.All(t => !t.IsAlive);
    public void Start(CancellationToken outer)
    {
        _registration = outer.Register(_cts.Cancel);
        for (int worker = 0; worker < workers; worker++)
        {
            var thread = new Thread(() =>
            {
                double value = 1.5;
                while (!_cts.IsCancellationRequested)
                {
                    for (int i = 0; i < 4096; i++) value = Math.Sqrt(value + 1.00001);
                    Interlocked.Add(ref _completed, 4096);
                }
                Interlocked.Exchange(ref _checksum, value);
            }) { IsBackground = true, Priority = ThreadPriority.BelowNormal, Name = "Bounded CPU validation load" };
            _threads.Add(thread);
            thread.Start();
        }
    }
    public void RequestStop() => _cts.Cancel();
    public async Task StopAsync()
    {
        _cts.Cancel();
        await Task.Run(() => { foreach (var thread in _threads) thread.Join(); }).ConfigureAwait(false);
    }
    public void Dispose() { _registration.Dispose(); _cts.Dispose(); }
}

internal sealed class CpuLoadDeadline(Action stopGeneratedLoad) : IDisposable
{
    private readonly object _gate = new();
    private Timer? _timer;
    private bool _disposed;
    private bool _armed;
    private long _startedAt;
    private long _expiredAt;
    public bool Expired => Volatile.Read(ref _expiredAt) != 0;
    public double? StopRequestedAfterSeconds => Expired
        ? Stopwatch.GetElapsedTime(_startedAt, Volatile.Read(ref _expiredAt)).TotalSeconds : null;

    public void Arm(TimeSpan duration)
    {
        if (duration <= TimeSpan.Zero || duration.TotalMilliseconds > int.MaxValue)
            throw new ArgumentOutOfRangeException(nameof(duration));
        lock (_gate)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            if (_armed) throw new InvalidOperationException("CPU load deadline is already armed.");
            _armed = true;
            _startedAt = Stopwatch.GetTimestamp();
            _timer = new Timer(_ =>
            {
                lock (_gate)
                {
                    if (_disposed) return;
                    Volatile.Write(ref _expiredAt, Stopwatch.GetTimestamp());
                    stopGeneratedLoad();
                }
            }, null, duration, Timeout.InfiniteTimeSpan);
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            _disposed = true;
            _timer?.Dispose();
            _timer = null;
        }
    }
}

internal sealed class LoadTemperatureGuard(Action stopGeneratedLoad)
{
    private long? _over90Since;
    private string? _reason;
    public string? Reason => Volatile.Read(ref _reason);
    public void Observe(CpuTelemetry sample)
    {
        if (!sample.IsFresh(Environment.TickCount64))
        {
            Interlocked.CompareExchange(ref _reason, "CPU telemetry became invalid or stale.", null);
            stopGeneratedLoad();
            return;
        }
        var temperature = sample.Source == "LenovoFirmwareAggregate" ? sample.FirmwareAggregateC!.Value
            : Math.Max(sample.PackageC!.Value, sample.CoreMaximumC!.Value);
        if (temperature > 90) _over90Since ??= sample.SampledAtMilliseconds;
        else _over90Since = null;
        if (temperature >= 91 || _over90Since is { } since && sample.SampledAtMilliseconds - since >= 1000)
        {
            Interlocked.CompareExchange(ref _reason, "CPU reached 91°C or remained above 90°C for one sampled second.", null);
            stopGeneratedLoad();
        }
    }
}

internal static class ValidatorSelfTests
{
    public static async Task<int> RunAsync()
    {
        int checks = 0;
        void Check(bool ok, string name) { if (!ok) throw new InvalidOperationException(name); checks++; }
        try
        {
            var now = Environment.TickCount64;
            CpuTelemetry Firmware(double? value, long tick) => new(tick, null, null, null, null, null, "LenovoFirmwareAggregate", value);
            var validJson = JsonSerializer.Serialize(Firmware(53, now));
            Check(validJson.Contains("\"PackageC\":null") && validJson.Contains("\"CoreMaximumC\":null"), "Firmware source never impersonates package/core telemetry");
            Check(!validJson.Contains("NaN") && !validJson.Contains("Readings"), "Computed invalid dual readings are not serialized");
            Check(!JsonSerializer.Serialize(Firmware(null, 0)).Contains("NaN"), "Unavailable firmware telemetry serializes as null");
            Check(JsonSerializer.Serialize(new ThermalControlResult(ThermalControlStatus.SensorFault, null, null, null, "Missing telemetry")).Contains("null"), "Missing control telemetry serializes successfully");
            int stopped = 0;
            var guard = new LoadTemperatureGuard(() => stopped++);
            guard.Observe(Firmware(90, now));
            Check(stopped == 0, "Exactly 90C does not trip the over-limit guard");
            guard.Observe(Firmware(91, now));
            Check(stopped == 1 && guard.Reason is not null, "91C immediately requests load stop from the sampler callback");
            stopped = 0;
            guard = new(() => stopped++);
            guard.Observe(Firmware(90.5, now - 1500));
            Check(stopped == 0, "Fractional over-limit permits less than one sampled second for control");
            guard.Observe(Firmware(90.5, now - 500));
            Check(stopped == 1, "One sampled second over 90C stops generated load");
            stopped = 0;
            guard = new(() => stopped++);
            guard.Observe(Firmware(null, 0));
            Check(stopped == 1, "Invalid telemetry immediately stops generated load");
            var stats = new TemperatureStatistics();
            stats.StartWindow(now - 1500);
            stats.Observe(Firmware(89, now - 1000));
            stats.Observe(Firmware(90.2, now - 750));
            stats.Observe(Firmware(90.2, now - 750));
            stats.Observe(Firmware(90.4, now - 500));
            stats.Observe(Firmware(89, now - 250));
            var measured = stats.Snapshot();
            Check(measured.ValidSamples == 4 && measured.Over90Samples == 2 && measured.MaximumC == 90.4, "Count every distinct sample and preserve transient thermal peaks");
            Check(measured.EstimatedMillisecondsOver90 == 500, "Over-limit time uses explicit sampled intervals");
            var window = new TemperatureStatistics();
            window.Observe(Firmware(92, now - 1500));
            window.StartWindow(now - 1000);
            window.Observe(Firmware(92, now - 1250));
            window.Observe(Firmware(85, now - 750));
            window.Observe(Firmware(88, now - 500));
            var windowResult = window.StopWindow();
            window.Observe(Firmware(92, now - 250));
            Check(windowResult.ValidSamples == 2 && windowResult.Over90Samples == 0 && windowResult.MaximumC == 88
                && window.Snapshot() == windowResult, "Pre-control and restored-limit samples cannot contaminate acceptance");
            window.StartWindow(now - 500);
            window.Observe(Firmware(91, now - 250));
            Check(window.Snapshot() is { ValidSamples: 1, Over90Samples: 1, MaximumC: 91 },
                "An actual active-window 91C sample remains an acceptance failure");
            Check(window.Observe(Firmware(null, 0)), "An invalid active-window callback still invokes the load guard");
            try { ValidationOptions.Parse(["--cpu-load"]); throw new InvalidOperationException("Load admitted without --active"); }
            catch (ArgumentException) { checks++; }
            var blockedReport = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            var stopRequested = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            using (var deadline = new CpuLoadDeadline(() => stopRequested.TrySetResult()))
            {
                deadline.Arm(TimeSpan.FromMilliseconds(40));
                // Main/report progress is deliberately held. No CPU worker is started.
                await stopRequested.Task.WaitAsync(TimeSpan.FromSeconds(2)).ConfigureAwait(false);
                Check(deadline.Expired && !blockedReport.Task.IsCompleted,
                    "Independent deadline requests load stop while report/main progress is stalled");
                Check(deadline.StopRequestedAfterSeconds is >= 0.01 and < 2,
                    "Deadline expiration is recorded separately from user cancellation and run duration");
                blockedReport.SetResult();
            }
            Console.WriteLine($"PASS: {checks} validator JSON, thermal guard and accounting checks (no firmware access or generated load)");
            return 0;
        }
        catch (Exception ex) { Console.Error.WriteLine(ex); return 1; }
    }
}

internal sealed class LenovoFirmwareProbe
{
    public Task<object> ReadAggregateTemperatureAsync() => Task.Run<object>(async () =>
    {
        try
        {
            var value = await WMI.LenovoOtherMethod.GetFeatureValueAsync(CapabilityID.CpuCurrentTemperature).ConfigureAwait(false);
            return new { Source = "LENOVO_OTHER_METHOD.GetFeatureValue(0x05040000)", Celsius = value,
                Scope = "Single firmware aggregate only; does not substitute for independently read CPU package and core maximum." };
        }
        catch (Exception ex) { return new { Source = "Firmware aggregate", Error = ex.Message }; }
    });

    public Task<ThermalMachineIdentity> ReadIdentityAsync() => Task.Run(() =>
    {
        string Read(string type, string field)
        {
            using var query = new System.Management.ManagementObjectSearcher("root\\CIMV2", $"SELECT {field} FROM {type}");
            using var rows = query.Get();
            foreach (System.Management.ManagementObject row in rows) using (row) return row[field]?.ToString()?.Trim() ?? "";
            return "";
        }
        return new ThermalMachineIdentity(Read("Win32_ComputerSystem", "Manufacturer"), Read("Win32_ComputerSystem", "Model"),
            Read("Win32_BIOS", "SMBIOSBIOSVersion"), Read("Win32_Processor", "Name"));
    });
}

internal sealed record PawnIoPrerequisite(string? InstallPath, string? Version, string ServiceState, bool DriverFileExists, bool Ready)
{
    public static PawnIoPrerequisite Read()
    {
        const string uninstall = @"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO";
        using var install = Registry.LocalMachine.OpenSubKey(uninstall);
        using var legacy = Registry.LocalMachine.OpenSubKey(@"SOFTWARE\WOW6432Node\PawnIO");
        using var serviceKey = Registry.LocalMachine.OpenSubKey(@"SYSTEM\CurrentControlSet\Services\PawnIO");
        var path = install?.GetValue("InstallLocation") as string ?? legacy?.GetValue("Install_Dir") as string
            ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.ProgramFiles), "PawnIO");
        string? version = install?.GetValue("DisplayVersion") as string;
        var image = serviceKey?.GetValue("ImagePath") as string;
        var driver = string.IsNullOrWhiteSpace(image) ? Path.Combine(path, "PawnIO.sys")
            : Environment.ExpandEnvironmentVariables(image).Trim('"').Replace(@"\??\", "");
        if (driver.StartsWith(@"\SystemRoot\", StringComparison.OrdinalIgnoreCase))
            driver = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Windows), driver[12..]);
        bool driverExists = File.Exists(driver);
        if (version is null && driverExists) version = FileVersionInfo.GetVersionInfo(driver).FileVersion;
        string state;
        try { using var service = new ServiceController("PawnIO"); state = service.Status.ToString(); }
        catch (Exception ex) { state = "Unavailable: " + ex.Message; }
        bool acceptableVersion = !global::System.Version.TryParse(version, out var parsed) || parsed >= new Version(2, 2);
        return new(path, version, state, driverExists, driverExists && state == "Running" && acceptableVersion);
    }
}
