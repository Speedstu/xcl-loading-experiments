param(
    [Parameter(Mandatory=$true)][string]$GameDir,
    [string]$Executable = 'game_scarlett_master.exe',
    [string]$StateRoot = (Join-Path $env:LOCALAPPDATA 'XCL-LoadingTrial'),
    [ValidateRange(60,3600)][int]$DurationSeconds = 600,
    [ValidateRange(0,15)][int]$PresentPlane = 1,
    [switch]$DiagnosticRenderTarget,
    [switch]$GraphicsDebug,
    [switch]$PrepareOnly
)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$runtime = Join-Path $repo 'target\release\xcl.exe'
$game = [IO.Path]::GetFullPath((Resolve-Path -LiteralPath $GameDir -ErrorAction Stop).ProviderPath)
if (-not (Test-Path -LiteralPath $game -PathType Container)) { throw 'GameDir must be a directory.' }
$gamePrefix = $game.TrimEnd('\') + '\'
$exe = [IO.Path]::GetFullPath((Join-Path $game $Executable))
if (-not $exe.StartsWith($gamePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Executable must be inside GameDir.'
}
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "Missing executable: $exe" }
$state = [IO.Path]::GetFullPath($StateRoot)
if ($state.TrimEnd('\').Equals($game.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase) -or
    $state.StartsWith($gamePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'StateRoot must be outside the game directory.'
}

# Only read the manifest; never rewrite or copy the user's game files.
$manifestPath = Join-Path $game 'MicrosoftGame.config'
$settings = New-Object System.Xml.XmlReaderSettings
$settings.DtdProcessing = [Xml.DtdProcessing]::Prohibit
$settings.XmlResolver = $null
$reader = [Xml.XmlReader]::Create($manifestPath, $settings)
try {
    $manifest = New-Object System.Xml.XmlDocument
    $manifest.XmlResolver = $null
    $manifest.Load($reader)
} finally { $reader.Dispose() }
$identityNode = $manifest.SelectSingleNode("//*[local-name()='Identity']")
if (-not $identityNode) { throw 'MicrosoftGame.config has no Identity element.' }
$identity = $identityNode.GetAttribute('Name')
if (-not $identity) { throw 'MicrosoftGame.config has no Identity Name.' }
$title = [regex]::Replace($identity, '[^\p{L}\p{N}._-]', '_')
$data = Join-Path $state $title
$config = Join-Path $data 'xcl.toml'
if (Test-Path -LiteralPath $config) {
    throw "A config already exists at $config. Use another StateRoot to keep that trial intact."
}
New-Item -ItemType Directory -Path $data -Force | Out-Null
$relativeExe = $exe.Substring($gamePrefix.Length).Replace('\','/')
$exeJson = ConvertTo-Json -InputObject $relativeExe -Compress
$presentMode = if ($DiagnosticRenderTarget) { 'render-target' } else { 'auto' }
$toml = @"
# Experimental XCL loading trial. No verified gameplay.
device_type = "lockhart"
console_type_raw = 6
title_memory_mb = 6144
graphics = "d3d12"
present = "$presentMode"
window_width = 1280
window_height = 720
xbox_live_tokens = false
keyboard_mouse = true
exe = $exeJson

[mounts]
"D:" = "{game}"
"G:" = "{game}"
"GAME:" = "{game}"
"S:" = '{data}\pls'
"T:" = '{data}\temp'
"@
[IO.File]::WriteAllText($config, $toml.Replace("`r`n","`n") + "`n", (New-Object Text.UTF8Encoding($false)))
Write-Output "Prepared: $config"
Write-Output 'Experimental trial: startup/loading only, no confirmed gameplay.'
if ($DiagnosticRenderTarget) { Write-Output 'Diagnostic render target selected: this is not proof of correct final presentation.' }
if ($PrepareOnly) { return }
if (-not (Test-Path -LiteralPath $runtime -PathType Leaf)) { throw 'Build first with scripts\Build-LoadingTrial.ps1.' }

$trialEnv = @{
    XCL_DATA_ROOT=$state; XCL_NETWORK='0'; XCL_DEFERRED_PSO='1'; XCL_VRAM_TILED='1'
    XCL_SPARSE_TEXTURES='1'; XCL_COPY_CPU_VIEWS='1'; XCL_DXC_THREADS='1'; XCL_ASYNC_PSO='1'
    XCL_LAZY_PSO='1'; XCL_BUFFERED_READS='1'; XCL_MIN_FREE_COMMIT_MB='2048'; XCL_ACTIVITY='1'
    XCL_TRACE_STORAGE='1'; XCL_TRACE_FRAME='1'; XCL_RUN_SECONDS=[string]$DurationSeconds
    XCL_PRESENT_PLANE=$(if ($DiagnosticRenderTarget) { $null } else { [string]$PresentPlane })
    XCL_D3D12_DEBUG=$(if ($GraphicsDebug) { '1' } else { $null })
    XCL_DRED=$null; XCL_THREAD_DUMP_AT=$null; XCL_FRAME_DUMP=$null; XCL_VRAM_WHOLE_WINDOW=$null
    XCL_RESOURCE_FAULT_PROBE=$null; XCL_WATCH_ADDRESS=$null; XCL_TRACE_TEXTURE_UPLOADS=$null
}
$saved = @{}
try {
    foreach ($name in $trialEnv.Keys) {
        $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $trialEnv[$name], 'Process')
    }
    & $runtime run $game
    $trialExit = $LASTEXITCODE
} finally {
    foreach ($name in $saved.Keys) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
Write-Output "Trial exited with code $trialExit. Local logs: $(Join-Path $data 'logs')"
exit $trialExit
