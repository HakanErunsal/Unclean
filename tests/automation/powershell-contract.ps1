[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$BinaryPath
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path -LiteralPath $BinaryPath -PathType Leaf)) {
    throw "PowerShell automation contract failed: the Unclean executable was not found. Build the release binary and retry."
}

$engineOutput = & $BinaryPath engines --format json 2>&1
$engineExitCode = $LASTEXITCODE
if ($engineExitCode -ne 0) {
    throw "PowerShell automation contract failed: engine discovery returned exit code $engineExitCode. Check the executable and retry."
}

try {
    $engineReport = $engineOutput | ConvertFrom-Json
} catch {
    throw "PowerShell automation contract failed: engine discovery did not return valid JSON. Check the stable schema and retry."
}

if ($engineReport.schema -ne 1 -or $engineReport.ok -ne $true -or $null -eq $engineReport.engines) {
    throw "PowerShell automation contract failed: the engine discovery envelope changed. Restore the schema 1 contract or publish a new schema."
}

$missingPreset = Join-Path $PSScriptRoot "unclean-contract-missing-preset.toml"
$previousErrorActionPreference = $ErrorActionPreference
$ErrorActionPreference = "Continue"
$failureOutput = & $BinaryPath preset validate $missingPreset --format json 2>&1
$failureExitCode = $LASTEXITCODE
$ErrorActionPreference = $previousErrorActionPreference
if ($failureExitCode -ne 4) {
    throw "PowerShell automation contract failed: a missing preset returned exit code $failureExitCode instead of 4. Restore the documented exit code."
}

try {
    $failureReport = $failureOutput | ConvertFrom-Json
} catch {
    throw "PowerShell automation contract failed: the missing-preset result was not valid JSON. Check the stable error envelope and retry."
}

if (
    $failureReport.schema -ne 1 -or
    $failureReport.ok -ne $false -or
    $failureReport.error.code -ne "not_found" -or
    $failureReport.error.exit_code -ne 4
) {
    throw "PowerShell automation contract failed: the missing-preset envelope changed. Restore the schema 1 error contract or publish a new schema."
}

Write-Output "PowerShell automation contract passed."
