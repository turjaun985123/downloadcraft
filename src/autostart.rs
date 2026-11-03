//! Start with Windows (Settings → Appearance → Window, on by default).
//!
//! A value under the user's `…\CurrentVersion\Run` key starts this program
//! with `--startup` at sign-in; it then opens straight into the tray. The
//! value is rewritten on every launch, so a portable copy that was moved to
//! another folder (or updated in place) keeps starting from where it is now.
//! No admin rights are needed, and Windows' own Startup apps switch (Task
//! Manager / Settings) still wins: that lives in a separate key we never touch.

/// The argument a sign-in launch carries.
pub const STARTUP_ARG: &str = "--startup";

/// Whether this launch came from sign-in.
pub fn is_startup_launch(args: &[String]) -> bool {
    args.iter().any(|a| a == STARTUP_ARG)
}
