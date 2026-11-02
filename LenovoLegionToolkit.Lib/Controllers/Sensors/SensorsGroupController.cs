// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at http://mozilla.org/MPL/2.0/.
// Copyright (C) RAMSPDToolkit and Contributors.
// Partial Copyright (C) Michael Möller <mmoeller@openhardwaremonitor.org> and Contributors.
// All Rights Reserved.

using System;
using System.Collections.Generic;
using System.Linq;
using System.Text.RegularExpressions;
using System.Threading;
using System.Threading.Tasks;
using System.Threading.Channels;
using LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;
using LenovoLegionToolkit.Lib.Features.Hybrid;
using LenovoLegionToolkit.Lib.Thermal;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.System;
using LenovoLegionToolkit.Lib.Utils;
using LibreHardwareMonitor.Hardware;

namespace LenovoLegionToolkit.Lib.Controllers.Sensors;

public class SensorsGroupController : IDisposable, IAsyncDisposable
{
    #region Constants

    private const string UNKNOWN_NAME = "UNKNOWN";
    private const string HARDWARE_ID_NVIDIA_GPU = "NvidiaGPU";

    private const string REGEX_STRIP_AMD = @"\s+with\s+Radeon\s+Graphics$";
    private const string REGEX_STRIP_INTEL = @"\s*\d+(?:th|st|nd|rd)?\s+Gen\b";
    private const string REGEX_STRIP_NVIDIA = @"(?i)\b(?:Nvidia\s+)?(GeForce\s+(?:RTX|GTX)\s+\d{3,4}(?:\s+(Ti|SUPER|Ti\s+SUPER|M))?)\b(?:\s+Laptop\s+GPU)?(?!\S)";
    private const string REGEX_CLEAN_SPACES = @"\s+";

    #endregion

    private bool _initialized;
    public LibreHardwareMonitorInitialState InitialState { get; private set; }
    public bool IsHybrid => _cpuProvider.IsHybrid;

    private readonly SemaphoreSlim _initSemaphore = new(1, 1);
    private readonly List<IHardware> _hardware = [];

    private Computer? _computer;

    private readonly CpuSensorProvider _cpuProvider;
    private readonly GpuSensorProvider _gpuProvider;
    private readonly MemorySensorProvider _memoryProvider;
    private readonly StorageSensorProvider _storageProvider;
    private readonly List<ISensorProvider> _allProviders = [];

    private volatile bool _isResetting;
    private volatile bool _needRefreshGpuHardware;
    private bool _dgpuHardwareSkipped;

    private bool _selectedGpuIsIgpu;
    public bool SelectedGpuIsIgpu
    {
        get => _selectedGpuIsIgpu;
        set
        {
            lock (_configLock)
            {
                if (_selectedGpuIsIgpu != value)
                {
                    _selectedGpuIsIgpu = value;
                    _cachedGpuName = string.Empty;
                }
            }
        }
    }

    private bool _showAverageCpuFrequency;
    public bool ShowAverageCpuFrequency
    {
        get => _showAverageCpuFrequency;
        set
        {
            lock (_configLock)
            {
                _showAverageCpuFrequency = value;
            }
        }
    }

    public CpuVoltageMode CpuVoltageMode
    {
        get => _cpuProvider.VoltageMode;
        set
        {
            lock (_configLock)
            {
                _cpuProvider.VoltageMode = value;
            }
        }
    }

    public int CpuVoltageCoreIndex
    {
        get => _cpuProvider.VoltageCoreIndex;
        set
        {
            lock (_configLock)
            {
                _cpuProvider.VoltageCoreIndex = value;
            }
        }
    }

    public CpuTemperatureSource CpuTemperatureSource
    {
        get => _cpuProvider.TemperatureSource;
        set
        {
            lock (_configLock)
            {
                _cpuProvider.TemperatureSource = value;
            }
        }
    }

    public int AvailableVoltageCoreCount => _cpuProvider.AvailableCoreCount;

