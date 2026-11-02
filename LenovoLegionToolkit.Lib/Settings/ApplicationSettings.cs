using System;
using System.Collections.Generic;
using System.IO;
using System.Runtime.CompilerServices;
using System.Text.RegularExpressions;
using LenovoLegionToolkit.Lib.Controllers;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;
using Newtonsoft.Json.Serialization;
using static LenovoLegionToolkit.Lib.Settings.ApplicationSettings;

namespace LenovoLegionToolkit.Lib.Settings;

public class ApplicationSettings : AbstractSettings<ApplicationSettingsStore>
{
    // Associate the source JSON with the loaded store, rather than the settings
    // instance, so Reset or a separate LoadStore cannot reuse stale extensions.
    private readonly ConditionalWeakTable<ApplicationSettingsStore, JObject> _originalStores = new();
    private static readonly Regex JsonNumberLexeme = new(@"\A-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?\z",
        RegexOptions.CultureInvariant | RegexOptions.NonBacktracking);

    public class ApplicationSettingsStore
    {
        public Theme Theme { get; set; }
        public RGBColor? AccentColor { get; set; }
        public AccentColorSource AccentColorSource { get; set; }
        public PowerModeMappingMode PowerModeMappingMode { get; set; } = PowerModeMappingMode.Disabled;
        private Dictionary<PowerModeState, Guid> _powerPlans = [];
        public Dictionary<PowerModeState, Guid> PowerPlans
        {
            get => _powerPlans;
            set => _powerPlans = value ?? [];
        }
        public Dictionary<PowerModeState, WindowsPowerMode> PowerModes { get; set; } = [];
        public Dictionary<PowerModeState, Dictionary<PowerOverrideKey, string>> Overrides { get; set; } = [];

        public Dictionary<ITSMode, Guid> ITSPowerPlans { get; set; } = [];
        public Dictionary<ITSMode, WindowsPowerMode> ITSPowerModes { get; set; } = [];
        public Dictionary<ITSMode, Dictionary<PowerOverrideKey, string>> ITSOverrides { get; set; } = [];

        public bool MinimizeToTray { get; set; } = true;
        public bool MinimizeOnClose { get; set; }
        public WindowSize? WindowSize { get; set; }
        public TemperatureUnit TemperatureUnit { get; set; }
        private List<RefreshRate> _excludedRefreshRates = [];
        public List<RefreshRate> ExcludedRefreshRates
        {
            get => _excludedRefreshRates;
            set => _excludedRefreshRates = value ?? [];
        }
        public WarrantyInfo? WarrantyInfo { get; set; }
        public bool SynchronizeBrightnessToAllPowerPlans { get; set; }
        public ModifierKey SmartFnLockFlags { get; set; }
        public bool ResetBatteryOnSinceTimerOnReboot { get; set; }
        public bool UseNewSensorDashboard { get; set; }
        public bool EnableHardwareSensors { get; set; }
        // Opt-in diagnostics only: never writes power limits or changes saved presets.
        public bool EnableThermalShadow { get; set; }
        public bool EnableCpuThermalControl { get; set; }
        public Thermal.ThermalControlOptions CpuThermalControl { get; set; } = new();
        public int CpuThermalSamplingMilliseconds { get; set; } = 250;
        public int CpuThermalFirmwareLimitC { get; set; } = 88;
        public bool LockWindowSize { get; set; }
        public bool AlwaysOnTop { get; set; }
        public bool CompactMode { get; set; }
        public WindowPosition? WindowPosition { get; set; }
        public bool RememberWindowPosition { get; set; } = true;
        public bool EnableLogging { get; set; }
        public string BackGroundImageFilePath { get; set; } = string.Empty;
        public double Opacity { get; set; } = 0.3;
        public int BackgroundImageBlur { get; set; } = 0;
        public BackgroundImageStretchMode BackgroundImageStretch { get; set; } = BackgroundImageStretchMode.Crop;
        private List<string> _excludedProcesses = [];
        public List<string> ExcludedProcesses
        {
            get => _excludedProcesses;
            set => _excludedProcesses = value ?? [];
        }
        private List<string> _includedProcesses = [];
        public List<string> IncludedProcesses
        {
            get => _includedProcesses;
            set => _includedProcesses = value ?? [];
        }
        private GameDetectionSettings _gameDetection = new();
        public GameDetectionSettings GameDetection
        {
            get => _gameDetection;
            set => _gameDetection = value ?? new();
        }
        public bool DynamicLightingWarningDontShowAgain { get; set; }
        public int PowerButtonLedBrightness { get; set; } = PowerButtonLedController.MaxBrightness;
        public int? PowerButtonLedLevelOffset { get; set; }
        public bool CustomModeWarningDontShowAgain { get; set; }
        public bool EnableHardwareAcceleration { get; set; }
        public int GPUMonitoringInterval { get; set; } = 5000;
        public int GPUMonitoringStartupDelay { get; set; } = 1000;
        public int GPUKillProcessDelay { get; set; } = 500;
        public WindowBackdropType BackdropType { get; set; } = WindowBackdropType.Mica;
        public bool KeepDgpuAwake { get; set; }
        public int LockedPStateId { get; set; } = -1;

    }
    public class GameDetectionSettings
    {
        public bool UseDiscreteGPU { get; set; } = true;
        public bool UseGameConfigStore { get; set; } = true;
        public bool UseEffectiveGameMode { get; set; } = true;
    }

