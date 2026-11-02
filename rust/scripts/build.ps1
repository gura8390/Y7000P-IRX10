[CmdletBinding()]
param(
    [ValidateSet('debug', 'release')][string]$Configuration = 'release',
    [ValidateSet('', 'x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')][string]$Target = '',
    [switch]$SkipTests
)
$ErrorActionPreference = 'Stop'
$cargoScript = Join-Path $PSScriptRoot 'cargo.ps1'
$localConfiguration = Join-Path $PSScriptRoot 'build.local.ps1'
if (Test-Path -LiteralPath $localConfiguration) { . $localConfiguration }
$targetArguments = @(if ($Target) { '--target'; $Target })
if (!$SkipTests) {
    & $cargoScript test --workspace --locked @targetArguments
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
$profileArguments = @(if ($Configuration -eq 'release') { '--release' })
$previousFlags = $env:CARGO_ENCODED_RUSTFLAGS
if ($Configuration -eq 'release') {
    # Keep absolute local source/cache/user paths out of panic/debug strings.
    # The encoded form preserves path arguments containing spaces.
    if ($env:RUSTFLAGS -and !$env:CARGO_ENCODED_RUSTFLAGS) { throw 'For a public release, move existing RUSTFLAGS into CARGO_ENCODED_RUSTFLAGS so path arguments can be preserved exactly.' }
    $root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
    $mappings = [ordered]@{}
    $mappings[$root] = '/jiks-source'
    foreach ($entry in @(@($env:JIKS_CARGO_HOME, '/cargo-home'), @($env:CARGO_HOME, '/cargo-home'), @($env:JIKS_TOOLCHAIN_ROOT, '/rust-toolchain'), @($env:USERPROFILE, '/user'))) {
        if ($entry[0]) { $mappings[$entry[0]] = $entry[1] }
    }
    $flags = @($mappings.GetEnumerator() | ForEach-Object { "--remap-path-prefix=$($_.Key)=$($_.Value)" })
    $env:CARGO_ENCODED_RUSTFLAGS = (@($previousFlags).Where({ $_ }) + $flags) -join [char]31
}
try {
    & $cargoScript build -p jiks-app --locked @profileArguments @targetArguments
    $buildResult = $LASTEXITCODE
} finally { $env:CARGO_ENCODED_RUSTFLAGS = $previousFlags }
exit $buildResult
