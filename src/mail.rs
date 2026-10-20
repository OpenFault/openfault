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
    Ok((t.build(), from))
}

pub fn configured() -> bool {
    SMTP.is_some()
}

/// Fire-and-forget: never blocks the request, logs failures.
pub async fn send_mail(_state: &AppState, to: &str, subject: &str, text: &str, html: &str) {
    let Some((t, from)) = SMTP.as_ref() else {
        return;
    };
    let msg = to
        .parse::<Mailbox>()
        .map_err(|e| e.to_string())
        .and_then(|to| {
            Message::builder()
                .from(from.clone())
                .to(to)
                .subject(subject)
                .multipart(MultiPart::alternative_plain_html(
                    text.to_string(),
                    html.to_string(),
                ))
                .map_err(|e| e.to_string())
        });
    let t = t.clone();
    tokio::spawn(async move {
        match msg {
            Ok(m) => {
                if let Err(e) = t.send(m).await {
                    tracing::error!("smtp send failed: {e}");
                }
            }
            Err(e) => tracing::error!("bad mail: {e}"),
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
