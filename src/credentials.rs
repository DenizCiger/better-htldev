use anyhow::{Context, Result};

use crate::core::app_data_dir;

const SERVICE: &str = "htldev";

fn accounts_path() -> std::path::PathBuf {
    app_data_dir().join("accounts.json")
}

/// Persist username list to disk and password to OS keychain.
pub fn save(username: &str, password: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, username)?
        .set_password(password)
        .context("save password to keychain")?;

    let mut accounts = list().unwrap_or_default();
    if !accounts.iter().any(|a| a == username) {
        accounts.push(username.to_string());
        let dir = app_data_dir();
        std::fs::create_dir_all(&dir)?;
        std::fs::write(accounts_path(), serde_json::to_string(&accounts)?)?;
    }
    Ok(())
}

/// Load password from OS keychain for a known username.
pub fn load_password(username: &str) -> Result<String> {
    keyring::Entry::new(SERVICE, username)?
        .get_password()
        .context("read password from keychain")
}

/// All usernames that have a stored credential.
pub fn list() -> Result<Vec<String>> {
    let path = accounts_path();
    if !path.exists() {
        return Ok(vec![]);
    }
    let data = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data)?)
}

/// Remove a stored credential (username from list + password from keychain).
pub fn delete(username: &str) -> Result<()> {
    if let Ok(entry) = keyring::Entry::new(SERVICE, username) { let _ = entry.delete_password(); }
    let mut accounts = list().unwrap_or_default();
    accounts.retain(|a| a != username);
    std::fs::write(accounts_path(), serde_json::to_string(&accounts)?)?;
    Ok(())
}
