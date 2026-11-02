// Host boundaries only. The controller, policy, supervisor and executor are production sources.
// These stubs cannot access hardware, persisted settings, UI dispatchers or application log files.
using System;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Thermal;

namespace LenovoLegionToolkit.Lib
{
    [Flags]
    public enum HardwareUpdateScope { None = 0, Cpu = 1 }
}

namespace LenovoLegionToolkit.Lib.Controllers.Sensors
{
    public sealed class SensorsGroupController
    {
        public int Subscriptions { get; private set; }
        public int MaximumSubscriptions { get; private set; }
        public Exception? SubscribeFailure { get; set; }
        public bool HardwareTelemetryAvailable { get; set; }
        public Task IsSupportedAsync() => Task.CompletedTask;
        public bool IsLibreHardwareMonitorInitialized() => true;
        public (TemperatureReading Package, TemperatureReading CoreMax) GetControlTemperatures()
        {
            var now = Environment.TickCount64;
            return HardwareTelemetryAvailable ? (new(50, now), new(50, now)) : (new(double.NaN, 0), new(double.NaN, 0));
        }
        public IDisposable Subscribe(TimeSpan interval, HardwareUpdateScope scope)
        {
            if (SubscribeFailure is not null) throw SubscribeFailure;
            Subscriptions++;
            MaximumSubscriptions = Math.Max(MaximumSubscriptions, Subscriptions);
            return new Subscription(() => Subscriptions--);
        }
        private sealed class Subscription(Action release) : IDisposable
        {
            private int _disposed;
            public void Dispose() { if (Interlocked.Exchange(ref _disposed, 1) == 0) release(); }
        }
    }
}

namespace LenovoLegionToolkit.Lib.Settings
{
    public sealed class ApplicationSettings
    {
        public StoreValues Store { get; } = new();
        public sealed class StoreValues
        {
            public ThermalControlOptions CpuThermalControl { get; set; } = new()
            {
                CommandTimeout = TimeSpan.FromMilliseconds(40),
                CommandLifetimeMilliseconds = 1000,
                ReadbackIntervalMilliseconds = 1
            };
            public int CpuThermalSamplingMilliseconds { get; set; } = 100;
            public int CpuThermalFirmwareLimitC { get; set; } = 88;
            public bool EnableHardwareSensors { get; set; }
        }
    }
}

namespace LenovoLegionToolkit.Lib.Utils
{
    public sealed class Log
    {
        public static Log Instance { get; } = new();
        public static Exception? Failure { get; set; }
        public static TaskCompletionSource? TraceEntered { get; set; }
        public static TaskCompletionSource? TraceRelease { get; set; }
        public void Trace(FormattableString message, Exception? exception = null)
        {
            var release = TraceRelease;
            if (release is not null)
            {
                TraceEntered?.TrySetResult();
                release.Task.GetAwaiter().GetResult();
            }
            if (Failure is not null) throw Failure;
        }
    }
}

