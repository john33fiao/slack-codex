# Operations

This document is public-safe. Keep real Slack credentials, local usernames, absolute host paths, service files with real values, runtime DBs, logs, and Codex session data outside the repository.

## Host-Local Files

Each host should keep these files in a private install directory:

- `slack-codex` binary or `slack-codex.exe`
- `.env` with real Slack app tokens, allowed team/user IDs, Codex profile values, DB path, and allowed workspaces
- runtime `sessions.db`
- service-manager files or wrapper scripts containing real host paths
- logs

Use `.env.example` only as a placeholder template.

## Windows Service Registration

Use a Windows service wrapper that can set the working directory to the private install directory. The app loads `.env` from its working directory.

Placeholder shape:

```powershell
# Run from an elevated shell on the target host.
$ServiceName = "SlackCodexHost"
$InstallDir = "<ABSOLUTE_PATH_TO_PRIVATE_INSTALL_DIR>"
$ExePath = "<ABSOLUTE_PATH_TO_SLACK_CODEX_EXE>"

# Register with your chosen service manager or wrapper.
# Configure:
# - executable: $ExePath
# - working directory: $InstallDir
# - startup: automatic or manual, according to host policy
# - restart policy: restart on failure with a bounded retry interval
```

Do not commit the generated service definition if it contains real paths or account names.

## launchd Registration

Use a host-local plist with placeholders replaced only on the target machine.

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>local.slack-codex.host</string>
  <key>ProgramArguments</key>
  <array>
    <string>&lt;ABSOLUTE_PATH_TO_SLACK_CODEX_BINARY&gt;</string>
  </array>
  <key>WorkingDirectory</key>
  <string>&lt;ABSOLUTE_PATH_TO_PRIVATE_INSTALL_DIR&gt;</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
</dict>
</plist>
```

Keep the real plist outside the repository unless all host-local values are replaced with placeholders.

## Manual Smoke Checklist

Run this checklist on each host-specific Slack app before calling v1 ready:

1. Start the host service and confirm the service manager reports it running.
2. Restart the host service and confirm it reconnects without creating a new runtime DB path.
3. Stop the host service, send `/codex-ping`, and record the observed Slack failure signal.
4. Start the service, send `/codex-ping`, and confirm the response contains the expected host identity and uptime.
5. Send `/codex <harmless prompt>` in the host bot DM and confirm one parent thread is created.
6. Confirm the first Codex result appears in that thread.
7. Reply in the same thread and confirm the reply resumes the same Codex session.
8. Send a message outside a registered thread and confirm Codex is not run and the guide message appears.
9. Produce output longer than `CODEX_OUTPUT_MAX_CHARS` and confirm it is attached with the external upload flow.
10. Check logs for token/path leaks before preserving or sharing any output.

## Slack Signals

Treat these signals as authoritative for readiness:

- `/codex-ping` response from the target host bot DM
- service-manager running/restart status on the host
- successful first `/codex` result in the created thread
- successful thread reply resume in the same thread

Treat these as non-authoritative unless verified in the target workspace:

- Slack presence or green-dot state for the bot
- whether the bot appears online immediately after service start
- whether a stopped bot is visually obvious before sending a command
- Socket Mode dispatch behavior inferred from UI presence alone

Record the real observed stopped-bot and presence behavior per workspace during smoke testing.