    public ApplicationSettings() : base("settings.json")
    {
        ConfigureSerializer();
    }

    internal ApplicationSettings(string storageDirectory) : base("settings.json", storageDirectory)
    {
        ConfigureSerializer();
    }

    private void ConfigureSerializer()
    {
        // This store contains concrete configuration types. Unknown metadata is
        // opaque compatibility data, never an instruction to construct a CLR type.
        JsonSerializerSettings.TypeNameHandling = TypeNameHandling.None;
        JsonSerializerSettings.MetadataPropertyHandling = MetadataPropertyHandling.Ignore;
    }

    protected override ApplicationSettingsStore? DeserializeStore(string json)
    {
        var store = JsonConvert.DeserializeObject<ApplicationSettingsStore>(json, JsonSerializerSettings);
        if (store is null)
            return null;

        _originalStores.Add(store, ReadOpaqueProperties(json));
        return store;
    }

    private static JObject ReadOpaqueProperties(string json)
    {
        // Retain unknown numeric lexemes instead of converting them to Double or
        // Decimal. Both types can lose values belonging to a newer settings model.
        // Use Json.NET's own token locations, preserving its legacy comment,
        // single-quote and trailing-comma input support without a second parser.
        var lineStarts = new List<int> { 0 };
        for (var index = 0; index < json.Length; index++)
        {
            if (json[index] == '\r')
            {
                if (index + 1 < json.Length && json[index + 1] == '\n') index++;
                lineStarts.Add(index + 1);
            }
            else if (json[index] == '\n') lineStarts.Add(index + 1);
        }

        using var reader = new JsonTextReader(new StringReader(json)) { DateParseHandling = DateParseHandling.None };
        using var writer = new JTokenWriter();
        while (reader.Read())
        {
            if (reader.TokenType == JsonToken.Comment) continue;
            if (reader.TokenType is JsonToken.Float or JsonToken.Integer)
            {
                var end = lineStarts[reader.LineNumber - 1] + reader.LinePosition;
                var start = end;
                while (start > 0 && json[start - 1] is >= '0' and <= '9' or >= 'a' and <= 'f' or >= 'A' and <= 'F' or 'x' or 'X' or '+' or '-' or '.') start--;
                // Hex/octal and other legacy spellings still normalize through
                // Json.NET; never emit a non-JSON number into the saved document.
                if (start < end && JsonNumberLexeme.IsMatch(json[start..end]))
                {
                    writer.WriteRawValue(json[start..end]);
                    continue;
                }
            }
            writer.WriteToken(reader, writeChildren: false);
        }
        return writer.Token as JObject ?? throw new JsonSerializationException("Settings must be a JSON object.");
    }

    protected override string SerializeStore(ApplicationSettingsStore store)
    {
        var serializer = JsonSerializer.Create(JsonSerializerSettings);
        var current = JObject.FromObject(store, serializer);
        if (_originalStores.TryGetValue(store, out var original))
            PreserveUnknownProperties(original, current, typeof(ApplicationSettingsStore), serializer);
        return JsonConvert.SerializeObject(current, JsonSerializerSettings);
    }

    private static void PreserveUnknownProperties(JObject original, JObject current, Type type, JsonSerializer serializer)
    {
        if (serializer.ContractResolver.ResolveContract(type) is not JsonObjectContract contract)
            return;

        foreach (var property in original.Properties())
        {
            // Match Json.NET's exact-then-case-insensitive member lookup. A legacy
            // spelling of a known field must not duplicate or override its new value.
            var member = contract.Properties.GetClosestMatchProperty(property.Name);
            if (member is null)
            {
                if (current.Property(property.Name, StringComparison.Ordinal) is null)
                    current.Add(property.Name, property.Value.DeepClone());
                continue;
            }

            if (member.Ignored || member.PropertyType is null || member.PropertyName is null)
                continue;

            // Recurse only through typed object models. Dictionaries and arrays are
            // current application data: removed keys/items must stay removed. Null
            // and scalar replacements also take precedence over their old objects.
            if (property.Value is JObject oldObject
                && current[member.PropertyName] is JObject newObject
                && serializer.ContractResolver.ResolveContract(member.PropertyType) is JsonObjectContract)
                PreserveUnknownProperties(oldObject, newObject, member.PropertyType, serializer);
        }
    }
}
