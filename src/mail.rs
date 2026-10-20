//! SMTP via OPENFAULT_SMTP_URL, e.g. `smtp://user:pass@host:587?from=OpenFault <noreply@x>`.
//! `smtps://` = implicit TLS, `smtp://` = STARTTLS (required).
use crate::AppState;
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
};
use std::sync::LazyLock;

type Smtp = (AsyncSmtpTransport<Tokio1Executor>, Mailbox);

static SMTP: LazyLock<Option<Smtp>> = LazyLock::new(|| {
    let url = std::env::var("OPENFAULT_SMTP_URL").ok()?;
    match parse(&url) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!("OPENFAULT_SMTP_URL ignored: {e}");
            None
        }
    }
});

fn parse(url: &str) -> Result<Smtp, String> {
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let from = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("from="))
        .ok_or("missing ?from=")?;
    // ponytail: `from` taken raw (spaces/<> allowed); only %20 and + are decoded.
    let from: Mailbox = from
        .replace("%20", " ")
        .replace('+', " ")
        .parse()
        .map_err(|e| format!("bad from: {e}"))?;
    // lettre's own URL parser handles host/port/credentials; we only map the TLS mode.
    let url = if base.starts_with("smtp://") {
        format!("{base}?tls=required")
    } else {
        base.to_string()
    };
    let t = AsyncSmtpTransport::<Tokio1Executor>::from_url(&url).map_err(|e| e.to_string())?;
    // lettre's default is 60s per command; one dead server would stall the notification worker.
    let t = t.timeout(Some(std::time::Duration::from_secs(15)));
    Ok((t.build(), from))
}

pub fn configured() -> bool {
    SMTP.is_some()
}

/// Debug/test builds without SMTP: messages land here as (to, subject, text, html) instead. Capped at 100.
#[cfg(debug_assertions)]
pub static OUTBOX: std::sync::Mutex<Vec<(String, String, String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Sends and waits for the SMTP result (notification worker records failures).
pub async fn try_send(to: &str, subject: &str, text: &str, html: &str) -> Result<(), String> {
    let Some((t, from)) = SMTP.as_ref() else {
        #[cfg(debug_assertions)]
        {
            let mut o = OUTBOX.lock().unwrap();
            if o.len() < 100 {
                o.push((to.into(), subject.into(), text.into(), html.into()));
            }
            return Ok(());
        }
        #[cfg(not(debug_assertions))]
        return Err("SMTP not configured".into());
    };
    let to = to.parse::<Mailbox>().map_err(|e| e.to_string())?;
    let m = Message::builder()
        .from(from.clone())
        .to(to)
        .subject(subject)
        .multipart(MultiPart::alternative_plain_html(
            text.to_string(),
            html.to_string(),
        ))
        .map_err(|e| e.to_string())?;
    t.send(m).await.map(|_| ()).map_err(|e| e.to_string())
}

/// Fire-and-forget: never blocks the request, logs failures.
pub async fn send_mail(_state: &AppState, to: &str, subject: &str, text: &str, html: &str) {
    if !configured() {
        return;
    }
    let (to, subject, text, html) = (
        to.to_string(),
        subject.to_string(),
        text.to_string(),
        html.to_string(),
    );
    tokio::spawn(async move {
        if let Err(e) = try_send(&to, &subject, &text, &html).await {
            tracing::error!("smtp send failed: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_url() {
        let (_, from) =
            super::parse("smtp://u:p@mail.example.com:587?from=OpenFault <noreply@x.io>").unwrap();
        assert_eq!(from.email.to_string(), "noreply@x.io");
        assert!(super::parse("smtps://u:p@mail.example.com").is_err());
    }
}
