[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$EnginePath,
    [string]$CliPath = "target\release\unclean.exe",
    [ValidateRange(1, 20)]
    [int]$Iterations = 3
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$resolvedCli = if ([System.IO.Path]::IsPathRooted($CliPath)) {
    $CliPath
} else {
    Join-Path $root $CliPath
}
if (-not (Test-Path -LiteralPath $resolvedCli -PathType Leaf)) {
    throw "Loading benchmark failed: the console program was not found at $resolvedCli. Build the release console program or pass -CliPath."
}
if (-not (Test-Path -LiteralPath $EnginePath -PathType Container)) {
    throw "Loading benchmark failed: the engine directory was not found at $EnginePath. Pass one installed engine root."
}

function Measure-UncleanCommand {
    param([string[]]$Arguments)

    $samples = 1..$Iterations | ForEach-Object {
        $duration = Measure-Command {
            & $resolvedCli @Arguments | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "Loading benchmark failed: unclean exited with code $LASTEXITCODE. Run the command without the benchmark and correct the reported error."
            }
        }
        [math]::Round($duration.TotalMilliseconds, 0)
    }
    $ordered = @($samples | Sort-Object)
    [PSCustomObject]@{
        samples_ms = @($samples)
        median_ms = $ordered[[math]::Floor($ordered.Count / 2)]
    }
}

[PSCustomObject]@{
    schema = 1
    iterations = $Iterations
    discovery = Measure-UncleanCommand @("engines", "--format", "json")
    selected_engine = Measure-UncleanCommand @(
        "plugins",
        "--engine-path",
        (Resolve-Path -LiteralPath $EnginePath).Path,
        "--format",
        "json"
    )
} | ConvertTo-Json -Depth 4
