using System;
using System.IO;
using System.Reflection;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Settings;
using Newtonsoft.Json;
using Newtonsoft.Json.Linq;

public static class LegacySettingsRegressionTests
{
    public static int Run()
    {
        var checks = 0;
        void Check(bool condition, string name) { if (!condition) throw new Exception(name); checks++; }
        var constructor = typeof(ApplicationSettings).GetConstructor(BindingFlags.Instance | BindingFlags.NonPublic,
            binder: null, [typeof(string)], modifiers: null)
            ?? throw new InvalidOperationException("Isolated production settings constructor not found.");
        var root = Path.Combine(Path.GetTempPath(), "Y7000p-LegacySettings-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(root);
        var path = Path.Combine(root, "settings.json");
        ApplicationSettings Settings() => (ApplicationSettings)constructor.Invoke([root]);
        JObject Read() => Parse(File.ReadAllText(path));
        try
        {
            // Only synthetic configuration lives in this injected temporary directory.
            // The fixture deliberately mixes removed properties and fields from a newer version.
            const string fixtureJson = """
                {
                  "TemperatureUnit": "C",
                  "MinimizeToTray": false,
                  "IncludedProcesses": ["old.exe", "delete-me.exe"],
                  "PowerPlans": {
                    "Quiet": "11111111-1111-1111-1111-111111111111",
                    "Balance": "22222222-2222-2222-2222-222222222222"
                  },
                  "SmartKeySinglePressActionId": "33333333-3333-3333-3333-333333333333",
                  "FutureEnum": "NotAKnownEnumMember",
                  "FutureOption": 1,
                  "futureoption": 2,
                  "FuturePrecision": 1.2345678901234567890123456789,
                  "FutureHugeExponent": 1e308,
                  "FutureNull": null,
                  "FutureArray": [1, "未来模式", null, true, { "nested": ["X", { "enabled": false }] }],
                  "FutureObject": {
                    "enum": "FutureState",
                    "dateText": "2026-11-02T00:00:00Z",
                    "empty": {},
                    "nothing": null,
                    "deep": { "array": [null, 42] }
                  },
                  "GameDetection": {
                    "UseDiscreteGPU": false,
                    "FutureOption": { "enabled": true },
                    "futureoption": { "enabled": false },
                    "FutureNested": { "enum": "FutureDetector", "values": [1, null, "未知"] },
                    "FutureNull": null
                  },
                  "CpuThermalControl": {
                    "PredictionMilliseconds": 500,
                    "FutureControl": { "state": "FutureControlState", "values": [null, false] },
                    "Policy": {
                      "TargetC": 88,
                      "WarmC": 85,
                      "RecoverC": 82,
                      "FuturePolicy": { "enum": "FuturePolicyState", "nullable": null }
                    }
                  },
                  "WindowSize": { "Width": 900, "Height": 600, "FutureBounds": { "unit": "DIP" } }
                }
                """;
            var fixture = Parse(fixtureJson);
            File.WriteAllText(path, fixtureJson);
            var settings = Settings();
            Check(!settings.Store.MinimizeToTray && settings.Store.TemperatureUnit == TemperatureUnit.C,
                "Legacy production LoadStore reads known fields without fallback");
            Check(!settings.Store.GameDetection.UseDiscreteGPU && settings.Store.CpuThermalControl.PredictionMilliseconds == 500,
                "Legacy production LoadStore reads known nested fields without fallback");

            void CheckUnknown(JObject actual, string operation)
            {
                foreach (var name in new[] { "SmartKeySinglePressActionId", "FutureEnum", "FutureOption", "futureoption", "FutureNull", "FutureArray", "FutureObject" })
                    Check(JToken.DeepEquals(actual[name], fixture[name]), $"{operation} preserves removed/unknown {name}");
                foreach (var nested in new[] { "GameDetection.FutureNested", "GameDetection.FutureNull", "GameDetection.FutureOption", "GameDetection.futureoption",
                    "CpuThermalControl.FutureControl", "CpuThermalControl.Policy.FuturePolicy", "WindowSize.FutureBounds" })
                    Check(JToken.DeepEquals(actual.SelectToken(nested), fixture.SelectToken(nested)),
                        $"{operation} preserves unknown fields inside the known {nested} model");
            }

            for (var round = 0; round < 3; round++)
            {
                var store = settings.Store;
                store.TemperatureUnit = TemperatureUnit.F;
                store.MinimizeToTray = true;
                store.IncludedProcesses = ["current.exe"];
                store.PowerPlans.Remove(PowerModeState.Quiet);
                store.PowerPlans[PowerModeState.Balance] = Guid.Parse("44444444-4444-4444-4444-444444444444");
                store.GameDetection.UseDiscreteGPU = true;
                // The application replaces these immutable records using `with`.
                store.CpuThermalControl = store.CpuThermalControl with
                {
                    PredictionMilliseconds = 700 + round,
                    Policy = store.CpuThermalControl.Policy with { TargetC = 89 }
                };
                store.WindowSize = new WindowSize(1000 + round, 650);
                if (round == 0) settings.Save();
                else if (round == 1) settings.SynchronizeStoreOrThrow();
                else settings.SynchronizeStore();
                var actual = Read();
                CheckUnknown(actual, $"Round {round + 1}");
                using (var exactNumbers = System.Text.Json.JsonDocument.Parse(File.ReadAllText(path)))
                {
                    Check(exactNumbers.RootElement.GetProperty("FuturePrecision").GetRawText() == "1.2345678901234567890123456789",
                        $"Round {round + 1} preserves every digit of an unknown high-precision JSON number");
                    Check(exactNumbers.RootElement.GetProperty("FutureHugeExponent").GetDouble() == 1e308,
                        $"Round {round + 1} preserves an unknown exponent larger than Decimal's numeric range");
                }
                Check((string?)actual["TemperatureUnit"] == "F" && (bool?)actual["MinimizeToTray"] == true,
                    $"Round {round + 1} writes current typed scalar fields over legacy values");
                Check((bool?)actual.SelectToken("GameDetection.UseDiscreteGPU") == true
                    && (long?)actual.SelectToken("CpuThermalControl.PredictionMilliseconds") == 700 + round
                    && (double?)actual.SelectToken("CpuThermalControl.Policy.TargetC") == 89
                    && (double?)actual.SelectToken("WindowSize.Width") == 1000 + round,
                    $"Round {round + 1} writes current nested models and replaced value types");
                Check(actual["IncludedProcesses"] is JArray { Count: 1 } processes && (string?)processes[0] == "current.exe"
                    && actual.SelectToken("PowerPlans.Quiet") is null
                    && (Guid?)actual.SelectToken("PowerPlans.Balance") == store.PowerPlans[PowerModeState.Balance],
                    $"Round {round + 1} does not resurrect deleted typed collection members");
                settings.Store.MinimizeToTray = false;
                settings.Reset();
                Check(settings.Store.MinimizeToTray && !ReferenceEquals(store, settings.Store),
                    $"Round {round + 1} Reset discards unsaved edits and reloads the actual persisted store");
            }

            // Known null values must take precedence over the old object and its extension fields.
            settings.Store.WindowSize = null;
            settings.Store.CpuThermalControl = null!;
            settings.Save();
            var withNulls = Read();
            Check(withNulls["WindowSize"]?.Type == JTokenType.Null && withNulls["CpuThermalControl"]?.Type == JTokenType.Null,
                "Explicit current known null values replace legacy objects rather than resurrecting them");
            Check(JToken.DeepEquals(withNulls["FutureNull"], fixture["FutureNull"]), "Unknown null values remain present");

            // A fresh file must not inherit extensions cached for a previous loaded store.
            File.Delete(path);
            settings.Reset();
            settings.EnsureFileExists();
            Check(Read()["FutureObject"] is null && settings.Store.TemperatureUnit == TemperatureUnit.C,
                "Reset after file removal creates clean defaults without stale extension data");

            var metadataFixture = Parse("""
                {
                  "$type": "Untrusted.Configuration.Type, Missing.Assembly",
                  "$id": "opaque-root-id",
                  "$ref": "opaque-root-ref",
                  "MinimizeToTray": false,
                  "GameDetection": {
                    "$type": "Untrusted.Nested.Type, Missing.Assembly",
                    "$ref": "opaque-nested-ref",
                    "UseDiscreteGPU": false
                  },
                  "FutureObject": { "$type": "Untrusted.Extension.Type, Missing.Assembly", "$values": [null, "FutureEnum"] }
                }
                """);
            File.WriteAllText(path, metadataFixture.ToString());
            settings = Settings();
            Check(!settings.Store.MinimizeToTray && !settings.Store.GameDetection.UseDiscreteGPU,
                "Untrusted metadata remains inert while the known production model loads normally");
            settings.Save();
            var metadataActual = Read();
            foreach (var tokenPath in new[] { "$type", "$id", "$ref", "GameDetection.$type", "GameDetection.$ref", "FutureObject" })
                Check(JToken.DeepEquals(metadataActual.SelectToken(tokenPath), metadataFixture.SelectToken(tokenPath)),
                    $"Unknown metadata at {tokenPath} round-trips as JSON without type resolution or reference handling");
            Check(Directory.GetFiles(root, "settings_backup_*.json").Length == 0,
                "All supported legacy/metadata fixtures load without a corrupt-settings backup or fallback");

            // JSON.NET accepts case-insensitive known names. They must not be mistaken for extensions.
            File.WriteAllText(path, "{\"temperatureunit\":\"F\",\"gamedetection\":{\"usediscretegpu\":false,\"FutureNested\":null}}");
            settings = Settings();
            settings.Store.TemperatureUnit = TemperatureUnit.C;
            settings.Store.GameDetection.UseDiscreteGPU = true;
            settings.Save();
            var caseInsensitive = Read();
            Check(caseInsensitive.Property("temperatureunit", StringComparison.Ordinal) is null
                && (string?)caseInsensitive["TemperatureUnit"] == "C"
                && caseInsensitive.Property("gamedetection", StringComparison.Ordinal) is null
                && (bool?)caseInsensitive.SelectToken("GameDetection.UseDiscreteGPU") == true
                && caseInsensitive.SelectToken("GameDetection.FutureNested")?.Type == JTokenType.Null,
                "Case-insensitive known names update once under the current model while nested unknown null survives");

            // The input syntax already accepted by Json.NET remains supported while
            // unknown numeric tokens retain their precision in the strict JSON output.
            File.WriteAllText(path, """
                {
                  // Synthetic legacy syntax: comments, single-quoted names, trailing commas.
                  'MinimizeToTray': false,
                  'FutureLoose': { 'precision': 1.2345678901234567890123456789, },
                  'GameDetection': { 'UseDiscreteGPU': false, 'FutureHuge': 1e308, },
                }
                """);
            settings = Settings();
            Check(!settings.Store.MinimizeToTray && !settings.Store.GameDetection.UseDiscreteGPU,
                "Legacy comments, single quotes and trailing commas still load through the production serializer");
            settings.Save();
            using (var looseNumbers = System.Text.Json.JsonDocument.Parse(File.ReadAllText(path)))
            {
                Check(looseNumbers.RootElement.GetProperty("FutureLoose").GetProperty("precision").GetRawText() == "1.2345678901234567890123456789",
                    "Unknown numbers from a loose Json.NET object retain every original digit in strict JSON output");
                Check(looseNumbers.RootElement.GetProperty("GameDetection").GetProperty("FutureHuge").GetDouble() == 1e308,
                    "Loose legacy syntax retains nested unknown huge exponents without Decimal overflow or fallback");
            }

            // Token locations use UTF-16 positions rather than encoded byte offsets.
            // Exercise each newline convention and keys that cannot be addressed by a simple JSONPath.
            const string arrayKey = "前缀😀.数组[0]";
            const string nestedKey = "未来.策略[0]";
            var numberLexemes = new[] { "1e9999", "1e-9999", "-0", "-0.0", "1.2345678901234567890123456789",
                "-9.8765432109876543210987654321E-123", "1234567890123456789012345678901234567890" };
            foreach (var (lineName, newline) in new[] { ("CRLF", "\r\n"), ("LF", "\n"), ("CR", "\r") })
            {
                var boundaryJson = string.Join(newline, new[]
                {
                    "{",
                    "  // 汉字、é和😀出现在数值前；注释也参与行位置计算。",
                    "  'MinimizeToTray': false,",
                    "  'GameDetection': { 'UseDiscreteGPU': false, '" + nestedKey + "': { 'large': 1e9999, 'tiny': 1e-9999, }, },",
                    "  '" + arrayKey + "': [",
                    "    " + string.Join(", ", numberLexemes) + ",",
                    "    { '含.点[2]😀': 2.5000e+0003, '另一[]': -0 },",
                    "  ],",
                    "  '同行前缀': '汉字😀é', '奇怪.根[2]': 0.000000000000000000000000000000000000001,",
                    "}"
                });
                File.WriteAllText(path, boundaryJson);
                settings = Settings();
                Check(!settings.Store.MinimizeToTray && !settings.Store.GameDetection.UseDiscreteGPU,
                    $"{lineName} numeric boundaries and Unicode prefixes load without default fallback");
                for (var round = 0; round < 2; round++)
                {
                    settings.Store.MinimizeToTray = true;
                    settings.Save();
                    // Parsing the output with a separate strict parser also rejects raw-token corruption.
                    using var boundaryNumbers = System.Text.Json.JsonDocument.Parse(File.ReadAllText(path));
                    var output = boundaryNumbers.RootElement;
                    Check(output.GetProperty("MinimizeToTray").GetBoolean(),
                        $"{lineName} round {round + 1} keeps current typed updates beside opaque numeric fields");
                    var array = output.GetProperty(arrayKey);
                    for (var index = 0; index < numberLexemes.Length; index++)
                        Check(array[index].GetRawText() == numberLexemes[index],
                            $"{lineName} round {round + 1} preserves array numeric lexeme {numberLexemes[index]}");
                    Check(array[7].GetProperty("含.点[2]😀").GetRawText() == "2.5000e+0003",
                        $"{lineName} round {round + 1} preserves exponent spelling in a Unicode key containing dot/brackets");
                    Check(array[7].GetProperty("另一[]").GetRawText() == "-0",
                        $"{lineName} round {round + 1} preserves integer negative zero inside an opaque array object");
                    var nested = output.GetProperty("GameDetection").GetProperty(nestedKey);
                    Check(nested.GetProperty("large").GetRawText() == "1e9999",
                        $"{lineName} round {round + 1} preserves a nested exponent beyond Double range");
                    Check(nested.GetProperty("tiny").GetRawText() == "1e-9999",
                        $"{lineName} round {round + 1} preserves a nested exponent below Double range");
                    Check(output.GetProperty("奇怪.根[2]").GetRawText() == "0.000000000000000000000000000000000000001",
                        $"{lineName} round {round + 1} uses character offsets after a multibyte Unicode prefix on the same line");
                    settings.Reset();
                }
                Check(Directory.GetFiles(root, "settings_backup_*.json").Length == 0,
                    $"{lineName} supported numeric boundaries never fall back to a corrupt-settings backup");
            }

            // Json.NET accepts hexadecimal/octal integers and a leading-dot fraction.
            // These spellings must use its strict-JSON normalization rather than an extracted suffix.
            File.WriteAllText(path, """
                {
                  'MinimizeToTray': false,
                  'FutureHex': 0x10,
                  'FutureOctal': 017,
                  'FutureFraction': .5,
                  'FutureArray': [0x10, 017, .5],
                  'GameDetection': { 'UseDiscreteGPU': false, 'FutureHex': 0x10, 'FutureOctal': 017 },
                }
                """);
            settings = Settings();
            Check(!settings.Store.MinimizeToTray && !settings.Store.GameDetection.UseDiscreteGPU,
                "Supported loose hexadecimal, octal and leading-dot inputs load without default fallback");
            settings.Save();
            using (var normalizedNumbers = System.Text.Json.JsonDocument.Parse(File.ReadAllText(path)))
            {
                var output = normalizedNumbers.RootElement;
                Check(output.GetProperty("FutureHex").GetInt32() == 16,
                    "An opaque hexadecimal integer becomes strict JSON 16 rather than a decimal suffix");
                Check(output.GetProperty("FutureOctal").GetInt32() == 15,
                    "An opaque octal integer becomes strict JSON 15 rather than the decimal spelling 17");
                Check(output.GetProperty("FutureFraction").GetDouble() == .5,
                    "An opaque leading-dot fraction becomes strict JSON with its original numeric meaning");
                var array = output.GetProperty("FutureArray");
                Check(array[0].GetInt32() == 16 && array[1].GetInt32() == 15 && array[2].GetDouble() == .5,
                    "Loose number normalization also preserves numeric meaning inside an opaque array");
                var nested = output.GetProperty("GameDetection");
                Check(nested.GetProperty("FutureHex").GetInt32() == 16 && nested.GetProperty("FutureOctal").GetInt32() == 15,
                    "Loose number normalization preserves nested opaque values in a known typed model");
            }
            Check(Directory.GetFiles(root, "settings_backup_*.json").Length == 0,
                "All supported loose-number spellings load without a corrupt-settings backup");

            Console.WriteLine($"PASS: {checks} production legacy-settings compatibility checks (synthetic JSON and injected temporary directory only)");
            return checks;
        }
        finally
        {
            var resolved = Path.GetFullPath(root);
            var temporaryParent = Path.GetFullPath(Path.GetTempPath()).TrimEnd(Path.DirectorySeparatorChar) + Path.DirectorySeparatorChar;
            if (!resolved.StartsWith(temporaryParent, StringComparison.OrdinalIgnoreCase)
                || !Path.GetFileName(resolved).StartsWith("Y7000p-LegacySettings-", StringComparison.Ordinal))
                throw new InvalidOperationException("Refusing to remove a legacy-settings fixture outside its temporary root.");
            Directory.Delete(resolved, recursive: true);
        }
    }

    private static JObject Parse(string json)
    {
        using var reader = new JsonTextReader(new StringReader(json)) { DateParseHandling = DateParseHandling.None };
        return JObject.Load(reader);
    }
}
