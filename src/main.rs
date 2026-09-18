//! Fake email catcher for 1KM dev (Mailhog-style): bare-bones SMTP
//! listener + web inbox. Ephemeral by design — bounded in-memory store,
//! restarts wipe it. No auth, no TLS, never exposed publicly.
//!
//! SMTP implements just enough for test clients (swaks, nodemailer,
//! Python smtplib): EHLO/HELO, MAIL FROM, RCPT TO, DATA (dot-stuffed,
//! headers + text body), RSET, NOOP, QUIT.
//!
//! ```text
//! :1025 (SMTP_PORT)   mail intake
//! :8080 (PORT)        GET /api/messages · GET /healthz · GET / (htmx web UI)
//!                     GET /rows · GET /stats · POST /clear (UI partials)
//! ```

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use axum::{
    Json, Router,
    extract::State,
    response::Html,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use uuid::Uuid;

/// Inbox cap: newest wins, oldest evicted (ephemeral dev store).
const MAX_KEPT: usize = 1000;

#[derive(Debug, Clone, Serialize)]
struct Email {
    id: Uuid,
    from: String,
    to: Vec<String>,
    subject: String,
    body: String,
    received_at: DateTime<Utc>,
}

#[derive(Clone)]
struct AppState {
    inbox: Arc<RwLock<Vec<Email>>>,
}

fn store(state: &AppState, from: String, to: Vec<String>, raw: &str) {
    let (subject, body) = parse_message(raw);
    let email = Email {
        id: Uuid::new_v4(),
        from,
        to,
        subject,
        body,
        received_at: Utc::now(),
    };
    state
        .inbox
        .write()
        .map(|mut g| {
            g.push(email.clone());
            let excess = g.len().saturating_sub(MAX_KEPT);
            if excess > 0 {
                g.drain(..excess);
            }
        })
        .unwrap_or_else(|_| tracing::error!("inbox poisoned"));
    tracing::info!(to = ?email.to, subject = %email.subject, "fake email received");
}

/// Split raw DATA into Subject header + text body. Folding, MIME parts
/// and encodings are out of scope: first `Subject:` wins, body is the
/// raw text after the header block (dot-unstuffed by the reader).
fn parse_message(raw: &str) -> (String, String) {
    let mut headers = HashMap::new();
    let mut lines = raw.lines().peekable();
    // Header block ends at the first blank line; continuation lines
    // (leading whitespace) append to the previous header.
    let mut last_key: Option<String> = None;
    for line in &mut lines {
        if line.trim().is_empty() {
            break;
        }
        if (line.starts_with(' ') || line.starts_with('\t')) && last_key.is_some() {
            let key = last_key.clone().unwrap();
            headers
                .entry(key)
                .and_modify(|v: &mut String| {
                    v.push(' ');
                    v.push_str(line.trim());
                })
                .or_insert_with(|| line.trim().to_string());
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            last_key = Some(k.trim().to_ascii_lowercase());
            headers.insert(last_key.clone().unwrap(), v.trim().to_string());
        }
    }
    let subject = headers.remove("subject").unwrap_or_default();
    let body: String = lines.collect::<Vec<_>>().join("\n");
    (subject, body.trim().to_string())
}

async fn handle_smtp(stream: TcpStream, state: AppState) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    async fn reply(writer: &mut tokio::net::tcp::OwnedWriteHalf, s: &str) -> std::io::Result<()> {
        writer.write_all(format!("{s}\r\n").as_bytes()).await?;
        writer.flush().await
    }

    if reply(&mut writer, "220 fake-email-server ESMTP")
        .await
        .is_err()
    {
        return;
    }
    let mut from = String::new();
    let mut to: Vec<String> = Vec::new();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            _ => return,
        };
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            // Single-line greeting (multi-line 250- replies desync
            // naive test clients; real senders accept either).
            if reply(&mut writer, "250 Hello").await.is_err() {
                return;
            }
        } else if upper.starts_with("MAIL FROM:") {
            from = addr(&line);
            to.clear();
            if reply(&mut writer, "250 OK").await.is_err() {
                return;
            }
        } else if upper.starts_with("RCPT TO:") {
            to.push(addr(&line));
            if reply(&mut writer, "250 OK").await.is_err() {
                return;
            }
        } else if upper == "DATA" {
            if reply(&mut writer, "354 End with . on its own line")
                .await
                .is_err()
            {
                return;
            }
            let mut data = String::new();
            loop {
                match lines.next_line().await {
                    Ok(Some(l)) => {
                        if l == "." {
                            break;
                        }
                        // Dot-unstuff (RFC 5321 §4.5.2).
                        data.push_str(l.strip_prefix('.').unwrap_or(&l));
                        data.push('\n');
                    }
                    _ => return,
                }
            }
            store(
                &state,
                std::mem::take(&mut from),
                std::mem::take(&mut to),
                &data,
            );
            if reply(&mut writer, "250 OK queued").await.is_err() {
                return;
            }
        } else if upper == "RSET" {
            from.clear();
            to.clear();
            if reply(&mut writer, "250 OK").await.is_err() {
                return;
            }
        } else if upper == "NOOP" {
            if reply(&mut writer, "250 OK").await.is_err() {
                return;
            }
        } else if upper == "QUIT" {
            let _ = reply(&mut writer, "221 Bye").await;
            return;
        } else if reply(&mut writer, "502 Command not implemented")
            .await
            .is_err()
        {
            return;
        }
    }
}

