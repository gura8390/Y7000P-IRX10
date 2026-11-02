using System;
using System.Threading;
using System.Threading.Channels;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Thermal;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.Utils;

namespace LenovoLegionToolkit.Lib.Controllers;

// Independent watchdog: neither UI notification handlers nor slow GPU samples drive control ticks.
public sealed class CpuThermalController(SensorsGroupController sensors, ApplicationSettings settings)
{
    private readonly SemaphoreSlim _lifecycle = new(1, 1);
    private readonly object _stopLock = new();
    private Task? _stopTask;
    private long _stopGeneration;
    private Exception? _restorationFailure;
    private CancellationTokenSource? _cts;
    private Task? _loop;
    private ThermalCpuTransport? _transport;
    private IDisposable? _subscription;
    private string _status = "未启用 CPU 温控";
    private Task<TemperatureReading>? _pendingFirmwareRead;
    private sealed record DiagnosticNotification(FormattableString Message, Exception? Error);
    private readonly Channel<DiagnosticNotification> _notifications = Channel.CreateBounded<DiagnosticNotification>(
        new BoundedChannelOptions(1) { SingleReader = true, FullMode = BoundedChannelFullMode.DropOldest,
            AllowSynchronousContinuations = false });
    private int _notificationConsumerStarted;
    private sealed record FirmwareSample(TemperatureReading Reading);
    private FirmwareSample _firmwareSample = new(new(double.NaN, 0));
    public string Status => Volatile.Read(ref _status);
    public bool IsRunning => _cts is not null;
    // Best-effort background notifications may coalesce intermediate transitions;
    // Status remains the immediate authoritative value, even if a subscriber blocks.
    public event Action<string>? StatusChanged;

    private void TraceSafely(FormattableString message, Exception? error = null)
    {
        // One bounded consumer for this controller's lifetime. Even diagnostics with a
        // synchronous blocking prefix must not occupy the watchdog or lifecycle lock.
        _notifications.Writer.TryWrite(new(message, error));
        if (Interlocked.CompareExchange(ref _notificationConsumerStarted, 1, 0) == 0)
            _ = Task.Run(PublishNotificationsAsync);
    }

    private void SetStatus(string status)
    {
        if (Interlocked.Exchange(ref _status, status) == status) return;
        TraceSafely($"CPU thermal control: {status}");
    }

    private async Task PublishNotificationsAsync()
    {
        string? publishedStatus = null;
        await foreach (var notification in _notifications.Reader.ReadAllAsync().ConfigureAwait(false))
        {
            WriteTraceSafely(notification.Message, notification.Error);
            // Logging may have stalled while control progressed; publish the current value.
            var status = Status;
            if (publishedStatus == status) continue;
            publishedStatus = status;
            var handlers = StatusChanged;
            if (handlers is null) continue;
            foreach (Action<string> handler in handlers.GetInvocationList())
                try { handler(status); }
                catch (Exception ex) { WriteTraceSafely($"Thermal status subscriber failed.", ex); }
        }
    }

    private static void WriteTraceSafely(FormattableString message, Exception? error = null)
    {
        try { Log.Instance.Trace(message, error); }
        catch { }
    }

