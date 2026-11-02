using System;
using System.Collections.Generic;
using System.IO;
using System.Reflection;
using System.Threading;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Automation;
using LenovoLegionToolkit.Lib.Automation.Pipeline;
using LenovoLegionToolkit.Lib.Automation.Pipeline.Triggers;
using LenovoLegionToolkit.Lib.Automation.Steps;
using LenovoLegionToolkit.Lib.Automation.Utils;

// No default IoC modules, listener initialization, WPF, persisted user settings or firmware adapters.
// Every step and event input is a fixture; the processor and pipeline are production implementations.
using var isolatedLog = RegressionTestHost.CreateIsolated();
using var fixtures = new StartupFixtures();
var baseline = Array.IndexOf(args, "--baseline") >= 0;
var checks = 0;
void Check(bool condition, string name)
{
    if (!condition) throw new InvalidOperationException(name);
    checks++;
}

// The event is emitted synchronously by an awaited startup step, through the actual async-void handler.
// A blocked derived step provides a deterministic completion boundary without any timing benchmark.
{
    var fixture = fixtures.Create();
    var preset = Guid.NewGuid();
    var entered = Signal();
    var release = Signal();
    var finished = Signal();
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            fixture.RaisePresetChanged(preset);
            return Task.CompletedTask;
        })),
        Pipeline(new PresetEventTrigger(preset), new DelegateStep(async () =>
        {
            entered.TrySetResult();
            await release.Task;
            finished.TrySetResult();
        })));
    var startup = fixture.Processor.RunOnStartupAsync();
    try
    {
        await entered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        if (baseline)
        {
            await startup.WaitAsync(TimeSpan.FromSeconds(5));
            Check(!finished.Task.IsCompleted, "BASELINE: startup returns while its derived preset pipeline is still blocked");
        }
        else
            Check(!startup.IsCompleted, "startup waits for the synchronously emitted preset pipeline");
    }
    finally { release.TrySetResult(); }
    await startup.WaitAsync(TimeSpan.FromSeconds(5));
    await finished.Task.WaitAsync(TimeSpan.FromSeconds(5));
    await fixture.Processor.RunNowAsync(new AutomationPipeline()).WaitAsync(TimeSpan.FromSeconds(5));
    Check(finished.Task.IsCompleted, "derived preset actions finish before successful startup completion");
}

{
    var fixture = fixtures.Create();
    fixture.SetPipelines(Pipeline(new OnStartupAutomationPipelineTrigger(),
        new DelegateStep(() => throw new InvalidOperationException("fixture startup step failed"))));
    var thermalStarts = 0;
    Exception? failure = null;
    try
    {
        await fixture.Processor.RunOnStartupAsync().WaitAsync(TimeSpan.FromSeconds(5));
        thermalStarts++;
    }
    catch (Exception ex) { failure = ex; }
    if (baseline)
        Check(failure is null && thermalStarts == 1, "BASELINE: a failed startup action reports success and permits the following thermal start");
    else
        Check(failure?.ToString().Contains("fixture startup step failed", StringComparison.Ordinal) == true && thermalStarts == 0,
            "startup step errors reach the caller and prevent the following thermal start");
}

