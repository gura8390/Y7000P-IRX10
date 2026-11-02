# Isolated WPF host

This console executable is an offscreen test fixture, not the production application.
It requires `--isolated-host`, an unelevated token, its own `asInvoker` manifest,
and temporary settings/log paths. It never calls `Application.Run`,
`Window.Show`, `Window.ShowDialog`, production `App`, or the default IoC modules.
Do not substitute the production executable if this fixture fails.

The fixture loads the actual application resource declaration and compiled style
dictionaries, then instantiates the actual `SensorSettingsWindow` on its STA
thread using an injected GPU topology delegate. The sensor controller is an
uninitialized, nonrunning fixture object with only an inert core-count provider.
WPF uses per-process software rendering for the fixture.
Application-root resource URIs are mechanically qualified with the actual WPF
assembly name because the fixture has its own executable assembly.
There is no GPU enumeration, WMI, sensor discovery/update/subscription, thermal
Start/Stop, or firmware invocation. Apply and Default button handlers are excluded
because they contain those boundaries. SelectionChanged, status notification,
Cancel, close/unsubscribe, initialization, template creation and resource lookup
are exercised offscreen. HWND, visibility, presentation source and controller
state guards are checked throughout.

The resource audit derives the external string keys from the copied current XAML
sources. It reports missing keys as failures instead of inserting test-only
fallback resources. A resource found in the WPF dictionaries is not a claim that
every application page or theme was rendered or visually accepted.

The same host loads Dark then Light theme resources directly through the real
ThemesDictionary URI setter, without invoking Theme.Apply, system-color reads or
native window APIs. It repeats the actual sensor window matrix in both themes.
A separate never-shown plain Window hosts the real LampArrayZoneControl and a
plain Grid evaluating the actual AMD root Background markup. This checks dynamic
theme refresh, actual lamp checked-state border color/thickness, and valid
primary/secondary accent brushes. The actual AMD overclocking window and AMD
controllers are never instantiated. Hover is checked from the actual template
markup and resource lookup, without generating mouse input.

Build and execution must be coordinated with other builds because the repository
shares output folders. Example (from the repository root):

```powershell
..\tools\dotnet\dotnet.exe build tests\WpfIsolation.Tests\WpfIsolation.Tests.csproj -c Release -p:Platform=x64
$env:DOTNET_ROOT = (Resolve-Path ..\tools\dotnet).Path
.\BuildLLT\bin\x64\Release\net9.0-windows10.0.26100.0\win-x64\WpfIsolation.IsolatedHost.exe --isolated-host
```

The emitted report states its temporary directory. The fixture sets the internal
`Folders.AppDataOverride` seam before any settings, logging, localization or IoC
initialization. Both settings classes use their actual storage-directory
constructors. Production application data is not a fixture destination.
