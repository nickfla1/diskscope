# DiskScope glossary

Terms the code and docs use. Add to this as modules are deepened.

**Risk** - how safe it is to delete something: *Safe* (regenerated automatically), *Review* (probably removable, look first), *Caution* (system-managed; advice only, never deletable from the app).

**Suggestion** - a node the cleanup rules flagged as deletable, with a Risk and a reason. Suggestions never nest: a flagged folder hides everything inside it.

**Deletion** - the module that removes things from disk. A **Plan** is built from a selection (it refuses the scan root, Caution items, unknown and already-removed nodes, and drops items nested inside another selected item); `run` executes the plan one item at a time, updating the tree as each item goes and continuing after failures.

**Remover** - the seam under Deletion: one call removes one path. Adapters: *SystemTrash* (Recycle Bin on Windows, Trash on macOS), *Permanent*, and an in-memory fake in tests. "Permanent" means skipping the trash.

**Scanner** - walks a root and builds the **Tree**: one task per directory, results streamed to a single collector. The Tree is an arena in which a directory's children have contiguous ids and every child id is greater than its parent's id; all aggregation (sizes, file counts, dominant type, application, risk) is one linear pass over that order.

**DirReader** - the seam under the Scanner: `read(dir, out) -> bool` lists one directory into a `DirBuilder` and returns `false` if the directory cannot be read (the Scanner then flags it *denied* and carries on). Symlinks and junctions are never reported. Adapters: *Win32Reader* (bulk `GetFileInformationByHandleEx`, Windows), *StdReader* (`std::fs`), fakes in tests; a raw-MFT reader would be another.

**Size on disk** - allocated size as opposed to logical length: Win32Reader reports allocation size, and StdReader on Unix reports `st_blocks`. Shared APFS extents and hard links mean this is not necessarily what deleting actually frees. **Cloud placeholder** - an online-only file (OneDrive, iCloud etc.); flagged, and counted as 0 bytes because its data is not on disk.

**Uninstall leftover** - a Suggestion (always graded Review) for something a removed program left behind: a `Program Files` folder that is no registered program's install location and contains no executables or libraries, or the data folder of a Store (MSIX) package that Windows no longer lists as installed. Only the current user's `%LOCALAPPDATA%\Packages` is judged, and only folders shaped like a package family (`Name_<13 chars>`); shared tooling folders (`Microsoft*`, `Windows*`, `MSBuild`, `NuGet`, ...) are never judged. The Cleanup tab can show all suggestions, only leftovers, or hide them.

**Platform** - the seam for everything that differs per operating system: default scan root, the DirReader, the list of installed programs, application roots, OS-specific cleanup rules, uninstall leftovers, protected paths, the trash's name and wording, and "show in file manager". Adapters: *Windows*, *MacOs*, and a portable *Generic*. A release build contains only the adapter for its own OS; tests compile Windows and macOS together so their rules run anywhere. Rules never see raw paths: they get **normalised paths** (lower-case, `/` separators, `Tree::path_norm`).

**Installed** - what the OS reports as installed right now (registry entries on Windows, `.app` bundles on macOS), resolved to folders in the scanned tree. Used both to name applications and to tell live installs from leftovers.

**Session** - owns the tree, its analysis, the epoch, every background job (scan, analysis, render, duplicate search, deletion) with its progress, and the derived lists. It knows nothing about egui: the GUI and the CLI are two adapters on it. Commands go in; **Changes** (`ScanFinished`, `AnalysisReady`, `Removed`, `DupsReady`, `DeleteFinished`, `RenderReady`) come out through `poll` (non-blocking) or `wait` (headless).

**Epoch** - a counter that increases whenever the tree or its analysis changes (scan finished, analysis ready, an item removed). Cached views and renders are keyed by it; an analysis computed for an older epoch is discarded and recomputed, so a stale result is never shown.

**Snapshot** - the read guard on the tree plus its analysis, taken with `Data::read` for the length of a frame. Never held across a call that blocks or polls the session.

**Views** - the cached derived lists (clusters, cleanup list, duplicate rows). A new epoch makes them stale; a changed filter recomputes only its own list; removed nodes are filtered out inside, not by callers.
