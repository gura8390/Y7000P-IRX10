# Verification and migration decisions

This record distinguishes source implementation, isolated execution and actual
hardware acceptance for Jiks 0.1.0. The archive manifest records its exact source
commit and file hashes. A build or unit test does not count as a firmware,
temperature or game acceptance result.

## Current implemented scope

The native interface preserves the original nine-page hierarchy and eight
settings categories, structured preset/rule/macro editing, device information,
the tray and status window, notifications and OSD. File/folder/color pickers,
startup preferences and first-language selection use ordinary controls. Package
filter/sort/hide-current-list and copy commands use the selected trusted item.

The runtime contains the native capability adapters and asynchronous execution
paths. Its hardware writer, read worker, command deadlines, late completions,
restore and shutdown barriers are tested through fake transports. A successful
queue admission never substitutes for actual completion. Cleanup failure keeps
the owner and exposes an explicit retry rather than pretending the application
can safely exit.

The configuration repository handles the 21 named settings documents, known-field
updates, legacy/unknown data preservation, backups and restore. Restore disables
thermal control, automation and macro listening until they are explicitly
enabled. Unknown CLR types are retained as data and never instantiated.

Feature support remains conditional on actual machine capabilities and access.
For this machine, read-only Lighting/HID metadata supports the fixed 24-zone
Spectrum candidate and excludes the old single-zone/four-zone fallback predicates.
Descriptors do not prove a Spectrum handshake or successful profile writes.

## Deliberate compatibility choices

- The UI uses native Win32 controls and GDI. Original layout hierarchy is retained;
  it does not embed WPF or a WebView.
- Quick-action icons offer a finite native symbol set. Unknown old `IconName`
  strings remain intact. Notification `SymbolRegular` numbers can be retained or
  edited, with category-marker fallback and a compatibility notice; the original
  WPF glyph catalog is not claimed as a native glyph API.
- Release notes open a validated URL under the project's GitHub release pages.
  This is the documented replacement for the embedded Markdown window; it does
  not assert a migrated or tested automatic installer.
- Legacy C#/CLR scripts and binary plugins remain inert data. Native script
  execution uses an explicitly selected executable/interpreter and raw arguments.
  Cancelling a wait does not silently kill the external program.
- FPS follows the original process-aggregate kernel Present event stream. It is
  submission cadence rather than displayed, scanout or generated-frame FPS.
  The 1% low is the mean instantaneous FPS of the slowest five intervals in a
  full 500-interval window. Unknown, lost or stale samples are unavailable values.
- Intel requested VID is not measured Vcore; ratio-derived frequency is not
  effective-clock telemetry. Missing sources remain unknown.
- The installed 方正公文楷体 font is preferred where available. Public source and
  packages exclude the proprietary font binary and use Windows fallback fonts.

## Evidence currently established

The owned UI stage completed 70 UI tests and a normal app `cargo check`, with no
compiler warnings/errors. The later frozen app gate passed 74 tests and the
notification locale regression passed separately. The earlier 566-test workspace
baseline is a different snapshot; these counts are not added together as one
final run. Tests include product-label/data separation, legacy
form round trips, per-notification overrides, cancellation/owner-generation
guards, revision-bound final recording events and native-window reentry.
The existing lifecycle fixture creates only its own never-visible HWND.

Separate isolated locale/status fixtures exercise real native UI lifecycle with
fake data. Their precise accepted/failed run records remain separate; a shorter
run that did not reach a new stage is not relabeled as a passing complete test.
Native GPU status classification and ABI packets have pure tests. Those tests do
not initialize NVAPI, perform a GPU query or prove a state transition on hardware.

The first local candidate froze 160 Rust/Cargo inputs with aggregate SHA-256
`776d52f9a7e78fb9b89568d6b52654cd41b83558e0293cbd12da26b45ab2c2a0`.
That historical GNU Release link completed with no warnings/errors. Its executable
is 9,072,626 bytes with SHA-256
`24360593713c70149e01f02fb06717bf3e4423a8e3c5d1b9f02c02de10520021`.
Static PE verification found the expected `requireAdministrator` manifest and
resolved static imports without a developer-PATH dependency. The executable was
not launched through an elevation bypass. This first binary and archive are
preserved as historical artifacts and are not the current candidate.

The first two MSVC CI runs exposed Windows 8.3/long-path identity defects in boot
backup binding and the profile's single-instance/CLI identities. Their failed
results are retained rather than relabeled. The intermediate boot-only snapshot
had aggregate `8c02a6c8763cf08d78efb7120ef1fc473e92e3baeb637e042407a1894b8cb9ca`
and Release executable SHA-256
`425aabc146fd19200f67502ff75d69805f8838bb2bb0ce774cf105d862ead39c`;
that intermediate executable is also superseded.