    public async Task StartAsync(CancellationToken cancellationToken = default)
    {
        long stopGeneration;
        lock (_stopLock)
        {
            if (_stopTask is { IsCompleted: false })
                throw new InvalidOperationException("温控仍在等待硬件停止，请等待停止状态确认。");
            stopGeneration = _stopGeneration;
        }
        await _lifecycle.WaitAsync(cancellationToken).ConfigureAwait(false);
        ThermalCpuTransport? transport = null;
        try
        {
            if (_restorationFailure is not null)
                throw new InvalidOperationException("上次固件恢复未确认，请检查日志和当前设置后重启程序。", _restorationFailure);
            lock (_stopLock)
                if (stopGeneration != _stopGeneration)
                    throw new InvalidOperationException("温控启动已被新的停止请求取代，请等待停止完成后重新启用。");
            if (_cts is not null)
            {
                if (_loop is { IsCompleted: true })
                    throw new InvalidOperationException("温控循环已停止，请先关闭温控以确认恢复，再重新启用。");
                return;
            }
            cancellationToken.ThrowIfCancellationRequested();
            if (_pendingFirmwareRead is { IsCompleted: false })
                throw new InvalidOperationException("上次固件温度读取仍未返回，请等待读取完成后重新启用温控。未创建更多温度读取。");
            var options = settings.Store.CpuThermalControl ?? new();
            options.Validate();
            var interval = settings.Store.CpuThermalSamplingMilliseconds;
            var firmwareLimit = settings.Store.CpuThermalFirmwareLimitC;
            if (interval < 100 || interval > 1000 || firmwareLimit > 88
                || options.Policy.TargetC > firmwareLimit || options.Policy.WarmC > 85)
                throw new InvalidOperationException("温控配置无效：采样需为100–1000ms，预控不高于85°C，目标不高于固件上限，固件上限不高于88°C。");
            SetStatus("正在验证本机能力、权限与固件状态…");
            transport = await ThermalCpuTransport.DiscoverAsync().WaitAsync(cancellationToken).ConfigureAwait(false);
            _pendingFirmwareRead = transport.ReadCpuTemperatureAsync();
            _ = ObserveFirmwareReadAsync(_pendingFirmwareRead);
            var firmwareTemperature = await _pendingFirmwareRead.WaitAsync(cancellationToken).ConfigureAwait(false);
            if (!firmwareTemperature.IsFresh(Environment.TickCount64, options.Policy.FreshnessMilliseconds))
                throw new InvalidOperationException("本机固件没有提供有效、新鲜的 CPU 温度，未启用温控。");
            Volatile.Write(ref _firmwareSample, new(firmwareTemperature));
            cancellationToken.ThrowIfCancellationRequested();
            await transport.EnableAsync(firmwareLimit).ConfigureAwait(false);
            cancellationToken.ThrowIfCancellationRequested();
            _transport = transport;
            if (settings.Store.EnableHardwareSensors)
                _subscription = sensors.Subscribe(TimeSpan.FromMilliseconds(interval), HardwareUpdateScope.Cpu);
            var supervisor = new ThermalControlSupervisor(transport, transport.Capability, transport.Baseline,
                sensors.GetControlTemperatures, () => Environment.TickCount64, () => (1L, 1L), options,
                getFirmwareTemperature: () => Volatile.Read(ref _firmwareSample).Reading);
            _cts = new CancellationTokenSource();
            var token = _cts.Token;
            _loop = Task.Run(() => RunAsync(transport, supervisor, interval, token));
            SetStatus($"已启用：CPU {options.Policy.WarmC:F0}°C 预控 / 固件 {firmwareLimit}°C 上限；GPU 常规调度");
        }
        catch (Exception ex) when (_cts is null)
        {
            _subscription?.Dispose();
            _subscription = null;
            try { if (transport is not null) await transport.RestoreIfOwnedAsync().ConfigureAwait(false); }
            catch (Exception restoreError)
            {
                _restorationFailure = restoreError;
                SetStatus($"启动已中止，固件恢复未确认：{restoreError.Message}");
                throw new AggregateException("启动失败且固件恢复未确认。", ex, restoreError);
            }
            finally { _transport = null; }
            SetStatus($"启动已中止：{ex.Message}");
            throw;
        }
        finally { _lifecycle.Release(); }
    }

