//! Pi neu starten oder ausschalten (systemd-logind über den D-Bus des Hosts).

use tokio::process::Command;

pub async fn power(action: &str) -> anyhow::Result<()> {
    let method = match action {
        "reboot" => "Reboot",
        "poweroff" => "PowerOff",
        _ => anyhow::bail!("Unbekannte Aktion"),
    };
    let path = std::env::var("HOST_DBUS_SOCKET")
        .unwrap_or_else(|_| "/run/host-dbus/system_bus_socket".into());
    if !std::path::Path::new(&path).exists() {
        anyhow::bail!("Kein Zugriff auf den Pi (D-Bus des Hosts nicht eingebunden)");
    }
    let out = Command::new("dbus-send")
        .env("DBUS_SYSTEM_BUS_ADDRESS", format!("unix:path={path}"))
        .args([
            "--system",
            "--print-reply",
            "--dest=org.freedesktop.login1",
            "/org/freedesktop/login1",
            &format!("org.freedesktop.login1.Manager.{method}"),
            "boolean:false",
        ])
        .output()
        .await?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
    }
}
