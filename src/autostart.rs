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

#[cfg(windows)]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
#[cfg(windows)]
const VALUE: &str = "Download Craft";

/// What the Run value should say for this program.
fn command_for(exe: &std::path::Path) -> String {
    format!("\"{}\" {STARTUP_ARG}", exe.display())
}

/// Make the Run entry match the setting: present and pointing at this
/// program when `enabled`, gone otherwise. Best effort, silent.
pub fn sync(enabled: bool) {
    #[cfg(windows)]
    {
        let Ok(exe) = crate::platform::launch_path() else { return };
        let want = command_for(&exe);
        let current = read_value();
        if enabled {
            if current.as_deref() != Some(want.as_str()) {
                let _ = reg(&["add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &want, "/f"]);
            }
        } else if current.is_some() {
            let _ = reg(&["delete", RUN_KEY, "/v", VALUE, "/f"]);
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
    }
}

/// Whether this launch came from sign-in.
pub fn is_startup_launch(args: &[String]) -> bool {
    args.iter().any(|a| a == STARTUP_ARG)
}

#[cfg(windows)]
fn reg(args: &[&str]) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    crate::platform::silent_command("reg")
        .args(args)
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .output()
        .ok()
}

/// The Run value's data, if the value exists.
#[cfg(windows)]
fn read_value() -> Option<String> {
    let out = reg(&["query", RUN_KEY, "/v", VALUE])?;
    if !out.status.success() {
        return None;
    }
    parse_query(&String::from_utf8_lossy(&out.stdout))
}

/// `reg query` prints `    Download Craft    REG_SZ    "C:\…\download_craft.exe" --startup`.
fn parse_query(text: &str) -> Option<String> {
    text.lines().find_map(|l| {
        let (_, rest) = l.split_once("REG_SZ")?;
        Some(rest.trim().to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_value_quotes_the_path_and_marks_the_launch() {
        let c = command_for(std::path::Path::new(r"D:\Apps\Download Craft\download_craft.exe"));
        assert_eq!(c, r#""D:\Apps\Download Craft\download_craft.exe" --startup"#);
        assert!(is_startup_launch(&["x".into(), "--startup".into()]));
        assert!(!is_startup_launch(&["x".into()]));
    }

    #[test]
    fn reg_query_output_is_read() {
        let out = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    Download Craft    REG_SZ    \"C:\\x y\\download_craft.exe\" --startup\r\n\r\n";
        assert_eq!(parse_query(out).as_deref(), Some("\"C:\\x y\\download_craft.exe\" --startup"));
        assert_eq!(parse_query("ERROR: nothing"), None);
    }
}