    private bool _isDgpuConnected = true;
    public bool IsDgpuConnected
    {
        get => _isDgpuConnected;
        set
        {
            lock (_configLock)
            {
                if (_isDgpuConnected != value)
                {
                    _isDgpuConnected = value;
                    _cachedGpuName = string.Empty;
                }
            }
        }
    }

    private string _cachedCpuName = string.Empty;
    private string _cachedGpuName = string.Empty;

    private readonly Lock _hardwareLock = new();
    private readonly Lock _configLock = new();
    public HardwareSensorSnapshot Snapshot { get; private set; } = new();
    private volatile bool _hardwareInitialized;

    private long _lastUpdateTick;
    private HardwareUpdateScope _lastUpdateScope;
    private const int MIN_UPDATE_INTERVAL_MS = 100;

    private readonly SensorUpdateScheduler _subscribers = new();
    private CancellationTokenSource? _producerCts;
    private Task? _producerTask;
    private readonly SemaphoreSlim _subscriptionChanged = new(0, 1);
    public event Action<HardwareSensorSnapshot>? SensorsUpdated;

    private readonly GPUController _gpuController = IoCContainer.Resolve<GPUController>();
    private readonly HybridModeFeature _hybridModeFeature = IoCContainer.Resolve<HybridModeFeature>();

    private static readonly IDisposable NoOpDisposable = new LambdaDisposable(() => { });

    private bool IsDgpuEjectInProgress() => _hybridModeFeature.ShouldKeepDGPUAsleep();

    public SensorsGroupController()
    {
        _cpuProvider = IoCContainer.Resolve<CpuSensorProvider>();
        _gpuProvider = IoCContainer.Resolve<GpuSensorProvider>();
        _memoryProvider = IoCContainer.Resolve<MemorySensorProvider>();
        _storageProvider = IoCContainer.Resolve<StorageSensorProvider>();

        _allProviders.Add(_cpuProvider);
        _allProviders.Add(_gpuProvider);
        _allProviders.Add(_memoryProvider);
        _allProviders.Add(_storageProvider);

        _cpuProvider.SensorResetNeeded += OnSensorResetNeeded;
    }

    private void OnSensorResetNeeded()
    {
        if (Interlocked.CompareExchange(ref _resetScheduled, 1, 0) != 0) return;
        _ = Task.Run(() =>
        {
            try { ResetSensors(); }
            catch (Exception ex) { Log.Instance.Trace($"Sensor reset failed.", ex); }
            finally { Volatile.Write(ref _resetScheduled, 0); }
        });
    }

    private int _resetScheduled;

    #region Subscription

    public IDisposable Subscribe(TimeSpan interval, IEnumerable<SensorItem> items)
    {
        if (items is null)
            return NoOpDisposable;
        var scope = ComputeScopeFromSensorItems(items);
        if (scope == HardwareUpdateScope.None)
            return NoOpDisposable;
        return Subscribe(interval, scope);
    }

    public IDisposable Subscribe(TimeSpan interval, IEnumerable<OsdItem> items)
    {
        if (items is null)
            return NoOpDisposable;
        var scope = ComputeScopeFromOsdItems(items);
        if (scope == HardwareUpdateScope.None)
            return NoOpDisposable;
        return Subscribe(interval, scope);
    }

    public IDisposable Subscribe(TimeSpan interval, HardwareUpdateScope scope)
    {
        var key = new object();
        Start(key, interval, scope);
        return new LambdaDisposable(() => Stop(key));
    }

    private HardwareUpdateScope ComputeScopeFromSensorItems(IEnumerable<SensorItem> items)
        => SensorSubscriptionScope.FromSensorItems(items, _allProviders);

    private HardwareUpdateScope ComputeScopeFromOsdItems(IEnumerable<OsdItem> items)
    {
        var set = items as HashSet<OsdItem> ?? new HashSet<OsdItem>(items);
        return _allProviders
            .Where(p => set.Overlaps(p.ProvidedOsdItems))
            .Select(p => p.Scope)
            .Aggregate(HardwareUpdateScope.None, (a, b) => a | b);
    }

    #endregion

    #region Initialization

