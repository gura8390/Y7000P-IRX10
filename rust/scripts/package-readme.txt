机控 / Jiks 0.1.0 — Y7000P IRX10 / 83NN native Rust application

The native application retains the original page hierarchy, settings categories,
device information, tray status, notifications and OSD. Presets, automation,
macros, keyboard effects, downloads and configuration maintenance use actual
asynchronous completion results. The GUI and broker are both native Rust.

Extract this folder to use it. The existing C# installation, desktop shortcuts
and settings remain separate. The package contains no user configuration.
Keep the original application and backups. Close the previous hardware
controller before enabling controls; Jiks checks for competing controllers.
Firmware operations require supported capabilities and an explicit feature or
action. Saved enabled features can run on subsequent normal launches.

The release executable requests Windows administrator approval. Debug test
builds use asInvoker. A permission failure means the capability could not be
verified, rather than that the hardware is absent. Temperature/load acceptance
belongs to the user; isolated tests do not establish firmware acceptance.

The interface uses Windows UI fonts, preferring Microsoft YaHei UI and falling
back to Segoe UI. No font binary is redistributed.

The application is GPL-3.0-only. Included notices describe the separately
licensed PawnIO IntelMSR, SmbusI801 and SmbusIntelSkylakeIMC modules embedded in
the application. Their matching source inputs and original build workflow are
included in licenses/PawnIO-source/. Sensor access requires an
existing compatible driver and explicit sensor enablement.
MPL-marked sensor files and the native protocol source, full MPL/LGPL texts and
upstream attribution are also included under licenses/. See NativeAdapters-NOTICE.txt
and package-manifest.json for the exact source revision and file hashes.

Compatibility choices are documented in the source verification notes. Native
quick-action icons use a finite symbol set; unknown legacy icon names/integers
are retained and may render as category markers. Release notes open the trusted
repository page. Old CLR scripts/plugins are retained inert; executable scripts
use an explicitly selected interpreter. FPS is process-level kernel Present
cadence, not proof of displayed/scanout frames.

Source, build instructions and implementation scope:
https://github.com/gura8390/Y7000P-IRX10/tree/rust-rewrite/rust

package-manifest.json records the exact source commit and every file's SHA-256.
Use that commit when reviewing source and verification notes. The accepted
performance comparison covers a matched SensorSettings component with simulated
250 ms scheduling and complete process trees; it does not prove a full-application
CPU/memory advantage. Final release gates and actual-device acceptance are
separate records.
