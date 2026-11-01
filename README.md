<p align="center">
  <img src="assets/icon.png" alt="Space Manager icon" width="96" height="96">
</p>

<h1 align="center">Space Manager</h1>

<p align="center">A fast disk usage analyzer for Windows. Find out what is eating your drive, then clean it up.</p>

## What it does

Space Manager scans a folder or a whole drive and shows where the space went:

- **Folder tree** with the size of every folder and its share of its parent, sorted largest first.
- **Largest files** list (top 100) for quick wins.
- **Size on disk**, not just file length. Cluster rounding is included, and hardlinked files are counted once.
- **Fast.** Scanning runs in parallel. About 570,000 files in roughly 15 seconds on an SSD.
- **Delete from the app**, to the Recycle Bin or permanently, after a confirmation.

It comes as a GUI (`space-manager-gui.exe`) and a command-line tool (`space-manager.exe`).

## Download

Grab the latest build from the [Releases page](https://github.com/Adarsh0603/spacemanager/releases/latest). No installer is needed. Run the exe.

Windows SmartScreen may warn about an unsigned app. Choose "More info", then "Run anyway".

## How to use the GUI

1. Run `space-manager-gui.exe`.
2. Type a path such as `C:\`, or press **Browse...** to pick a folder.
3. Press **Scan**. Live file and size counts show while it runs. **Cancel** keeps what was found so far.
4. Expand folders in the tree to drill down. The right panel lists the largest files.
5. Right-click any folder or file for:
   - **Open in Explorer** / **Show in Explorer**
   - **Copy path**
   - **Delete...**

### Deleting

**Delete...** opens a confirmation showing the full path and size. You can:

- **Move to Recycle Bin** (default). You can restore it later.
- **Delete permanently.** This cannot be undone.

After a delete, sizes and counts update without a rescan. Drive roots, the scan root, and system folders (`Windows`, `Program Files`, `ProgramData`, `Users`, your profile) are refused.

### Access denied?

Some files are owned by SYSTEM or other accounts and only administrators can delete them. Click **Restart as administrator** in the top bar and accept the Windows prompt. Then scan again and retry. Check the path carefully, because administrator rights remove the usual permission protection.

## How to use the command line

```
space-manager.exe [PATH] [--top N] [--skip NAME]...
```

| Option | Meaning |
| --- | --- |
| `PATH` | Folder or drive to scan. Default `C:\` |
| `-t, --top N` | Number of folders and files to show. Default 20 |
| `--skip NAME` | Folder name to skip. Repeatable. Defaults: `$Recycle.Bin`, `System Volume Information`, `WinSxS` |

Example:

```
space-manager.exe D:\ --top 10
```
