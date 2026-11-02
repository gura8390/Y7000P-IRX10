using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Security.Principal;
using System.Text.Json;
using System.Text.RegularExpressions;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Interop;
using System.Windows.Markup;
using System.Windows.Media;
using System.Windows.Threading;
using System.Xml.Linq;
using Autofac;
using LenovoLegionToolkit.Lib;
using LenovoLegionToolkit.Lib.Controllers;
using LenovoLegionToolkit.Lib.Controllers.Sensors;
using LenovoLegionToolkit.Lib.Controllers.Sensors.Providers;
using LenovoLegionToolkit.Lib.Settings;
using LenovoLegionToolkit.Lib.Utils;
using LenovoLegionToolkit.WPF.Resources;
using LenovoLegionToolkit.WPF.Settings;
using LenovoLegionToolkit.WPF.Controls.LampArray;
using LenovoLegionToolkit.WPF.Windows.Settings;
using Wpf.Ui.Appearance;
using Wpf.Ui.Markup;

namespace WpfIsolation.Tests;

internal static class Program
{
    private const BindingFlags InstanceFields = BindingFlags.Instance | BindingFlags.NonPublic | BindingFlags.Public;
    private const BindingFlags StaticFields = BindingFlags.Static | BindingFlags.NonPublic | BindingFlags.Public;
    private static readonly List<CheckResult> Results = [];
    private static string _temporaryRoot = string.Empty;
    private static bool _sourceInitialized;
    private static int _gpuStubCalls;
    private static int _windowCount;
    private static int _themeProbeCount;
    private static int _resourceCount;
    private static string[] _missingResources = [];
    private static readonly List<ResourceAudit> ResourceAudits = [];
    private static SensorsGroupController _sensors = null!;
    private static CpuThermalController _thermal = null!;
    private static ApplicationSettings _settings = null!;
    private static HardwareSensorSettings _sensorSettings = null!;

    [STAThread]
    public static int Main(string[] args)
    {
        if (args.Length != 1 || args[0] != "--isolated-host")
        {
            Console.Error.WriteLine("REFUSED: use only the isolated apphost with --isolated-host; production launch is prohibited.");
            return 2;
        }

        using var identity = WindowsIdentity.GetCurrent();
        if (new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator))
        {
            Console.Error.WriteLine("REFUSED: this fixture must run unelevated; it does not request or accept UAC elevation.");
            return 2;
        }

        _temporaryRoot = Path.Combine(Path.GetTempPath(), "LLT-WpfIsolation", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_temporaryRoot);
        try
        {
            Check(System.Threading.Thread.CurrentThread.GetApartmentState() == System.Threading.ApartmentState.STA,
                "isolated host entry thread is STA");
            InstallTemporaryPathsAndFixtureServices();
            RenderOptions.ProcessRenderMode = RenderMode.SoftwareOnly;
            var app = LoadActualResourceHost();
            Check(app.GetType() == typeof(Application), "host uses plain Application, never production App");
            Check(app.Resources.MergedDictionaries.Count >= 14, "actual application merged dictionaries loaded");
            Check(app.TryFindResource(typeof(ComboBox)) is Style, "actual application ComboBox implicit style resolves");
            Check(app.TryFindResource(typeof(CheckBox)) is Style, "actual application CheckBox implicit style resolves");

            var themeDictionary = app.Resources.MergedDictionaries.OfType<ThemesDictionary>().Single();
            var probe = CreateThemeProbe();
            Color? darkBackground = null;
            foreach (var theme in new[] { ThemeType.Dark, ThemeType.Light })
            {
                themeDictionary.Theme = theme; // resource URI only; never Theme.Apply or native window/theme methods.
                PumpDispatcher();
                AuditExternalResourceKeys(app, theme.ToString());
                var background = VerifyThemeProbe(app, probe, theme.ToString());
                if (theme == ThemeType.Dark) darkBackground = background;
                else Check(Luminance(darkBackground!.Value) < Luminance(background),
                    "actual Light background is brighter than actual Dark background");

                foreach (var cultureName in new[] { "en", "zh-Hans", "ar" })
                {
                    Resource.Culture = CultureInfo.GetCultureInfo(cultureName);
                    foreach (var multipleGpus in new[] { false, true })
                        RunWindowScenario(app, cultureName, multipleGpus, theme.ToString());
                }
            }
            probe.Host.Close();
            PumpDispatcher();
            AssertOffscreen(probe.Host, "closed theme probe");
            Check(app.Windows.Count == 0, "all offscreen windows closed and removed from Application.Windows");
            Check(!_sourceInitialized, "no tested window ever raised SourceInitialized");
            Check(_gpuStubCalls == _windowCount, "GPU queries used only the injected pure topology stub");
            AssertNoHardwareActivity("final fixture");
            Check(!File.Exists(Path.Combine(_temporaryRoot, "application", "settings.json"))
                && !File.Exists(Path.Combine(_temporaryRoot, "sensors", "hardware_sensors.json")),
                "construction, selection and cancel did not persist settings");
            app.Shutdown(); // plain Application only: no production Exit handler is installed.
        }
        catch (Exception exception)
        {
            Results.Add(new("isolated host completed without exception", false, exception.ToString()));
        }

