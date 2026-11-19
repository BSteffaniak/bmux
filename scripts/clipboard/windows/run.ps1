# Run in an already provisioned native x64 MSVC build environment. Never installs.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Shipping,
    [Parameter(Mandatory)][string]$Candidate,
    [Parameter(Mandatory)][string]$Output,
    [Parameter(Mandatory)][string]$VolumeId,
    [Parameter(Mandatory)][string]$DiskId,
    [Parameter(Mandatory)][string]$RustVersion,
    [Parameter(Mandatory)][string]$ToolchainBin,
    [switch]$BuildOnly
)
$ErrorActionPreference = 'Stop'
# Process-scoped environment only. Launch this script in a fresh child PowerShell.
$v = Get-Volume -DriveLetter E
$d = Get-Partition -DriveLetter E | Get-Disk
$os = Get-Partition -DriveLetter ($env:SystemDrive.Substring(0,1)) | Get-Disk
if ($v.FileSystemLabel -ne 'Data' -or $v.UniqueId -ne $VolumeId -or
    $d.UniqueId -ne $DiskId -or $d.Number -eq $os.Number -or $d.IsBoot -or $d.IsSystem) {
    throw 'External volume identity mismatch'
}
function External([string]$p) {
    if ($p -notmatch '^E:\\' -or $p.Substring(2).Contains(':')) {throw "Not an absolute E: path: $p"}
    $full = [IO.Path]::GetFullPath($p)
    $ancestor = $full
    while ($ancestor) {
        if ((Test-Path -LiteralPath $ancestor) -and
            ((Get-Item -Force -LiteralPath $ancestor).Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Reparse point: $ancestor"
        }
        $ancestor = Split-Path -Parent $ancestor
    }
    return $full
}
$Shipping = External $Shipping; $Candidate = External $Candidate; $Output = External $Output
if ($Shipping -eq $Candidate -or (Test-Path -LiteralPath $Output)) {throw 'Need distinct checkouts and fresh output directory'}
$null = External $PSScriptRoot
foreach ($n in 'CARGO_HOME','RUSTUP_HOME','TEMP','TMP') {
    $p = External ([Environment]::GetEnvironmentVariable($n)); if (!(Test-Path -LiteralPath $p -PathType Container)) {throw "Missing $n"}
}
foreach ($n in 'RUSTC_WRAPPER','RUSTC_WORKSPACE_WRAPPER','RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CL','_CL_','LINK','_LINK_','CARGO_BUILD_TARGET','CARGO_BUILD_BUILD_DIR','CARGO_BUILD_RUSTC_WRAPPER','CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER') {
    if ([Environment]::GetEnvironmentVariable($n)) {throw "Conflicting environment: $n"}
}
$rust = External (Join-Path $ToolchainBin 'rustc.exe'); $cargo = External (Join-Path $ToolchainBin 'cargo.exe')
$env:RUSTC = $rust; $env:RUSTDOC = External (Join-Path $ToolchainBin 'rustdoc.exe')
$version = & $rust -vV
if ($LASTEXITCODE -ne 0 -or $version -notcontains "release: $RustVersion" -or $version -notcontains 'host: x86_64-pc-windows-msvc') {throw 'Wrong native compiler'}
$version
if (!(Get-Command cl.exe -CommandType Application)) {throw 'No native MSVC compiler'}
$src = External (Join-Path $Candidate 'packages\clipboard\examples\interop_peer.rs')
$template = [IO.File]::ReadAllText($src)
$hook = "fn run() -> Result<(), &'static str> {"
if ([regex]::Matches($template,[regex]::Escape($hook)).Count -ne 1) {throw 'Shared example changed; review the guard insertion point'}
$guard = [IO.File]::ReadAllText((External (Join-Path $PSScriptRoot 'station_guard.rs')))
$guarded = $template.Replace($hook, $hook + @'

    verify_test_desktop()?;
    if std::env::args().nth(1).as_deref() == Some("verify-only") {
        println!("verified");
        return Ok(());
    }
'@) + "`n" + $guard
$null = New-Item -ItemType Directory -Path $Output
Set-Location -LiteralPath $Output
$env:CARGO_NET_OFFLINE = 'true'; $env:CARGO_INCREMENTAL = '0'
foreach ($label in 'shipping','candidate') {
    $tree = if ($label -eq 'shipping') {$Shipping} else {$Candidate}
    $examples = External (Join-Path $tree 'packages\clipboard\examples')
    $path = External (Join-Path $examples 'win_guarded_peer.rs')
    if (!(Test-Path -LiteralPath $examples)) {$null = New-Item -ItemType Directory -Path $examples}
    # CreateNew refuses to overwrite any existing source, even on reruns.
    $stream = [IO.File]::Open($path,[IO.FileMode]::CreateNew)
    try { $bytes = [Text.Encoding]::UTF8.GetBytes($guarded); $stream.Write($bytes,0,$bytes.Length) } finally { $stream.Dispose() }
    try {
        $env:CARGO_TARGET_DIR = Join-Path $Output "$label-target"
        $env:CARGO_BUILD_BUILD_DIR = Join-Path $Output "$label-build"
        Push-Location -LiteralPath $tree
        try {
            & $cargo build --frozen -p bmux_clipboard --example win_guarded_peer
            if ($LASTEXITCODE) {throw "$label build failed; no online retry"}
            if ($label -eq 'candidate') {
                # The selected unit tests exercise conversion only, never a system board.
                & $cargo test --frozen -p bmux_clipboard --lib -- native::windows::tests::
                if ($LASTEXITCODE) {throw 'clipboard-free Windows unit tests failed'}
            }
        } finally {Pop-Location}
        Copy-Item -LiteralPath (External (Join-Path $env:CARGO_TARGET_DIR 'debug\examples\win_guarded_peer.exe')) -Destination "$label.exe"
    } finally {Remove-Item -LiteralPath $path}
}
& cl.exe /nologo /EHsc /std:c++17 /utf-8 /MT /W4 (External (Join-Path $PSScriptRoot 'lane.cpp')) /Felane.exe /link user32.lib /INCREMENTAL:NO
if ($LASTEXITCODE) {throw 'launcher build failed'}
Get-FileHash .\lane.exe,.\shipping.exe,.\candidate.exe | Format-Table -AutoSize
if ($BuildOnly) {return}
& .\lane.exe run (Join-Path $Output 'shipping.exe') (Join-Path $Output 'candidate.exe')
if ($LASTEXITCODE) {throw "Windows clipboard acceptance failed ($LASTEXITCODE)"}
