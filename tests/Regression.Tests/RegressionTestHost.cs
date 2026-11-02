using System;
using System.IO;
using System.Reflection;
using LenovoLegionToolkit.Lib.Utils;

public static class RegressionTestHost
{
    public static IDisposable CreateIsolated()
    {
        var singleton = typeof(Log).GetField("_instance", BindingFlags.Static | BindingFlags.NonPublic)
            ?? throw new InvalidOperationException("Production log singleton not found.");
        if (singleton.GetValue(null) is not null)
            throw new InvalidOperationException("Install the isolated test host before accessing any production logger.");
        var constructor = typeof(Log).GetConstructor(BindingFlags.Instance | BindingFlags.NonPublic,
            binder: null, [typeof(string)], modifiers: null)
            ?? throw new InvalidOperationException("Isolated production log constructor not found.");
        var directory = Path.Combine(Path.GetTempPath(), "Y7000p-RegressionLog-" + Guid.NewGuid().ToString("N"));
        var instance = (Log)constructor.Invoke([directory]);
        singleton.SetValue(null, instance);
        return new IsolatedLogHost(singleton, instance, directory);
    }

    private sealed class IsolatedLogHost(FieldInfo singleton, Log instance, string directory) : IDisposable
    {
        private bool _disposed;
        public void Dispose()
        {
            if (_disposed) return;
            _disposed = true;
            if (!ReferenceEquals(singleton.GetValue(null), instance))
                throw new InvalidOperationException("The regression process replaced its isolated logger unexpectedly.");
            singleton.SetValue(null, null);
            var resolved = Path.GetFullPath(directory);
            var temporaryParent = Path.GetFullPath(Path.GetTempPath()).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            if (!resolved.StartsWith(temporaryParent, StringComparison.OrdinalIgnoreCase)
                || !Path.GetFileName(resolved).StartsWith("Y7000p-RegressionLog-", StringComparison.Ordinal))
                throw new InvalidOperationException("Refusing to remove a log fixture outside its temporary root.");
            Directory.Delete(resolved, recursive: true);
        }
    }
}
