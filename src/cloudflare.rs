//! Cloudflare Zero Trust mit einem Klick einrichten: Mit einem API-Token (wird nicht gespeichert) legt der Lautsprecher
//! seinen Service-Token an, erlaubt ihm die WARP-Anmeldung, sorgt dafür, dass IDA durch den Tunnel geht
//! (Split Tunnel und Route) und trägt alles selbst ein.

use crate::state::App;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::net::Ipv4Addr;
use std::sync::Arc;

fn api() -> String {
    std::env::var("CLOUDFLARE_API")
        .unwrap_or_else(|_| "https://api.cloudflare.com/client/v4".into())
}

#[derive(Debug, Deserialize)]
pub struct Request {
    pub api_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub tunnel_id: String,
    /// "domain" (Service-Token für Access vor der Voice-Domain) oder "warp"; leer = wie unter IDA eingestellt.
    #[serde(default)]
    pub mode: String,
}

#[derive(Debug, Serialize, Default)]
pub struct Report {
    pub ok: bool,
    pub steps: Vec<String>,
    /// "account" oder "tunnel": die Website muss nachfragen und noch einmal aufrufen.
    pub needs: String,
    pub choices: Vec<Value>,
    pub error: String,
}

struct Cf {
    http: reqwest::Client,
    token: String,
}

