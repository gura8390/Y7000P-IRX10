# Third-party notices

拯救者工具箱 is a GPL-3.0-only larger work; see `LICENSE`. Explicitly MPL-2.0-marked
sensor files retain their file-level license. The user interface and native
adapters were derived from the behavior and source of Lenovo Legion Toolkit,
including contributions by Bartosz Cichecki, Kaguya, Dr. Skinner and contributors.
The C# upstream license and attribution remain in the repository's C# branch.
The executable, window, and tray use the original application icon from
`LenovoLegionToolkit.WPF/Assets/icon.ico` in the C# 2.34.0.3 source.

The exact Cargo dependency graph is recorded in `Cargo.lock`. The source package
retains those crates' upstream references; Cargo downloads their license files
with each crate. The application uses the Rust standard library and the `anyhow`,
`serde`, `serde_json`, `windows`, `windows-sys`, `wmi`, `rust_decimal`, `sha2`,
`quick-xml`, and `image`
families and their transitive dependencies. A build records the complete graph
with `cargo metadata --locked`.

`intel_sensors.rs`, `gpu_sensors.rs`, `intel_gpu.rs` and `memory_sensors.rs` are
marked MPL-2.0. They retain the LibreHardwareMonitor and RAMSPDToolkit attribution,
full licenses and source references in `licenses/NativeAdapters-NOTICE.txt`.
Binary archives include those matching source files under
`licenses/NativeSensors-source/`. The source repository retains the full build
inputs at the commit recorded in the package manifest.

The fixed NvAPIWrapper protocol reference uses LGPL-3.0, not MIT. Its unmodified
license, original attribution and immutable source URL are in `licenses/`.
Matching Jiks protocol source is included under `licenses/NvAPI-protocol-source/`
in archives. This is a native Rust implementation; the managed wrapper assembly
is not bundled. See `licenses/source-manifest.json` for verified upstream license
text hashes.

The unmodified signed IntelMSR, SmbusI801 and SmbusIntelSkylakeIMC modules are
redistributed with the upstream LGPL-2.1-or-later license. See
`licenses/PawnIO-COPYING.txt` and `licenses/PawnIO-NOTICE.txt` in binary packages,
or `crates/jiks-platform/resources/pawnio/` in source. It is a separate native
module set, not a new driver installation. The complete source/include inputs
for these three modules, original build workflow, immutable commit and per-file
hashes are included in source and under `licenses/PawnIO-source/` in packages.
No private signing material or generic Pawn compiler binary is redistributed.

NVIDIA and Windows libraries are accessed through their installed system or
driver interfaces; their binaries are not redistributed in this package.
No FZGWKaiS font binary is redistributed. Users may use a font they already
have installed, or the built-in Windows fallback font.

The native FPS adapter follows the MIT-licensed LLT.PresentMonFps provider and
500-interval statistics contract. Its exact reference commit, attribution and
metric limitations are retained in `crates/jiks-platform/resources/presentmon-fps/`.
Binary packages include `licenses/PresentMonFps-LICENSE.txt` and its NOTICE. No
CLR wrapper or third-party PresentMon DLL is required by the Rust adapter.
