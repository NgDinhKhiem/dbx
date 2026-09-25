/// Keychain service prefix DataGrip / IntelliJ use for saved database
/// passwords (`IntelliJ Platform DB — <data source uuid>`), the only services
/// the DataGrip import reads.
const DATAGRIP_KEYCHAIN_SERVICE_PREFIX: &str = "IntelliJ Platform DB \u{2014} ";
const MAX_KEYCHAIN_SERVICES_PER_CALL: usize = 500;

fn is_uuid_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// The webview may only read Keychain items that the connection import
/// features need; anything else (browser, Wi-Fi, other apps' secrets) is
/// rejected before `security` runs.
fn ensure_allowed_keychain_service(service: &str, account: Option<&str>) -> Result<(), String> {
    if account.is_some() {
        return Err("Keychain account lookups are not allowed".to_string());
    }
    match service.strip_prefix(DATAGRIP_KEYCHAIN_SERVICE_PREFIX) {
        Some(uuid) if is_uuid_like(uuid) => Ok(()),
        _ => Err("Keychain service is not allowed".to_string()),
    }
}

/// Read a macOS Keychain generic password by service name.
/// Triggers a system authorization dialog (Touch ID / password) for each unique service.
#[tauri::command]
pub async fn read_keychain_password(service: String, account: Option<String>) -> Result<String, String> {
    ensure_allowed_keychain_service(&service, account.as_deref())?;
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (service, account);
        Err("Keychain access is only available on macOS".to_string())
    }

    #[cfg(target_os = "macos")]
    {
        tauri::async_runtime::spawn_blocking(move || read_keychain_password_blocking(service, account))
            .await
            .map_err(|err| err.to_string())?
    }
}

#[cfg(target_os = "macos")]
fn read_keychain_password_blocking(service: String, account: Option<String>) -> Result<String, String> {
    let mut cmd = dbx_core::process::new_std_command("security");
    cmd.args(["find-generic-password", "-s", &service, "-w"]);
    if let Some(ref acct) = account {
        cmd.args(["-a", acct]);
    }

    let output = cmd.output().map_err(|e| format!("Failed to run security command: {e}"))?;

    if output.status.success() {
        let password = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(password)
    } else {
        // Exit code 44 = user cancelled the authorization dialog
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.code() == Some(44)
            || stderr.contains("User canceled")
            || stderr.contains("could not be found")
            || stderr.contains("The specified item could not be found")
        {
            Ok(String::new())
        } else {
            Err(format!("Keychain read failed: {}", stderr.trim()))
        }
    }
}

/// Read multiple Keychain passwords in one call. Returns a map of service -> password.
/// Services that fail or are cancelled get an empty string.
#[tauri::command]
pub async fn read_keychain_passwords(services: Vec<String>) -> Result<Vec<(String, String)>, String> {
    if services.len() > MAX_KEYCHAIN_SERVICES_PER_CALL {
        return Err(format!("At most {MAX_KEYCHAIN_SERVICES_PER_CALL} Keychain services can be read at once"));
    }
    for service in &services {
        ensure_allowed_keychain_service(service, None)?;
    }
    let mut results = Vec::with_capacity(services.len());
    for service in services {
        let password = read_keychain_password(service.clone(), None).await.unwrap_or_default();
        results.push((service, password));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::ensure_allowed_keychain_service;

    #[test]
    fn allows_only_datagrip_data_source_services() {
        assert!(ensure_allowed_keychain_service(
            "IntelliJ Platform DB \u{2014} 0f8fad5b-d9cb-469f-a165-70867728950e",
            None
        )
        .is_ok());
        assert!(ensure_allowed_keychain_service(
            "IntelliJ Platform DB \u{2014} 0f8fad5b-d9cb-469f-a165-70867728950e",
            Some("root")
        )
        .is_err());
        assert!(ensure_allowed_keychain_service("Chrome Safe Storage", None).is_err());
        assert!(ensure_allowed_keychain_service("IntelliJ Platform DB \u{2014} ../x", None).is_err());
        assert!(ensure_allowed_keychain_service("IntelliJ Platform DB - 0f8fad5b-d9cb-469f-a165-70867728950e", None)
            .is_err());
    }
}
