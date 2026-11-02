using System;
using System.Linq;
using System.Threading.Tasks;
using Autofac;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Extensions;

public static class IoCInitializationRegressionTests
{
    // This test must be last: production IoC intentionally has process lifetime.
    // Only a private fixture module is registered; never load Lib.IoCModule.
    public static int Run()
    {
        var checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new Exception(name); checks++; }
        void Reject(Action action, string name)
        {
            var rejected = false;
            try { action(); } catch (InvalidOperationException) { rejected = true; }
            Check(rejected, name);
        }
        Check(!IoCContainer.IsInitialized, "IoC is uninitialized before the explicit startup stage");
        Reject(() => IoCContainer.Resolve<FixtureService>(), "Resolve rejects access before startup initialization");
        Reject(() => IoCContainer.TryResolve<FixtureService>(), "TryResolve rejects access before startup initialization");
        IoCContainer.Initialize(new FixtureModule());
        Check(IoCContainer.IsInitialized, "Successful initialization exposes the initialized guard");
        var instance = IoCContainer.Resolve<FixtureService>();
        Check(ReferenceEquals(instance, IoCContainer.TryResolve<FixtureService>()),
            "TryResolve and Resolve retain a shared production registration");
        var resolves = Enumerable.Range(0, 128).Select(_ => Task.Run(IoCContainer.Resolve<FixtureService>)).ToArray();
        Task.WhenAll(resolves).GetAwaiter().GetResult();
        Check(resolves.All(task => ReferenceEquals(task.Result, instance)),
            "Concurrent resolutions honor production singleton registration and synchronization");
        Check(IoCContainer.TryResolve<MissingFixtureService>() is null, "Optional unresolved dependencies return null after initialization");
        Reject(() => IoCContainer.Initialize(new FixtureModule()), "A repeated initialization cannot replace the active container");
        Check(ReferenceEquals(instance, IoCContainer.Resolve<FixtureService>()),
            "A rejected initialization leaves the original singleton and container intact");
        Console.WriteLine($"PASS: {checks} production IoC startup checks (fixture module only; no default modules, listeners or hardware)");
        return checks;
    }

    public sealed class FixtureService { }
    public sealed class MissingFixtureService { }
    private sealed class FixtureModule : Autofac.Module
    {
        protected override void Load(ContainerBuilder builder) => builder.Register<FixtureService>();
    }
}
