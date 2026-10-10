# DiskScope

A fast disk space analyzer for **Windows and macOS**, in the spirit of WinDirStat, written in Rust.

Scan a drive or folder, see where the space went as a cushion-shaded treemap, find which applications own it, spot the large clusters, and clean up what is safe to delete.

## Features

- **Fast, parallel scan.** One async task per directory, blocking reads spread over an I/O thread pool, results streamed to a single collector. On Windows it reads directories in bulk (`GetFileInformationByHandleEx`); on macOS it uses `std::fs` with allocated-size (`st_blocks`) reporting. Both report *size on disk* and skip symlinks.
- **Treemap.** Squarified layout with cushion shading, rendered once on a background thread into a texture, with an id buffer so hover and click are O(1) however many files are on screen. Color by file type, by application, or by cleanup risk.
- **Applications.** Folders are attributed to applications using the OS's own list of installed programs plus well-known application folders (Windows: registry, Program Files, ProgramData, AppData; macOS: `/Applications` bundles, `~/Library`, Homebrew), and game libraries (Steam, Epic).
- **Large clusters.** The non-overlapping set of folders (or loose-file groups) where space actually concentrates, with an adjustable threshold.
- **Cleanup suggestions.** Rule-based, graded **Safe / Review / Caution**: build output (`target`, `node_modules`, `bin`/`obj`), caches, temp files, crash dumps, old installers, large untouched files, plus OS-specific rules (Windows: Temp, Windows Update cache, `Windows.old`, Recycle Bin; macOS: `~/Library/Caches` and `Logs`, Xcode DerivedData and device support, iOS simulators, iPhone backups, Trash, Docker image). **Uninstall leftovers** on Windows: Program Files folders with no program in them and data of Store apps that are no longer installed. Filter by risk, size, file type and leftovers.
- **Duplicates.** Size, then a 64 KiB prefix hash, then a full BLAKE3 hash, all in parallel.
- **Types, large files, folder tree**, breadcrumb zoom, and a context menu (show in file manager, copy path, delete).

## Download

Prebuilt builds are attached to each [release](../../releases):

| Platform | File | Notes |
| --- | --- | --- |
| Windows 10/11 (x64) | `diskscope-<version>-windows-x86_64.exe` (or the `.zip`) | Single self-contained executable, no installer, no admin rights needed. |
| macOS 11+ (Apple Silicon and Intel) | `diskscope-<version>-macos-universal.zip` | A universal `DiskScope.app`. See below. |

### macOS first launch

The app is **ad-hoc signed, not notarized** (that needs a paid Apple Developer account), so Gatekeeper blocks the first launch: right-click `DiskScope.app` → **Open** → **Open**, or run `xattr -dr com.apple.quarantine DiskScope.app` once.

