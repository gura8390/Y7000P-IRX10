# Optional environment variables: JIKS_TOOLCHAIN_ROOT, JIKS_CARGO_HOME,
# JIKS_BINUTILS, JIKS_CARGO_TARGET, JIKS_LINKER, JIKS_RC and
# JIKS_RC_PREPROCESSOR. A private build.local.ps1 may set these for this machine.
$CargoArguments = @($args)
$ErrorActionPreference = 'Stop'
$sourceRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$localConfiguration = Join-Path $PSScriptRoot 'build.local.ps1'
if (Test-Path -LiteralPath $localConfiguration) { . $localConfiguration }

$cargoProgram = 'cargo'
if ($env:JIKS_TOOLCHAIN_ROOT) {
    $toolchainRoot = [IO.Path]::GetFullPath($env:JIKS_TOOLCHAIN_ROOT)
    $cargoProgram = Join-Path $toolchainRoot 'bin/cargo.exe'
    $env:RUSTC = Join-Path $toolchainRoot 'bin/rustc.exe'
    $env:RUSTDOC = Join-Path $toolchainRoot 'bin/rustdoc.exe'
    foreach ($required in @($cargoProgram, $env:RUSTC, $env:RUSTDOC)) {
        if (!(Test-Path -LiteralPath $required)) { throw "Rust toolchain component is absent: $required" }
    }
    $env:PATH = (Join-Path $toolchainRoot 'bin') + ';' + $env:PATH
}
if ($env:JIKS_CARGO_HOME) { $env:CARGO_HOME = $env:JIKS_CARGO_HOME }
if ($env:JIKS_CARGO_TARGET) { $env:CARGO_BUILD_TARGET = $env:JIKS_CARGO_TARGET }
if ($env:JIKS_BINUTILS) { $env:PATH = $env:JIKS_BINUTILS + ';' + $env:PATH }
if ($env:JIKS_EXTRA_PATH) { $env:PATH = $env:JIKS_EXTRA_PATH + ';' + $env:PATH }
if ($env:JIKS_LINKER) {
    if (!$env:CARGO_BUILD_TARGET) { throw 'JIKS_LINKER requires an explicit JIKS_CARGO_TARGET or CARGO_BUILD_TARGET.' }
    $linkerVariable = 'CARGO_TARGET_' + $env:CARGO_BUILD_TARGET.Replace('-', '_').ToUpperInvariant() + '_LINKER'
    [Environment]::SetEnvironmentVariable($linkerVariable, $env:JIKS_LINKER, 'Process')
}
if ($env:JIKS_RUST_SYSROOT) {
    if (!$env:JIKS_TOOLCHAIN_ROOT) { throw 'The optional sysroot workaround requires JIKS_TOOLCHAIN_ROOT.' }
    $wrapperDirectory = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { Join-Path $sourceRoot 'target' }
    [IO.Directory]::CreateDirectory($wrapperDirectory) | Out-Null
    $wrapper = Join-Path $wrapperDirectory 'jiks-rustc-wrapper.exe'
    $wrapperSource = Join-Path $PSScriptRoot 'rustc-wrapper.rs'
    if (!(Test-Path -LiteralPath $wrapper) -or (Get-Item -LiteralPath $wrapper).LastWriteTimeUtc -lt (Get-Item -LiteralPath $wrapperSource).LastWriteTimeUtc) {
        & $env:RUSTC --edition 2024 -O $wrapperSource -o $wrapper
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    }
    $env:RUSTC_WRAPPER = $wrapper
}
Push-Location -LiteralPath $sourceRoot
try {
    & $cargoProgram @CargoArguments
    $result = $LASTEXITCODE
} finally { Pop-Location }
exit $result