if (!baseline)
{
    // Multi-generation derivation must drain, while a genuine unrelated event keeps runtime behavior.
    var fixture = fixtures.Create();
    var first = Guid.NewGuid();
    var second = Guid.NewGuid();
    var calls = new List<int>();
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            calls.Add(1);
            fixture.RaisePresetChanged(first);
            return Task.CompletedTask;
        })),
        Pipeline(new PresetEventTrigger(first), new DelegateStep(async () =>
        {
            await Task.Yield();
            calls.Add(2);
            fixture.RaisePresetChanged(second);
        })),
        Pipeline(new PresetEventTrigger(second), new DelegateStep(() =>
        {
            calls.Add(3);
            return Task.CompletedTask;
        })));
    await fixture.Processor.RunOnStartupAsync().WaitAsync(TimeSpan.FromSeconds(5));
    Check(string.Join(",", calls) == "1,2,3", "startup drains multiple event generations in order across asynchronous continuations");

    fixture = fixtures.Create();
    var failedPreset = Guid.NewGuid();
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            fixture.RaisePresetChanged(failedPreset);
            return Task.CompletedTask;
        })),
        Pipeline(new PresetEventTrigger(failedPreset),
            new DelegateStep(() => throw new InvalidOperationException("fixture derived step failed"))));
    var failedDerived = await Capture(fixture.Processor.RunOnStartupAsync());
    Check(failedDerived?.ToString().Contains("fixture derived step failed", StringComparison.Ordinal) == true,
        "a derived pipeline failure also fails the startup batch");

    fixture = fixtures.Create();
    var followingStartupRuns = 0;
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() => throw new InvalidOperationException("fixture first pipeline failed"))),
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            followingStartupRuns++;
            return Task.CompletedTask;
        })));
    Check(await Capture(fixture.Processor.RunOnStartupAsync()) is not null && followingStartupRuns == 0,
        "startup fails closed before running a subsequent nonexclusive pipeline after an action error");

    fixture = fixtures.Create();
    var loopPreset = Guid.NewGuid();
    var loopRuns = 0;
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            fixture.RaisePresetChanged(loopPreset);
            return Task.CompletedTask;
        })),
        Pipeline(new PresetEventTrigger(loopPreset), new DelegateStep(() =>
        {
            loopRuns++;
            fixture.RaisePresetChanged(loopPreset);
            return Task.CompletedTask;
        })));
    var loopFailure = await Capture(fixture.Processor.RunOnStartupAsync());
    Check(loopFailure is InvalidOperationException && loopFailure.Message.Contains("event limit", StringComparison.OrdinalIgnoreCase)
        && loopRuns > 0 && loopRuns <= 256,
        "an event-trigger cycle stops at a finite startup budget and fails explicitly");

    // A failed batch must discard already queued descendants instead of leaking them into runtime work.
    fixture = fixtures.Create();
    var queuedPreset = Guid.NewGuid();
    var descendantRuns = 0;
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            fixture.RaisePresetChanged(queuedPreset);
            throw new InvalidOperationException("fixture parent failed after emitting event");
        })),
        Pipeline(new PresetEventTrigger(queuedPreset), new DelegateStep(() =>
        {
            descendantRuns++;
            return Task.CompletedTask;
        })));
    Check(await Capture(fixture.Processor.RunOnStartupAsync()) is not null && descendantRuns == 0,
        "failed startup discards pending derived actions before they can mutate a later baseline");

    fixture = fixtures.Create();
    var latePreset = Guid.NewGuid();
    var lateRelease = Signal();
    var lateRuns = 0;
    Task? lateEvent = null;
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
        {
            // A detached task inherits its origin's ExecutionContext, but emits nothing until after completion.
            lateEvent = Task.Run(async () =>
            {
                await lateRelease.Task;
                await fixture.ProcessPresetEventAsync(latePreset);
            });
            return Task.CompletedTask;
        })),
        Pipeline(new PresetEventTrigger(latePreset), new DelegateStep(() =>
        {
            lateRuns++;
            return Task.CompletedTask;
        })));
    await fixture.Processor.RunOnStartupAsync().WaitAsync(TimeSpan.FromSeconds(5));
    lateRelease.TrySetResult();
    await lateEvent!.WaitAsync(TimeSpan.FromSeconds(5));
    Check(lateRuns == 0, "late events carrying a completed startup context cannot leak into ordinary runtime work");

    fixture = fixtures.Create();
    var backgroundPreset = Guid.NewGuid();
    var runtimeFinished = Signal();
    var startupEntered = Signal();
    var startupRelease = Signal();
    var followingRuntimeRuns = 0;
    fixture.SetPipelines(
        Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(async () =>
        {
            startupEntered.TrySetResult();
            await startupRelease.Task;
        })),
        Pipeline(new PresetEventTrigger(backgroundPreset), new DelegateStep(() =>
        {
            runtimeFinished.TrySetResult();
            throw new InvalidOperationException("fixture runtime failure remains logged");
        })),
        Pipeline(new PresetEventTrigger(backgroundPreset), new DelegateStep(() =>
        {
            followingRuntimeRuns++;
            return Task.CompletedTask;
        })));
    var inProgress = fixture.Processor.RunOnStartupAsync();
    await startupEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
    // Invoked by this caller, outside the startup ExecutionContext; never assign a global active batch.
    var runtime = fixture.ProcessPresetEventAsync(backgroundPreset);
    startupRelease.TrySetResult();
    await inProgress.WaitAsync(TimeSpan.FromSeconds(5));
    await runtime.WaitAsync(TimeSpan.FromSeconds(5));
    Check(runtimeFinished.Task.IsCompleted && followingRuntimeRuns == 1,
        "ordinary background events remain separate and preserve runtime continuation after a pipeline error");

    fixture = fixtures.Create();
    fixture.Settings.Store.IsEnabled = false;
    var disabledRuns = 0;
    fixture.SetPipelines(Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() =>
    {
        disabledRuns++;
        return Task.CompletedTask;
    })));
    await fixture.Processor.RunOnStartupAsync();
    Check(disabledRuns == 0, "disabled automation still skips startup actions");

    fixture = fixtures.Create();
    fixture.SetPipelines(Pipeline(new OnStartupAutomationPipelineTrigger(), new DelegateStep(() => Task.CompletedTask)));
    await fixture.Processor.RunOnStartupAsync();
    await fixture.Processor.RunOnStartupAsync();
    Check(true, "a completed startup batch does not retain its ExecutionContext or lock on a subsequent call");
}

