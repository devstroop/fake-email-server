# fake-email-server

Mailhog-style fake SMTP catcher for 1KM development. No auth, no TLS,
ephemeral by design — restarts wipe the inbox. Never expose publicly.

```text
:1025 (SMTP_PORT)   mail intake (EHLO/MAIL/RCPT/DATA/RSET/NOOP/QUIT)
:8080 (PORT)        GET /api/messages · GET /healthz · GET / (web UI)
```

## Run

```bash
cargo run --release                       # SMTP :1025, HTTP :8080
docker build -t fake-email . && docker run -p 8080:8080 -p 1025:1025 fake-email
```

Send test mail with anything speaking SMTP:

```bash
swaks --to ops@1km.test --from desk@1km.test \
  --header "Subject: Weekly dues" --body "Hello" \
  --server localhost --port 1025
```

Then open http://localhost:8080/ — everything lands there.

## 1KM server integration

None yet: the server sends no email today. When it does (password
resets, staff notifications), point it at `fake-email:1025` in compose
and verify in this inbox.
