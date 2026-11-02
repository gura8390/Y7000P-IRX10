using System;
using System.Collections.Generic;
using System.Linq;
using System.Management;
using System.Threading.Tasks;
using LenovoLegionToolkit.Lib.Utils;

namespace LenovoLegionToolkit.Lib.System.Management;

public static partial class WMI
{
    private static Task<bool> ExistsAsync(string scope, FormattableString query) => Task.Run(() =>
    {
        try
        {
            var queryFormatted = query.ToString(WMIPropertyValueFormatter.Instance);
            using var mos = new ManagementObjectSearcher(scope, queryFormatted);
            using var managementObjects = mos.Get();
            foreach (ManagementObject row in managementObjects)
            {
                using (row) return true;
            }
            return false;
        }
        catch
        {
            return false;
        }
    });

    private static LambdaDisposable Listen(string scope, FormattableString query, Action<PropertyDataCollection> handler)
    {
        var queryFormatted = query.ToString(WMIPropertyValueFormatter.Instance);
        var watcher = new ManagementEventWatcher(scope, queryFormatted);
        watcher.EventArrived += (_, e) => handler(e.NewEvent.Properties);
        try { watcher.Start(); }
        catch { watcher.Dispose(); throw; }

        return new LambdaDisposable(() =>
        {
            try { watcher.Stop(); }
            finally { watcher.Dispose(); }
        });
    }

    private static Task<IEnumerable<T>> ReadAsync<T>(string scope, FormattableString query, Func<PropertyDataCollection, T> converter) => Task.Run<IEnumerable<T>>(() =>
    {
        try
        {
            var queryFormatted = query.ToString(WMIPropertyValueFormatter.Instance);
            using var mos = new ManagementObjectSearcher(scope, queryFormatted);
            using var managementObjects = mos.Get();
            var result = new List<T>();
            foreach (ManagementObject row in managementObjects)
            {
                using (row) result.Add(converter(row.Properties));
            }
            // Conversion and enumeration finish while the COM-backed rows are still owned.
            return result.ToArray();
        }
        catch (ManagementException ex)
        {
            throw new ManagementException($"Read failed: {ex.Message} [scope={scope}, query={query}]", ex);
        }
    });

    private static Task CallAsync(string scope, FormattableString query, string methodName, Dictionary<string, object> methodParams) => Task.Run(() =>
    {
        try
        {
            var queryFormatted = query.ToString(WMIPropertyValueFormatter.Instance);
            using var mos = new ManagementObjectSearcher(scope, queryFormatted);
            using var managementObjects = mos.Get();
            foreach (ManagementObject row in managementObjects)
            {
                using (row)
                using (var input = row.GetMethodParameters(methodName))
                {
                    foreach (var pair in methodParams) input[pair.Key] = pair.Value;
                    using var result = row.InvokeMethod(methodName, input, new InvokeMethodOptions());
                    return;
                }
            }
            throw new InvalidOperationException("No results in query");
        }
        catch (ManagementException ex)
        {
            var methodParamsStr = string.Join(", ", methodParams.Select(kv => $"{kv.Key}={kv.Value}"));
            throw new ManagementException($"Call failed: {ex.Message}. [scope={scope}, query={query}, methodName={methodName}, methodParams={{{methodParamsStr}}}]", ex);
        }
    });

    private static Task<T> CallAsync<T>(string scope, FormattableString query, string methodName, Dictionary<string, object> methodParams, Func<PropertyDataCollection, T> converter) => Task.Run(() =>
    {
        try
        {
            var queryFormatted = query.ToString(WMIPropertyValueFormatter.Instance);

            using var mos = new ManagementObjectSearcher(scope, queryFormatted);
            using var managementObjects = mos.Get();
            foreach (ManagementObject row in managementObjects)
            {
                using (row)
                using (var input = row.GetMethodParameters(methodName))
                {
                    foreach (var pair in methodParams) input[pair.Key] = pair.Value;
                    using var result = row.InvokeMethod(methodName, input, new InvokeMethodOptions());
                    return converter(result.Properties);
                }
            }
            throw new InvalidOperationException("No results in query");
        }
        catch (ManagementException ex)
        {
            var methodParamsStr = string.Join(", ", methodParams.Select(kv => $"{kv.Key}={kv.Value}"));
            throw new ManagementException($"Call failed: {ex.Message}. [scope={scope}, query={query}, methodName={methodName}, methodParams={{{methodParamsStr}}}]", ex);
        }
    });

    private class WMIPropertyValueFormatter : IFormatProvider, ICustomFormatter
    {
        public static readonly WMIPropertyValueFormatter Instance = new();

        private WMIPropertyValueFormatter() { }

        public object GetFormat(Type? formatType)
        {
            if (formatType == typeof(ICustomFormatter))
                return this;

            throw new InvalidOperationException("Invalid type of formatted");
        }

        public string Format(string? format, object? arg, IFormatProvider? formatProvider)
        {
            var stringArg = arg?.ToString()?.Replace("\\", "\\\\");
            return stringArg ?? string.Empty;
        }
    }
}
