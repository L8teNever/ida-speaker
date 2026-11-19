//! Cloudflare WARP im Container: verbindet den Lautsprecher mit Zero Trust (Service-Token), damit er IDA über
//! die interne Adresse erreicht. Die Einstellungen kommen von der Website und landen als `mdm.xml` bei warp-svc.

use crate::config::Cloudflare;
use crate::state::App;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::{Child, Command};

fn mdm_path() -> PathBuf {
    std::env::var_os("WARP_MDM")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/cloudflare-warp/mdm.xml"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Zero-Trust-Anmeldung per Service-Token, ohne Browser.
pub fn mdm_xml(cf: &Cloudflare) -> String {
    format!(
        "<dict>\n  <key>organization</key>\n  <string>{}</string>\n  <key>auth_client_id</key>\n  <string>{}</string>\n  \
         <key>auth_client_secret</key>\n  <string>{}</string>\n  <key>service_mode</key>\n  <string>warp</string>\n  \
         <key>auto_connect</key>\n  <integer>1</integer>\n  <key>onboarding</key>\n  <false/>\n</dict>\n",
        xml_escape(cf.team.trim()),
        xml_escape(cf.client_id.trim()),
        xml_escape(cf.client_secret.trim())
    )
}

fn installed() -> bool {
    which("warp-svc") && which("warp-cli")
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

async fn cli(args: &[&str]) -> String {
    match Command::new("warp-cli")
        .arg("--accept-tos")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
    {
        Ok(o) => format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
        .trim()
        .to_string(),
        Err(e) => e.to_string(),
    }
}

/// Läuft für immer: startet warp-svc, wenn Cloudflare eingeschaltet ist, und startet es bei Änderungen neu.
pub async fn run(app: Arc<App>) {
    let mut rx = app.config.subscribe();
    let mut child: Option<Child> = None;
    let mut applied: Option<Cloudflare> = None;
    loop {
        let cf = rx.borrow_and_update().cloudflare.clone();
        let have = installed();
        if !have {
            let mut s = app.status.lock().unwrap();
            s.warp.installed = false;
            s.warp.running = false;
            s.warp.connected = false;
            s.warp.detail = "WARP ist in diesem Image nicht installiert".into();
        } else if !cf.ready() {
            if let Some(mut c) = child.take() {
                app.log("Cloudflare aus – stoppe WARP");
                let _ = c.kill().await;
            }
            let _ = std::fs::remove_file(mdm_path());
            applied = None;
            let mut s = app.status.lock().unwrap();
            s.warp = crate::state::WarpStatus {
                installed: true,
                running: false,
                connected: false,
                detail: if cf.enabled {
                    "Team-Name und Service-Token fehlen".into()
                } else {
                    "aus".into()
                },
            };
        } else {
            let changed = applied.as_ref() != Some(&cf);
            let dead = match child.as_mut() {
                Some(c) => c.try_wait().ok().flatten().is_some(),
                None => true,
            };
            if changed || dead {
                if let Some(mut c) = child.take() {
                    let _ = c.kill().await;
                }
                let path = mdm_path();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::write(&path, mdm_xml(&cf)) {
                    Ok(()) => {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            let _ = std::fs::set_permissions(
                                &path,
                                std::fs::Permissions::from_mode(0o600),
                            );
                        }
                    }
                    Err(e) => app.log(format!("mdm.xml lässt sich nicht schreiben: {e}")),
                }
                app.log(if changed {
                    "Cloudflare-Einstellungen übernommen – starte WARP"
                } else {
                    "WARP neu gestartet"
                });
                child = Command::new("warp-svc")
                    .arg("--accept-tos")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| app.log(format!("warp-svc startet nicht: {e}")))
                    .ok();
                applied = Some(cf.clone());
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            let status = cli(&["status"]).await;
            let connected = status.contains("Connected") && !status.contains("Disconnected");
            if !connected && status.contains("Disconnected") {
                let _ = cli(&["connect"]).await;
            }
            let mut s = app.status.lock().unwrap();
            s.warp = crate::state::WarpStatus {
                installed: true,
                running: child.is_some(),
                connected,
                detail: status.lines().take(3).collect::<Vec<_>>().join(" · "),
            };
        }
        // Alle 10 s den Status auffrischen, sofort bei Änderungen der Einstellungen
        let _ = tokio::time::timeout(Duration::from_secs(10), rx.changed()).await;
    }
}

/// Knopf „Neu verbinden“ auf der Website.
pub async fn reconnect() -> String {
    if !installed() {
        return "WARP ist nicht installiert".into();
    }
    let _ = cli(&["disconnect"]).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    cli(&["connect"]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdm_is_escaped() {
        let cf = Cloudflare {
            enabled: true,
            team: "my-team".into(),
            client_id: "id.access".into(),
            client_secret: "a<b&c".into(),
        };
        let x = mdm_xml(&cf);
        assert!(x.contains("<string>my-team</string>"));
        assert!(x.contains("<string>a&lt;b&amp;c</string>"));
        assert!(x.contains("<string>warp</string>"));
    }
}
