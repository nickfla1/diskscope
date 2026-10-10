<#
  Builds a fictional Windows disk for screenshots: no personal paths, invented applications, and
  *sparse* files, so ~190 GB of "data" costs almost no real disk space.

    pwsh docs/demo/make-demo-disk.ps1                 # creates C:\demo
    diskscope --shot out.png C:\demo folders --logical

  Use --logical (or untick "Size on disk"): sparse files occupy ~0 bytes on disk but have their full length.
  Delete the folder when you are done:  Remove-Item -Recurse -Force C:\demo
#>
param([string]$Root = 'C:\demo')

Add-Type @"
using System;
using System.IO;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;
public static class Sparse {
  [DllImport("kernel32.dll", SetLastError = true)]
  static extern bool DeviceIoControl(SafeFileHandle h, uint code, IntPtr i, int il, IntPtr o, int ol, out int ret, IntPtr ov);
  public static void Create(string path, long size) {
    using (var fs = new FileStream(path, FileMode.Create, FileAccess.ReadWrite)) {
      int r;
      DeviceIoControl(fs.SafeFileHandle, 0x900C4, IntPtr.Zero, 0, IntPtr.Zero, 0, out r, IntPtr.Zero); // FSCTL_SET_SPARSE
      fs.SetLength(size);
    }
  }
}
"@

if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
New-Item -ItemType Directory -Force $Root | Out-Null
$rnd = [Random]::new(2024)

# One file of $mb megabytes, optionally back-dated by $days.
function F([string]$rel, [double]$mb, [int]$days = 0) {
  $p = Join-Path $Root $rel
  $d = Split-Path $p
  if (-not (Test-Path $d)) { New-Item -ItemType Directory -Force $d | Out-Null }
  [Sparse]::Create($p, [long]($mb * 1MB))
  if ($days -gt 0) { (Get-Item -LiteralPath $p -Force).LastWriteTime = (Get-Date).AddDays(-$days) }
}
# $count files named by $pattern ({0} = index), sizes spread between $min and $max MB.
function Many([string]$dir, [string]$pattern, [int]$count, [double]$min, [double]$max, [int]$days = 0) {
  for ($i = 1; $i -le $count; $i++) {
    F "$dir/$($pattern -f $i)" ($min + $rnd.NextDouble() * ($max - $min)) $days
  }
}

$u = 'Users/alex'

# ---- user data -------------------------------------------------------------------------------------
Many "$u/Videos" 'trip-{0:00}.mp4' 14 400 1800
F "$u/Videos/holiday-2023.mp4" 2400
F "$u/Videos/wedding-raw.mkv" 5200
Many "$u/Videos/Screen recordings" 'rec-{0:000}.mp4' 20 80 600
Many "$u/Pictures/2023" 'IMG_{0:0000}.jpg' 140 3 9
Many "$u/Pictures/2022" 'DSC_{0:0000}.NEF' 40 25 45
Many "$u/Pictures/Wallpapers" 'wall-{0:00}.png' 25 4 14
Many "$u/Music/Albums" 'track-{0:000}.flac' 90 20 45
Many "$u/Music/Albums" 'single-{0:000}.mp3' 80 4 9
Many "$u/Documents/Reports" 'report-{0:000}.pdf' 60 0.3 6
Many "$u/Documents/Reports" 'notes-{0:000}.docx' 40 0.05 1.2
Many "$u/Documents/Finance" 'sheet-{0:000}.xlsx' 25 0.05 2
F "$u/Documents/Archive/backup-2021.zip" 3100
F "$u/Documents/Archive/backup-2021 (copy).zip" 3100
# old installers and archives in Downloads
F "$u/Downloads/photon-studio-setup.exe" 780 200
F "$u/Downloads/ubuntu-24.04-desktop-amd64.iso" 5800 150
F "$u/Downloads/dataset-v2.zip" 2200 120
F "$u/Downloads/holiday-2023 (1).mp4" 2400
F "$u/Downloads/nova3d-installer.msi" 410 300

# ---- projects (build output and dependencies) -------------------------------------------------------
F "$u/Projects/web-app/package.json" 0.002
Many "$u/Projects/web-app/src" 'component-{0:00}.js' 30 0.01 0.2
Many "$u/Projects/web-app/node_modules" 'pkg-{0:000}/index.js' 300 0.05 3
F "$u/Projects/web-app/node_modules/typescript/lib/tsc.js" 62
F "$u/Projects/web-app/node_modules/esbuild/bin/esbuild.exe" 9.5
F "$u/Projects/rust-tool/Cargo.toml" 0.001
Many "$u/Projects/rust-tool/src" 'module{0:00}.rs' 20 0.005 0.08
Many "$u/Projects/rust-tool/target/debug/deps" 'libdep{0:000}.rlib' 60 3 40
Many "$u/Projects/rust-tool/target/debug/deps" 'tool-{0:0}.exe' 4 30 80
Many "$u/Projects/rust-tool/target/debug/deps" 'tool-{0:0}.pdb' 4 40 120
Many "$u/Projects/rust-tool/target/release/deps" 'libdep{0:000}.rlib' 60 2 25
Many "$u/.cargo/registry/cache/index" 'crate-{0:000}.crate' 150 0.1 4
Many "$u/.rustup/toolchains/stable/lib/rustlib/x86_64-pc-windows-msvc/lib" 'libstd-{0:00}.rlib' 80 2 25
Many "$u/.rustup/toolchains/stable/bin" 'tool{0:0}.exe' 8 10 60
Many "$u/.cache/huggingface/hub/models--demo--llm/blobs" 'blob{0:0}' 6 700 3800

