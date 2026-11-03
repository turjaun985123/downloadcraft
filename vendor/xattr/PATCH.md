# xattr 0.2.3 compatibility patch

This is the MIT/Apache-2.0 `xattr` 0.2.3 crate, vendored because GPUI 0.2.2
depends on that version. The only source change maps the missing extended
attribute error to `libc::ENODATA` on Linux and Android; macOS and BSD retain
`libc::ENOATTR`. Remove this patch after the GPUI dependency chain uses a
maintained `xattr` release.