/// `<addr>` brackets stripped, surrounding whitespace trimmed.
fn addr(line: &str) -> String {
    line.split_once(':')
        .map(|(_, v)| {
            v.trim()
                .trim_matches(|c| c == '<' || c == '>')
                .trim()
                .to_string()
        })
        .unwrap_or_default()
}

async fn messages(State(state): State<AppState>) -> Json<serde_json::Value> {
    let mut items = state.inbox.read().map(|g| g.clone()).unwrap_or_default();
    items.sort_by_key(|m| std::cmp::Reverse(m.received_at));
    Json(serde_json::json!({ "data": items, "total": items.len() }))
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

/// HTML-escape user-controlled text (message content lands in the UI).
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Debug, serde::Deserialize)]
struct RowsQuery {
    #[serde(default)]
    q: Option<String>,
}

/// Newest-first inbox with an optional substring filter over
/// to/subject/from/body (case-insensitive).
fn filtered(state: &AppState, q: &RowsQuery) -> Vec<Email> {
    let mut items = state.inbox.read().map(|g| g.clone()).unwrap_or_default();
    items.sort_by_key(|m| std::cmp::Reverse(m.received_at));
    let needle = q.q.as_deref().map(str::to_lowercase).unwrap_or_default();
    items
        .into_iter()
        .filter(|m| {
            needle.is_empty()
                || m.to.iter().any(|t| t.to_lowercase().contains(&needle))
                || m.subject.to_lowercase().contains(&needle)
                || m.from.to_lowercase().contains(&needle)
                || m.body.to_lowercase().contains(&needle)
        })
        .collect()
}

fn short_subject(subject: &str) -> String {
    if subject.is_empty() {
        "(no subject)".to_string()
    } else {
        subject.to_string()
    }
}

fn rows_html(items: &[Email]) -> String {
    if items.is_empty() {
        return "<tr><td colspan=\"3\" class=\"empty\">No mail yet — point an SMTP client at :1025 and it lands here.</td></tr>"
            .to_string();
    }
    let mut out = String::new();
    for m in items {
        out.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td class=\"at\">{}</td></tr><tr class=\"detail\"><td colspan=\"3\"><details><summary>from: {}</summary><div class=\"full\">{}</div><div class=\"meta\">id: {}</div></details></td></tr>",
            esc(&m.to.join(", ")),
            esc(&short_subject(&m.subject)),
            m.received_at.format("%d %b %H:%M:%S"),
            esc(&m.from),
            esc(&m.body),
            m.id,
        ));
    }
    out
}

