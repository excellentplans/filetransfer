# AGENTS.md — guide for coding agents

Read this before touching anything. It compresses what you would otherwise
learn by reading every module.

## What this is

**filetransfer** (until v1.0.0: *nascopy*) — a small Rust + egui GUI for
copying files/folders to a destination of the user's choice: network shares
(CIFS/NFS/SSHFS/GVfs), USB sticks, local folders. Two entry points:

1. Dolphin service menu (`filetransfer.desktop`, `%F` → argv paths)
2. Standalone: no args → intake screen with drag & drop and a native file
   picker (rfd via xdg-portal)

The copy engine is external `/usr/bin/rsync` (runs A/B) plus an in-process
copy engine for FAT32 chunk-splitting and renamed files.

**The UI language is German.** Do not switch it to English, do not add an
i18n layer unless asked. Sizes/speeds use German decimal comma (`fmt_size`,
`fmt_speed` in `fsutil.rs`).

## Hard rules (violating these breaks the app)

- **Never block the UI thread.** `statfs` on a dead CIFS mount can hang for
  minutes. Every filesystem probe, listing, mkdir, scan, transfer, verify
  runs on a `std::thread` and reports back via `std::sync::mpsc` channels;
  the UI polls receivers each frame with `try_recv` and calls
  `ctx.request_repaint_after(...)` while waiting. Follow this pattern for
  anything new that touches the filesystem. `App::ui` additionally fires a
  cheap 300 ms heartbeat so screen transitions, the completion hold and the
  auto-close grace happen without user input (a transition frame without a
  pending repaint would otherwise present a blank window).
- **Never delete data.** No `--delete` anywhere, ever. Conflicts default to
  Skip. The self-copy-loop guard (`args::overlaps`) must stay.
- **Single instance.** `flock` on `~/.local/state/filetransfer.lock`; a
  second launch shows a notice instead of racing rsyncs.
- **The Plan is the single source of truth** for what runs. See
  `conflicts.rs`: `Plan.items` (every copy), `Plan.engine` (indices of
  non-rsync items), `rsync_a_srcs` (new/merge/resume) vs `rsync_b_srcs`
  (forced overwrite with `--checksum`), `rsync_rename` (renamed top-level
  items need their own single-source rsync run), `excludes` (anchored
  patterns protecting engine-owned paths from rsync). `apply_conflict` keeps
  these lists consistent — always change policy through it, and run
  `plan_invariants()` in tests.

## Build / test / run

```sh
cargo build --release        # ~40 s; no system Qt/GTK deps needed
cargo test                   # 26 unit tests
./target/release/filetransfer --selftest   # headless end-to-end round:
                                           # rsync + FAT32 split + resume + verify in /tmp
```

Install (the desktop file hardcodes `/home/erni/.local/bin/filetransfer`):

```sh
cp target/release/filetransfer ~/.local/bin/
cp filetransfer.desktop ~/.local/share/kservices5/ServiceMenus/
kbuildsycoca5
```

Screenshot-driven UI review works on this machine — use the background
mode so no dialog blocks the run: launch the binary, then
`spectacle -b -n -o /tmp/opencode/shots/x.png` (full screen, no GUI, no
notification), read the PNG. Mouse clicks/moves for driving the UI can be
scripted via a tiny XTEST helper compiled ad hoc (`-lX11 -lXtst`). Kill the
instance before the next run (single-instance lock).

## Module map

