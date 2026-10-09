# DiskScope

A fast disk space analyzer for Windows, in the spirit of WinDirStat, written in Rust.

Scan a drive or folder, see where the space went as a cushion-shaded treemap, find which applications own it, spot the large clusters, and clean up what is safe to delete.

## Features

- **Fast, parallel scan.** One async task per directory, blocking reads spread over an I/O thread pool, results streamed to a single collector. On Windows it reads directories in bulk (`GetFileInformationByHandleEx`) and reports *size on disk*.
- **Treemap.** Squarified layout with cushion shading, rendered once on a background thread into a texture, with an id buffer so hover and click are O(1) however many files are on screen. Color by file type, by application, or by cleanup risk.
- **Applications.** Folders are attributed to applications through the installed-programs registry, Program Files, ProgramData, AppData, Steam/Epic game libraries and user folders.
- **Large clusters.** The non-overlapping set of folders (or loose-file groups) where space actually concentrates, with an adjustable threshold.
- **Cleanup suggestions.** Rule-based, graded **Safe / Review / Caution**: build output (`target`, `node_modules`, `bin`/`obj`), caches, temp files, crash dumps, old installers, large untouched files, and **uninstall leftovers** (Program Files folders with no program in them, data of Store apps that are no longer installed). Filter by risk, size, file type and leftovers.
- **Duplicates.** Size, then a 64 KiB prefix hash, then a full BLAKE3 hash, all in parallel.
- **Types, large files, folder tree**, breadcrumb zoom, and a context menu (show in Explorer, copy path, delete).

## Download

Prebuilt `diskscope.exe` builds are attached to each [release](../../releases). It is a single self-contained executable; no installer and no admin rights needed.

## Build from source

Requires a recent stable Rust toolchain (MSVC target on Windows).

```bash
cargo run --release          # GUI
cargo test --release         # run the tests
```

### Command line

```bash
diskscope --cli C:\          # headless scan + report (types, apps, clusters, cleanup)
diskscope --cli C:\ --logical   # logical file length instead of size on disk
```

Environment variable `DISKSCOPE_IO_THREADS` overrides the size of the I/O thread pool.

## Deleting files

Nothing is ever deleted without a confirmation dialog.

- Deletion goes to the **Recycle Bin** by default. Windows may delete items that are too large for the bin permanently.
- An opt-in checkbox deletes **permanently**, with a warning. Permanent deletion cannot be undone.
- Deletion refuses the scan root and **Caution** (system-managed) items, and drops items nested inside another selected item.
- Items are removed one at a time, the views update as they go, a progress bar with a Stop button shows in the status bar, and a failure on one item does not stop the rest.

## How it works

| Module | Role |
| --- | --- |
| `model` | Compact arena tree: 40-byte nodes, names in a byte arena, a directory's children are a contiguous id range and every child id is greater than its parent's, so each aggregation is one linear pass. |
| `scan`, `dirreader` | Async scanner and the `DirReader` seam beneath it (Win32 bulk reader, `std::fs` reader, in-memory fakes in tests). |
| `analysis`, `apps`, `cleanup`, `dupes` | Aggregation, application attribution, cleanup rules, duplicate detection. |
| `deletion` | Plans and executes deletions behind a `Remover` seam (Recycle Bin, permanent, in-memory fake). |
| `treemap` | Layout and rasterization. |
| `app` | The egui front-end. |

Domain terms are defined in [`CONTEXT.md`](CONTEXT.md).

## Performance

Measured on a 3.7 million file, ~870 GiB NTFS drive: about **21 s** for a warm scan (about 90 s the first time, when the disk cache is cold), ~280 MiB of memory for 4.5 million entries, and a treemap render of roughly 70 ms. Treemap and analysis time scale with the number of items, not with the size of the window.

On a machine with antivirus or EDR, most scan time goes to opening directories through file-system filter drivers, so adding threads stops helping beyond about 8. Reading the NTFS MFT directly would avoid that but needs administrator rights, and it is not implemented.

## Limitations

- Windows 10/11 is the supported platform. A portable `std::fs` reader is included but has not been run on other systems.
- Hard links are counted once per link. Symlinks and junctions are skipped.
- Names are stored as lossy UTF-8.
- Application attribution is heuristic and will sometimes show odd entries.

## License

[MIT](LICENSE)

## Releasing

The **Build and release** workflow is manual: *Actions → Build and release → Run workflow*, enter a tag such as `v0.1.0`. It runs the tests, builds `diskscope.exe`, and uploads the exe, a zip and SHA-256 checksums as a workflow artifact. With *publish* ticked it also creates the GitHub Release and tag.