    public async Task<LibreHardwareMonitorInitialState> IsSupportedAsync()
    {
        LibreHardwareMonitorInitialState result = await InitializeAsync().ConfigureAwait(false);
        try
        {
            bool haveHardware;
            lock (_hardwareLock) { haveHardware = _hardware.Count != 0; }
            if (haveHardware && result is LibreHardwareMonitorInitialState.Initialized or LibreHardwareMonitorInitialState.Success) return result;
        }
        catch (Exception ex)
        {
            Log.Instance.Trace($"Sensor group check failed: {ex}");
            return result;
        }

        return LibreHardwareMonitorInitialState.Fail;
    }

    private void PopulateHardware(bool updateSensors)
    {
        if (_computer is null)
        {
            return;
        }

        foreach (var h in _computer.Hardware)
        {
            try
            {
                if (h.HardwareType == HardwareType.GpuNvidia && IsDgpuEjectInProgress())
                {
                    _dgpuHardwareSkipped = true;
                    _hardware.Add(h);
                    continue;
                }

                if (updateSensors)
                {
                    h.Update();
                }

                _hardware.Add(h);
            }
            catch { /* Ignore */ }
        }
    }

    private void GetHardware()
    {
        lock (_hardwareLock)
        {
            if (_disposed) return;
            if (_hardwareInitialized)
            {
                return;
            }

            if (PawnIOHelper.GetPawnIOState() != PawnIOState.Installed)
            {
                return;
            }

            try
            {
                _computer = new Computer
                {
                    IsCpuEnabled = true,
                    IsGpuEnabled = true,
                    IsMemoryEnabled = true,
                    IsMotherboardEnabled = false,
                    IsControllerEnabled = false,
                    IsNetworkEnabled = false,
                    IsStorageEnabled = true
                };

                _computer.Open();

                var dgpuEjectInProgress = IsDgpuEjectInProgress();
                if (!dgpuEjectInProgress)
                {
                    _computer.Accept(new UpdateVisitor());
                }

                PopulateHardware(updateSensors: dgpuEjectInProgress);
                DiscoverHardware();
                _hardwareInitialized = true;
            }
            catch (Exception ex)
            {
                Log.Instance.Trace($"GetHardware failed: {ex}");
                _computer?.Close();
                _computer = null;
                _hardware.Clear();
                throw;
            }
        }
    }

    private void DiscoverHardware()
    {
        _needRefreshGpuHardware = true;

        foreach (var provider in _allProviders)
        {
            provider.Discover(_hardware);
        }

        LogGpuFallback();
    }

