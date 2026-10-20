//! SMTP from the instance settings (src/config.rs) or OPENFAULT_SMTP_URL, which wins:
//! `smtp://user:pass@host:587?from=OpenFault <noreply@x>`; `smtps://` = implicit TLS, `smtp://` = STARTTLS (required).
use crate::{AppState, config, db::Db};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, MultiPart},
    transport::smtp::authentication::Credentials,
};
use std::time::Duration;

pub type Smtp = (AsyncSmtpTransport<Tokio1Executor>, Mailbox);

pub fn parse(url: &str) -> Result<Smtp, String> {
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

/// From the settings form's fields.
pub fn build(s: &config::Smtp) -> Result<Smtp, String> {
    let from: Mailbox = s
        .from
        .trim()
        .parse()
        .map_err(|e| format!("bad from: {e}"))?;
    let host = s.host.trim();
    let b = match s.mode.as_str() {
        "tls" => AsyncSmtpTransport::<Tokio1Executor>::relay(host).map_err(|e| e.to_string())?,
        "none" => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        _ => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host).map_err(|e| e.to_string())?
        }
    };
    let b = if s.port == 0 { b } else { b.port(s.port) };
    let b = if s.username.is_empty() {
        b
    } else {
        b.credentials(Credentials::new(s.username.clone(), s.password.clone()))
    };
    Ok((b.timeout(Some(Duration::from_secs(15))).build(), from))
}

pub fn configured(db: &Db) -> bool {
    config::get(db).smtp.is_some()
}

/// Recipient for logs: the domain only.
fn redact(to: &str) -> String {
    format!("*@{}", to.rsplit_once('@').map_or("?", |x| x.1))
}

/// Debug/test builds without SMTP: messages land here as (to, subject, text, html) instead. Capped at 100.
#[cfg(debug_assertions)]
pub static OUTBOX: std::sync::Mutex<Vec<(String, String, String, String)>> =
    std::sync::Mutex::new(Vec::new());

fn message(
    from: &Mailbox,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> Result<Message, String> {
    Message::builder()
        .from(from.clone())
        .to(to.parse::<Mailbox>().map_err(|e| e.to_string())?)
        .subject(subject)
        .multipart(MultiPart::alternative_plain_html(
            text.to_string(),
            html.to_string(),
        ))
        .map_err(|e| e.to_string())
}

/// Sends and waits for the SMTP result. Every attempt is logged (domain only) and lands in `mail_log`.
pub async fn try_send(
    db: &Db,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> Result<(), String> {
    send(db, to, subject, text, html, Duration::from_secs(60)).await
}

async fn send(
    db: &Db,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
    limit: Duration,
) -> Result<(), String> {
    let cfg = config::get(db);
    let Some((t, from)) = cfg.smtp.as_ref() else {
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
    if let (false, Some(s)) = (cfg.smtp_env, &cfg.stored.smtp)
        && s.mode == "none"
        && !s.username.is_empty()
    {
        tracing::warn!("SMTP security is none: credentials will be sent unencrypted");
    }
    let r = match message(from, to, subject, text, html) {
        Ok(m) => match tokio::time::timeout(limit, t.send(m)).await {
            Ok(r) => r.map(|_| ()).map_err(|e| e.to_string()),
            Err(_) => Err(format!("timed out after {}s", limit.as_secs())),
        },
        Err(e) => Err(e),
    };
    let rcpt = redact(to);
    match &r {
        Ok(()) => tracing::info!(to = %rcpt, subject, "mail sent"),
        Err(e) => tracing::error!(to = %rcpt, subject, "mail failed: {e}"),
    }
    let (subject, err) = (subject.to_string(), r.clone().err());
    let _ = db
        .write(move |c| {
            c.execute(
                "INSERT INTO mail_log (to_domain, subject, error, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![rcpt, subject, err, crate::db::now()],
            )
        })
        .await;
    r
}

/// Settings "Send test email": an error string when SMTP is off or the server refuses.
pub async fn send_test(db: &Db, to: &str) -> Result<(), String> {
    let cfg = config::get(db);
    if cfg.smtp.is_none() {
        return Err(cfg
            .smtp_error
            .clone()
            .unwrap_or_else(|| "SMTP not configured".into()));
    }
    let text = "This is a test email from OpenFault. SMTP works.";
    let html = format!("<p>{text}</p>");
    send(
        db,
        to,
        "OpenFault test email",
        text,
        &html,
        Duration::from_secs(10),
    )
    .await
}

/// Fire-and-forget: never blocks the request, logs failures.
pub async fn send_mail(state: &AppState, to: &str, subject: &str, text: &str, html: &str) {
    if !configured(&state.db) {
        return;
    }
    let (to, subject, text, html) = (
        to.to_string(),
        subject.to_string(),
        text.to_string(),
        html.to_string(),
    );
    let db = state.db.clone();
    tokio::spawn(async move {
        let _ = try_send(&db, &to, &subject, &text, &html).await;
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
