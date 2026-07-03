# Architecture

## Summary

Slack Codex is a local bridge between Slack DM threads and local Codex sessions. The product goal is convenience, not a hosted multi-user automation service.

## Runtime Shape

```text
slack-codex
├── Socket Mode receiver
├── slash command handlers: /codex, /codex-ping
├── DM thread message handler
├── SQLite state: sessions, processed_events
├── per-session execution lock
├── Codex child process runner
└── Slack result publisher
```

## Core Invariants

- One Slack thread maps to one Codex session.
- Messages outside a registered thread never resume a session.
- The host is selected by choosing the bot DM, not by automatic routing.
- The same Slack input must be processed at most once.
- Slack credentials must never reach the Codex child process.

## State Model

```sql
CREATE TABLE sessions (
  thread_ts   TEXT PRIMARY KEY,
  session_id  TEXT NOT NULL,
  status      TEXT NOT NULL DEFAULT 'idle',
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

CREATE TABLE processed_events (
  event_key   TEXT PRIMARY KEY,
  thread_ts   TEXT,
  source      TEXT NOT NULL,
  created_at  TEXT NOT NULL
);
```

Event key priority:

1. Socket Mode `envelope_id`
2. Events API `event_id`
3. Slack message `client_msg_id`
4. fallback `channel_id:ts:user_id`

## Security Boundary

The Slack bot process owns Slack tokens. Codex child processes must start from a cleared environment and receive only an explicit allowlist. Host-specific paths are validated by canonical path comparison against `CODEX_ALLOWED_WORKSPACES`.

Blocked from repository and child process by default:

- `SLACK_BOT_TOKEN`
- `SLACK_APP_TOKEN`
- `SLACK_SIGNING_SECRET`
- runtime DB paths containing private machine details
- local logs and event dumps

## Failure Model

v1 uses best-effort response after receipt. If the bot receives and acknowledges an event, it should try to post either success or failure to the same thread. If the process dies immediately after ack, v1 accepts that as part of the failure budget. Durable queues are v2 scope.

## Public Repository Policy

The repository may describe architecture and contain implementation code, but it must not contain host-local operational values. All real operational state belongs in untracked `.env`, local service manager config, and runtime data directories.
