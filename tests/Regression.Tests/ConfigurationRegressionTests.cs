using System;
using System.IO;
using System.Linq;
using System.Reflection;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.Thermal;
using Newtonsoft.Json;

public static class ConfigurationRegressionTests
{
    public static int Run()
    {
        var checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new Exception(name); checks++; }
        var constructor = typeof(ApplicationSettings).GetConstructor(BindingFlags.Instance | BindingFlags.NonPublic,
            binder: null, [typeof(string)], modifiers: null)
            ?? throw new InvalidOperationException("Isolated production settings constructor not found.");
        var root = Path.Combine(Path.GetTempPath(), "Y7000p-ConfigRegression-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        ApplicationSettings Settings(string name)
        {
            var directory = Path.Combine(root, name);
            Directory.CreateDirectory(directory);
            return (ApplicationSettings)constructor.Invoke([directory]);
        }
        string PathFor(string name) => Path.Combine(root, name, "settings.json");
        try
        {
            var settings = Settings("roundtrip");
            Check(!File.Exists(PathFor("roundtrip")), "Settings construction performs no file read/write");
            var store = settings.Store;
            Check(!store.EnableCpuThermalControl && !store.EnableThermalShadow,
                "A new config never opts into thermal writes or diagnostics");
            Check(store.CpuThermalSamplingMilliseconds == 250 && store.CpuThermalFirmwareLimitC == 88,
                "Thermal sampling and firmware defaults retain the machine policy");
            store.CpuThermalControl.Validate();
            Check(store.CpuThermalControl.Policy.TargetC == 88 && store.CpuThermalControl.Policy.WarmC == 85
                && store.CpuThermalControl.Policy.RecoverC == 82, "Default thermal thresholds survive the actual nested model");
            Check(ReferenceEquals(store, settings.Store), "Store is cached until Reset");
            store.TemperatureUnit = TemperatureUnit.F;
            store.UseNewSensorDashboard = true;
            store.EnableHardwareSensors = true;
            store.EnableThermalShadow = true;
            store.CpuThermalControl = store.CpuThermalControl with { PredictionMilliseconds = 500, MaximumPredictionC = 3.5 };
            store.IncludedProcesses = ["游戏 fixture.exe", "process=with=equals.exe"];
            store.BackGroundImageFilePath = "C:\\fixture\\图像.png";
            settings.Save();
            var json = File.ReadAllText(PathFor("roundtrip"));
            Check(json.Contains("\"TemperatureUnit\": \"F\"", StringComparison.Ordinal),
                "Production serializer writes enum names");
            Check(json.Contains("游戏 fixture.exe", StringComparison.Ordinal), "Production serializer preserves Unicode");
            var loaded = Settings("roundtrip").Store;
            Check(loaded.TemperatureUnit == TemperatureUnit.F && loaded.UseNewSensorDashboard && loaded.EnableHardwareSensors
                && loaded.EnableThermalShadow && !loaded.EnableCpuThermalControl, "Production Save and LoadStore round-trip the feature switches");
            Check(loaded.CpuThermalControl.PredictionMilliseconds == 500 && loaded.CpuThermalControl.MaximumPredictionC == 3.5,
                "Production Save and LoadStore round-trip nested thermal options");
            Check(loaded.IncludedProcesses.SequenceEqual(store.IncludedProcesses) && loaded.BackGroundImageFilePath == store.BackGroundImageFilePath,
                "Collections and escaped file paths round-trip without replacement loss");
            store.TemperatureUnit = TemperatureUnit.C;
            Check(settings.Store.TemperatureUnit == TemperatureUnit.C, "Unsaved edits remain in the current cached store");
            settings.Reset();
            Check(settings.Store.TemperatureUnit == TemperatureUnit.F && !ReferenceEquals(store, settings.Store),
                "Reset reloads the persisted store rather than the unsaved object");
            settings.Store.MinimizeToTray = false;
            settings.SynchronizeStore();
            Check(!Settings("roundtrip").Store.MinimizeToTray, "SynchronizeStore persists through the actual production method");
            var beforeEnsure = File.ReadAllText(PathFor("roundtrip"));
            settings.Store.MinimizeToTray = true;
            settings.EnsureFileExists();
            Check(File.ReadAllText(PathFor("roundtrip")) == beforeEnsure, "EnsureFileExists does not replace an existing config");
            var ensure = Settings("ensure");
            ensure.EnsureFileExists();
            Check(File.Exists(PathFor("ensure")) && Settings("ensure").Store.MinimizeToTray,
                "EnsureFileExists creates a usable default config in the injected directory");

            _ = Settings("old");
            File.WriteAllText(PathFor("old"), "{\"TemperatureUnit\":\"C\",\"FutureUnknownProperty\":{\"answer\":42}}");
            var old = Settings("old").Store;
            Check(!old.EnableCpuThermalControl && old.CpuThermalControl.Policy.TargetC == 88
                && old.CpuThermalSamplingMilliseconds == 250, "Older configs and unknown properties retain safe new-property defaults");

            foreach (var (name, contents) in new[] { ("malformed", "{"), ("null", "null"),
                ("enum", "{\"TemperatureUnit\":\"not-a-unit\"}"), ("overflow", "{\"CpuThermalSamplingMilliseconds\":2147483648}") })
            {
                _ = Settings(name);
                File.WriteAllText(PathFor(name), contents);
                var invalid = Settings(name);
                Check(invalid.LoadStore() is null, $"Production LoadStore rejects invalid {name} JSON");
                var backups = Directory.GetFiles(Path.Combine(root, name), "settings_backup_*.json");
                Check(backups.Length == 1 && File.ReadAllText(backups[0]) == contents,
                    $"Invalid {name} config is backed up inside the injected temporary directory");
                // A separate instance and directory avoid same-second backup-name collisions.
                var fallbackName = name + "-fallback";
                _ = Settings(fallbackName);
                File.WriteAllText(PathFor(fallbackName), contents);
                Check(!Settings(fallbackName).Store.EnableCpuThermalControl,
                    $"Invalid {name} JSON cannot enable thermal control through fallback");
            }

            _ = Settings("invalid-options");
            File.WriteAllText(PathFor("invalid-options"), "{\"CpuThermalControl\":{\"Policy\":{\"TargetC\":\"NaN\"}}}");
            var invalidOptions = Settings("invalid-options").Store.CpuThermalControl;
            bool rejected = false;
            try { invalidOptions.Validate(); } catch (ArgumentOutOfRangeException) { rejected = true; }
            Check(rejected, "A non-finite threshold accepted by JSON is rejected by the production thermal validator");
            _ = Settings("explicit-null");
            File.WriteAllText(PathFor("explicit-null"), "{\"GameDetection\":null,\"CpuThermalControl\":null}");
            var explicitNull = Settings("explicit-null").Store;
            Check(explicitNull.GameDetection is not null, "Explicit null game-detection settings retain a usable default");
            Check(explicitNull.CpuThermalControl is null, "Legacy nullable thermal options remain compatible with StartAsync's default fallback");
            _ = Settings("null-collections");
            File.WriteAllText(PathFor("null-collections"), "{\"IncludedProcesses\":null,\"ExcludedProcesses\":null,\"ExcludedRefreshRates\":null,\"PowerPlans\":null}");
            var nullCollectionsSettings = Settings("null-collections");
            var nullCollections = nullCollectionsSettings.Store;
            Check(nullCollections.IncludedProcesses.Count == 0
                && nullCollections.IncludedProcesses.ToHashSet(StringComparer.OrdinalIgnoreCase).Count == 0,
                "Explicit null included-process lists retain the Count/ToHashSet operations used by game detection and its editor");
            Check(nullCollections.ExcludedProcesses.Count == 0
                && nullCollections.ExcludedProcesses.ToHashSet(StringComparer.OrdinalIgnoreCase).Count == 0,
                "Explicit null excluded-process lists retain the ToHashSet operation used by the process editor");
            Check(!Array.Empty<RefreshRate>().Union(nullCollections.ExcludedRefreshRates).Any(),
                "Explicit null excluded-refresh-rate lists retain the Union operation used by the refresh-rate editor");
            Check(nullCollections.PowerPlans.Count == 0 && !nullCollections.PowerPlans.TryGetValue(PowerModeState.GodMode, out _),
                "Explicit null power-plan dictionaries retain the TryGetValue operation used by the power-plan editor");
            nullCollectionsSettings.Save();
            var normalized = Settings("null-collections").Store;
            Check(normalized.IncludedProcesses.Count == 0 && normalized.ExcludedProcesses.Count == 0
                && normalized.ExcludedRefreshRates.Count == 0 && normalized.PowerPlans.Count == 0,
                "Production Save and LoadStore preserve normalized empty collections through a second round-trip");

            var blockedDirectory = Path.Combine(root, "file-in-place-of-directory");
            File.WriteAllText(blockedDirectory, "fixture sentinel");
            var blockedSettings = (ApplicationSettings)constructor.Invoke([blockedDirectory]);
            blockedSettings.Store.EnableCpuThermalControl = true;
            bool saveFailureReported = false;
            try { blockedSettings.SynchronizeStoreOrThrow(); }
            catch (IOException) { saveFailureReported = true; }
            Check(saveFailureReported, "Critical settings persistence reports an I/O failure instead of claiming a saved autostart flag");
            Check(File.ReadAllText(blockedDirectory) == "fixture sentinel", "A rejected settings write does not damage the blocking file");
            blockedSettings.SynchronizeStore();
            Check(File.ReadAllText(blockedDirectory) == "fixture sentinel", "Legacy best-effort persistence retains its non-throwing behavior without altering the failed target");

            var fallback = TimeSpan.FromSeconds(1);
            foreach (var seconds in new[] { double.NaN, double.NegativeInfinity, double.PositiveInfinity,
                double.MinValue, double.MaxValue, -1, 0, double.Epsilon, int.MaxValue / 1000d + 0.001 })
                Check(LatestSensorRefresh.IntervalOrDefault(seconds, fallback) == fallback,
                    $"Invalid saved refresh interval {seconds} uses its model default");
            foreach (var seconds in new[] { .0001, .1, .5, 1, 2, 3, 4, 5, 60, int.MaxValue / 1000d })
                Check(LatestSensorRefresh.IntervalOrDefault(seconds, fallback) == TimeSpan.FromSeconds(seconds),
                    $"Valid saved refresh interval {seconds} remains unchanged");
            foreach (var invalidFallback in new[] { TimeSpan.Zero, TimeSpan.FromTicks(-1), TimeSpan.FromMilliseconds((double)int.MaxValue + 1) })
            {
                bool throws = false;
                try { LatestSensorRefresh.IntervalOrDefault(1, invalidFallback); }
                catch (ArgumentOutOfRangeException) { throws = true; }
                Check(throws, "An invalid developer-supplied default is rejected rather than silently scheduled");
            }
            Console.WriteLine($"PASS: {checks} production configuration/interval checks (temporary directory only; no AppFlags initialization, user settings or hardware I/O)");
            return checks;
        }
        finally
        {
            var resolved = Path.GetFullPath(root);
            var temporaryParent = Path.GetFullPath(Path.GetTempPath()).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            if (!resolved.StartsWith(temporaryParent, StringComparison.OrdinalIgnoreCase)
                || !Path.GetFileName(resolved).StartsWith("Y7000p-ConfigRegression-", StringComparison.Ordinal))
                throw new InvalidOperationException("Refusing to remove a config test directory outside its temporary root.");
            Directory.Delete(resolved, recursive: true);
        }
    }
}