        var failed = Results.Count(result => !result.Passed);
        var report = new
        {
            passed = failed == 0,
            checks = Results.Count,
            failed,
            windowsConstructed = _windowCount,
            auxiliaryThemeProbeWindows = _themeProbeCount,
            externalResourceKeys = _resourceCount,
            missingExternalResourceKeys = _missingResources,
            resourceAuditsByTheme = ResourceAudits,
            gpuStubCalls = _gpuStubCalls,
            hardwareCalls = 0,
            shownWindows = 0,
            productionStartupInvoked = false,
            defaultIoCModulesLoaded = false,
            productionThermalStartStopInvoked = false,
            temporaryRoot = _temporaryRoot,
            limitations = new[]
            {
                "Apply and Default handlers are excluded because they invoke real thermal/sensor boundaries.",
                "No HWND, visible UI, visual acceptance, real hardware acceptance, firmware call, mode switch or deployment.",
                "AMD overclocking window/controllers are not instantiated; its actual Background markup is evaluated on a plain Grid.",
                "Lamp checked-state template executes; hover is verified by its actual markup and theme resource, without native mouse input.",
                "String-key resolution covers declared external XAML keys; it does not instantiate every page/control/template state."
            },
            results = Results
        };
        var reportPath = Path.Combine(_temporaryRoot, "report.json");
        File.WriteAllText(reportPath, JsonSerializer.Serialize(report, new JsonSerializerOptions { WriteIndented = true }));
        foreach (var result in Results.Where(result => !result.Passed))
            Console.Error.WriteLine($"FAIL: {result.Name}\n{result.Error}");
        Console.WriteLine($"{(failed == 0 ? "PASS" : "FAIL")}: {Results.Count - failed}/{Results.Count} isolated WPF checks; "
            + $"{_windowCount} offscreen sensor windows + {_themeProbeCount} theme probe; {_resourceCount - _missingResources.Length}/{_resourceCount} external keys resolved across {ResourceAudits.Count} themes.");
        Console.WriteLine($"Report: {reportPath}");
        return failed == 0 ? 0 : 1;
    }

    private static void InstallTemporaryPathsAndFixtureServices()
    {
        // A production path seam is required before LocalizationHelper's static initializer
        // can run. Refuse instead of silently creating/touching real application data.
        var overrideField = typeof(Folders).GetField("AppDataOverride", StaticFields);
        if (overrideField is null || overrideField.FieldType != typeof(string))
            throw new InvalidOperationException("Safe Folders.AppDataOverride seam is absent; refusing all window construction.");
        overrideField.SetValue(null, Path.Combine(_temporaryRoot, "appdata"));
        Check(Path.GetFullPath(Folders.AppData).StartsWith(_temporaryRoot + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase),
            "Folders.AppData is isolated before localization/static resource initialization");

        var log = (Log)Activator.CreateInstance(typeof(Log), InstanceFields, null,
            [Path.Combine(_temporaryRoot, "logs")], CultureInfo.InvariantCulture)!;
        typeof(Log).GetField("_instance", StaticFields)!.SetValue(null, log);
        Check(log.LogPath.StartsWith(_temporaryRoot + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase),
            "actual logger destination is under temporary fixture root");

        _settings = (ApplicationSettings)Activator.CreateInstance(typeof(ApplicationSettings), InstanceFields, null,
            [Path.Combine(_temporaryRoot, "application")], CultureInfo.InvariantCulture)!;
        _settings.Store.EnableCpuThermalControl = false;
        _settings.Store.EnableHardwareSensors = false;
        _settings.Store.EnableThermalShadow = false;

        _sensorSettings = (HardwareSensorSettings)Activator.CreateInstance(typeof(HardwareSensorSettings), InstanceFields, null,
            [Path.Combine(_temporaryRoot, "sensors")], CultureInfo.InvariantCulture)!;

        // Never execute SensorsGroupController or CpuSensorProvider constructors.
        // Their only tested read is AvailableVoltageCoreCount -> empty-list Count.
        var cpuProvider = (CpuSensorProvider)RuntimeHelpers.GetUninitializedObject(typeof(CpuSensorProvider));
        var coresField = typeof(CpuSensorProvider).GetField("_coreVoltageSensors", InstanceFields)!;
        coresField.SetValue(cpuProvider, Activator.CreateInstance(coresField.FieldType));
        _sensors = (SensorsGroupController)RuntimeHelpers.GetUninitializedObject(typeof(SensorsGroupController));
        typeof(SensorsGroupController).GetField("_cpuProvider", InstanceFields)!.SetValue(_sensors, cpuProvider);
        _thermal = new CpuThermalController(_sensors, _settings); // no lifecycle methods are invoked.

        Check(!IoCContainer.IsInitialized, "no production container existed before fixture registration");
        IoCContainer.Initialize(new FixtureModule(_settings, _sensorSettings, _sensors, _thermal));
        Check(IoCContainer.TryResolve<LenovoLegionToolkit.Lib.System.DgpuAwakeManager>() is null,
            "minimal fixture container has no GPU-awake manager or default module side effects");
        AssertNoHardwareActivity("initial fixture");
    }

    private static Application LoadActualResourceHost()
    {
        var source = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "FixtureSource", "App.xaml"));
        var root = source.Root!;
        XNamespace x = "http://schemas.microsoft.com/winfx/2006/xaml";
        root.Attribute(x + "Class")!.Remove();
        foreach (var name in new[] { "Startup", "Exit", "StartupUri" }) root.Attribute(name)?.Remove();
        root.SetAttributeValue("ShutdownMode", "OnExplicitShutdown");
        foreach (var attribute in root.Attributes().Where(attribute => attribute.IsNamespaceDeclaration).ToArray())
        {
            var value = attribute.Value;
            if (value.StartsWith("clr-namespace:", StringComparison.Ordinal) && !value.Contains(";assembly=", StringComparison.Ordinal))
                value += ";assembly=" + typeof(SensorSettingsWindow).Assembly.GetName().Name;
            attribute.Value = value;
        }
        // The fixture is a separate executable, so application-root pack URIs must
        // explicitly target the real WPF assembly. Styles/content remain unchanged.
        const string applicationRoot = "pack://application:,,,/";
        var assemblyName = Uri.EscapeDataString(typeof(SensorSettingsWindow).Assembly.GetName().Name!);
        foreach (var sourceAttribute in root.Descendants().Attributes("Source"))
        {
            var uri = sourceAttribute.Value;
            if (uri.StartsWith(applicationRoot, StringComparison.Ordinal) && !uri.Contains(";component/", StringComparison.OrdinalIgnoreCase))
                sourceAttribute.Value = applicationRoot + assemblyName + ";component/" + uri[applicationRoot.Length..];
        }
        // A generic Application root establishes the same dictionary ownership while
        // it loads. The production class and every application event/StartupUri were
        // removed before parsing: no production Application instance or startup runs.
        return (Application)XamlReader.Parse(root.ToString());
    }

    private static void AuditExternalResourceKeys(Application app, string themeName)
    {
        var referencePattern = new Regex(@"\{(?:StaticResource|DynamicResource)\s+([^{}]+)\}");
        var definitionPattern = new Regex("x:Key=\"([^\"{}]+)\"");
        var references = new SortedSet<string>(StringComparer.Ordinal);
        var definitions = new HashSet<string>(StringComparer.Ordinal);
        foreach (var path in Directory.EnumerateFiles(Path.Combine(AppContext.BaseDirectory, "FixtureSource"), "*.xaml", SearchOption.AllDirectories))
        {
            var text = File.ReadAllText(path);
            foreach (Match match in referencePattern.Matches(text)) references.Add(match.Groups[1].Value.Trim());
            foreach (Match match in definitionPattern.Matches(text)) definitions.Add(match.Groups[1].Value);
        }
        references.ExceptWith(definitions);
        _resourceCount = references.Count;
        var missing = new List<string>();
        foreach (var key in references)
        {
            try
            {
                var resolved = app.TryFindResource(key);
                if (resolved is null) missing.Add(key);
                Check(resolved is not null, $"external resource resolves ({themeName}): {key}", "Missing in actual application resource dictionaries.", fatal: false);
            }
            catch (Exception exception)
            {
                missing.Add(key);
                Check(false, $"external resource resolves ({themeName}): {key}", exception.ToString(), fatal: false);
            }
        }
        ResourceAudits.Add(new(themeName, references.Count, missing.ToArray()));
        _missingResources = ResourceAudits.SelectMany(audit => audit.Missing).Distinct(StringComparer.Ordinal).ToArray();
    }

    private static ThemeProbe CreateThemeProbe()
    {
        XNamespace p = "http://schemas.microsoft.com/winfx/2006/xaml/presentation";
        var fixtureSource = Path.Combine(AppContext.BaseDirectory, "FixtureSource");
        var amdSource = XDocument.Load(Path.Combine(fixtureSource, "Windows", "Overclocking", "Amd", "AmdOverclocking.xaml"));
        var backgroundReference = amdSource.Root!.Element(p + "Grid")!.Attribute("Background")!.Value;
        Check(backgroundReference == "{DynamicResource ApplicationBackgroundBrush}",
            "actual AMD markup references the existing application background resource");
        var lampSource = XDocument.Load(Path.Combine(fixtureSource, "Controls", "LampArray", "LampArrayZoneControl.xaml"));
        foreach (var (property, key) in new[] { ("IsChecked", "SystemAccentColorPrimaryBrush"), ("IsMouseOver", "SystemAccentColorSecondaryBrush") })
        {
            var trigger = lampSource.Descendants(p + "Trigger").Single(element => element.Attribute("Property")?.Value == property);
            var brushSetter = trigger.Elements(p + "Setter").Single(element => element.Attribute("Property")?.Value == "BorderBrush");
            Check(brushSetter.Attribute("Value")?.Value == "{DynamicResource " + key + "}",
                "actual lamp " + property + " trigger references the existing theme accent resource");
        }

        // This small panel evaluates the actual AMD Background markup without
        // constructing any AMD window/controller. The Lamp control is the real class.
        var backgroundPanel = (Grid)XamlReader.Parse(new XElement(p + "Grid", new XAttribute("Background", backgroundReference)).ToString());
        var lamp = new LampArrayZoneControl { Width = 80, Height = 40, Label = "isolated theme probe" };
        var panel = new StackPanel();
        panel.Children.Add(backgroundPanel);
        panel.Children.Add(lamp);
        var host = new Window { Width = 200, Height = 100, Content = panel, ShowInTaskbar = false };
        _themeProbeCount++;
        host.SourceInitialized += (_, _) => _sourceInitialized = true;
        panel.Measure(new Size(200, 100));
        panel.Arrange(new Rect(0, 0, 200, 100));
        ForceTemplates(panel);
        var button = Field<ToggleButton>(lamp, "_button");
        var border = (Border)button.Template.FindName("border", button);
        AssertOffscreen(host, "theme probe constructor/templates");
        return new(host, lamp, backgroundPanel, border);
    }

    private static Color VerifyThemeProbe(Application app, ThemeProbe probe, string themeName)
    {
        PumpDispatcher();
        var background = app.TryFindResource("ApplicationBackgroundBrush") as SolidColorBrush;
        var selected = app.TryFindResource("SystemAccentColorPrimaryBrush") as SolidColorBrush;
        var hover = app.TryFindResource("SystemAccentColorSecondaryBrush") as SolidColorBrush;
        Check(background is { Color.A: > 0 } && selected is { Color.A: > 0 } && hover is { Color.A: > 0 },
            "background, selected and hover theme resources are nontransparent brushes: " + themeName);
        Check(probe.BackgroundPanel.Background is SolidColorBrush actual && actual.Color == background!.Color,
            "actual AMD background markup updates to active theme brush: " + themeName);
        probe.Lamp.IsChecked = false;
        PumpDispatcher();
        Check(probe.SelectionBorder.BorderThickness == new Thickness(0), "actual lamp unchecked trigger state: " + themeName);
        probe.Lamp.IsChecked = true;
        PumpDispatcher();
        Check(probe.SelectionBorder.BorderBrush is SolidColorBrush selectedActual && selectedActual.Color == selected!.Color,
            "actual lamp checked template border resolves active primary accent: " + themeName);
        Check(probe.SelectionBorder.BorderThickness == new Thickness(2), "actual lamp checked border thickness: " + themeName);
        Check(selected!.Color != background!.Color && hover!.Color != background.Color,
            "selected and hover accents remain distinguishable from the background: " + themeName);
        AssertOffscreen(probe.Host, "after theme resources/selection " + themeName);
        return background.Color;
    }

    private static double Luminance(Color color) => .2126 * color.R + .7152 * color.G + .0722 * color.B;

    private static void RunWindowScenario(Application app, string cultureName, bool multipleGpus, string themeName)
    {
        var label = $"{themeName}, {cultureName}, gpu-stub={multipleGpus}";
        var coreCount = cultureName == "zh-Hans" ? 4 : cultureName == "ar" ? 1 : 0;
        var provider = typeof(SensorsGroupController).GetField("_cpuProvider", InstanceFields)!.GetValue(_sensors)!;
        var coreField = typeof(CpuSensorProvider).GetField("_coreVoltageSensors", InstanceFields)!;
        var inertCoreSlots = (IList)Activator.CreateInstance(coreField.FieldType)!;
        for (var index = 0; index < coreCount; index++) inertCoreSlots.Add(null); // Count only; no ISensor getter can run.
        coreField.SetValue(provider, inertCoreSlots);
        _sensorSettings.Store.ShowCpuAverageFrequency = cultureName != "en";
        _sensorSettings.Store.DisplayMemoryInGigabytes = multipleGpus;
        _sensorSettings.Store.SelectedGpuIsIgpu = multipleGpus;
        _sensorSettings.Store.CpuVoltageMode = cultureName == "zh-Hans" ? CpuVoltageMode.Core : CpuVoltageMode.Average;
        _sensorSettings.Store.CpuVoltageCoreIndex = cultureName == "ar" ? -10 : 99;
        _sensorSettings.Store.CpuTemperatureSource = CpuTemperatureSource.Package;
        _settings.Store.TemperatureUnit = cultureName == "ar" ? TemperatureUnit.F : TemperatureUnit.C;
        var constructor = typeof(SensorSettingsWindow).GetConstructor(InstanceFields, null, [typeof(Func<bool>)], null)
            ?? throw new InvalidOperationException("Safe SensorSettingsWindow(Func<bool>) seam is absent; refusing default constructor.");
        Func<bool> gpuStub = () => { _gpuStubCalls++; return multipleGpus; };
        var window = (SensorSettingsWindow)constructor.Invoke([gpuStub]);
        _windowCount++;
        window.SourceInitialized += (_, _) => _sourceInitialized = true;
        AssertOffscreen(window, "after actual constructor " + label);
        Check(window.Title == Resource.SettingsPage_SensorSettings_Title, "actual localized title " + label);
        Check(window.FlowDirection == (cultureName == "ar" ? FlowDirection.RightToLeft : FlowDirection.LeftToRight),
            "actual localized flow direction " + label);
        Check(Field<FrameworkElement>(window, "_gpuSelectionCard").Visibility == (multipleGpus ? Visibility.Visible : Visibility.Collapsed),
            "GPU card follows pure topology stub " + label);
        Check(Field<ComboBox>(window, "_voltageCoreSelector").Items.Count == Math.Max(1, coreCount),
            "inert core count (including zero fallback) initializes options " + label);
        Check(Field<ComboBox>(window, "_voltageCoreSelector").SelectedIndex == Math.Clamp(_sensorSettings.Store.CpuVoltageCoreIndex, 0, Math.Max(1, coreCount) - 1),
            "saved negative/oversized core index is clamped " + label);
        Check(Field<ComboBox>(window, "_cpuFrequencySelector").SelectedIndex == (_sensorSettings.Store.ShowCpuAverageFrequency ? 1 : 0),
            "saved CPU frequency option initializes " + label);
        Check(Field<ComboBox>(window, "_memoryDisplayModeSelector").SelectedIndex == (multipleGpus ? 1 : 0),
            "saved memory display option initializes " + label);
        Check(Field<ComboBox>(window, "_cpuTemperatureSourceSelector").SelectedIndex == (int)CpuTemperatureSource.Package,
            "saved CPU temperature source initializes " + label);
        Check(Field<ComboBox>(window, "_temperatureUnitSelector").SelectedIndex == (cultureName == "ar" ? 1 : 0),
            "saved C/F option initializes " + label);
        Check(Field<TextBlock>(window, "_cpuThermalStatus").Text == _thermal.Status, "thermal status initialized from nonrunning controller " + label);
        Check(Field<CheckBox>(window, "_cpuThermalCheckBox").IsChecked == false, "thermal control remains unchecked " + label);

        window.ApplyTemplate();
        if (window.Content is FrameworkElement content)
        {
            content.Measure(new Size(500, 540));
            content.Arrange(new Rect(0, 0, 500, 540));
            content.UpdateLayout();
            ForceTemplates(content);
        }
        PumpDispatcher();
        AssertOffscreen(window, "after real templates/layout " + label);

        var selector = Field<ComboBox>(window, "_voltageSelector");
        foreach (var mode in new[] { CpuVoltageMode.Core, CpuVoltageMode.Maximum, CpuVoltageMode.Average, CpuVoltageMode.Core })
        {
            selector.SelectedIndex = (int)mode; // raises actual wired SelectionChanged.
            Check(Field<ComboBox>(window, "_voltageCoreSelector").Visibility == (mode == CpuVoltageMode.Core ? Visibility.Visible : Visibility.Collapsed),
                "wired voltage SelectionChanged " + mode + " " + label);
        }

        var eventField = typeof(CpuThermalController).GetField("StatusChanged", InstanceFields)!;
        var listeners = (Action<string>?)eventField.GetValue(_thermal);
        Check(listeners?.GetInvocationList().Length == 1, "one actual thermal status subscription " + label);
        listeners!("isolated status event"); // invoke only the event delegate; no producer, logger or thermal lifecycle.
        PumpDispatcher();
        Check(Field<TextBlock>(window, "_cpuThermalStatus").Text == "isolated status event", "actual status handler reaches Dispatcher/text " + label);
        AssertOffscreen(window, "after status event " + label);
        AssertNoHardwareActivity(label);

        var buttons = Descendants(window).OfType<Button>().ToArray();
        var cancel = buttons.Single(button => Equals(button.Content, Resource.Cancel));
        var closed = false;
        window.Closed += (_, _) => closed = true;
        cancel.RaiseEvent(new RoutedEventArgs(Button.ClickEvent, cancel)); // actual XAML wired CancelButton_Click.
        PumpDispatcher();
        Check(closed, "actual Cancel Click closes never-shown window " + label);
        Check(eventField.GetValue(_thermal) is null, "actual Closed cleanup unsubscribes thermal status " + label);
        Check(!app.Windows.Cast<Window>().Contains(window), "closed window removed from plain Application " + label);
        Check(!_settings.Store.EnableCpuThermalControl && !_settings.Store.EnableThermalShadow,
            "cancel never enables thermal control or shadow " + label);
        AssertOffscreen(window, "after Cancel " + label);
    }

    private static void AssertNoHardwareActivity(string label)
    {
        Check(!_thermal.IsRunning, "thermal controller is not running: " + label);
        foreach (var field in new[] { "_transport", "_cts", "_loop", "_subscription", "_pendingFirmwareRead", "_stopTask" })
            Check(typeof(CpuThermalController).GetField(field, InstanceFields)!.GetValue(_thermal) is null,
                "no thermal activity field " + field + ": " + label);
        Check((int)typeof(CpuThermalController).GetField("_notificationConsumerStarted", InstanceFields)!.GetValue(_thermal)! == 0,
            "no thermal notification producer started: " + label);
        foreach (var field in new[] { "_computer", "_producerTask", "_producerCts" })
            Check(typeof(SensorsGroupController).GetField(field, InstanceFields)!.GetValue(_sensors) is null,
                "no sensor activity field " + field + ": " + label);
        Check(!(bool)typeof(SensorsGroupController).GetField("_initialized", InstanceFields)!.GetValue(_sensors)!,
            "sensor controller was never initialized: " + label);
    }

    private static void AssertOffscreen(Window window, string label)
    {
        Check(!window.IsVisible, "window remains invisible: " + label);
        Check(new WindowInteropHelper(window).Handle == IntPtr.Zero, "window has no HWND: " + label);
        Check(PresentationSource.FromVisual(window) is null, "window has no presentation source: " + label);
    }

    private static void ForceTemplates(DependencyObject root)
    {
        if (root is Control control) control.ApplyTemplate();
        var count = VisualTreeHelper.GetChildrenCount(root);
        for (var index = 0; index < count; index++) ForceTemplates(VisualTreeHelper.GetChild(root, index));
    }

    private static IEnumerable<DependencyObject> Descendants(DependencyObject root)
    {
        foreach (var child in LogicalTreeHelper.GetChildren(root).OfType<DependencyObject>())
        {
            yield return child;
            foreach (var nested in Descendants(child)) yield return nested;
        }
    }

    private static T Field<T>(object owner, string name) => (T)owner.GetType().GetField(name, InstanceFields)!.GetValue(owner)!;

    private static void PumpDispatcher()
    {
        var frame = new DispatcherFrame();
        Dispatcher.CurrentDispatcher.BeginInvoke(DispatcherPriority.ApplicationIdle, new Action(() => frame.Continue = false));
        Dispatcher.PushFrame(frame);
    }

    private static void Check(bool passed, string name, string? error = null, bool fatal = true)
    {
        Results.Add(new(name, passed, passed ? null : error ?? "Assertion failed."));
        if (!passed && fatal) throw new InvalidOperationException(name + ": " + error);
    }

    private sealed record CheckResult(string Name, bool Passed, string? Error);
    private sealed record ResourceAudit(string Theme, int ExternalKeys, string[] Missing);
    private sealed record ThemeProbe(Window Host, LampArrayZoneControl Lamp, Grid BackgroundPanel, Border SelectionBorder);

    private sealed class FixtureModule(ApplicationSettings settings, HardwareSensorSettings sensorSettings,
        SensorsGroupController sensors, CpuThermalController thermal) : Autofac.Module
    {
        protected override void Load(ContainerBuilder builder)
        {
            builder.RegisterInstance(settings).ExternallyOwned();
            builder.RegisterInstance(sensorSettings).ExternallyOwned();
            builder.RegisterInstance(sensors).ExternallyOwned();
            builder.RegisterInstance(thermal).ExternallyOwned();
        }
    }
}
