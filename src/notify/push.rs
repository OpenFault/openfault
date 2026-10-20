//! Server side of iOS push: POST device tokens to the relay (`openfault relay`, see src/relay.rs).
//! Payload is tokens, title, project, deep link; nothing else leaves the server (PLAN §10).
use super::{Info, label, one_line, webhook::HTTP};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub struct Relay {
    pub url: String,
    pub token: String,
}

/// OPENFAULT_PUSH_RELAY_URL + OPENFAULT_PUSH_RELAY_TOKEN; push is off unless both are set.
pub fn relay() -> Option<Relay> {
    let url = std::env::var("OPENFAULT_PUSH_RELAY_URL")
        .ok()
        .filter(|s| !s.is_empty())?;
    let token = std::env::var("OPENFAULT_PUSH_RELAY_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    Some(Relay {
        url: url.trim_end_matches('/').to_string(),
        token,
    })
}

/// For Settings → Organization: "off", or the relay URL push goes through.
pub fn status() -> String {
    match (
        std::env::var("OPENFAULT_PUSH_RELAY_URL")
            .ok()
            .filter(|s| !s.is_empty()),
        relay(),
    ) {
        (_, Some(r)) => format!("on via {}", r.url),
        (Some(_), None) => "off: OPENFAULT_PUSH_RELAY_TOKEN missing".into(),
        _ => "off".into(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResult {
    pub token: String,
    pub status: u16,
    #[serde(default)]
    pub reason: Option<String>,
}

impl TokenResult {
    pub fn ok(&self) -> bool {
        self.status == 200
    }
    /// The device row should be deleted.
    pub fn dead(&self) -> bool {
        self.status == 410
            || matches!(
                self.reason.as_deref(),
                Some("BadDeviceToken" | "Unregistered")
            )
    }
}

#[derive(Deserialize)]
struct Resp {
    results: Vec<TokenResult>,
}

pub async fn send(r: &Relay, i: &Info, tokens: &[String]) -> Result<Vec<TokenResult>, String> {
    let body = json!({
        "tokens": tokens,
        "title": one_line(&format!("[{}] {}", i.project, label(&i.kind)), 120),
        "body": one_line(&i.title, 180),
        "url": i.url,
        "collapse_id": format!("issue-{}", i.id),
    });
    let res = HTTP
        .post(format!("{}/v1/push", r.url))
        .bearer_auth(&r.token)
        // the relay calls APNs once per token; 5s (the shared default) is too short for a team.
        .timeout(std::time::Duration::from_secs(30))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    Ok(res.json::<Resp>().await.map_err(|e| e.to_string())?.results)
}
