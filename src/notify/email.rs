//! Issue emails: HTML (askama, autoescaped) + plain text, monochrome, one deep link.
use super::{Info, base_url, fmt_time, label, one_line};
use askama::Template;

#[derive(Template)]
#[template(path = "email/issue.html")]
struct Html<'a> {
    i: &'a Info,
    label: &'static str,
    first: String,
    last: String,
    settings: String,
}

pub fn subject(i: &Info) -> String {
    format!(
        "[{}] {}: {}",
        one_line(&i.project, 60),
        label(&i.kind),
        one_line(&i.title, 150)
    )
}

pub fn render(i: &Info) -> Result<(String, String), String> {
    let (first, last) = (fmt_time(i.first_seen), fmt_time(i.last_seen));
    let settings = format!("{}/settings/notifications", base_url());
    let text = format!(
        "{}: {}\n{}\n\nLevel: {}\nEvents: {}  Users: {}\nFirst seen: {first}\nLast seen: {last}\nRelease: {}\n\nOpen: {}\n\n--\nChange which emails you get: {settings}\n",
        label(&i.kind),
        i.title,
        i.culprit,
        i.level,
        i.events,
        i.users,
        i.release.as_deref().unwrap_or("-"),
        i.url,
    );
    let html = Html {
        i,
        label: label(&i.kind),
        first,
        last,
        settings,
    }
    .render()
    .map_err(|e| e.to_string())?;
    Ok((text, html))
}

pub async fn send(i: &Info, to: &str) -> Result<(), String> {
    let (text, html) = render(i)?;
    crate::mail::try_send(to, &subject(i), &text, &html).await
}
