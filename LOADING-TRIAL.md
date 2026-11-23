# Experimental loading trial

This branch contains the source changes used for Speedstu's local startup/loading experiments on 23–24 November
2026. It builds a modified XCL runtime. The title's intro and setup UI have appeared in separate runs; a
controllable gameplay scene has **not** been reached. Black/green output, loading stalls and crashes remain.

This is a separate GPL-2.0-only XCL experiment, not Leonardo's runtime. Do not copy these sources into a project
with an incompatible licence. The observations can be investigated and implemented independently there.

## Build and test

Use Windows x64, Rust's stable MSVC toolchain, Visual Studio C++ Build Tools and the Windows SDK with DXC.
The GPU must support D3D12, Shader Model 6.6 and mesh shaders. See the upstream [build requirements](README.md#building-and-usage).

From this checkout in PowerShell:

```powershell
.\scripts\Build-LoadingTrial.ps1 -RunFocusedTests
```

The script builds with `Cargo.lock` and runs the focused tests in separate processes. Check their output:
a GPU-dependent test may report `SKIPPED` on unsupported hardware. That is not a GPU pass. These are small
regressions, not a validation of the full game. The complete upstream test suite has not been rerun for this snapshot.

## Try with your own files

The package must already be readable on disk and contain `MicrosoftGame.config` and its executable.
No game files, caches, shaders, keys or executable patches are supplied. The script does not download, decrypt
or modify game files, adjust the Windows pagefile, or contact game services.

```powershell
.\scripts\Start-LoadingTrial.ps1 -GameDir 'E:\MyGame' -StateRoot 'F:\Trials\attempt-01'
```

Use a fresh `StateRoot` for each run; the helper refuses to overwrite an existing title config. Saves, caches
and local logs go there, outside the game directory. The default test lasts 600 seconds; use `-DurationSeconds
1800` for a longer trial. A native crash or the commit-memory guard can end it earlier. The guard keeps a
2,048 MiB reserve of available system commit; the script does not disable it. A local profile prompt may need
Return or the controller's A button. There is no automated input through title menus.

The default shows the title's explicit PresentX plane 1. To investigate intermediate output instead:

```powershell
.\scripts\Start-LoadingTrial.ps1 -GameDir 'E:\MyGame' -StateRoot 'F:\Trials\diagnostic-01' -DiagnosticRenderTarget
```

An intermediate image does not prove correct final presentation. `-GraphicsDebug` enables D3D12 validation
when Windows Graphics Tools are installed. DRED and scheduled thread-stack dumps are left off. Raw logs and
dumps stay local: review and remove title data and personal paths before sharing a small technical summary.

**Reproduction limit:** the longer original trials used a local title copy with an extended startup watchdog
timeout. That proprietary executable change is not included. This checkout reproduces the runtime code and
focused tests; it does not promise the same full-title loading progress with an unmodified executable.

## Test PC and unresolved failures

- Ryzen 7 5700X, 8 cores / 16 threads; 16 GB RAM.
- Radeon RX 9060 XT, 16 GB VRAM; Windows driver `32.0.32029.2010`.
- Windows 11 Pro build 26300; 48 GiB initial / 64 GiB maximum pagefile during later tests.
- `lockhart` profile, 6,144 MiB title-memory setting, 1280 × 720 output window, offline local profile.

The queue-ordering regression passes, but the full title still reports D3D12 validation ID 1047. Some
DirectStorage BCPack requests remain unsupported. A later run failed inside the AMD driver during a texture
upload even though an isolated upload with matching format and dimensions passed. GPU address coverage and
final presentation remain incomplete. More texture objects or PresentX calls do not establish working gameplay.

## Snapshot and credits

The upstream base is `ivall/xcl` at `db5dc26cba45278948dca2549b15ae53b9435d89`. The runtime sources in this
branch use the runtime logic from the archived patch for `run54-native-order-release-trial`, rather than the later
moving work directory, with dated modification notices added to changed source files. The old README/HANDOFF
edits were omitted because they described earlier results. New helper scripts
and this note were added for sharing; they were not part of the original runtime build.

Archived full-patch SHA-256: `D1FC7BB617E880A90B361180E4712D03EAE16DED118E47167B66ED8F8B222EA4`.
Original local runtime SHA-256: `DD9B66ABEB829BB87C24C9C64E3187C05B30F29CF3614B46C710D64C9819B6DC`.
A fresh build is not claimed to be byte-identical across toolchains and build paths.

XCL and its upstream contributors retain their credits and licences; see [THIRD_PARTY.md](THIRD_PARTY.md).
This is accumulated work: selected mainline changes including `956a476`, selected or adapted work from
ivall/xcl PRs [#146](https://github.com/ivall/xcl/pull/146), [#162](https://github.com/ivall/xcl/pull/162),
[#187](https://github.com/ivall/xcl/pull/187), [#192](https://github.com/ivall/xcl/pull/192),
[#195](https://github.com/ivall/xcl/pull/195), [#199](https://github.com/ivall/xcl/pull/199),
[#203](https://github.com/ivall/xcl/pull/203), [#210](https://github.com/ivall/xcl/pull/210),
[#211](https://github.com/ivall/xcl/pull/211), [#215](https://github.com/ivall/xcl/pull/215),
[#218](https://github.com/ivall/xcl/pull/218), [#219](https://github.com/ivall/xcl/pull/219),
[#252](https://github.com/ivall/xcl/pull/252), [#254](https://github.com/ivall/xcl/pull/254) and
[#283](https://github.com/ivall/xcl/pull/283), plus local fixes and regression tests. Some backports include
only part of the upstream change. This branch should not be read as a replacement for those upstream PRs.

The local diagnosis, changes and preparation of this branch were AI-assisted.