    private async Task RunAsync(ThermalCpuTransport transport, ThermalControlSupervisor supervisor, int interval, CancellationToken token)
    {
        Task<TemperatureReading>? firmwareRead = null;
        long nextFirmwareRead = Environment.TickCount64;
        using var ticks = new PeriodicTimer(TimeSpan.FromMilliseconds(interval));
        try
        {
            while (!token.IsCancellationRequested)
            {
                try
                {
                    if (firmwareRead is { IsCompleted: true })
                    {
                        try { Volatile.Write(ref _firmwareSample, new(await firmwareRead.ConfigureAwait(false))); }
                        catch (Exception ex) { TraceSafely($"Firmware CPU temperature read failed.", ex); }
                        firmwareRead = null;
                    }
                    if (firmwareRead is null && Environment.TickCount64 >= nextFirmwareRead)
                    {
                        nextFirmwareRead = Environment.TickCount64 + interval;
                        firmwareRead = Task.Run(transport.ReadCpuTemperatureAsync);
                        _pendingFirmwareRead = firmwareRead;
                    }
                    // The executor performs bounded periodic readback through the transport.
                    // No parallel verification task may accumulate behind a stalled vendor call.
                    var result = await supervisor.EvaluateAsync(token).ConfigureAwait(false);
                    var temperature = result.ObservedTemperatureC ?? double.NaN;
                    var state = result.Status switch
                    {
                        ThermalControlStatus.Holding => "维持当前功耗",
                        ThermalControlStatus.Verified => "功耗已读回确认",
                        ThermalControlStatus.SensorFault => "温度数据失效，限制为最低合法功耗",
                        ThermalControlStatus.ActuatorFault => "功耗执行故障，禁止自动恢复",
                        ThermalControlStatus.UnknownCompletion => "硬件操作超时，保留限制等待确认",
                        ThermalControlStatus.Superseded => "用户或模式已改变，请重新启用温控",
                        _ => "正在调度"
                    };
                    var source = result.Source switch
                    {
                        ThermalTemperatureSource.LenovoFirmwareAggregate => "固件温度",
                        ThermalTemperatureSource.FirmwareWithPartialHardware => "固件与可用热点",
                        _ => "硬件热点"
                    };
                    SetStatus($"CPU {(double.IsFinite(temperature) ? temperature.ToString("F0") + "°C" : "温度未知")}（{source}）· {state}");
                }
                catch (OperationCanceledException) when (token.IsCancellationRequested) { break; }
                catch (Exception ex)
                {
                    // Do not remove a verified firmware limit or silently restore performance after a fault.
                    SetStatus($"温控状态未确认，请检查：{ex.Message}");
                }
                await ticks.WaitForNextTickAsync(token).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        finally
        {
            if (firmwareRead is not null) _ = ObserveFirmwareReadAsync(firmwareRead);
        }
    }

    private async Task ObserveFirmwareReadAsync(Task<TemperatureReading> read)
    {
        try { await read.ConfigureAwait(false); }
        catch (Exception ex) { TraceSafely($"Late firmware temperature read failed.", ex); }
    }

    public Task StopAsync()
    {
        lock (_stopLock)
        {
            if (_stopTask is { IsCompleted: false }) return _stopTask;
            _stopGeneration++;
            return _stopTask = StopCoreAsync();
        }
    }

    private async Task StopCoreAsync()
    {
        await _lifecycle.WaitAsync().ConfigureAwait(false);
        try
        {
            if (_restorationFailure is not null) throw new InvalidOperationException("上次固件恢复未确认。", _restorationFailure);
            if (_cts is null && _transport is null) return;
            SetStatus("正在停止；等待正在执行的硬件操作并恢复本工具持有的设置…");
            _cts?.Cancel();
            Exception? workerFailure = null;
            if (_loop is not null)
            {
                try { await _loop.ConfigureAwait(false); }
                catch (Exception ex)
                {
                    workerFailure = ex;
                    TraceSafely($"CPU thermal worker failed; restoration will still be attempted.", ex);
                }
            }
            // The transport's shared gate also drains late physical operations before restoration.
            try { if (_transport is not null) await _transport.RestoreIfOwnedAsync().ConfigureAwait(false); }
            catch (Exception ex) { _restorationFailure = ex; throw; }
            finally
            {
                _subscription?.Dispose();
                _subscription = null;
                _transport = null;
                _loop = null;
                _cts?.Dispose();
                _cts = null;
            }
            SetStatus(workerFailure is null ? "已停止 CPU 温控，并恢复仍由本工具持有的设置"
                : "已停止 CPU 温控并恢复设置；控制循环曾异常退出，请检查日志");
        }
        catch (Exception ex)
        {
            SetStatus($"停止或恢复未确认：{ex.Message}");
            throw;
        }
        finally { _lifecycle.Release(); }
    }
}