| File | Responsibility |
|---|---|
| `main.rs` | argv, instance lock, eframe bootstrap, programmatic icon |
| `ui.rs` | all screens + theme. The only file that should contain layout/colors |
| `args.rs` | source validation (off-thread), overlap guard |
| `dest.rs` | location discovery from mountinfo, background listing/mkdir, rfd pickers |
| `conflicts.rs` | conflict scan → `Plan`, FAT name rules, `unique_name` ("name (2).ext") |
| `transfer.rs` | rsync process mgmt, `--info=progress2` parser, chunked copy engine, cancel (SIGTERM→SIGKILL) |
| `verify.rs` | post-copy check: every file re-read on both sides, streamed SHA-256 compare with byte-accurate progress (no rsync — its progress2 doesn't tick for single big files) |
| `fsutil.rs` | statfs probe (`FsKind::Fat/ExFat/Posix`), mountinfo parse, human formatting |
| `state.rs` | persisted settings `~/.local/state/filetransfer.last` (key=value: dest/verify/autoclose) |
| `log.rs` | state paths, log, `InstanceLock` |
| `selftest.rs` | the headless pipeline test |

## UI flow and state machine

`Screen`: Intake → Startup → Pick → (Conflicts) → Transfer → Summary, plus
Fatal / AlreadyRunning notices. All screen state lives in `App` structs
(`pick`, `conf`, `xfer`, `summ`); workers communicate through `Receiver`
fields that the draw functions poll.

Transfer screen specifics (user-requested, **do not "simplify" away**):

- **Green** copy progress bar with a large painted completion symbol at its
  right (gray ring → blue ring while verifying → green disc + check when
  done). There is deliberately **no ETA/remaining-time display**.
- **Blue** verification bar appears below while checksums run (byte counter
  + blue check symbol on completion); verification happens on the transfer
  screen, not the summary. `Outcome::Ok` + verify enabled → stay on
  Transfer; after success a ~2 s hold (`complete_at`) shows the completed
  state (green disc + check + „Fertig“), then `goto_summary`. The summary
  receives the verify report prefilled.
- Petrol accent = interactive elements only. Green = copy/OK, blue = verify,
  warn/err reserved for status.

## UI conventions

- Palette: `LIGHT`/`DARK` consts in `ui.rs`; applied via
  `ctx.style_mut_of(theme, …)` for both themes, then `ctx.set_theme` for the
  system preference (read from `~/.config/kdeglobals`). Dark mode must keep
  working — test both.
- Panel ordering matters: add `egui::Panel::bottom(...)` **first**, then
  left/central panels. A full-height scroll area in CentralPanel otherwise
  pushes the bottom bar out of view.
- Custom rows (`draw_row`) paint sidebar locations and folder entries by
  hand; `primary_button` is the single filled accent button per screen.
- eframe 0.36: `eframe::App` implements `fn ui(&mut self, ui, frame)` (not
  the older `update(ctx, frame)`), and there is no `on_close_event` — window
  close is detected by polling `close_requested()` each frame, which
  triggers cancel during transfer/verify.
- Copy: plain German verbs, one vocabulary across screens
  (übertragen/abbrechen/prüfen). Buttons name the action exactly.

## Gotchas

- `fsutil::mounts()`: mountpoint is **field 4 (0-based)** of the left half
  of a mountinfo line; field 3 is the fs-internal root. This was a real bug
  once — don't "fix" it back.
- `log::state_dir()` is a *file stem* (`~/.local/state/filetransfer`), not a
  directory. Call `create_dir_all(state_base())`, never on `state_dir()`.
- Legacy migration: `legacy_last_path()` reads the old `nascopy.last` once.
  Keep it; old state files still exist on this machine.
- argv flags we may emit are filtered by the `--filetransfer-` prefix in
  `args::raw_args`.
- rfd needs a running xdg-desktop-portal (KDE has it). Pickers run on worker
  threads; `None` result = user cancelled, must be handled.
- rsync is parsed with `LC_ALL=C` and assumed at `/usr/bin/rsync`; the
  progress2 parser (`transfer::parse_progress2`) has tests — extend them for
  any new format handling.
- Tests use `/tmp/ft-*` temp dirs and clean up after themselves; the selftest
  asserts a full pipeline, so run it before shipping.

## When adding something new

- New screen: add to `Screen` enum, a `draw_*` in `ui.rs`, arm in
  `App::ui`'s match, and a bottom action bar if it takes user decisions.
- New destination type: extend `LocationKind` + classification in
  `dest::locations_from_mounts` (pure function, testable without mounts).
- New conflict policy: `conflicts::Policy` + `label()` + `apply_conflict`
  wiring + options list where conflicts are created.
- Bump `version` in `Cargo.toml`, `print_help()` in `main.rs`, and the
  README together. Commit message style: short subject, bullet body.