Check(!IoCContainer.IsInitialized, "startup fixtures never initialize any production IoC module");
Console.WriteLine($"PASS: {checks} {(baseline ? "baseline counterexample" : "startup batch regression")} checks; fake events/steps, temporary settings/logs, no listeners, WPF or hardware I/O");

static TaskCompletionSource Signal() => new(TaskCreationOptions.RunContinuationsAsynchronously);
static AutomationPipeline Pipeline(IAutomationPipelineTrigger trigger, IAutomationStep step) =>
    new() { Trigger = trigger, Steps = { step }, IsExclusive = false };
static async Task<Exception?> Capture(Task task)
{
    try { await task.WaitAsync(TimeSpan.FromSeconds(5)); return null; }
    catch (Exception ex) { return ex; }
}

sealed class DelegateStep(Func<Task> action) : IAutomationStep
{
    public Task<bool> IsSupportedAsync() => Task.FromResult(true);
    public Task RunAsync(AutomationContext context, AutomationEnvironment environment, CancellationToken token) => action();
    public IAutomationStep DeepCopy() => new DelegateStep(action);
}

sealed class PresetEventTrigger(Guid preset) : IAutomationPipelineTrigger
{
    public string DisplayName => "Fixture preset event";
    public Task<bool> IsMatchingEvent(IAutomationEvent value) =>
        Task.FromResult(value is CustomModePresetAutomationEvent e && e.Id == preset);
    public Task<bool> IsMatchingState() => Task.FromResult(false);
    public void UpdateEnvironment(AutomationEnvironment environment) { }
    public IAutomationPipelineTrigger DeepCopy() => new PresetEventTrigger(preset);
}

sealed class StartupFixtures : IDisposable
{
    private readonly string _root = Path.Combine(Path.GetTempPath(), "Y7000p-AutomationStartup-" + Guid.NewGuid().ToString("N"));
    private int _next;
    public StartupFixture Create()
    {
        var directory = Path.Combine(_root, (++_next).ToString());
        Directory.CreateDirectory(directory);
        // Loading an explicit empty fixture store avoids constructing default hardware-backed steps.
        File.WriteAllText(Path.Combine(directory, "automation.json"), "{\"IsEnabled\":true,\"Pipelines\":[]}");
        var constructor = typeof(AutomationSettings).GetConstructor(BindingFlags.Instance | BindingFlags.NonPublic,
            binder: null, [typeof(string)], modifiers: null)
            ?? throw new InvalidOperationException("Isolated automation settings constructor missing.");
        return new((AutomationSettings)constructor.Invoke([directory]));
    }
    public void Dispose()
    {
        var absolute = Path.GetFullPath(_root);
        var parent = Path.GetFullPath(Path.GetTempPath()).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
        if (!absolute.StartsWith(parent, StringComparison.OrdinalIgnoreCase)
            || !Path.GetFileName(absolute).StartsWith("Y7000p-AutomationStartup-", StringComparison.Ordinal))
            throw new InvalidOperationException("Refusing to remove an automation fixture outside its temporary root.");
        if (Directory.Exists(absolute)) Directory.Delete(absolute, recursive: true);
    }
}

sealed class StartupFixture(AutomationSettings settings)
{
    public AutomationSettings Settings { get; } = settings;
    public AutomationProcessor Processor { get; } = new(settings, null!, null!, null!, null!, null!, null!, null!, null!, null!, null!, null!, null!);
    public void SetPipelines(params AutomationPipeline[] pipelines) =>
        typeof(AutomationProcessor).GetField("_pipelines", BindingFlags.Instance | BindingFlags.NonPublic)!
            .SetValue(Processor, new List<AutomationPipeline>(pipelines));
    public void RaisePresetChanged(Guid preset) =>
        typeof(AutomationProcessor).GetMethod("GodModeController_PresetChanged", BindingFlags.Instance | BindingFlags.NonPublic)!
            .Invoke(Processor, [null, preset]);
    public Task ProcessPresetEventAsync(Guid preset) =>
        (Task)typeof(AutomationProcessor).GetMethod("ProcessEvent", BindingFlags.Instance | BindingFlags.NonPublic)!
            .Invoke(Processor, [new CustomModePresetAutomationEvent(preset)])!;
}
