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

## Current candidate evidence (2026-11-03)

The current local source candidate freezes 162 Rust/Cargo/embedded inputs with
aggregate SHA-256
`645ab21d28fee360e1927fe0e4c7a3102de692dabadcf5b4ae7af7eb4c017a55`.
One complete workspace invocation passed **600 tests**, with no warnings,
failures, ignored or filtered tests: app 88, core 47, platform 187, runtime 271
and store 7. The count includes the subsets described below; they are not added
again. Source and build-tool hashes remained unchanged across this invocation
and the subsequent builds.

The matching GNU Debug executable is an `asInvoker` console build with SHA-256
`af0d1686c652eda0e5d677d1cb868e3f0608719e038766b602cb9f490b7a6f1d`.
The matching GNU production Release is 9,118,097 bytes with SHA-256
`b9eb4114b25d6050b81fb4a6a556a86e5a7869f8ceb75e3a6afe2367cf371270`.
Static PE inspection verified AMD64, the Release GUI subsystem and its
`requireAdministrator` manifest, with no unresolved static imports. Debug's
console subsystem and `asInvoker` resource were checked separately. A static
import audit does not exercise optional runtime vendor libraries. The production
Release has not been executed by this build stage. A new commit, CI result and
clean archive must be bound to this source before it is treated as a published
candidate; the existing RC.1 package is a different snapshot.

### Accessibility implementation and its acceptance boundary

The source now annotates standard HWND clients through Windows
`IAccPropServices`, adding semantic AutomationId, complete localized Name and
needed HelpText. Field keys, command roles, page/category keys and editor modes
define identity independently of translated captions and row order. Duplicate
save/apply entries have explicit roles. Existing native control IDs still
dispatch the same commands. Standard providers retain their original Value,
ControlType, enabled/checked state and Patterns; no synthetic SelectionItem or
LabelledBy relationship is claimed.

The UI owns a lifetime STA guard. Annotation creation does not retain a TLS
borrow across COM calls. Native generation checks reject an HWND retired during
annotation, and subclass cleanup clears properties at destruction before a handle
can be reused. Unchanged metadata skips repeated COM property writes; an empty
explicit Name remains distinct from leaving the native Name provider in charge.
OSD exposes a read-only root summary only when its rendered rows change, without
creating mirror controls or fictitious interactive Patterns.

Formal native-COM UIA tests use a separate, windowless MTA client and directly
query exact owned HWNDs while the UI thread pumps messages. Never-visible standard
controls, device information, the status window and OSD tests cover actual
Name/ID/Help, native Value/read-only/enabled/checked state, Pattern availability,
locale changes, private-value masking and destruction. No test invokes a device
command or injects input. Model tests check distinct semantic control IDs for all
nine pages and eight settings categories. These tests do **not** establish full
main-window subtree discovery or cross-client compatibility.

A separate visible fake-fixture check using the .NET Framework 4.8
`System.Windows.Automation` client enumerated 85 elements and read native captions,
values, states and Invoke availability. It exposed numeric native IDs rather than
the new semantic annotations, and HelpText was empty. Semantic-ID navigation was
not accepted and no application action was invoked in that check. Native-COM
cross-process comparison is still pending. The observed client difference must
be resolved or explicitly bounded before claiming complete accessibility
acceptance; it is not explained solely by launching a hidden window.

### OSD and consumer scheduling corrections

An earlier fake UI fixture failed during exit with a render-state `RefCell`
reentry panic. The corrected OSD keeps native HWND lifetime in an independent
cell; visibility queries and retirement do not reborrow rendering state.
Nested sync/flush leaves dirty work for a later UI tick. A real never-visible
OSD HWND test holds a render borrow while querying and destroying the window,
then verifies retirement and balanced open/close ownership. Bar size and paint
now use one DPI-scaled grid, including padding, so two-item and wrapped layouts
allocate the same columns and rows. Geometry tests include 24 items, narrow
widths and DPI 96 through 288. These are lifecycle/geometry tests, not a full
visible-layout review at every resolution.

Two distinct consumer scheduling defects were corrected without retiming the
thermal supervisor or changing hardware permissions:

- CPU/GPU/firmware presentation now takes the fastest actually active consumer
  interval. An OSD-only 100ms demand no longer inherits a hidden dashboard's 60s
  interval. Hidden OSD settings create no demand; legacy/modern dashboards with
  sensors hidden do not keep source or slow auxiliary polling active. Active
  battery/status and the exporter retain their separate consumer semantics.
  Existing slow deadlines are shortened when a faster consumer becomes active.
- An otherwise idle status/battery-only consumer now includes the slow auxiliary
  worker's next 2s sample deadline in the engine wait. A retained worker port does
  not cause a busy poll. Pending work, due deadlines and failed cleanup retain
  their existing finite-poll and ownership rules. The retained regression first
  reproduced the old 30s wait versus the expected 2s through actual Fake ticket
  completions, then passed after correction.

The 600-test invocation contains the consumer/deadline regressions. They do not
prove actual 100ms hardware cadence, temperature control or resource superiority.

### Historical evidence, not the current candidate

The directory-identity and self-import corrections at source commit
`27d1f7b19959fcc529a75404871060f49b29abce` passed 574 workspace tests locally and
in MSVC CI. Their 160-input aggregate and executable hashes belong to RC.1, not
to the 162-input, 600-test candidate above. Earlier failed MSVC jobs exposed the
8.3/long-path identity defects and remain part of the history. The intermediate
573-test run and earlier 566/74/70-test stages are separate snapshots, not
additional tests to sum into a current result.

Before those identity fixes, a separate Debug/asInvoker read-only full-app
session observed a roughly 283ms window and 1,381ms backend readiness, verified
shutdown, no remaining broker, zero stderr and no profile files. Those are
one-run historical observations with sensors/control disabled, not current
Release timings or a matched C# comparison.

Normal production Release GUI/device lifecycle, final visible UI review,
complete semantic-ID UIA navigation and the matched whole-application benchmark
remain pending. No firmware, temperature, game or full-load acceptance is implied.
Temperature/load acceptance belongs to the user. C# main, its version tag, its
release and the RC.1 assets remain separate. A successful build or isolated test
does not select a desktop replacement.

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
