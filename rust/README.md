# 机控 / Jiks

Jiks 0.1.0 is the native Rust/Win32 reconstruction for this Y7000P IRX10 / 83NN.
It retains the original Dashboard, keyboard, battery, automation, macro,
downloads, settings, donate and about hierarchy, with eight settings categories,
device information, a tray status window, notifications and OSD.

The GUI uses native controls and CPU/GDI drawing. A separate broker coordinates
one hardware writer and independent reads; commands complete after their actual
results, including shutdown cleanup. There is no CLR business host, WebView or
continuously running GPU renderer. The existing C# installation remains separate.

Implemented workflows include structured presets/rules/macros, native event
sources and hotkeys, explicit recording/playback, power and display controls,
Spectrum editing, local settings migration/backup/restore, service/autorun tools,
downloads, and a native executable/script console. Capabilities are checked for
this machine. A permission or getter failure stays unknown and is not reported
as hardware absence. [Verification and migration decisions](docs/verification.md)
distinguish implemented paths, isolated tests and actual-device acceptance.

## Build on Windows

Rust 1.95.0 and the checked-in `Cargo.lock` pin the language toolchain and crates.
The default uses the installed host target. The recommended public build uses
Visual Studio Build Tools with **Desktop development with C++** and a Windows
SDK, then a Developer PowerShell with `rc.exe`, `link.exe` and `cl.exe` on PATH:

```powershell
rustup toolchain install 1.95.0 --profile minimal
rustup target add x86_64-pc-windows-msvc --toolchain 1.95.0
$env:CARGO_ENCODED_RUSTFLAGS = '-Ctarget-feature=+crt-static'
./scripts/build.ps1 -Target x86_64-pc-windows-msvc
```

GNU is also supported. Install the GNU Rust host/target and MinGW-w64 binutils
(`windres`, `as`, linker) on PATH. Select it explicitly:

```powershell
./scripts/build.ps1 -Target x86_64-pc-windows-gnu
```

`scripts/cargo.ps1` accepts ordinary Cargo arguments. Optional environment
variables are `JIKS_TOOLCHAIN_ROOT`, `JIKS_CARGO_HOME`, `JIKS_CARGO_TARGET`,
`JIKS_BINUTILS`, `JIKS_LINKER`, `JIKS_RC`, `JIKS_RC_PREPROCESSOR`, and
`JIKS_RC_PREPROCESSOR_TARGET`. No private tool directory is required. An ignored
`scripts/build.local.ps1` may set these for a local toolchain. GNU installations
whose non-ASCII sysroot path causes linker response-file problems may opt into
`JIKS_RUST_SYSROOT`; the wrapper only passes an equivalent explicit sysroot.

Debug manifests use `asInvoker`; release manifests request administrator access.
Building, unit tests and packaging do not run the application, operate firmware,
install drivers, change the desktop, or modify an existing installation.

The recommended MSVC command and CI explicitly select the static C runtime.
Direct Cargo builds use their selected toolchain's defaults; a dynamic MSVC
runtime build may require the matching Visual C++ runtime. The final PE imports
must be checked rather than inferred from a successful build. The release
package must not depend on MinGW or build-tool DLLs found only on a developer's
PATH. See the [Rust runtime linkage reference](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes).

## Source-pinned package

From the repository's `rust` directory, after the source freeze and release gates:

```powershell
./scripts/verify-pe.ps1 -Binary ./target/x86_64-pc-windows-msvc/release/Jiks.exe `
  -Output ./target/PE-DEPENDENCIES.json
./scripts/dependency-licenses.ps1 -OutputFile ./target/DEPENDENCY_LICENSES.txt -Target x86_64-pc-windows-msvc
$sourceRevision = (git rev-parse HEAD).Trim()
./scripts/package.ps1 -Binary ./target/x86_64-pc-windows-msvc/release/Jiks.exe `
  -OutputDirectory ./dist -DependencyLicenses ./target/DEPENDENCY_LICENSES.txt `
  -Label 0.1.0 -SourceRevision $sourceRevision
```

The static PE audit checks the actual AMD64 GUI image, embedded execution-level
manifest and normal/delay imports. It deliberately ignores the developer PATH.
The single-executable package rejects dependencies on adjacent DLLs that are not
in its allowlist; the standalone audit can recursively inspect them only with an
explicit `-AllowAdjacentDlls`. Windows system DLLs and the actual Windows API-set
namespace/hosts are treated as OS dependencies. It does not execute
the image, verify optional runtime-loaded GPU libraries, or establish normal
administrator startup. Packaging repeats this gate before copying any files.

The package script requires a full source commit ID, copies an explicit
artifact/license/readme allowlist, verifies each ZIP entry against its source
and writes SHA-256 metadata. It refuses to overwrite an existing package or
package a benchmark fixture. Settings, logs, personal reports, private build
configuration, PDBs and font binaries are excluded. It does not publish a release.
The three signed PawnIO modules remain embedded in the executable and checked
into clean source. Their original source/include/build inputs, hashes, licenses
and provenance are also included under `licenses/PawnIO-source/` in the package.
MPL-marked sensor files and the native protocol source are included with their
full licenses and attribution. See `licenses/NativeAdapters-NOTICE.txt`.

Use the installed 方正公文楷体 font where licensed; the interface falls back to
Windows fonts. No proprietary font is included in public source or packages.

## Verification boundaries

`cargo test --workspace --locked` runs pure/temporary-directory tests and the
explicitly isolated never-visible native-window fixtures. A separately requested
`Jiks.exe ui-smoke` uses a fake action consumer, then exits. These gates cover
logic, ownership and UI contracts; normal startup, elevated firmware operations
and live hardware acceptance are recorded separately for the final source and
binary hashes. Temperature and load acceptance are performed by the user.

Release manifests request Windows administrator approval. An `asInvoker` fixture
can check its own loader and UI paths; it cannot establish that the normal
administrator executable has been launched or that firmware access succeeded.

Performance comparisons must use matched telemetry frequency, enabled features,
permissions and window/tray state, include the entire broker process tree, and
separate native-window creation from backend readiness. An old active thermal
controller versus a new sensor-off interface is not a fair language comparison.

The accepted existing benchmark covers only the actual SensorSettings component
and simulated 250ms status scheduling. It uses Release on both sides, a gated
900x900 client viewport, DPI 144 and complete process trees (C# 1, Rust 2 with its
simulated broker). Resource deltas span about 4.7 seconds; hidden CPU samples are
too coarsely quantized for a robust general CPU-advantage claim. It does not
establish full-application, tray, hardware or thermal-control performance.
