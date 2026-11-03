# Third-party notices

Download Craft is original Rust code written for this project. Its interface
design was inspired by modern download managers. The libraries below are
built into the program; their own licences apply to them.

## UnRAR — UnRAR licence (freeware, not open source)

RAR archives are extracted with the UnRAR source code by Alexander Roshal,
built in through the `unrar` / `unrar_sys` crates (MIT / Apache-2.0 for the
Rust wrapper). UnRAR may be used to extract RAR archives free of charge; it
may not be used to re-create the RAR compression algorithm. Download Craft
only extracts. The full licence is in `licenses/UnRAR-license.txt` and ships
next to the program.

- Source: https://www.rarlab.com/rar_add.htm

## sevenz-rust2 — Apache License 2.0

7z archives are extracted with `sevenz-rust2` (© sevenz-rust contributors).

- Source: https://github.com/hasenbanck/sevenz-rust2

## zip — MIT

ZIP archives (and the portable update) use the `zip` crate (© zip-rs
contributors).

## Material Icons font — Apache License 2.0

`assets/fonts/MaterialIcons-Regular.ttf` (© Google) is bundled for UI glyphs.
Redistribution of this font requires keeping this attribution.

- Source: https://github.com/google/material-design-icons
- License: Apache-2.0 — https://www.apache.org/licenses/LICENSE-2.0

## Design inspiration

The layout and usability of this app were inspired by modern desktop
download managers (including AB Download Manager and FluxDown). Inspiration
means ideas only: every line of code here was written independently for
this project, and no source files from those projects were copied,
adapted, or embedded.

## Rust crates

- `gpui` (© Zed Industries) — Apache-2.0, used for the native desktop UI.
- `sha2` (© RustCrypto contributors) — MIT / Apache-2.0, used for optional
  SHA-256 file-integrity verification.
- `stacksafe-macro` (© StackSafe contributors) — Apache-2.0. The vendored
  compatibility patch removes an unmaintained diagnostics dependency while
  preserving the upstream macro transformation; its license is in
  `vendor/stacksafe-macro/LICENSE-APACHE`.
- `xattr` (© xattr contributors) — MIT / Apache-2.0. The vendored Linux
  compatibility patch is in `vendor/xattr/` with both license texts.
