//! Browser native-messaging host installation and status.
//!
//! This module is deliberately independent of the GPUI UI so the desktop app, CLI, and
//! setup flows can share the browser integration.

pub(crate) const EXTENSION_ID: &str = "pgdhmfgefjnfijmlheboncdmfhcnoikk";
/// IDs assigned by additional extension stores, if the extension is listed
/// there. The unpacked Chrome extension currently has a single pinned ID.
pub(crate) const STORE_EXTENSION_IDS: &[&str] = &[];

#[cfg(windows)]
pub(crate) fn install_native_host(browser: &str) -> Result<String, String> {
    let base = crate::model::config_dir()
        .map(|p| p.join("native-host"))
        .ok_or_else(|| "no config dir".to_string())?;
    std::fs::create_dir_all(&base).map_err(|e| e.to_string())?;

    let exe = crate::platform::launch_path().map_err(|e| e.to_string())?;
    let bat = base.join("download_craft_host.bat");
    // @echo off is mandatory: cmd echoing to stdout corrupts native messaging.
    std::fs::write(
        &bat,
        format!("@echo off\r\n\"{}\" --native-host %*\r\n", exe.display()),
    )
    .map_err(|e| e.to_string())?;
    // cmd reads .bat files in the OEM code page. For non-ASCII paths, give
    // the browser the executable directly; launched_directly recognizes it.
    let host = if exe.to_string_lossy().is_ascii() { bat.clone() } else { exe.clone() };

    const HOST_NAME: &str = "com.download_craft";
    let manifest_path = base.join(format!("{browser}.json"));
    let manifest = match browser {
        "firefox" => serde_json::json!({
            "name": HOST_NAME,
            "description": "Download Craft native host",
            "path": host.to_string_lossy(),
            "type": "stdio",
            "allowed_extensions": ["send-to-download-craft@example"]
        }),
        _ => serde_json::json!({
            "name": HOST_NAME,
            "description": "Download Craft native host",
            "path": host.to_string_lossy(),
            "type": "stdio",
            "allowed_origins": std::iter::once(EXTENSION_ID)
                .chain(STORE_EXTENSION_IDS.iter().copied())
                .map(|id| format!("chrome-extension://{id}/"))
                .collect::<Vec<_>>()
        }),
    };
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let mut command = crate::platform::silent_command("reg");
    command.args(["add", native_host_key(browser), "/ve", "/d"]);
    command.arg(&manifest_path).arg("/f");
    let output = command.output().map_err(|e| format!("reg failed: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }
    // Best-effort cleanup of the legacy host registration.
    let mut command = crate::platform::silent_command("reg");
    command.args(["delete", legacy_host_key(browser), "/f"]);
    let _ = command.output();
    Ok(manifest_path.to_string_lossy().to_string())
}

#[cfg(not(windows))]
pub(crate) fn install_native_host(browser: &str) -> Result<String, String> {
    crate::platform::install_native_host(browser)
}

#[cfg(windows)]
pub(crate) fn native_host_status(browser: &str) -> bool {
    registry_default(native_host_key(browser)).is_some_and(|path| std::path::Path::new(&path).is_file())
}

#[cfg(not(windows))]
pub(crate) fn native_host_status(browser: &str) -> bool {
    crate::platform::native_host_directory(browser)
        .map(|dir| dir.join("com.download_craft.json").is_file())
        .unwrap_or(false)
}

#[cfg(windows)]
fn registry_default(key: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let sub = key.strip_prefix(r"HKCU\")?;
    let wide: Vec<u16> = sub.encode_utf16().chain(std::iter::once(0)).collect();
    let mut buffer = vec![0u16; 1024];
    let mut len = (buffer.len() * 2) as u32;
    // SAFETY: `wide` is NUL-terminated and buffer/len describe writable storage.
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide.as_ptr(),
            std::ptr::null(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if result != 0 {
        return None;
    }
    let chars = (len as usize / 2).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..chars]).trim_end_matches('\0').to_string())
}

#[cfg(windows)]
fn native_host_key(browser: &str) -> &'static str {
    match browser {
        "chrome" => r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.download_craft",
        "edge" => r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.download_craft",
        "brave" => r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.download_craft",
        "opera" => r"HKCU\Software\Opera Software\NativeMessagingHosts\com.download_craft",
        "vivaldi" => r"HKCU\Software\Vivaldi\NativeMessagingHosts\com.download_craft",
        _ => r"HKCU\Software\Mozilla\NativeMessagingHosts\com.download_craft",
    }
}

#[cfg(windows)]
fn legacy_host_key(browser: &str) -> &'static str {
    match browser {
        "chrome" => r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.idm_rust",
        "edge" => r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.idm_rust",
        "brave" => r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.idm_rust",
        "opera" => r"HKCU\Software\Opera Software\NativeMessagingHosts\com.idm_rust",
        "vivaldi" => r"HKCU\Software\Vivaldi\NativeMessagingHosts\com.idm_rust",
        _ => r"HKCU\Software\Mozilla\NativeMessagingHosts\com.idm_rust",
    }
}

/// Reinstall a missing or stale registration on Windows, or install it on
/// Unix systems. Returns true when a change was made.
#[cfg(windows)]
pub(crate) fn ensure_native_host(browser: &str) -> Result<bool, String> {
    let base = crate::model::config_dir()
        .map(|path| path.join("native-host"))
        .ok_or_else(|| "no config dir".to_string())?;
    let exe = crate::platform::launch_path().map_err(|e| e.to_string())?;
    let exe_string = exe.to_string_lossy().into_owned();
    let manifest_path = base.join(format!("{browser}.json"));
    let manifest_string = manifest_path.to_string_lossy().into_owned();
    let direct = !exe_string.is_ascii();
    let fresh = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<serde_json::Value>(&contents).ok())
        .and_then(|value| value.get("path").and_then(|path| path.as_str()).map(str::to_owned))
        .map(|host| {
            if direct {
                host == exe_string
            } else {
                std::fs::read_to_string(&host)
                    .map(|contents| contents.contains(exe_string.as_str()))
                    .unwrap_or(false)
            }
        })
        .unwrap_or(false);
    let registered = registry_default(native_host_key(browser)).is_some_and(|value| value == manifest_string);
    if fresh && registered {
        return Ok(false);
    }
    install_native_host(browser)?;
    Ok(true)
}

#[cfg(not(windows))]
pub(crate) fn ensure_native_host(browser: &str) -> Result<bool, String> {
    crate::platform::install_native_host(browser).map(|_| true)
}
