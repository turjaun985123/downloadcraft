# Download Craft 0.9.13

Use [the current release instructions](RELEASE-NOTES.md) for Windows, universal
Mac, Fedora/RPM, Debian-family DEB and AppImage installation and media helpers.

`VERSION` contains the public app version. Cargo, UI, CLI and browser versions
are aligned at 0.9.13.

Official binaries are built by CI only — see [AGENTS.md](AGENTS.md). Do not run
Cargo locally for verification. The `Desktop packages` workflow
(`.github/workflows/portable.yml`) runs `cargo test --locked --release
--workspace`, builds the platform binaries and packages them with
`scripts/package.py`; the Linux job cross-builds glibc-2.28 binaries with
cargo-zigbuild and runs `scripts/package_linux.py`.
Mac cross-packaging accepts `--signer <rcodesign>` to sign the app's resources as
well as its universal executable. Native Mac packaging uses system codesign.

Paused/failed downloads may retain `.dcmeta` for resuming; successful jobs
remove it. What is still open is listed in [docs/ROADMAP.md](docs/ROADMAP.md).