namespace LenovoLegionToolkit.Lib.Controllers
{
    public sealed class ThermalCpuTransport : ICpuPowerTransport
    {
        public static Func<Task<ThermalCpuTransport>> Discover { get; set; } =
            () => throw new InvalidOperationException("Each lifecycle scenario must inject its own transport.");
        public static Task<ThermalCpuTransport> DiscoverAsync() => Discover();
        public CpuPowerCapability Capability { get; } = new(10, 100, 10, 150, 1);
        public CpuPowerEnvelope Baseline { get; } = new(80, 100);
        private CpuPowerEnvelope _value = new(80, 100);
        private readonly SemaphoreSlim _physicalGate = new(1, 1);
        public TaskCompletionSource EnableEntered { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource EnableRelease { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource ReadEntered { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource ReadRelease { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource WriteEntered { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource WriteRelease { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource RestoreEntered { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource RestoreRelease { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource TemperatureReadEntered { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource TemperatureReadRelease { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource TemperatureReadCompleted { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public TaskCompletionSource MinimumPowerApplied { get; } = new(TaskCreationOptions.RunContinuationsAsynchronously);
        public bool BlockEnable { get; set; }
        public bool BlockRead { get; set; }
        public bool BlockWrite { get; set; }
        public bool BlockRestore { get; set; }
        public bool BlockInitialTemperature { get; set; }
        public bool BlockTemperatureAfterInitial { get; set; }
        public double TemperatureC { get; set; } = 50;
        public Exception? TemperatureFailure { get; set; }
        public Exception? EnableFailure { get; set; }
        public Exception? ReadFailure { get; set; }
        public Exception? WriteFailure { get; set; }
        public Exception? RestoreFailure { get; set; }
        public Action<bool>? OwnershipChanged { get; set; }
        public bool OwnsFirmware { get; private set; }
        public int ReadCalls;
        public int RestoreCalls;
        public int VerifyCalls;
        public int MaximumPhysicalConcurrency;
        public int TemperatureReadCalls;
        public int MaximumTemperatureConcurrency;
        public int WriteCalls;
        public int EnableCalls;
        public int WritesWithoutOwnership;
        public CpuPowerEnvelope? LastWrittenTarget { get; private set; }
        private int _physicalConcurrency;
        private int _temperatureConcurrency;

        // Firmware temperature reads intentionally do not occupy the power/restore gate.
        public async Task<TemperatureReading> ReadCpuTemperatureAsync()
        {
            var call = Interlocked.Increment(ref TemperatureReadCalls);
            var concurrency = Interlocked.Increment(ref _temperatureConcurrency);
            MaximumTemperatureConcurrency = Math.Max(MaximumTemperatureConcurrency, concurrency);
            try
            {
                if (BlockInitialTemperature && call == 1 || BlockTemperatureAfterInitial && call > 1)
                {
                    TemperatureReadEntered.TrySetResult();
                    await TemperatureReadRelease.Task;
                }
                if (TemperatureFailure is not null) throw TemperatureFailure;
                return new(TemperatureC, Environment.TickCount64);
            }
            finally
            {
                Interlocked.Decrement(ref _temperatureConcurrency);
                if (call > 1 || BlockInitialTemperature) TemperatureReadCompleted.TrySetResult();
            }
        }

        public async Task EnableAsync(int temperature)
        {
            Interlocked.Increment(ref EnableCalls);
            OwnsFirmware = true;
            OwnershipChanged?.Invoke(true);
            EnableEntered.TrySetResult();
            if (BlockEnable) await EnableRelease.Task;
            if (EnableFailure is not null) throw EnableFailure;
        }

        public async Task<CpuPowerEnvelope> ReadAsync()
        {
            await _physicalGate.WaitAsync();
            try
            {
                var concurrency = Interlocked.Increment(ref _physicalConcurrency);
                MaximumPhysicalConcurrency = Math.Max(MaximumPhysicalConcurrency, concurrency);
                Interlocked.Increment(ref ReadCalls);
                ReadEntered.TrySetResult();
                if (BlockRead) await ReadRelease.Task;
                if (ReadFailure is not null) throw ReadFailure;
                return _value;
            }
            finally { Interlocked.Decrement(ref _physicalConcurrency); _physicalGate.Release(); }
        }

        public async Task WriteAsync(CpuPowerEnvelope target)
        {
            await _physicalGate.WaitAsync();
            try
            {
                var concurrency = Interlocked.Increment(ref _physicalConcurrency);
                MaximumPhysicalConcurrency = Math.Max(MaximumPhysicalConcurrency, concurrency);
                Interlocked.Increment(ref WriteCalls);
                if (!OwnsFirmware) Interlocked.Increment(ref WritesWithoutOwnership);
                WriteEntered.TrySetResult();
                if (BlockWrite) await WriteRelease.Task;
                if (WriteFailure is not null) throw WriteFailure;
                _value = target;
                LastWrittenTarget = target;
                if (target == Capability.GetMinimumEnvelope()) MinimumPowerApplied.TrySetResult();
            }
            finally { Interlocked.Decrement(ref _physicalConcurrency); _physicalGate.Release(); }
        }

        // Kept as an observable host API: production controller must not start parallel verifiers.
        public Task VerifyOwnershipAsync() { Interlocked.Increment(ref VerifyCalls); return Task.CompletedTask; }

        public async Task RestoreIfOwnedAsync()
        {
            Interlocked.Increment(ref RestoreCalls);
            RestoreEntered.TrySetResult();
            await _physicalGate.WaitAsync();
            try
            {
                if (BlockRestore) await RestoreRelease.Task;
                if (RestoreFailure is not null) throw RestoreFailure;
                if (OwnsFirmware)
                {
                    OwnsFirmware = false;
                    OwnershipChanged?.Invoke(false);
                }
            }
            finally { _physicalGate.Release(); }
        }
    }
}
