use std::{fs, path::PathBuf};

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::config::app_data_dir;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct ProxyBackup {
    enabled: u32,
    server: Option<String>,
    auto_config_url: Option<String>,
    proxy_override: Option<String>,
    auto_detect: Option<u32>,
}

fn backup_path() -> PathBuf {
    app_data_dir().join("system-proxy-backup.toml")
}

#[cfg(windows)]
fn internet_settings() -> Result<winreg::RegKey> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    current_user
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
        )
        .context("无法打开 Windows Internet Settings")
}

#[cfg(windows)]
pub fn enable_system_proxy(server: &str) -> Result<()> {
    use winreg::types::FromRegValue;

    let key = internet_settings()?;
    let enabled = key.get_value::<u32, _>("ProxyEnable").unwrap_or(0);
    let server_value = key
        .get_raw_value("ProxyServer")
        .ok()
        .and_then(|raw| String::from_reg_value(&raw).ok());
    if enabled != 0 && server_value.as_deref() == Some(server) {
        if backup_path().exists() {
            return Ok(());
        }
        return Err(anyhow!(
            "系统代理已经指向 {server}，但没有可验证的 Clash 恢复快照"
        ));
    }

    let backup = ProxyBackup {
        enabled,
        server: server_value,
        auto_config_url: key.get_value("AutoConfigURL").ok(),
        proxy_override: key.get_value("ProxyOverride").ok(),
        auto_detect: key.get_value("AutoDetect").ok(),
    };
    fs::create_dir_all(app_data_dir())?;
    fs::write(backup_path(), toml::to_string(&backup)?)?;
    let update = (|| -> Result<()> {
        key.set_value("ProxyServer", &server)?;
        key.set_value("ProxyEnable", &1u32)?;
        refresh_proxy_settings()?;
        Ok(())
    })();
    if let Err(error) = update {
        let _ = restore_system_proxy();
        return Err(error).context("系统代理写入失败，已尝试恢复 Clash 设置");
    }
    Ok(())
}

#[cfg(windows)]
pub fn restore_system_proxy() -> Result<()> {
    let path = backup_path();
    if !path.exists() {
        return Ok(());
    }
    let backup: ProxyBackup = toml::from_str(&fs::read_to_string(&path)?)?;
    let key = internet_settings()?;
    key.set_value("ProxyEnable", &backup.enabled)?;
    match backup.server {
        Some(server) => key.set_value("ProxyServer", &server)?,
        None => {
            let _ = key.delete_value("ProxyServer");
        }
    }
    match backup.auto_config_url {
        Some(value) => key.set_value("AutoConfigURL", &value)?,
        None => {
            let _ = key.delete_value("AutoConfigURL");
        }
    }
    match backup.proxy_override {
        Some(value) => key.set_value("ProxyOverride", &value)?,
        None => {
            let _ = key.delete_value("ProxyOverride");
        }
    }
    match backup.auto_detect {
        Some(value) => key.set_value("AutoDetect", &value)?,
        None => {
            let _ = key.delete_value("AutoDetect");
        }
    }
    refresh_proxy_settings()?;
    fs::remove_file(path)?;
    Ok(())
}

#[cfg(windows)]
pub fn is_our_proxy(server: &str) -> bool {
    internet_settings()
        .ok()
        .and_then(|key| {
            let enabled = key.get_value::<u32, _>("ProxyEnable").ok()?;
            let value = key.get_value::<String, _>("ProxyServer").ok()?;
            Some(enabled != 0 && value == server)
        })
        .unwrap_or(false)
}

#[cfg(windows)]
pub fn current_system_proxy() -> Option<String> {
    let key = internet_settings().ok()?;
    let enabled = key.get_value::<u32, _>("ProxyEnable").ok()?;
    if enabled == 0 {
        return None;
    }
    key.get_value::<String, _>("ProxyServer").ok()
}

#[cfg(windows)]
pub fn recover_stale_takeover(server: &str) -> Result<bool> {
    if is_our_proxy(server) && backup_path().exists() {
        restore_system_proxy()?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(windows)]
fn refresh_proxy_settings() -> Result<()> {
    use windows::Win32::Networking::WinInet::{
        INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW,
    };
    unsafe {
        InternetSetOptionW(None, INTERNET_OPTION_SETTINGS_CHANGED, None, 0)
            .map_err(|error| anyhow!(error))?;
        InternetSetOptionW(None, INTERNET_OPTION_REFRESH, None, 0)
            .map_err(|error| anyhow!(error))?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn enable_system_proxy(_server: &str) -> Result<()> {
    Err(anyhow!("系统代理自动配置目前仅支持 Windows"))
}

#[cfg(not(windows))]
pub fn restore_system_proxy() -> Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn is_our_proxy(_server: &str) -> bool {
    false
}

#[cfg(not(windows))]
pub fn current_system_proxy() -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn recover_stale_takeover(_server: &str) -> Result<bool> {
    Ok(false)
}

#[cfg(windows)]
pub fn set_startup(enabled: bool, minimized: bool) -> Result<()> {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = current_user.create_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")?;
    if enabled {
        let executable = std::env::current_exe().context("无法获取程序路径")?;
        let mut command = format!("\"{}\"", executable.display());
        if minimized {
            command.push_str(" --minimized");
        }
        key.set_value("NetSentinel", &command)?;
    } else {
        let _ = key.delete_value("NetSentinel");
    }
    Ok(())
}

#[cfg(windows)]
pub fn startup_enabled() -> bool {
    use winreg::{RegKey, enums::HKEY_CURRENT_USER};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Run")
        .ok()
        .and_then(|key| key.get_value::<String, _>("NetSentinel").ok())
        .is_some()
}

#[cfg(not(windows))]
pub fn set_startup(_enabled: bool, _minimized: bool) -> Result<()> {
    Err(anyhow!("开机启动配置目前仅支持 Windows"))
}

#[cfg(not(windows))]
pub fn startup_enabled() -> bool {
    false
}
