[CmdletBinding()]
param(
    [string]$Version,
    [string]$OutputDirectory,
    [string]$CargoPath = "cargo",
    [string]$CargoAboutPath = "cargo-about",
    [switch]$SkipBuild,
    [switch]$AllowDirty
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $root "dist"
}
$outputRoot = [System.IO.Path]::GetFullPath($OutputDirectory)

function Resolve-RequiredCommand {
    param(
        [string]$Command,
        [string]$InstallAction
    )

    if ([System.IO.Path]::IsPathRooted($Command)) {
        if (Test-Path -LiteralPath $Command -PathType Leaf) {
            return (Resolve-Path -LiteralPath $Command).Path
        }
    } else {
        $resolved = Get-Command $Command -ErrorAction SilentlyContinue
        if ($null -ne $resolved) {
            return $resolved.Source
        }
    }

    throw "Required command was not found: $Command. $InstallAction"
}

function Invoke-Checked {
    param(
        [string]$Command,
        [string[]]$Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Command failed with exit code $LASTEXITCODE`: $Command $($Arguments -join ' ')"
    }
}

function Add-DeterministicZipEntry {
    param(
        [System.IO.Compression.ZipArchive]$Archive,
        [string]$SourcePath,
        [string]$EntryName
    )

    $entry = $Archive.CreateEntry($EntryName, [System.IO.Compression.CompressionLevel]::Optimal)
    $entry.LastWriteTime = [DateTimeOffset]::new(1980, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
    $source = [System.IO.File]::OpenRead($SourcePath)
    try {
        $destination = $entry.Open()
        try {
            $source.CopyTo($destination)
        } finally {
            $destination.Dispose()
        }
    } finally {
        $source.Dispose()
    }
}

$cargo = Resolve-RequiredCommand $CargoPath "Install the pinned Rust toolchain and retry."
$cargoAbout = Resolve-RequiredCommand $CargoAboutPath "Run cargo install --locked cargo-about --version 0.9.1 --features cli and retry."

$metadataText = & $cargo metadata --format-version 1 --no-deps --locked
if ($LASTEXITCODE -ne 0) {
    throw "Cargo metadata failed. Restore the locked dependency graph and retry."
}
$metadata = $metadataText | ConvertFrom-Json
$package = $metadata.packages | Where-Object { $_.name -eq "unclean-core" } | Select-Object -First 1
if ($null -eq $package) {
    throw "Workspace version was not found. Restore the unclean-core package and retry."
}
$workspaceVersion = [string]$package.version
if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = $workspaceVersion
}
if ($Version -ne $workspaceVersion) {
    throw "Release version $Version does not match workspace version $workspaceVersion. Update the workspace or use the recorded version."
}
if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$') {
    throw "Release version is invalid: $Version. Use a Cargo-compatible semantic version."
}

if (-not $SkipBuild) {
    Invoke-Checked $cargo @("build", "--workspace", "--release", "--locked")
}

$binaryDirectory = Join-Path $root "target\release"
$binaryPaths = @(
    Join-Path $binaryDirectory "unclean.exe"
    Join-Path $binaryDirectory "unclean-gui.exe"
)
foreach ($binaryPath in $binaryPaths) {
    if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
        throw "Release binary was not found: $binaryPath. Build the release workspace and retry."
    }
}

$revision = (& git -C $root rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $revision -notmatch '^[0-9a-f]{40}$') {
    throw "Source revision was not resolved. Run packaging from a Git checkout and retry."
}
$sourceStatus = (& git -C $root status --porcelain --untracked-files=normal) -join "`n"
if ($LASTEXITCODE -ne 0) {
    throw "Source status was not resolved. Restore the Git checkout and retry."
}
$sourceDirty = -not [string]::IsNullOrWhiteSpace($sourceStatus)
if ($sourceDirty -and -not $AllowDirty) {
    throw "Source checkout has uncommitted files. Commit or remove them before release packaging."
}
$rustVersion = (& $cargo --version).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Cargo version was not resolved. Restore the pinned Rust toolchain and retry."
}

$packageName = "Unclean-$Version-windows-x86_64"
$stagingDirectory = Join-Path $outputRoot $packageName
$archivePath = Join-Path $outputRoot "$packageName.zip"
$checksumPath = Join-Path $outputRoot "$packageName.sha256"
foreach ($path in @($stagingDirectory, $archivePath, $checksumPath)) {
    if (Test-Path -LiteralPath $path) {
        throw "Release output already exists: $path. Remove that output or select an empty directory."
    }
}

New-Item -ItemType Directory -Path $stagingDirectory -Force | Out-Null
$packageFiles = @(
    @{ Source = $binaryPaths[0]; Destination = "unclean.exe" }
    @{ Source = $binaryPaths[1]; Destination = "unclean-gui.exe" }
    @{ Source = (Join-Path $root "README.md"); Destination = "README.md" }
    @{ Source = (Join-Path $root "SECURITY.md"); Destination = "SECURITY.md" }
    @{ Source = (Join-Path $root "PRIVACY.md"); Destination = "PRIVACY.md" }
    @{ Source = (Join-Path $root "LICENSE-APACHE"); Destination = "LICENSE-APACHE" }
    @{ Source = (Join-Path $root "LICENSE-MIT"); Destination = "LICENSE-MIT" }
    @{ Source = (Join-Path $root "docs\14-operator-guide.md"); Destination = "OPERATOR-GUIDE.md" }
    @{ Source = (Join-Path $root "docs\15-release-policy.md"); Destination = "RELEASE-POLICY.md" }
    @{ Source = (Join-Path $root "docs\16-code-signing-policy.md"); Destination = "CODE-SIGNING-POLICY.md" }
    @{ Source = (Join-Path $root "presets\review-first.toml"); Destination = "presets\review-first.toml" }
    @{ Source = (Join-Path $root "presets\project-first.toml"); Destination = "presets\project-first.toml" }
    @{ Source = (Join-Path $root "presets\windows-desktop-lean.toml"); Destination = "presets\windows-desktop-lean.toml" }
    @{ Source = (Join-Path $root "presets\README.md"); Destination = "presets\README.md" }
)
foreach ($file in $packageFiles) {
    $destination = Join-Path $stagingDirectory $file.Destination
    $destinationDirectory = Split-Path -Parent $destination
    New-Item -ItemType Directory -Path $destinationDirectory -Force | Out-Null
    Copy-Item -LiteralPath $file.Source -Destination $destination
}

$noticePath = Join-Path $stagingDirectory "THIRD-PARTY-NOTICES.txt"
Push-Location $root
$previousPath = $env:PATH
try {
    $env:PATH = "$([System.IO.Path]::GetDirectoryName($cargo));$previousPath"
    Invoke-Checked $cargoAbout @("generate", "--locked", "--config", "about.toml", "--output-file", $noticePath, "about.hbs")
} finally {
    $env:PATH = $previousPath
    Pop-Location
}

$manifest = [ordered]@{
    schema = 1
    product = "Unclean"
    version = $Version
    target = "x86_64-pc-windows-msvc"
    source_revision = $revision
    source_dirty = $sourceDirty
    cargo = $rustVersion
    executables = @("unclean.exe", "unclean-gui.exe")
}
$manifestJson = $manifest | ConvertTo-Json -Depth 4
[System.IO.File]::WriteAllText(
    (Join-Path $stagingDirectory "release-manifest.json"),
    $manifestJson,
    [System.Text.UTF8Encoding]::new($false)
)

Add-Type -AssemblyName System.IO.Compression
$archiveStream = [System.IO.File]::Open($archivePath, [System.IO.FileMode]::CreateNew)
try {
    $archive = [System.IO.Compression.ZipArchive]::new($archiveStream, [System.IO.Compression.ZipArchiveMode]::Create, $false)
    try {
        $files = Get-ChildItem -LiteralPath $stagingDirectory -File -Recurse | Sort-Object FullName
        foreach ($file in $files) {
            $relativePath = [System.IO.Path]::GetRelativePath($stagingDirectory, $file.FullName).Replace("\", "/")
            Add-DeterministicZipEntry $archive $file.FullName "$packageName/$relativePath"
        }
    } finally {
        $archive.Dispose()
    }
} finally {
    $archiveStream.Dispose()
}

$archiveHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
"$archiveHash *$([System.IO.Path]::GetFileName($archivePath))" | Set-Content -LiteralPath $checksumPath -Encoding ascii

Write-Output "Release package created: $archivePath"
Write-Output "SHA-256 checksum created: $checksumPath"
