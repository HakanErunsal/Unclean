[CmdletBinding()]
param(
    [string]$BinaryPath,
    [string]$OutputPath,
    [string]$CargoPath = "cargo",
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ([string]::IsNullOrWhiteSpace($BinaryPath)) {
    $BinaryPath = Join-Path $root "target\release\unclean.exe"
}
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $OutputPath = Join-Path $root ".local-fixtures\installed-engine-acceptance.json"
}
$BinaryPath = [System.IO.Path]::GetFullPath($BinaryPath)
$OutputPath = [System.IO.Path]::GetFullPath($OutputPath)

function ConvertTo-NativeArgument {
    param([string]$Value)

    if ($Value.Length -gt 0 -and $Value -notmatch '[\s"]') {
        return $Value
    }

    $builder = [System.Text.StringBuilder]::new()
    [void]$builder.Append('"')
    $backslashCount = 0
    foreach ($character in $Value.ToCharArray()) {
        if ($character -eq '\') {
            $backslashCount += 1
            continue
        }
        if ($character -eq '"') {
            [void]$builder.Append(('\' * (($backslashCount * 2) + 1)))
            [void]$builder.Append('"')
            $backslashCount = 0
            continue
        }
        [void]$builder.Append(('\' * $backslashCount))
        [void]$builder.Append($character)
        $backslashCount = 0
    }
    [void]$builder.Append(('\' * ($backslashCount * 2)))
    [void]$builder.Append('"')
    return $builder.ToString()
}

function Invoke-UncleanJson {
    param([string[]]$Arguments)

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $BinaryPath
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.Arguments = (($Arguments | ForEach-Object { ConvertTo-NativeArgument $_ }) -join " ")

    $stopwatch = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::Start($startInfo)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    $stopwatch.Stop()

    if ($process.ExitCode -ne 0) {
        throw "Read-only acceptance command failed with exit code $($process.ExitCode): $stderr"
    }
    try {
        $value = $stdout | ConvertFrom-Json
    } catch {
        throw "Read-only acceptance command returned invalid JSON. Rebuild the console program and retry."
    }
    return [ordered]@{
        value = $value
        duration_ms = $stopwatch.ElapsedMilliseconds
    }
}

if (-not $SkipBuild) {
    $cargo = Get-Command $CargoPath -ErrorAction SilentlyContinue
    if ($null -eq $cargo) {
        throw "Cargo was not found: $CargoPath. Install the pinned Rust toolchain or pass -CargoPath."
    }
    & $cargo.Source build --workspace --release --locked
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed. Correct the build failure and retry."
    }
}
if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "Console binary was not found: $BinaryPath. Build the release workspace and retry."
}

$engineResult = Invoke-UncleanJson @("engines", "--format", "json")
$engineReports = @()
foreach ($engine in $engineResult.value.engines) {
    $pluginScan = $null
    if ($engine.health -ne "unavailable") {
        $scanResult = Invoke-UncleanJson @("plugins", "--engine-path", [string]$engine.path, "--format", "json")
        $pluginScan = [ordered]@{
            ok = [bool]$scanResult.value.ok
            plugin_count = @($scanResult.value.plugins).Count
            scan_warning_count = @($scanResult.value.warnings).Count
            dependency_warning_count = @($scanResult.value.dependency_warnings).Count
            discovery_warning_count = @($scanResult.value.discovery_warnings).Count
            duration_ms = $scanResult.duration_ms
        }
    }
    $engineReports += [ordered]@{
        path = [string]$engine.path
        version = $engine.version
        source = [string]$engine.source
        health = [string]$engine.health
        descriptor_count = [int]$engine.descriptor_count
        issue_count = @($engine.issues).Count
        plugin_scan = $pluginScan
    }
}

$report = [ordered]@{
    schema = 1
    generated_at_utc = [DateTimeOffset]::UtcNow.ToString("O")
    product_version = (& $BinaryPath --version).Trim()
    discovery_duration_ms = $engineResult.duration_ms
    discovery_warning_count = @($engineResult.value.warnings).Count
    engines = $engineReports
}
$parent = Split-Path -Parent $OutputPath
New-Item -ItemType Directory -Path $parent -Force | Out-Null
$reportJson = $report | ConvertTo-Json -Depth 8
[System.IO.File]::WriteAllText($OutputPath, $reportJson, [System.Text.UTF8Encoding]::new($false))

if (@($engineReports | Where-Object { $_.health -ne "unavailable" }).Count -eq 0) {
    throw "No selectable engine was found. Pass a machine with an installed or source-built engine."
}

Write-Output "Read-only installed-engine acceptance report created: $OutputPath"
