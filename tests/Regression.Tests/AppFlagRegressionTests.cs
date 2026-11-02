using System;
using System.Reflection;
using LenovoLegionToolkit.Lib.Utils;

public static class AppFlagRegressionTests
{
    public static int Run()
    {
        var parse = typeof(AppFlags).GetMethod("StringValue", BindingFlags.Static | BindingFlags.NonPublic)
            ?? throw new InvalidOperationException("Production argument parser not found.");
        var boolParse = typeof(AppFlags).GetMethod("BoolValue", BindingFlags.Static | BindingFlags.NonPublic)
            ?? throw new InvalidOperationException("Production boolean argument parser not found.");
        int checks = 0;
        string? Parse(string[] arguments, string key) => (string?)parse.Invoke(null, [arguments, key]);
        void Check(bool value, string name) { if (!value) throw new Exception(name); checks++; }
        foreach (var key in new[] { "--proxy-url", "--proxy-username", "--proxy-password" })
        {
            Check(Parse([key], key) is null, "Bare option does not throw or consume the following argument");
            Check(Parse([key + "-other=ignored"], key) is null, "Similar option names do not match");
            Check(Parse([key + "="], key) == "", "Empty explicit value is safely parsed");
            Check(Parse([key + "=example=with spaces"], key) == "example=with spaces", "Exact option preserves its complete value");
        }
        Check(Parse(["--proxy-url", "--proxy-url=http://localhost"], "--proxy-url") == "http://localhost",
            "A preceding bare option cannot shadow a valid exact option");
        Check(Parse([], "--proxy-url") is null, "An empty argument list has no string option");
        Check(Parse(["--proxy-url=first", "--proxy-url=second"], "--proxy-url") == "first",
            "Explicit startup values retain precedence over appended persisted values");
        Check(Parse(["--proxy-password=  fixture=秘密  "], "--proxy-password") == "  fixture=秘密  ",
            "String options preserve Unicode, whitespace and equal signs");
        Check(Parse(["--PROXY-URL=fixture"], "--proxy-url") is null,
            "String option names use ordinal case-sensitive matching");
        foreach (var key in new[] { "--trace", "--minimized", "--skip-compat-check", "--debug",
            "--disable-tray-tooltip", "--allow-all-power-modes-on-battery", "--force-disable-rgbkb",
            "--force-disable-spectrumkb", "--force-disable-lenovolighting", "--enable-lamp-array",
            "--experimental-gpu-working-mode", "--experimental-its-mode", "--proxy-allow-all-certs",
            "--disable-update-checker", "--disable-conflicting-software-warning" })
        {
            bool ParseBool(params string[] values) => (bool)boolParse.Invoke(null, [values, key])!;
            Check(!ParseBool(), $"Absent {key} defaults to false");
            Check(ParseBool(key, key), $"Repeated exact {key} remains true");
            Check(!ParseBool(key + "=false", key + "-suffix", key.ToUpperInvariant()),
                $"Only an exact standalone {key} enables a boolean option");
        }
        Console.WriteLine($"PASS: {checks} production AppFlags parsing checks (no settings I/O)");
        return checks;
    }
}
