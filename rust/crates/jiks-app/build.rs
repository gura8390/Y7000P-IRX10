//! Native manifest/version resources using the standard Windows SDK (MSVC) or
//! MinGW binutils (GNU). JIKS_RC and JIKS_RC_PREPROCESSOR are explicit overrides.
use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(jiks_benchmark_fixture)");
    for name in [
        "JIKS_RC",
        "JIKS_RC_PREPROCESSOR",
        "JIKS_RC_PREPROCESSOR_TARGET",
        "JIKS_BENCHMARK_FIXTURE",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let target = env::var("TARGET").expect("Cargo TARGET");
    assert!(
        target.contains("windows"),
        "Jiks is a native Windows application"
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let release = env::var("PROFILE").as_deref() == Ok("release");
    let fixture = env::var("JIKS_BENCHMARK_FIXTURE").as_deref() == Ok("1");
    assert!(
        !fixture || release,
        "The isolated optimized benchmark requires --release"
    );
    if fixture {
        println!("cargo:rustc-cfg=jiks_benchmark_fixture");
    }
    let description = if fixture {
        "Jiks isolated benchmark fixture - no production entry"
    } else {
        "Jiks Y7000P IRX10 native manager"
    };
    let level = if release && !fixture {
        "requireAdministrator"
    } else {
        "asInvoker"
    };
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let mut parts = version
        .split('.')
        .take(3)
        .map(|n| n.parse::<u16>().expect("Numeric package version"));
    let (major, minor, patch) = (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    );
    let manifest = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
 <assemblyIdentity version="{major}.{minor}.{patch}.0" processorArchitecture="amd64" name="Jiks" type="win32"/>
 <dependency><dependentAssembly><assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0" processorArchitecture="amd64" publicKeyToken="6595b64144ccf1df" language="*"/></dependentAssembly></dependency>
 <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3"><security><requestedPrivileges><requestedExecutionLevel level="{level}" uiAccess="false"/></requestedPrivileges></security></trustInfo>
 <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1"><application><supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/></application></compatibility>
 <application xmlns="urn:schemas-microsoft-com:asm.v3"><windowsSettings><dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness><longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware></windowsSettings></application>
</assembly>"#
    );
    let manifest_path = out.join("Jiks.manifest");
    fs::write(&manifest_path, manifest).expect("Write application manifest");
    let rc = out.join("Jiks.rc");
    let manifest_name = manifest_path
        .to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\"\"");
    fs::write(
        &rc,
        format!(
            r#"#pragma code_page(65001)
1 24 "{manifest_name}"
1 VERSIONINFO
 FILEVERSION {major},{minor},{patch},0
 PRODUCTVERSION {major},{minor},{patch},0
 FILEFLAGSMASK 0x3fL
 FILEFLAGS 0x0L
 FILEOS 0x40004L
 FILETYPE 0x1L
BEGIN
 BLOCK "StringFileInfo"
 BEGIN
  BLOCK "040904b0"
  BEGIN
   VALUE "CompanyName", "Jiks contributors\0"
   VALUE "FileDescription", "{description}\0"
   VALUE "FileVersion", "{version}.0\0"
   VALUE "InternalName", "Jiks\0"
   VALUE "OriginalFilename", "Jiks.exe\0"
   VALUE "ProductName", "Jiks\0"
   VALUE "ProductVersion", "{version}\0"
  END
 END
 BLOCK "VarFileInfo"
 BEGIN
  VALUE "Translation", 0x409, 1200
 END
END
"#
        ),
    )
    .expect("Write Windows resource source");
    let msvc = target.ends_with("msvc");
    let program = env::var_os("JIKS_RC")
        .unwrap_or_else(|| if msvc { "rc.exe" } else { "windres.exe" }.into());
    let object = out.join(if msvc {
        "Jiks-resource.res"
    } else {
        "Jiks-resource.o"
    });
    let mut command = Command::new(&program);
    if msvc {
        command.arg("/nologo").arg("/fo").arg(&object).arg(&rc);
    } else {
        if let Some(preprocessor) = env::var_os("JIKS_RC_PREPROCESSOR") {
            command.arg(format!("--preprocessor={}", preprocessor.to_string_lossy()));
            if let Ok(target) = env::var("JIKS_RC_PREPROCESSOR_TARGET") {
                command.arg(format!("--preprocessor-arg=--target={target}"));
            }
            command.args([
                "--preprocessor-arg=-E",
                "--preprocessor-arg=-xc",
                "--preprocessor-arg=-DRC_INVOKED",
            ]);
        }
        command
            .arg("--input")
            .arg(&rc)
            .arg("--output")
            .arg(&object)
            .arg("--output-format=coff");
    }
    let status = command.status().unwrap_or_else(|error| panic!("Cannot run Windows resource compiler {program:?}: {error}. Use a Windows SDK Developer PowerShell (MSVC), or install MinGW windres on PATH (GNU); JIKS_RC overrides its path."));
    assert!(
        status.success(),
        "Windows resource compilation failed: {status}"
    );
    println!("cargo:rustc-link-arg={}", object.display());
}
