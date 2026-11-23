param([switch]$RunFocusedTests)
$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent $PSScriptRoot
$target = Join-Path $repo 'target'
Push-Location $repo
try {
    & cargo build -p xcl --release --locked --target-dir $target
    if ($LASTEXITCODE -ne 0) { throw "Build failed ($LASTEXITCODE)." }
    if ($RunFocusedTests) {
        # Each filter runs separately: some tests share process-global GPU state.
        $filters = @(
            'gpu_texture_source_reads_late_cpu_writes_without_changing_title_address',
            'mapped_gpu_view_replaces_a_previously_registered_cpu_source',
            'gpu_memory_copy_from_cpu_view_observes_writes_after_recording',
            'remap_waits_for_prior_reads_and_precedes_later_reads',
            'gpu_streamed_textures_start_zero_then_preserve_data_and_zero_updates',
            'clear_and_capture_a_uav_state_target_preserves_pixels_and_state',
            'explicit_present_plane_uses_the_titles_second_resource_array',
            'two_pass_tests::'
        )
        $listed = @(& cargo test -p xcl --lib --release --locked --target-dir $target -- --list)
        if ($LASTEXITCODE -ne 0) { throw "Test discovery failed ($LASTEXITCODE)." }
        foreach ($filter in $filters) {
            if (-not ($listed | Where-Object { $_.Contains($filter) -and $_.EndsWith(': test') })) {
                throw "Required test filter '$filter' matches no tests. Use the loading experiment checkout."
            }
            & cargo test -p xcl --lib --release --locked --target-dir $target $filter -- --nocapture --test-threads=1
            if ($LASTEXITCODE -ne 0) { throw "Test filter '$filter' failed ($LASTEXITCODE)." }
        }
    }
    Write-Output "Built: $(Join-Path $target 'release\xcl.exe')"
} finally {
    Pop-Location
}
