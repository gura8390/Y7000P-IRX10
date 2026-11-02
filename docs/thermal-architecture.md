# Thermal architecture implementation status

## 2026-11-02 takeover update

The phase-two implementation now includes a machine-specific Lenovo CPU firmware transport,
unified legacy CPU write ownership, an active supervisor, explicit single-source firmware
temperature fallback, independent sensor subscription deadlines, and bounded lifecycle handling.
The original phase-one status below is retained as history and does not describe the new code.
See `phase2-handoff.txt` for the current checks and acceptance limits.

No physical power/temperature write or full-load acceptance is claimed by this public release.
Activation requires a separately verified Custom-mode baseline. The firmware aggregate
temperature is explicitly distinguished from independently measured package/core-maximum data.

## Historical phase-one status

Base: `0bb045be29c4fd772ba9887a3507b6a9b0b56d58`, branch `thermal-architecture`.

## Implemented

- Sensor updates serialize each requested scope instead of losing broader requests behind a narrower in-flight update. Throttling only skips scopes covered by the previous sample.
- CPU-only updates avoid GPU state dependency. Hardware failures invalidate matching providers.
- Per-metric monotonic timestamps survive partial updates. Snapshot copies preserve the original data.
- Notifications use a bounded latest-value channel outside the hardware lock. Exceptions are isolated per callback. A slow subscriber can still delay other notification subscribers, but cannot hold the hardware lock or stop sampling.
- One sensor producer survives subscription changes and sleeps on a signal with no subscribers. Shutdown cancels and joins producer and shadow tasks before closing hardware. Notification consumers are not joined on the UI thread.
- Quick-action expansion detects indirect reference cycles; failed steps no longer emit success messages.
- CPU control telemetry explicitly reads package and core maximum, independently of the dashboard's average-temperature preference. Missing sources fail closed in the policy.
- Independent 500ms **shadow** supervisor detects stale samples even when sampling stalls. Policy includes preemptive restriction, 90°C intervention, hysteresis, slow recovery, saturation and invalid-data handling.
- A pure CPU power arbiter validates baseline/capabilities and rejects outdated constraint epochs. A bounded single-flight executor validates commands, verifies readback, and retains hardware ownership after timeout until underlying I/O completes.
- The executor has no production WMI transport attached. No new path writes CPU/GPU power, temperature limits or fans.
- Previously floating LLT packages are pinned to versions actually resolved by the baseline build; NuGet lock files added. Cross-platform package diagnostics no longer invoke PowerShell on Linux.

## Try observation on Windows

Build the WPF project using .NET 9 and the Windows SDK specified by the repository. Enable hardware sensors and logging, open sensor settings, enable “90°C CPU thermal observation”, then restart the application. Decisions appear in trace logs; default is disabled. Existing PawnIO/hardware-monitor requirements still apply. This is **not** an active temperature limiter.

```sh
dotnet build LenovoLegionToolkit.WPF/LenovoLegionToolkit.WPF.csproj -p:RestoreLockedMode=true
# Dependency-free policy/arbiter/executor/snapshot checks:
dotnet run --project tests/Thermal.Tests/Thermal.Tests.csproj
```

The test executable links the actual production core files. Only SensorItem's Windows-dependent host enum is represented by a three-member test stub. Tests exercise hotspot selection, stale/invalid data, hysteresis/recovery, saturation, epoch rejection, deadline propagation, timeout/late completion serialization and partial snapshot freshness. The 517 assertions include repeated saturation checks; they do not represent 517 independent scenarios.

## Validation limits and next integration work

Linux cross-compilation covers the WPF application and referenced libraries, not Windows runtime behavior. The full solution additionally includes a DesktopBridge/MSIX packaging project that requires Windows tooling. No hardware sensor, fan or WMI integration test ran on a Lenovo laptop. Callback isolation, shutdown and hardware reset behavior still need Windows integration tests.

Before enabling physical thermal control, implement a validated Lenovo CPU transport with legal mode/capability discovery and readback; migrate existing UI/CLI/automation writes to the same arbiter/executor. The new executor currently serializes only its own callers, not all legacy hardware writers. Resolve external software ownership and add actuator-fault UI. Do not wire a thermal loop directly to legacy full-preset application.

The current power model supports continuous bounds with a common step and a long/short relationship. Real discrete capabilities, per-field steps and additional firmware coupling need richer validation before transport integration. Relative policy fractions must be mapped to the current verified user baseline, rounded within real bounds; the policy does not claim wattage validity.

A process exit or persistent WMI/hardware stall cannot be treated as a verified cooling action. Shutdown currently waits for in-flight sensor I/O to finish; a permanently blocked vendor call can delay exit. A future isolated transport process may be needed if hardware calls prove unbounded. Shadow output is diagnostic, not a hardware safety guarantee. Firmware thermal protections remain authoritative.

## Suggested Windows acceptance

1. Compare all-sensor and CPU-only requests; verify CPU timestamps do not refresh from memory updates.
2. Attach throwing/slow notification subscribers; sampling and UI must remain responsive.
3. Repeatedly open/close dashboards and suspend/resume; verify one producer and clean shutdown.
4. Enable shadow mode; stale/absent package/core sources must report SensorFault, never recovery.
5. Run fixed CPU and mixed loads; compare observations with a trusted monitor and inspect trace decisions.
6. Test cyclic quick actions and ordinary reused quick actions; cycle must fail without stack overflow.
7. Only after validated transport and unified ownership, test actual firmware temperature limits and closed-loop power changes.