The corrected source uses one noncreating directory-identity primitive for boot
backups, instance/pipe identity and autorun paths. The settings store resolves its
created root before deriving cooperating read/write locks, so short/long aliases
share the same lock. Existing file-name checks, atomic replacement and previous
copies remain in place. This intermediate workspace passed 573 tests in one
run, with no warnings, failures, ignored or filtered tests. This includes seven
store/path tests, fourteen boot-backup tests, four autorun tests and three CLI
identity tests; those subsets are not added again to the workspace count.

That intermediate candidate froze 160 compilation inputs with aggregate SHA-256
`66984f9af5cb418cb35733655bc92b118dcce6fd7b65ed2a0e5783975061571c`.
Its GNU Release link completed with no warnings/errors. Its historical executable
is 9,077,431 bytes with SHA-256
`8f51fb7cd7bbdd46932ebf7d1b19611c43805ac44473d147220416c5c6e6f422`.
The intermediate source commit `28e84739fc7eab89789d9aad83c4ed3fa66bd82e`
also passed all 573 tests, Release linking and clean packaging in MSVC CI
[41517181522](https://github.com/gura8390/Y7000P-IRX10/actions/runs/41517181522).
Its source, GNU package and successful CI are preserved as historical evidence.

The final self-import correction uses the same directory identity for both
configuration source and destination. A normal path, extended path or short-name
alias of the current directory is rejected before changing configuration bytes,
file inventory or repository revision. The complete workspace passed 574 tests
in one subsequent run, with no warnings, failures, ignored or filtered tests.
The corresponding 160-input aggregate is
`f5b183d819d5c8b0456a15aee1064ce4054f91af17a38f0f83f24b0e71bc8280`.
The matching GNU Release link completed with no warnings/errors. Its executable
is 9,077,431 bytes with SHA-256
`1f46ad5e88dfd7d72df3fc59bd54289b0597c7a59203c1f7888463249fd1b832`.
The exact archive source revision and file hashes are recorded in its manifest;
do not substitute the preserved intermediate hashes.
The normal Release still requires the operating system's administrator
confirmation. Static inspection and isolated tests do not prove its normal
elevated application or device lifecycle has been accepted.

Before these directory-identity corrections, a separate actual full-app
Debug/asInvoker read-only session exited successfully
with backend readiness, verified shutdown, no remaining broker, zero stderr bytes
and zero profile files. Window creation was about 283 ms; backend readiness was
about 1,381 ms in this one observation. The complete observed process tree included
two Jiks processes and two console hosts. Sensors/control were off; there were no
hardware setters, temperature sampling, GPU-library initialization, input hooks or
FPS ETW. This is a standalone lifecycle/resource observation, not a matched C#
comparison or proof of the Release administrator path.

UI Automation could read the isolated native layout. The occluded screenshot
attempt was not accepted as visual validation. Visual review, the normal Release
OS-confirmation path and actual-device acceptance remain explicit user-session
checks. Final archive hashes are recorded outside this source file to avoid a
commit/hash cycle. The source branch is published separately from any installer
or stable release. Remote CI results must be checked against the exact archive
source commit in the [Rust build workflow](https://github.com/gura8390/Y7000P-IRX10/actions/workflows/rust.yml);
the earlier failed runs and local GNU tests do not imply that a later MSVC job
has passed. C# main, its version tag and its release remain separate.

## Performance evidence

The accepted component comparison uses the actual C# SensorSettings window and
Rust sensor editor, deterministic 250 ms scheduling, Release on both sides,
900×900 actual client areas and DPI 144. Complete process trees were counted:
C# had one process; Rust had its GUI plus the simulated broker. Three short
alternating repetitions were collected for visible and hidden states.

Observed deltas span about 4.7 seconds between samples, not ten seconds. Native
font rasterization rounds 15 DIP to an integer pixel size. The retained hashes
were collected after the run; their file times precede it, but they are not a
contemporaneous tamper-proof binary/load trace. Earlier Debug/DPI/viewport-mismatch
runs are diagnostic and excluded from the accepted summary.

This demonstrates the measured component scenario only. Hidden CPU deltas are
too coarsely quantized to establish a robust general CPU advantage. It does not
prove full-application startup, memory, CPU, tray or thermal-control superiority.
The running C# thermal/sensor profile cannot fairly be compared against a Rust
sensor-off session. A whole-application matched plan remains separate and needs
the required session/permission arrangements; no temperature or load test was run.