impl Cf {
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<Value> {
        let mut req = self
            .http
            .request(method, format!("{}{path}", api()))
            .bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Cloudflare nicht erreichbar: {e}"))?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() || v["success"] == false {
            let msg = v["errors"]
                .as_array()
                .map(|e| {
                    e.iter()
                        .map(|x| x["message"].as_str().unwrap_or("").to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| format!("HTTP {status}"));
            if status.as_u16() == 403 || status.as_u16() == 401 {
                anyhow::bail!(
                    "{path}: keine Berechtigung ({msg}) – fehlt dem API-Token ein Recht?"
                );
            }
            anyhow::bail!("{path}: {msg}");
        }
        Ok(v["result"].clone())
    }
    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
    async fn put(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(reqwest::Method::PUT, path, Some(body)).await
    }
}

pub fn parse_cidr(s: &str) -> Option<(u32, u8)> {
    let (ip, len) = s.split_once('/').unwrap_or((s, "32"));
    let ip: Ipv4Addr = ip.trim().parse().ok()?;
    let len: u8 = len.trim().parse().ok().filter(|l| *l <= 32)?;
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    Some((u32::from(ip) & mask, len))
}

pub fn contains(cidr: &str, ip: Ipv4Addr) -> bool {
    parse_cidr(cidr).is_some_and(|(net, len)| {
        let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
        u32::from(ip) & mask == net
    })
}

/// `cidr` ohne `ip`: die Hälften, in denen die Adresse nicht liegt (z. B. 10.0.0.0/8 ohne 10.1.2.3 → 24 Netze).
pub fn subtract(cidr: &str, ip: Ipv4Addr) -> Vec<String> {
    let Some((net, len)) = parse_cidr(cidr) else {
        return vec![cidr.to_string()];
    };
    if !contains(cidr, ip) {
        return vec![cidr.to_string()];
    }
    let ip = u32::from(ip);
    let _ = net;
    (len + 1..=32)
        .map(|p| {
            let shift = 32 - p as u32;
            let sibling = ((ip >> shift) ^ 1) << shift;
            format!("{}/{p}", Ipv4Addr::from(sibling))
        })
        .collect()
}

/// IPv4-Adresse aus der IDA-Adresse (nur wenn dort direkt eine IP steht).
pub fn target_ip(url: &str) -> Option<Ipv4Addr> {
    let u = reqwest::Url::parse(url).ok()?;
    u.host_str()?.parse().ok()
}

async fn ensure_token(
    cf: &Cf,
    a: &str,
    cfg: &crate::config::Config,
    r: &mut Report,
) -> anyhow::Result<(String, String, String, String)> {
    let name = format!(
        "ida-speaker-{}",
        if cfg.ida.device_name.is_empty() {
            "lautsprecher"
        } else {
            &cfg.ida.device_name
        }
    );
    let tokens = cf.get(&format!("{a}/access/service_tokens")).await?;
    let existing = tokens
        .as_array()
        .and_then(|t| t.iter().find(|x| x["name"] == name.as_str()).cloned());
    let (id, client_id, secret) = match existing {
        Some(t) => {
            let id = t["id"].as_str().unwrap_or_default().to_string();
            let rot = cf
                .post(&format!("{a}/access/service_tokens/{id}/rotate"), json!({}))
                .await?;
            r.steps.push(format!("Service-Token „{name}“ erneuert"));
            (
                id,
                t["client_id"].as_str().unwrap_or_default().to_string(),
                rot["client_secret"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        }
        None => {
            let t = cf
                .post(
                    &format!("{a}/access/service_tokens"),
                    json!({"name": name, "duration": "forever"}),
                )
                .await?;
            r.steps.push(format!("Service-Token „{name}“ angelegt"));
            (
                t["id"].as_str().unwrap_or_default().to_string(),
                t["client_id"].as_str().unwrap_or_default().to_string(),
                t["client_secret"].as_str().unwrap_or_default().to_string(),
            )
        }
    };
    if client_id.is_empty() || secret.is_empty() {
        anyhow::bail!("Cloudflare hat keinen Service-Token zurückgegeben");
    }
    Ok((id, client_id, secret, name))
}

/// Alle Access-Apps (seitenweise).
async fn list_apps(cf: &Cf, a: &str) -> anyhow::Result<Vec<Value>> {
    let mut all = Vec::new();
    for page in 1..=20 {
        let got = cf
            .get(&format!("{a}/access/apps?page={page}&per_page=100"))
            .await?;
        let list = got.as_array().cloned().unwrap_or_default();
        let n = list.len();
        all.extend(list);
        if n < 100 {
            break;
        }
    }
    Ok(all)
}

/// Hängt eine Policy „Service Auth“ für den Token an die App. `false` = war schon erlaubt.
async fn allow_token(
    cf: &Cf,
    a: &str,
    app: &Value,
    token_id: &str,
    name: &str,
) -> anyhow::Result<bool> {
    let policies = app["policies"].as_array().cloned().unwrap_or_default();
    let has = policies.iter().any(|p| {
        p["decision"] == "non_identity"
            && p["include"].as_array().is_some_and(|inc| {
                inc.iter().any(|i| {
                    i["service_token"]["token_id"] == token_id
                        || i.get("any_valid_service_token").is_some()
                })
            })
    });
    if has {
        return Ok(false);
    }
    let p = cf
        .post(
            &format!("{a}/access/policies"),
            json!({"name": format!("{name} (Service Auth)"), "decision": "non_identity", "include": [{"service_token": {"token_id": token_id}}]}),
        )
        .await?;
    let pid = p["id"].as_str().unwrap_or_default().to_string();
    let mut body = app.clone();
    if let Some(o) = body.as_object_mut() {
        for k in ["id", "aud", "uid", "created_at", "updated_at"] {
            o.remove(k);
        }
    }
    let mut refs: Vec<Value> = policies
        .iter()
        .enumerate()
        .map(|(i, p)| json!({"id": p["id"], "precedence": i + 1}))
        .collect();
    refs.push(json!({"id": pid, "precedence": refs.len() + 1}));
    body["policies"] = Value::Array(refs);
    let app_id = app["id"].as_str().unwrap_or_default();
    cf.put(&format!("{a}/access/apps/{app_id}"), body).await?;
    Ok(true)
}

/// Domains einer Access-App (`domain` und `self_hosted_domains`/`destinations`).
fn app_domains(app: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(d) = app["domain"].as_str() {
        out.push(d.to_string());
    }
    for k in ["self_hosted_domains", "destinations"] {
        for d in app[k].as_array().into_iter().flatten() {
            if let Some(x) = d.as_str().or_else(|| d["uri"].as_str()) {
                out.push(x.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Greift die Domain einer App (z. B. `voice.example.com` oder `voice.example.com/device`) für die Wege des Lautsprechers?
pub fn app_covers(domain: &str, host: &str) -> bool {
    let d = domain
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let (dh, path) = d
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((d, String::new()));
    let host_ok = dh.eq_ignore_ascii_case(host)
        || dh
            .strip_prefix("*.")
            .is_some_and(|w| host.ends_with(&format!(".{w}")));
    let path = path.trim_end_matches('*').trim_end_matches('/');
    host_ok && (path.is_empty() || "/device/ticket".starts_with(path) || "/live".starts_with(path))
}

/// App lässt ohnehin alle durch (nur Bypass-Policies) – dort braucht es keinen Service-Token.
pub fn bypass_only(app: &Value) -> bool {
    let p = app["policies"].as_array().cloned().unwrap_or_default();
    !p.is_empty() && p.iter().all(|x| x["decision"] == "bypass")
}

/// Über die Domain: Service-Token für die Access-App(s), die die Voice-Domain schützen.
async fn domain(app: &Arc<App>, cf: &Cf, a: &str, r: &mut Report) -> anyhow::Result<()> {
    let cfg = app.cfg();
    let url = reqwest::Url::parse(&cfg.ida.url).map_err(|_| {
        anyhow::anyhow!("Erst unter IDA die Adresse eintragen, z. B. https://voice.example.com")
    })?;
    let host = url.host_str().unwrap_or_default().to_string();
    if host.parse::<Ipv4Addr>().is_ok() {
        anyhow::bail!("Für „über die Domain“ muss unter IDA eine Domain stehen, keine IP");
    }
    let apps: Vec<Value> = list_apps(cf, a)
        .await?
        .into_iter()
        .filter(|x| {
            x["type"] == "self_hosted"
                && app_domains(x).iter().any(|d| app_covers(d, &host))
                && !bypass_only(x)
        })
        .collect();
    let mut next = cfg.clone();
    next.ida.via = "domain".into();
    next.cloudflare.enabled = false;
    if apps.is_empty() {
        r.steps.push(format!(
            "{host} ist nicht mit Cloudflare Access geschützt – kein Service-Token nötig"
        ));
        next.ida.access_client_id.clear();
        next.ida.access_client_secret.clear();
        app.save_config(next)?;
        return Ok(());
    }
    let (token_id, client_id, secret, name) = ensure_token(cf, a, &cfg, r).await?;
    for x in &apps {
        let label = x["name"].as_str().unwrap_or("App").to_string();
        if allow_token(cf, a, x, &token_id, &name).await? {
            r.steps.push(format!(
                "Access-App „{label}“: Service Auth für den Lautsprecher erlaubt"
            ));
        } else {
            r.steps.push(format!(
                "Access-App „{label}“: Lautsprecher war schon erlaubt"
            ));
        }
    }
    next.ida.access_client_id = client_id;
    next.ida.access_client_secret = secret;
    app.save_config(next)?;
    r.steps
        .push("Service-Token auf dem Lautsprecher eingetragen, WARP aus".into());
    Ok(())
}

pub async fn auto(app: &Arc<App>, req: Request) -> Report {
    let mut r = Report::default();
    match run(app, req, &mut r).await {
        Ok(()) => r.ok = r.needs.is_empty(),
        Err(e) => r.error = format!("{e:#}"),
    }
    app.log(format!(
        "Cloudflare automatisch: {}",
        if r.ok {
            "fertig".to_string()
        } else if !r.error.is_empty() {
            r.error.clone()
        } else {
            format!("Auswahl nötig ({})", r.needs)
        }
    ));
    r
}

async fn run(app: &Arc<App>, req: Request, r: &mut Report) -> anyhow::Result<()> {
    let token = req.api_token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("API-Token fehlt");
    }
    let cf = Cf {
        http: crate::session::http(),
        token,
    };
    let cfg = app.cfg();

    // 1. Konto
    let accounts = cf.get("/accounts").await?;
    let list = accounts.as_array().cloned().unwrap_or_default();
    let account = if !req.account_id.is_empty() {
        req.account_id.clone()
    } else if list.len() == 1 {
        list[0]["id"].as_str().unwrap_or_default().to_string()
    } else {
        r.needs = "account".into();
        r.choices = list
            .iter()
            .map(|a| json!({"id": a["id"], "name": a["name"]}))
            .collect();
        if list.is_empty() {
            anyhow::bail!("Der API-Token sieht kein Konto");
        }
        return Ok(());
    };
    let a = format!("/accounts/{account}");

    let mode = if req.mode.is_empty() {
        cfg.ida.via.clone()
    } else {
        req.mode.clone()
    };
    if mode == "domain" {
        return domain(app, &cf, &a, r).await;
    }

    // 2. Team-Name
    let org = cf.get(&format!("{a}/access/organizations")).await?;
    let team = org["auth_domain"]
        .as_str()
        .unwrap_or_default()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_string();
    if team.is_empty() {
        anyhow::bail!("Zero Trust ist in diesem Konto noch nicht aktiviert");
    }
    r.steps.push(format!("Team „{team}“ gefunden"));

    // 3. Service-Token (gibt es ihn schon, bekommt er ein neues Secret)
    let (token_id, client_id, client_secret, name) = ensure_token(&cf, &a, &cfg, r).await?;

    // 4. WARP-Anmeldung mit diesem Token erlauben (Policy „Service Auth“ an der WARP-App)
    let apps = list_apps(&cf, &a).await?;
    let warp_app = apps
        .iter()
        .find(|x| x["type"] == "warp")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("WARP-Anmeldung ist in Zero Trust noch nicht eingerichtet (Einstellungen → WARP-Client → Geräteregistrierung)"))?;
    if allow_token(&cf, &a, &warp_app, &token_id, &name).await? {
        r.steps
            .push("WARP-Anmeldung per Service-Token erlaubt".into());
    } else {
        r.steps
            .push("WARP-Anmeldung per Service-Token war schon erlaubt".into());
    }

    // Ab hier kann der Lautsprecher sich schon verbinden
    let mut next = cfg.clone();
    next.cloudflare.enabled = true;
    next.cloudflare.team = team;
    next.cloudflare.client_id = client_id;
    next.cloudflare.client_secret = client_secret;
    next.ida.via = "warp".into();
    app.save_config(next)?;
    r.steps
        .push("Auf dem Lautsprecher eingetragen – WARP verbindet sich".into());

    // 5. IDA durch den Tunnel: Split Tunnel und Route
    let Some(ip) = target_ip(&cfg.ida.url) else {
        r.steps.push("IDA-Adresse ist keine IP – Split Tunnel und Route übersprungen (erst unter IDA eintragen, dann noch einmal)".into());
        return Ok(());
    };
    let policy = cf.get(&format!("{a}/devices/policy")).await?;
    let include = policy["include"].as_array().cloned().unwrap_or_default();
    if !include.is_empty() {
        if include
            .iter()
            .any(|e| e["address"].as_str().is_some_and(|c| contains(c, ip)))
        {
            r.steps
                .push(format!("Split Tunnel: {ip} geht schon durch WARP"));
        } else {
            let mut list = include.clone();
            list.push(json!({"address": format!("{ip}/32"), "description": "ida-speaker → IDA"}));
            cf.put(&format!("{a}/devices/policy/include"), Value::Array(list))
                .await?;
            r.steps
                .push(format!("Split Tunnel: {ip} zur Include-Liste hinzugefügt"));
        }
    } else {
        let exclude = cf.get(&format!("{a}/devices/policy/exclude")).await?;
        let exclude = exclude.as_array().cloned().unwrap_or_default();
        if exclude
            .iter()
            .any(|e| e["address"].as_str().is_some_and(|c| contains(c, ip)))
        {
            let mut list = Vec::new();
            for e in exclude {
                match e["address"].as_str() {
                    Some(c) if contains(c, ip) => {
                        let desc = e["description"].as_str().unwrap_or("").to_string();
                        list.extend(
                            subtract(c, ip)
                                .into_iter()
                                .map(|n| json!({"address": n, "description": desc})),
                        );
                    }
                    _ => list.push(e),
                }
            }
            cf.put(&format!("{a}/devices/policy/exclude"), Value::Array(list))
                .await?;
            r.steps.push(format!(
                "Split Tunnel: {ip} aus der Ausschlussliste genommen"
            ));
        } else {
            r.steps
                .push(format!("Split Tunnel: {ip} geht schon durch WARP"));
        }
    }

    let routes = cf
        .get(&format!(
            "{a}/teamnet/routes?is_deleted=false&per_page=1000"
        ))
        .await?;
    let covered = routes.as_array().and_then(|l| {
        l.iter()
            .find(|x| x["network"].as_str().is_some_and(|c| contains(c, ip)))
            .cloned()
    });
    if let Some(route) = covered {
        r.steps.push(format!(
            "Route: {} über Tunnel „{}“",
            route["network"].as_str().unwrap_or(""),
            route["tunnel_name"].as_str().unwrap_or("")
        ));
    } else if !req.tunnel_id.is_empty() {
        cf.post(
            &format!("{a}/teamnet/routes"),
            json!({"network": format!("{ip}/32"), "tunnel_id": req.tunnel_id, "comment": "ida-speaker → IDA"}),
        )
        .await?;
        r.steps.push(format!("Route {ip}/32 angelegt"));
    } else {
        let tunnels = cf.get(&format!("{a}/cfd_tunnel?is_deleted=false")).await?;
        r.needs = "tunnel".into();
        r.choices = tunnels
            .as_array()
            .map(|l| {
                l.iter()
                    .map(|t| json!({"id": t["id"], "name": t["name"], "status": t["status"]}))
                    .collect()
            })
            .unwrap_or_default();
        r.steps.push(format!(
            "Kein Tunnel führt zu {ip} – bitte den Tunnel zum IDA-Server wählen"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_math() {
        let ip: Ipv4Addr = "172.18.0.5".parse().unwrap();
        assert!(contains("172.16.0.0/12", ip));
        assert!(!contains("192.168.0.0/16", ip));
        assert!(contains("172.18.0.5", ip));
        let rest = subtract("172.16.0.0/12", ip);
        assert_eq!(rest.len(), 20);
        assert!(rest.iter().all(|c| !contains(c, ip)));
        // Alles andere aus dem Netz bleibt abgedeckt
        for probe in ["172.16.0.1", "172.31.255.254", "172.18.0.4", "172.18.0.6"] {
            let p: Ipv4Addr = probe.parse().unwrap();
            assert!(rest.iter().any(|c| contains(c, p)), "{probe}");
        }
        assert_eq!(subtract("10.0.0.0/8", ip), vec!["10.0.0.0/8"]);
    }

    #[test]
    fn access_app_matching() {
        assert!(app_covers("voice.example.com", "voice.example.com"));
        assert!(app_covers(
            "https://voice.example.com/",
            "voice.example.com"
        ));
        assert!(app_covers("voice.example.com/device", "voice.example.com"));
        assert!(app_covers("voice.example.com/live*", "voice.example.com"));
        assert!(app_covers("*.example.com", "voice.example.com"));
        assert!(!app_covers("voice.example.com/telnyx", "voice.example.com"));
        assert!(!app_covers("ida.example.com", "voice.example.com"));
        let app = json!({"domain": "voice.example.com", "destinations": [{"type": "public", "uri": "voice2.example.com"}]});
        assert_eq!(
            app_domains(&app),
            vec!["voice.example.com", "voice2.example.com"]
        );
    }

    #[test]
    fn target_ip_from_url() {
        assert_eq!(
            target_ip("http://172.18.0.5:8000"),
            Some("172.18.0.5".parse().unwrap())
        );
        assert_eq!(target_ip("https://voice.example.com"), None);
        assert_eq!(target_ip(""), None);
    }
}
