<#
.SYNOPSIS
    Decompiles native binaries with Ghidra (headless) to readable C.

.DESCRIPTION
    For each binary: imports it into a Ghidra project, runs the full
    analysis, and writes the decompiled C of every function to
    <OutDir>\<name>.c. The project is kept, so it can be opened in the
    Ghidra GUI afterwards with the analysis already done.

.EXAMPLE
    .\scripts\ghidra\decompile.ps1 research\msi\bin\BYProtocol_x64.dll
#>
param(
    [Parameter(Mandatory, Position = 0)]
    [string[]]$Binaries,
    [string]$GhidraDir = (Get-ChildItem "$env:USERPROFILE\Downloads\ghidra_*\ghidra_*_PUBLIC" -Directory | Select-Object -Last 1).FullName,
    [string]$JavaHome = (Get-ChildItem "C:\Program Files\Eclipse Adoptium\jdk-*" -Directory | Select-Object -Last 1).FullName,
    [string]$ProjectDir = (Join-Path $PSScriptRoot "..\..\research\msi\ghidra"),
    [string]$OutDir = (Join-Path $PSScriptRoot "..\..\research\msi\decompiled")
)
$ErrorActionPreference = "Stop"
if (-not $GhidraDir) { throw "Ghidra not found; pass -GhidraDir" }
if (-not $JavaHome) { throw "JDK not found; pass -JavaHome" }
$env:JAVA_HOME = $JavaHome
New-Item -ItemType Directory -Force $ProjectDir, $OutDir | Out-Null
$headless = Join-Path $GhidraDir "support\analyzeHeadless.bat"
# Ghidra prints Java warnings on stderr; Windows PowerShell would treat those as errors.
$ErrorActionPreference = "Continue"

foreach ($binary in $Binaries) {
    $path = (Resolve-Path $binary).Path
    $name = [IO.Path]::GetFileNameWithoutExtension($path)
    $out = Join-Path (Resolve-Path $OutDir) "$name.c"
    Write-Host "== $name"
    & $headless (Resolve-Path $ProjectDir) "msi" -import $path -overwrite `
        -scriptPath $PSScriptRoot -postScript ExportDecompiled.java $out 2>&1 |
        Select-String -Pattern "Exported|ERROR|Import succeeded|REPORT" | ForEach-Object { "   $_" }
}