fn stats_html(state: &AppState) -> String {
    let n = state.inbox.read().map(|g| g.len()).unwrap_or(0);
    let plural = if n == 1 { "message" } else { "messages" };
    format!("<strong>{n}</strong> {plural}")
}

async fn rows(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<RowsQuery>,
) -> Html<String> {
    Html(rows_html(&filtered(&state, &q)))
}

async fn stats(State(state): State<AppState>) -> Html<String> {
    Html(stats_html(&state))
}

async fn clear(State(state): State<AppState>) -> Html<String> {
    state
        .inbox
        .write()
        .map(|mut g| g.clear())
        .unwrap_or_else(|_| tracing::error!("inbox poisoned"));
    tracing::info!("inbox cleared from web UI");
    Html(rows_html(&[]))
}

async fn htmx_js() -> (
    [(axum::http::header::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
        include_str!("../htmx.min.js"),
    )
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>fake-email-server inbox</title>
<script src="/htmx.min.js"></script>
<style>
:root { color-scheme: light dark; }
body { font-family: system-ui, sans-serif; max-width: 860px; margin: 2rem auto; padding: 0 1rem; }
header { display: flex; align-items: baseline; gap: 1rem; flex-wrap: wrap; }
header h1 { margin: 0; font-size: 1.4rem; }
#stats { color: #666; font-size: .9rem; }
form#filters { display: flex; gap: .5rem; margin: 1rem 0; flex-wrap: wrap; }
input[type=search] { font: inherit; padding: .4rem .6rem; border: 1px solid #ccc; border-radius: 6px; flex: 1; min-width: 200px; }
table { border-collapse: collapse; width: 100%; }
th, td { border: 1px solid #ccc; padding: .4rem .6rem; text-align: left; font-size: .9rem; vertical-align: top; }
td.at { white-space: nowrap; }
td.empty { text-align: center; color: #666; padding: 2rem; }
tr.detail td { background: rgba(128,128,128,.08); }
details summary { cursor: pointer; color: #666; font-size: .8rem; }
.full { white-space: pre-wrap; margin-top: .4rem; }
.meta { color: #666; font-size: .75rem; margin-top: .4rem; }
.toolbar { display: flex; justify-content: space-between; align-items: center; margin-top: 1rem; color: #666; font-size: .8rem; }
button.danger { font: inherit; padding: .4rem .8rem; border-radius: 6px; border: 1px solid #c00; background: none; color: #c00; cursor: pointer; }
</style>
</head>
<body>
<header>
<h1>fake-email-server inbox</h1>
<div id="stats" hx-get="/stats" hx-trigger="load, every 3s" hx-swap="innerHTML">…</div>
</header>
<form id="filters" hx-get="/rows" hx-target="#rows" hx-swap="innerHTML"
      hx-trigger="load, every 3s, keyup changed delay:300ms from:#q">
<input id="q" name="q" type="search" placeholder="Search to / subject / from / body…" autocomplete="off">
</form>
<table>
<thead><tr><th>To</th><th>Subject</th><th>At</th></tr></thead>
<tbody id="rows"></tbody>
</table>
<div class="toolbar">
<span>Ephemeral — restarts wipe the inbox. SMTP on :1025.</span>
<button class="danger" hx-post="/clear" hx-confirm="Delete all messages?" hx-target="#rows" hx-swap="innerHTML">Clear inbox</button>
</div>
</body>
</html>"##;

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

fn http_router(state: AppState) -> Router {
    Router::new()
        .route("/api/messages", get(messages))
        .route("/rows", get(rows))
        .route("/stats", get(stats))
        .route("/clear", post(clear))
        .route("/htmx.min.js", get(htmx_js))
        .route("/healthz", get(healthz))
        .route("/", get(index))
        .with_state(state)
}

async fn serve_smtp(listener: TcpListener, state: AppState) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%e, "smtp accept failed");
                continue;
            }
        };
        tokio::spawn(handle_smtp(stream, state.clone()));
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let host = std::env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let http_port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8080);
    let smtp_port: u16 = std::env::var("SMTP_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1025);
    let state = AppState {
        inbox: Arc::new(RwLock::new(Vec::new())),
    };
    let smtp = TcpListener::bind(format!("{host}:{smtp_port}"))
        .await
        .expect("bind smtp");
    let http = tokio::net::TcpListener::bind(format!("{host}:{http_port}"))
        .await
        .expect("bind http");
    tracing::info!("fake-email-server smtp on {host}:{smtp_port}, http on {host}:{http_port}");
    tokio::join!(serve_smtp(smtp, state.clone()), async {
        axum::serve(http, http_router(state))
            .await
            .expect("serve http");
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    fn state() -> AppState {
        AppState {
            inbox: Arc::new(RwLock::new(Vec::new())),
        }
    }

    async fn smtp_port(state: AppState) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_smtp(listener, state));
        port
    }

    /// Speak SMTP the way swaks/smtplib do: commands + DATA block.
    async fn send_mail(port: u16, from: &str, to: &[&str], subject: &str, body: &str) {
        let stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        assert!(lines.next_line().await.unwrap().unwrap().starts_with("220"));
        async fn cmd(
            writer: &mut tokio::net::tcp::OwnedWriteHalf,
            lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
            s: &str,
        ) -> String {
            writer
                .write_all(format!("{s}\r\n").as_bytes())
                .await
                .unwrap();
            lines.next_line().await.unwrap().unwrap()
        }
        assert!(
            cmd(&mut writer, &mut lines, "EHLO test")
                .await
                .starts_with("250")
        );
        assert!(
            cmd(&mut writer, &mut lines, &format!("MAIL FROM:<{from}>"))
                .await
                .starts_with("250")
        );
        for rcpt in to {
            assert!(
                cmd(&mut writer, &mut lines, &format!("RCPT TO:<{rcpt}>"))
                    .await
                    .starts_with("250")
            );
        }
        assert!(
            cmd(&mut writer, &mut lines, "DATA")
                .await
                .starts_with("354")
        );
        writer
            .write_all(
                format!("From: {from}\r\nSubject: {subject}\r\n\r\n{body}\r\n.\r\n").as_bytes(),
            )
            .await
            .unwrap();
        assert!(lines.next_line().await.unwrap().unwrap().starts_with("250"));
        assert!(
            cmd(&mut writer, &mut lines, "QUIT")
                .await
                .starts_with("221")
        );
    }

    #[tokio::test]
    async fn full_send_lands_parsed() {
        let st = state();
        let port = smtp_port(st.clone()).await;
        send_mail(
            port,
            "desk@1km.test",
            &["ops@1km.test", "boss@1km.test"],
            "Weekly dues",
            "Hello\nworld",
        )
        .await;
        let inbox = st.inbox.read().unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].from, "desk@1km.test");
        assert_eq!(inbox[0].to, vec!["ops@1km.test", "boss@1km.test"]);
        assert_eq!(inbox[0].subject, "Weekly dues");
        assert_eq!(inbox[0].body, "Hello\nworld");
    }

    #[tokio::test]
    async fn rset_clears_envelope_and_unknown_rejected() {
        let st = state();
        let port = smtp_port(st.clone()).await;
        let stream = TcpStream::connect(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        assert!(lines.next_line().await.unwrap().unwrap().starts_with("220"));
        async fn cmd(
            writer: &mut tokio::net::tcp::OwnedWriteHalf,
            lines: &mut tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
            s: &str,
        ) -> String {
            writer
                .write_all(format!("{s}\r\n").as_bytes())
                .await
                .unwrap();
            lines.next_line().await.unwrap().unwrap()
        }
        assert!(
            cmd(&mut writer, &mut lines, "MAIL FROM:<a@x>")
                .await
                .starts_with("250")
        );
        assert!(
            cmd(&mut writer, &mut lines, "RSET")
                .await
                .starts_with("250")
        );
        assert!(
            cmd(&mut writer, &mut lines, "FROBNICATE")
                .await
                .starts_with("502")
        );
        assert!(
            cmd(&mut writer, &mut lines, "QUIT")
                .await
                .starts_with("221")
        );
        assert!(st.inbox.read().unwrap().is_empty());
    }

    #[test]
    fn parse_message_splits_subject_and_body() {
        let (s, b) = parse_message("Subject: Hi\r\nX-A: 1\r\n\r\nline1\nline2\n");
        assert_eq!(s, "Hi");
        assert_eq!(b, "line1\nline2");
        let (s, b) = parse_message("No-Headers: x\r\n\r\njust body");
        assert_eq!(s, "");
        assert_eq!(b, "just body");
        let (s, _) = parse_message("Subject: Re: long\r\n thing\r\n\r\nb");
        assert_eq!(s, "Re: long thing");
    }

    #[test]
    fn inbox_caps_newest_and_addr_trims_brackets() {
        let st = state();
        for i in 0..(MAX_KEPT + 3) {
            store(
                &st,
                format!("{i}@x"),
                vec![],
                &format!("Subject: s{i}\n\nb"),
            );
        }
        let inbox = st.inbox.read().unwrap();
        assert_eq!(inbox.len(), MAX_KEPT);
        assert_eq!(inbox.last().unwrap().from, format!("{}@x", MAX_KEPT + 2));
        assert_eq!(addr("RCPT TO:<ops@1km.test>"), "ops@1km.test");
        assert_eq!(addr("MAIL FROM: boss@1km.test"), "boss@1km.test");
    }

    #[tokio::test]
    async fn ui_rows_search_clear_and_stats() {
        use axum_test::TestServer;
        let server = || {
            TestServer::new(http_router(AppState {
                inbox: Arc::new(RwLock::new(Vec::new())),
            }))
            .unwrap()
        };
        let s = server();
        let idx = s.get("/").await;
        idx.assert_status_ok();
        idx.assert_text_contains("htmx.min.js");
        s.get("/htmx.min.js").await.assert_status_ok();

        store(
            &AppState {
                inbox: Arc::new(RwLock::new(Vec::new())),
            },
            "a@x".to_string(),
            vec![],
            "Subject: x\n\nb",
        );
        // Fresh server is empty → empty state renders.
        let empty = s.get("/rows").await;
        empty.assert_text_contains("No mail yet");

        // Seed through the shared state instead: rebuild with content.
        let st = state();
        store(
            &st,
            "ops@1km.test".to_string(),
            vec!["boss@1km.test".to_string()],
            "Subject: Weekly dues\n\nHello",
        );
        store(
            &st,
            "spam@x".to_string(),
            vec![],
            "Subject: x\n\n<b>bold</b>",
        );
        let s2 = TestServer::new(http_router(st)).unwrap();
        let rows = s2.get("/rows").await;
        rows.assert_text_contains("Weekly dues");
        let q = s2.get("/rows?q=dues").await;
        q.assert_text_contains("Weekly dues");
        let none = s2.get("/rows?q=zzz-no-match").await;
        none.assert_text_contains("No mail yet");
        // Bodies are HTML-escaped (XSS-safe).
        let esc = s2.get("/rows?q=bold").await;
        let html = esc.text();
        assert!(html.contains("&lt;b&gt;"));
        assert!(!html.contains("<b>bold</b>"));
        let stats = s2.get("/stats").await;
        stats.assert_text_contains("2</strong> messages");
        let cleared = s2.post("/clear").await;
        cleared.assert_text_contains("No mail yet");
    }
}
