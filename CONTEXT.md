# DiskScope glossary

Terms the code and docs use. Add to this as modules are deepened.

**Risk** - how safe it is to delete something: *Safe* (regenerated automatically), *Review* (probably removable, look first), *Caution* (system-managed; advice only, never deletable from the app).

**Suggestion** - a node the cleanup rules flagged as deletable, with a Risk and a reason. Suggestions never nest: a flagged folder hides everything inside it.

**Deletion** - the module that removes things from disk. A **Plan** is built from a selection (it refuses the scan root, Caution items, unknown and already-removed nodes, and drops items nested inside another selected item); `run` executes the plan one item at a time, updating the tree as each item goes and continuing after failures.

**Remover** - the seam under Deletion: one call removes one path. Adapters: *RecycleBin*, *Permanent*, and an in-memory fake in tests. "Permanent" means skipping the Recycle Bin.

**Scanner** - walks a root and builds the **Tree**: one task per directory, results streamed to a single collector. The Tree is an arena in which a directory's children have contiguous ids and every child id is greater than its parent's id; all aggregation (sizes, file counts, dominant type, application, risk) is one linear pass over that order.

**DirReader** - the seam under the Scanner: `read(dir, out) -> bool` lists one directory into a `DirBuilder` and returns `false` if the directory cannot be read (the Scanner then flags it *denied* and carries on). Symlinks and junctions are never reported. Adapters: *Win32Reader* (bulk `GetFileInformationByHandleEx`, Windows), *StdReader* (`std::fs`), fakes in tests; a raw-MFT reader would be another.

**Size on disk** - allocated size (cluster-rounded, what deleting actually frees) as opposed to logical length. Only the Win32Reader can report it. **Cloud placeholder** - an online-only file (OneDrive etc.); flagged, and counted as 0 bytes because its data is not on disk.

**Uninstall leftover** - a Suggestion (always graded Review) for something a removed program left behind: a `Program Files` folder that is no registered program's install location and contains no executables or libraries, or the data folder of a Store (MSIX) package that Windows no longer lists as installed. Only the current user's `%LOCALAPPDATA%\Packages` is judged, and only folders shaped like a package family (`Name_<13 chars>`); shared tooling folders (`Microsoft*`, `Windows*`, `MSBuild`, `NuGet`, ...) are never judged. The Cleanup tab can show all suggestions, only leftovers, or hide them.