# ---- application data and caches -------------------------------------------------------------------
Many "$u/AppData/Local/Temp" 'tmp{0:0000}.tmp' 60 5 80
Many "$u/AppData/Local/Google/Chrome/User Data/Default/Cache/Cache_Data" 'f_{0:000000}' 200 0.2 9
Many "$u/AppData/Local/Google/Chrome/User Data/Default/Code Cache/js" 'c_{0:000}' 40 0.1 3
F "$u/AppData/Local/Docker/wsl/disk/docker_data.vhdx" 34000
F "$u/AppData/Local/Packages/CanonicalGroupLimited.Ubuntu_79rhkp1fndgsc/LocalState/ext4.vhdx" 22000
F "$u/AppData/Local/Programs/orbit-editor/orbit.exe" 150
Many "$u/AppData/Local/Programs/orbit-editor" 'lib{0:00}.dll' 40 1 25
F "$u/AppData/Local/Programs/orbit-editor/resources/app.pak" 120
Many "$u/AppData/Roaming/Chatly/Cache" 'f_{0:000}' 80 1 12
Many "$u/AppData/Roaming/Orbit Editor/User" 'workspace-{0:00}.db' 15 5 90

# ---- installed programs ----------------------------------------------------------------------------
F 'Program Files/Photon Studio/photon.exe' 220
Many 'Program Files/Photon Studio/plugins' 'plug-{0:00}.dll' 60 1 30
Many 'Program Files/Photon Studio/resources' 'assets-{0:00}.pak' 12 80 420
F 'Program Files/Nova 3D/nova.exe' 140
Many 'Program Files/Nova 3D/lib' 'nova{0:00}.dll' 80 1 40
Many 'Program Files/Nova 3D/datafiles' 'scene-{0:0}.blend' 8 40 300
# a program that was uninstalled but left only data behind
Many 'Program Files/Old Racing Game/saves' 'slot{0:00}.sav' 12 0.2 5
F 'Program Files/Old Racing Game/movies/intro.bik' 380
F 'Program Files/Old Racing Game/data/tracks.pak' 1400

# Steam library
$g = 'Program Files (x86)/Steam/steamapps/common'
F "$g/Deep Core/Binaries/Win64/DeepCore.exe" 300
Many "$g/Deep Core/Content/Paks" 'pakchunk{0}-WindowsNoEditor.pak' 10 1500 5500
Many "$g/Mountain Racer/data" 'car{0:0}.pak' 8 400 2200
F "$g/Pixel Quest/game.pak" 900
Many "$g/Pixel Quest/audio" 'music{0:00}.ogg' 30 3 25
F 'Program Files (x86)/Steam/steamapps/downloading/880100/chunk.partial' 6200

# ---- Windows itself --------------------------------------------------------------------------------
Many 'Windows/System32' 'sys{0:000}.dll' 250 0.1 30
Many 'Windows/System32/DriverStore/FileRepository' 'driver{0:00}.sys' 40 5 300
Many 'Windows/WinSxS' 'component{0:000}.dll' 400 0.05 40
Many 'Windows/SoftwareDistribution/Download' 'update{0:00}.cab' 12 20 380
Many 'Windows/Temp' 'etl{0:00}.tmp' 15 1 15
Many 'Windows/Installer' 'package{0:00}.msi' 40 5 150
Many 'Windows/Logs/CBS' 'CBS-{0:0}.log' 8 5 40
F 'pagefile.sys' 6144
F 'hiberfil.sys' 12800
F 'swapfile.sys' 256
Many 'ProgramData/Package Cache/{demo-guid}' 'setup{0:00}.exe' 30 2 90
F 'Users/Public/placeholder.txt' 0.001
Many '$Recycle.Bin/S-1-5-21-DEMO' '$R{0:0000}.dat' 25 20 200
F '$Recycle.Bin/S-1-5-21-DEMO/$RA1B2C3.zip' 1800

$n = (Get-ChildItem -Recurse -File -Force $Root).Count
Write-Host "Created $n files under $Root"