To scan protected folders (Mail, Messages, Photos, other users' data) grant the app **Full Disk Access** in *System Settings → Privacy & Security*. Without it those folders are listed as unreadable, and everything else still works.

## Build from source

Requires a recent stable Rust toolchain.

```bash
cargo run --release          # GUI
cargo test --release         # run the tests
```

### Command line

```bash
diskscope --cli C:\             # headless scan + report (types, apps, clusters, cleanup)
diskscope --cli ~               # same on macOS, scanning the home folder
diskscope --cli C:\ --logical   # logical file length instead of size on disk
```

Environment variable `DISKSCOPE_IO_THREADS` overrides the size of the I/O thread pool.

## Deleting files

Nothing is ever deleted without a confirmation dialog.

- Deletion goes to the **Recycle Bin** (Windows) or **Trash** (macOS) by default. Windows may delete items that are too large for the bin permanently.
- An opt-in checkbox deletes **permanently**, with a warning. Permanent deletion cannot be undone.
- Deletion refuses the scan root and **Caution** (system-managed) items, and drops items nested inside another selected item.
- Items are removed one at a time, the views update as they go, a progress bar with a Stop button shows in the status bar, and a failure on one item does not stop the rest.

### Xcode simulators

On macOS, **Cleanup → Manage simulator runtimes** lists Xcode's installed runtime images and their sizes, even when scanning only your home folder. Select **Remove…**, then confirm permanent removal. DiskScope runs `xcrun simctl runtime delete <UUID>` in the background and displays failures from Apple's tool. This operation skips Trash and may shut down simulators using the runtime. Refresh the list or rescan afterward. System-wide CoreSimulator caches are Caution entries rather than ordinary deletion suggestions.

Cleanup lists individual simulator device folders under `~/Library/Developer/CoreSimulator/Devices`, rather than suggesting deletion of all CoreSimulator data. Devices are **Caution** entries: use `xcrun simctl list devices` to identify a device, then `xcrun simctl delete <UUID>` to permanently remove it and its data. Remove simulator runtime versions through **Xcode → Settings → Components**. Simulator caches are listed separately. Rescan after removing a device or runtime to update sizes.

## Architecture

The program is split along seams so each part can be tested without the others (and without a window).

| Module | Role |
| --- | --- |
| `model` | Compact arena tree: 40-byte nodes, names in a byte arena, a directory's children are a contiguous id range and every child id is greater than its parent's, so each aggregation is one linear pass. |
| `platform` | **Everything OS-specific behind one `Platform` trait**: default scan root, directory reader, installed-program list, application roots, OS cleanup rules, uninstall leftovers, protected paths, Trash wording, show-in-file-manager. `windows`, `macos` and a portable `generic` adapter; a release build contains only its own OS (`cfg(target_os)`), tests compile both so the rules are tested on any machine. |
| `scan`, `dirreader` | Async scanner and the `DirReader` seam beneath it (Win32 bulk reader, `std::fs` reader, in-memory fakes in tests). |
| `analysis`, `apps`, `cleanup`, `dupes` | Aggregation, application attribution, cleanup rule engine, duplicate detection: platform-neutral engines that ask the `Platform`. |
| `deletion` | Plans and executes deletions behind a `Remover` seam (system trash, permanent, in-memory fake). |
| `session`, `views` | Owns the tree, analysis, epoch, every background job and the derived lists (clusters, cleanup list, duplicate rows). Knows nothing about egui: **the GUI and the CLI are two adapters on it**, and its tests run headless. |
| `treemap` | Layout and rasterization. |
| `app`, `cli` | The egui front-end (painting and UI state only) and the headless report. |

Domain terms are defined in [`CONTEXT.md`](CONTEXT.md).

### Developing for macOS from Windows or Linux

You cannot run the macOS build elsewhere, but you can check that it compiles, tests included:

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin
# blake3 compiles a little C; a stub compiler is enough for `check` (nothing is linked)
CC_aarch64_apple_darwin=true AR_aarch64_apple_darwin=true \
  cargo check --target aarch64-apple-darwin --all-targets
```

The macOS cleanup and application rules also run as ordinary unit tests on any OS (`src/tests_platform.rs`), against synthetic folder trees laid out like a Mac.

## Performance

Measured on a 3.7 million file, ~870 GiB NTFS drive: about **21 s** for a warm scan (about 90 s the first time, when the disk cache is cold), ~280 MiB of memory for 4.5 million entries, and a treemap render of roughly 70 ms. Treemap and analysis time scale with the number of items, not with the size of the window.

On a machine with antivirus or EDR, most scan time goes to opening directories through file-system filter drivers, so adding threads stops helping beyond about 8. Reading the NTFS MFT directly would avoid that but needs administrator rights, and it is not implemented. Nothing has been measured on macOS yet.

## Limitations

- The macOS build has been compiled for both architectures and its rules are unit-tested, but it has not been run on a Mac by the author. Treat it as a first release and report problems.
- macOS: no uninstall-leftover detection yet (there is no reliable installed-app list to compare `~/Library` folders against), no iCloud-aware sizes beyond skipping "dataless" placeholders, and scanning `/` skips other volumes and the firmlinked Data volume to avoid counting files twice.
- Hard links are counted once per link. Symlinks and junctions are skipped.
- Names are stored as lossy UTF-8.
- Application attribution is heuristic and will sometimes show odd entries.

## License

[MIT](LICENSE)

## Releasing

The **Build and release** workflow is manual: *Actions → Build and release → Run workflow*, enter a tag such as `v0.1.0`. It runs the tests on Windows and macOS, builds `diskscope.exe` and a universal `DiskScope.app`, and uploads them as workflow artifacts. With *publish* ticked it also creates the GitHub Release and tag with both builds and SHA-256 checksums.