    private async Task<LibreHardwareMonitorInitialState> InitializeAsync()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (_initialized) { InitialState = LibreHardwareMonitorInitialState.Initialized; return InitialState; }
        await _initSemaphore.WaitAsync().ConfigureAwait(false);
        try
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            if (_initialized) { InitialState = LibreHardwareMonitorInitialState.Initialized; return InitialState; }
            await Task.Run(GetHardware).ConfigureAwait(false);
            ObjectDisposedException.ThrowIf(_disposed, this);
            _initialized = _hardwareInitialized;
            InitialState = _hardware.Count == 0 ? LibreHardwareMonitorInitialState.Fail : LibreHardwareMonitorInitialState.Success;
            return InitialState;
        }
        catch (ObjectDisposedException) when (_disposed) { throw; }
        catch (DllNotFoundException) { HandleInitException("DLL Not Found"); InitialState = LibreHardwareMonitorInitialState.PawnIONotInstalled; return InitialState; }
        catch (Exception ex) { HandleInitException(ex.Message); throw; }
        finally { _initSemaphore.Release(); }
    }

    private void HandleInitException(string reason)
    {
        var settings = IoCContainer.Resolve<ApplicationSettings>();
        settings.Store.EnableHardwareSensors = false;
        settings.SynchronizeStore();
        InitialState = LibreHardwareMonitorInitialState.Fail;
    }

    #endregion

    #region Name Getters

    public Task<string> GetCpuNameAsync()
    {
        lock (_hardwareLock)
        {
            if (_isResetting || !IsLibreHardwareMonitorInitialized() || !_cpuProvider.IsAvailable)
                return Task.FromResult(UNKNOWN_NAME);

            if (!string.IsNullOrEmpty(_cachedCpuName))
                return Task.FromResult(_cachedCpuName);

            var cpu = _hardware.FirstOrDefault(h => h.HardwareType == HardwareType.Cpu);
            var name = cpu is not null ? StripName(cpu.Name) : UNKNOWN_NAME;

            if (name != UNKNOWN_NAME)
            {
                _cachedCpuName = name;
            }

            return Task.FromResult(name);
        }
    }

    public Task<string> GetGpuNameAsync()
    {
        lock (_hardwareLock)
        {
            if (_isResetting || !IsLibreHardwareMonitorInitialized())
                return Task.FromResult(UNKNOWN_NAME);

            if (!string.IsNullOrEmpty(_cachedGpuName) && !_needRefreshGpuHardware)
                return Task.FromResult(_cachedGpuName);

            var gpu = ResolveGpuSelection().Hardware;
            var name = gpu is not null ? StripName(gpu.Name) : UNKNOWN_NAME;

            if (name != UNKNOWN_NAME)
            {
                _cachedGpuName = name;
            }

            _needRefreshGpuHardware = false;
            return Task.FromResult(name);
        }
    }

    #endregion

    #region GPU Selection

    private readonly record struct GpuSelection(IHardware? Hardware, bool IsIgpu, bool IsFallback);

    private GpuSelection ResolveGpuSelection()
    {
        var iGpu = _gpuProvider.IgpuHardware;
        var dGpu = _gpuProvider.DgpuHardware;
        var dGpuAvailable = dGpu is not null && _isDgpuConnected;

        if (_selectedGpuIsIgpu)
        {
            return iGpu is not null
                ? new GpuSelection(iGpu, IsIgpu: true, IsFallback: false)
                : new GpuSelection(dGpuAvailable ? dGpu : null, IsIgpu: false, IsFallback: true);
        }

        return dGpuAvailable
            ? new GpuSelection(dGpu, IsIgpu: false, IsFallback: false)
            : new GpuSelection(iGpu, IsIgpu: true, IsFallback: true);
    }

    private void LogGpuFallback()
    {
        var selection = ResolveGpuSelection();
        if (!selection.IsFallback)
        {
            return;
        }

        Log.Instance.Trace($"Selected GPU is unavailable — falling back. [selected={(_selectedGpuIsIgpu ? "iGPU" : "dGPU")}, used={(selection.IsIgpu ? "iGPU" : "dGPU")}, hardware={selection.Hardware?.Name ?? "<null>"}]");
    }

    #endregion

    #region Hardware Refresh

    public void NeedRefreshHardware(string hardwareId)
    {
        if (!IsLibreHardwareMonitorInitialized() || _computer == null || hardwareId != HARDWARE_ID_NVIDIA_GPU) return;
        lock (_hardwareLock)
        {
            ResetSensors();

            try
            {
                NVAPI.Initialize();
            }
            catch { }

            _needRefreshGpuHardware = true;
        }
    }

    #endregion

    #region Update

    private readonly SemaphoreSlim _updateGate = new(1, 1);
    private readonly Channel<HardwareSensorSnapshot> _notifications = Channel.CreateBounded<HardwareSensorSnapshot>(
        new BoundedChannelOptions(1) { FullMode = BoundedChannelFullMode.DropOldest, SingleReader = true });
    private Task? _notificationTask;
    private volatile bool _disposed;
    private readonly ThermalPolicy _thermalPolicy = new();
    public ThermalDecision? LastThermalDecision { get; private set; }
    private bool _thermalShadowEnabled;
    private readonly object _thermalShadowOwner = new();
    public bool ThermalShadowEnabled
    {
        get => Volatile.Read(ref _thermalShadowEnabled);
        set
        {
            lock (_subscribers)
            {
                if (_disposed || _thermalShadowEnabled == value) return;
                if (value) Start(_thermalShadowOwner, TimeSpan.FromMilliseconds(500), HardwareUpdateScope.Cpu);
                else Stop(_thermalShadowOwner);
                Volatile.Write(ref _thermalShadowEnabled, value);
                if (!value) LastThermalDecision = null;
            }
        }
    }

    public (TemperatureReading Package, TemperatureReading CoreMax) GetControlTemperatures()
    {
        var temperatures = Volatile.Read(ref _controlTemperatures);
        return (temperatures.Package, temperatures.CoreMax);
    }

    private sealed record ControlTemperatures(TemperatureReading Package, TemperatureReading CoreMax);
    private ControlTemperatures _controlTemperatures = new(new(double.NaN, 0), new(double.NaN, 0));
    private Task? _thermalTask;

    private void CaptureControlTemperatures(long tick)
    {
        var available = _cpuProvider.IsAvailable;
        Volatile.Write(ref _controlTemperatures, new(
            new TemperatureReading(available ? _cpuProvider.PackageTemperature ?? double.NaN : double.NaN, tick),
            new TemperatureReading(available ? _cpuProvider.CoreMaxTemperature ?? double.NaN : double.NaN, tick)));
    }

    private async Task RunThermalShadowAsync(CancellationToken token)
    {
        try
        {
            while (!token.IsCancellationRequested)
            {
                if (ThermalShadowEnabled)
                {
                    var input = Volatile.Read(ref _controlTemperatures);
                    var next = _thermalPolicy.Evaluate(input.Package, input.CoreMax, Environment.TickCount64, LastThermalDecision);
                    if (LastThermalDecision?.Phase != next.Phase || LastThermalDecision?.PowerFraction != next.PowerFraction)
                        Log.Instance.Trace($"Thermal shadow: {next.Phase}, ceiling fraction={next.PowerFraction:F2}. {next.Reason}");
                    LastThermalDecision = next;
                }
                await Task.Delay(500, token).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
    }

    public async Task UpdateAsync(bool force = false, HardwareUpdateScope scope = HardwareUpdateScope.All)
    {
        await _updateGate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (_disposed || _isResetting || !IsLibreHardwareMonitorInitialized()) return;
            // Each caller's requested scope is sampled; a CPU-only operation cannot satisfy an All request.
            if (!force && SensorUpdateScheduler.Covers(_lastUpdateScope, scope) && Environment.TickCount64 - _lastUpdateTick < MIN_UPDATE_INTERVAL_MS) return;
            lock (_subscribers) _notificationTask ??= Task.Run(PublishNotificationsAsync);
            await PerformUpdateInternal(scope).ConfigureAwait(false);
        }
        finally { _updateGate.Release(); }
    }

    private async Task PublishNotificationsAsync()
    {
        await foreach (var snapshot in _notifications.Reader.ReadAllAsync().ConfigureAwait(false))
        {
            var handlers = SensorsUpdated;
            if (handlers is null) continue;
            foreach (Action<HardwareSensorSnapshot> handler in handlers.GetInvocationList())
            {
                try { handler(snapshot); }
                catch (Exception ex) { Log.Instance.Trace($"Sensor subscriber failed.", ex); }
            }
        }
    }

    private async Task PerformUpdateInternal(HardwareUpdateScope scope)
    {
            if (scope == HardwareUpdateScope.None)
                return;

            var needsGpu = Includes(scope, HardwareUpdateScope.Gpu);
            var dgpuEjectInProgress = needsGpu && IsDgpuEjectInProgress();
            var gpuInactive = needsGpu && (dgpuEjectInProgress ||
                IsGpuInActive(await _gpuController.GetLastKnownStateAsync().ConfigureAwait(false)));

            await Task.Run(() =>
            {
                lock (_hardwareLock)
                {
                    if (_isResetting || _computer == null || !_hardwareInitialized) return;
                    try
                    {
                        var sampleStarted = Environment.TickCount64;
                        bool cpuSampled = false;
                        var brokenHardware = new List<IHardware>();
                        foreach (var h in _hardware)
                        {
                            if (h == null) continue;
                            if (!ShouldUpdateHardware(h, scope, gpuInactive)) continue;
                            try
                            {
                                var hardwareStarted = Environment.TickCount64;
                                h.Update();
                                if (h.HardwareType == HardwareType.Cpu && _cpuProvider.IsAvailable)
                                {
                                    _cpuProvider.Read();
                                    CaptureControlTemperatures(hardwareStarted);
                                    cpuSampled = true;
                                }
                            }
                            catch (Exception ex)
                            {
                                Log.Instance.Trace($"Failed to update hardware {h.Name}: {ex.Message}. It will be removed from the update list.", ex);
                                brokenHardware.Add(h);
                            }
                        }

                        var affectedProviders = new HashSet<ISensorProvider>();
                        foreach (var h in brokenHardware)
                        {
                            _hardware.Remove(h);
                            foreach (var provider in _allProviders.Where(p => ShouldUpdateHardware(h, p.Scope, false)))
                                affectedProviders.Add(provider);
                        }
                        // A failed iGPU/disk must not discard other healthy devices in its domain.
                        foreach (var provider in affectedProviders) provider.Discover(_hardware);

                        if (needsGpu && !dgpuEjectInProgress && _dgpuHardwareSkipped)
                        {
                            DiscoverHardware();
                            _dgpuHardwareSkipped = false;
                        }

                        // Read from providers — each populates provider.Values
                        if (!cpuSampled && Includes(scope, HardwareUpdateScope.Cpu))
                            Volatile.Write(ref _controlTemperatures, new(new(double.NaN, sampleStarted), new(double.NaN, sampleStarted)));

                        IReadOnlyDictionary<SensorItem, float> gpuValues = new Dictionary<SensorItem, float>();

                        if (_gpuProvider.IsAvailable && Includes(scope, HardwareUpdateScope.Gpu))
                        {
                            var selection = ResolveGpuSelection();

                            if (selection.Hardware is null)
                            {
                                gpuValues = GpuSensorProvider.ReadInactive();
                            }
                            else if (selection.IsIgpu)
                            {
                                gpuValues = _gpuProvider.ReadIgpu();
                            }
                            else if (gpuInactive)
                            {
                                gpuValues = GpuSensorProvider.ReadInactive();
                            }
                            else
                            {
                                gpuValues = _gpuProvider.ReadDgpu();
                            }
                        }

                        if (_memoryProvider.IsAvailable && Includes(scope, HardwareUpdateScope.Memory))
                            _memoryProvider.Read();

                        if (Includes(scope, HardwareUpdateScope.Storage))
                            _storageProvider.Read();

                        // Merge all provider values into a single snapshot
                        var samples = new Dictionary<SensorItem, float>();
                        foreach (var provider in _allProviders.Where(p => Includes(scope, p.Scope)))
                        {
                            IReadOnlyDictionary<SensorItem, float> values = provider switch
                            {
                                CpuSensorProvider cpu => cpu.Values,
                                MemorySensorProvider memory => memory.Values,
                                StorageSensorProvider storage => storage.Values,
                                _ => gpuValues
                            };
                            foreach (var item in provider.ProvidedSensorItems)
                                samples[item] = provider.IsAvailable ? values.GetValueOrDefault(item, -1) : -1;
                        }
                        // A later slow GPU/storage operation must not rejuvenate an old CPU reading.
                        // Fans-only subscriptions are firmware/battery refresh heartbeats.
                        // They must neither allocate a hardware copy nor refresh its timestamps.
                        if (samples.Count != 0) Snapshot = Snapshot.WithSamples(samples, sampleStarted);
                        _lastUpdateScope = scope;
                        _lastUpdateTick = sampleStarted;

                        _notifications.Writer.TryWrite(Snapshot);
                    }
                    catch (Exception ex)
                    {
                        Log.Instance.Trace($"Sensor update failed.", ex);
                        if (ex is IndexOutOfRangeException) OnSensorResetNeeded();
                    }
                }
            }).ConfigureAwait(false);
    }

    private void ResetSensors()
    {
        lock (_hardwareLock)
        {
            if (_disposed) return;
            _isResetting = true;
            try
            {
                Snapshot = new HardwareSensorSnapshot();
                _lastUpdateScope = HardwareUpdateScope.None;
                Volatile.Write(ref _controlTemperatures, new(new(double.NaN, 0), new(double.NaN, 0)));
                foreach (var provider in _allProviders) provider.Reset();
                _cachedCpuName = _cachedGpuName = string.Empty;
                _computer?.Close(); _hardware.Clear();
                _computer?.Open();
                _computer?.Reset();
                if (_computer == null)
                {
                    return;
                }

                var dgpuEjectInProgress = IsDgpuEjectInProgress();
                if (!dgpuEjectInProgress)
                {
                    _computer.Accept(new UpdateVisitor());
                }

                PopulateHardware(updateSensors: dgpuEjectInProgress);
                DiscoverHardware();
            }
            finally { _isResetting = false; }
        }
    }

    #endregion

    #region Name Helpers

    private static string StripName(string name)
    {
        if (string.IsNullOrEmpty(name)) return UNKNOWN_NAME;
        var cleaned = name.Trim();
        if (cleaned.Contains("AMD", StringComparison.OrdinalIgnoreCase)) cleaned = Regex.Replace(cleaned, REGEX_STRIP_AMD, "", RegexOptions.IgnoreCase);
        else if (cleaned.Contains("Intel", StringComparison.OrdinalIgnoreCase)) cleaned = Regex.Replace(cleaned, REGEX_STRIP_INTEL, "", RegexOptions.IgnoreCase);
        else if (cleaned.Contains("Nvidia", StringComparison.OrdinalIgnoreCase) || cleaned.Contains("GeForce", StringComparison.OrdinalIgnoreCase))
        {
            var m = Regex.Match(cleaned, REGEX_STRIP_NVIDIA);
            if (m.Success) cleaned = m.Groups[1].Value;
        }
        return Regex.Replace(cleaned, REGEX_CLEAN_SPACES, " ").Trim();
    }

    #endregion

    #region Helpers

    public bool IsGpuInActive(GPUState state) => state is GPUState.Inactive or GPUState.PoweredOff or GPUState.NvidiaGpuNotFound;
    public bool IsLibreHardwareMonitorInitialized() => InitialState is LibreHardwareMonitorInitialState.Initialized or LibreHardwareMonitorInitialState.Success;

    #endregion

    #region Producer Loop

    public void Start(object subscriber, TimeSpan interval) => Start(subscriber, interval, HardwareUpdateScope.All);

    public void Start(object subscriber, TimeSpan interval, HardwareUpdateScope scope)
    {
        lock (_subscribers)
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            _subscribers.Set(subscriber, interval, scope, Environment.TickCount64);
            UpdateProducerLoop();
        }
    }

    public void Stop(object subscriber)
    {
        lock (_subscribers)
        {
            if (_subscribers.Remove(subscriber))
            {
                UpdateProducerLoop();
            }
        }
    }

    private void UpdateProducerLoop()
    {
        // One producer for the lifetime of this controller. Subscription changes do not restart I/O.
        if (_disposed) return;
        if (_subscriptionChanged.CurrentCount == 0) _subscriptionChanged.Release();
        if (_producerTask is not null) return;
        _producerCts = new CancellationTokenSource();
        _notificationTask ??= Task.Run(PublishNotificationsAsync);
        var token = _producerCts.Token;
        _producerTask = Task.Run(() => ProducerLoop(token));
        _thermalTask = Task.Run(() => RunThermalShadowAsync(token));
    }

    private async Task ProducerLoop(CancellationToken token)
    {
        while (!token.IsCancellationRequested)
        {
            HardwareUpdateScope scope;
            lock (_subscribers)
            {
                scope = IsLibreHardwareMonitorInitialized() ? _subscribers.TakeDue(Environment.TickCount64) : HardwareUpdateScope.None;
            }

            try
            {
                if (scope == HardwareUpdateScope.None)
                {
                    TimeSpan delay;
                    lock (_subscribers) delay = _subscribers.Count == 0 ? Timeout.InfiniteTimeSpan
                        : IsLibreHardwareMonitorInitialized() ? _subscribers.NextDelay(Environment.TickCount64) : TimeSpan.FromMilliseconds(500);
                    await _subscriptionChanged.WaitAsync(delay, token).ConfigureAwait(false);
                    continue;
                }
                if (!IsLibreHardwareMonitorInitialized())
                {
                    await _subscriptionChanged.WaitAsync(TimeSpan.FromMilliseconds(500), token).ConfigureAwait(false);
                    continue;
                }

                await UpdateAsync(true, scope).ConfigureAwait(false);

                // Re-evaluate deadlines after I/O; do not add the sampling duration to the period.
            }
            catch (OperationCanceledException)
            {
                break;
            }
            catch (Exception ex)
            {
                Log.Instance.Trace($"ProducerLoop error: {ex}");
                try { await Task.Delay(1000, token).ConfigureAwait(false); }
                catch (OperationCanceledException) { break; }
            }
        }
    }

    #endregion

    #region Static Helpers

    private static bool ShouldUpdateHardware(IHardware hardware, HardwareUpdateScope scope, bool gpuInactive)
    {
        if (gpuInactive && hardware.HardwareType == HardwareType.GpuNvidia)
            return false;

        return hardware.HardwareType switch
        {
            HardwareType.Cpu => Includes(scope, HardwareUpdateScope.Cpu),
            HardwareType.GpuAmd or HardwareType.GpuIntel or HardwareType.GpuNvidia => Includes(scope, HardwareUpdateScope.Gpu),
            HardwareType.Memory => Includes(scope, HardwareUpdateScope.Memory),
            HardwareType.Storage => Includes(scope, HardwareUpdateScope.Storage),
            _ => false
        };
    }

    private static bool Includes(HardwareUpdateScope scope, HardwareUpdateScope flag) => (scope & flag) != 0;

    #endregion

    private Task? _shutdownTask;

    public void Dispose()
    {
        var task = DisposeAsync().AsTask();
        try
        {
            if (!task.Wait(TimeSpan.FromSeconds(2)))
                Log.Instance.Trace($"Sensor shutdown is waiting for a pending hardware operation; cleanup retains hardware ownership.");
        }
        catch (Exception ex) { Log.Instance.Trace($"Sensor shutdown failed.", ex); }
    }

    public ValueTask DisposeAsync()
    {
        lock (_subscribers)
        {
            if (_shutdownTask is not null) return new ValueTask(_shutdownTask);
            _disposed = true;
            _subscribers.Clear();
            _producerCts?.Cancel();
            _notifications.Writer.TryComplete();
            _shutdownTask = Task.Run(FinishShutdownAsync);
            return new ValueTask(_shutdownTask);
        }
    }

    private async Task FinishShutdownAsync()
    {
        if (_thermalTask is not null) await _thermalTask.ConfigureAwait(false);
        if (_producerTask is not null) await _producerTask.ConfigureAwait(false);
        await _initSemaphore.WaitAsync().ConfigureAwait(false);
        await _updateGate.WaitAsync().ConfigureAwait(false);
        try
        {
            _cpuProvider.SensorResetNeeded -= OnSensorResetNeeded;
            lock (_hardwareLock)
            {
                _computer?.Close(); _computer = null; _hardwareInitialized = false; _initialized = false;
                _hardware.Clear();
                foreach (var provider in _allProviders) provider.Reset();
                Snapshot = new HardwareSensorSnapshot();
                InitialState = LibreHardwareMonitorInitialState.Fail;
                Volatile.Write(ref _controlTemperatures, new(new(double.NaN, 0), new(double.NaN, 0)));
            }
        }
        finally { _updateGate.Release(); _initSemaphore.Release(); }
        // Subscribers may dispatch to the UI; never synchronously join their dispatcher here.
        _producerCts?.Dispose();
        GC.SuppressFinalize(this);
    }
}
