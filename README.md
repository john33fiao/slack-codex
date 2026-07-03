# Slack Codex

Slack Codex는 개인용 Slack DM을 로컬 Codex 비동기 작업 콘솔로 쓰기 위한 Rust 단일 바이너리입니다. Slack에서 host bot DM에 메시지를 보내면 이 호스트의 Codex CLI가 실행되고, 결과는 같은 Slack thread로 돌아옵니다.

v1은 단일 사용자와 수동 호스트 선택에 집중합니다. 여러 호스트를 쓰는 경우 호스트마다 별도 Slack app/bot을 만들고, 사용자는 원하는 호스트의 bot DM을 직접 선택합니다.

## 현재 구현 상태

현재 바이너리는 다음 v1 흐름을 구현합니다.

- Slack Socket Mode 연결
- `/codex-ping` host/uptime 응답
- host bot DM의 top-level 일반 메시지 또는 `/codex <prompt>`로 새 Codex session 시작
- 생성된 Slack thread와 Codex session ID 매핑
- 같은 Slack thread의 후속 메시지를 `codex exec resume`으로 재개
- SQLite 기반 session/event idempotency 저장
- session별 resume mutex
- Codex child process `env_clear`와 allowlist 기반 환경변수 주입
- 허용 workspace canonical path 검증
- 짧은 출력은 Slack message, 긴 출력은 external file upload

실제 Slack app dispatch, Slack presence, host service manager 동작, Codex CLI 인증 상태는 각 호스트에서 manual smoke test로 확인해야 합니다.

## 준비물

- Rust 1.75 이상과 Cargo
- 대상 호스트에서 로그인/인증된 `codex` CLI
- Socket Mode가 켜진 host 전용 Slack app
- Slack slash command 2개: `/codex`, `/codex-ping`
- Slack DM message event 수신 설정
- Slack message posting과 external file upload에 필요한 bot 권한
- 이 바이너리가 실행할 수 있는 host-local workspace 경로

Slack token, app token, signing secret, 실제 team/user ID, Codex profile 경로, workspace 절대경로, runtime DB, 로그는 공개 저장소에 넣지 않습니다. 실제 값은 host-local `.env`와 service manager 설정에만 둡니다.

## 로컬 개발 세팅

저장소를 받은 뒤 placeholder `.env`를 복사합니다.

```powershell
Copy-Item .env.example .env
```

`.env`의 placeholder를 실제 host-local 값으로 바꿉니다. 배포 실행 시에는 실행파일 옆 `.env`를 우선 로드하고, 없으면 현재 작업 디렉터리 또는 상위 디렉터리의 `.env`를 찾습니다.

```dotenv
SLACK_BOT_TOKEN=xoxb-...
SLACK_APP_TOKEN=xapp-...
SLACK_ALLOWED_TEAM_ID=T...
SLACK_ALLOWED_USER_IDS=U...
BOT_HOSTNAME=desk-host

MAX_SESSION_TIMEOUT_SECS=600
CODEX_CLI_PATH=codex
CODEX_OUTPUT_MAX_CHARS=39000

CODEX_CHILD_ENV_ALLOWLIST=HOME,PATH,USER,SHELL,CODEX_HOME,CODEX_PROFILE_ROOT,CODEX_ALLOWED_WORKSPACES
CODEX_PROFILE_ROOT='C:\path\to\codex-profile'
CODEX_DEFAULT_WORKSPACE='C:\workspace\repo-a'
CODEX_ALLOWED_WORKSPACES='C:\workspace\repo-a;C:\workspace\repo-b'
QUEUE_DB_PATH='.\data\sessions.db'

RUST_LOG=info
```

메모:

- `SLACK_ALLOWED_USER_IDS`는 쉼표로 여러 user ID를 넣을 수 있습니다.
- `CODEX_CLI_PATH`는 기본값이 `codex`입니다. Windows 서비스에서 PATH 탐색이 불안정하면 실제 `codex.exe` 절대경로를 넣습니다.
- `CODEX_ALLOWED_WORKSPACES`는 `;` 또는 `,`로 여러 경로를 넣을 수 있습니다.
- Windows 경로처럼 `\`가 들어간 `.env` 값은 single quote로 감싸야 합니다.
- `CODEX_DEFAULT_WORKSPACE`를 설정하면 `/codex`에서 workspace를 지정하지 않아도 해당 workspace에서 시작합니다.
- `CODEX_DEFAULT_WORKSPACE`를 설정하지 않으면 `/codex`에서 workspace를 지정하지 않았을 때 바이너리의 현재 작업 디렉터리를 사용합니다. service working directory도 허용 workspace 안에 두는 편이 안전합니다.
- 기본 workspace와 요청 workspace는 모두 `CODEX_ALLOWED_WORKSPACES` 안에 있어야 합니다.
- 요청한 workspace와 허용 root는 canonical path로 비교합니다. 존재하지 않는 경로는 허용되지 않습니다.
- Slack token 변수는 `CODEX_CHILD_ENV_ALLOWLIST`에 실수로 넣어도 Codex child process로 전달되지 않습니다.

## 빌드와 실행

개발 중 빠른 확인:

```powershell
cargo fmt --all
cargo test
cargo build
```

로컬에서 직접 실행:

```powershell
cargo run
```

배포용 바이너리 빌드:

```powershell
cargo build --release
```

Windows에서는 `target\release\slack-codex.exe`, macOS/Linux에서는 `target/release/slack-codex`를 host-local install directory에 두고 실행합니다. service 등록 예시는 `docs/OPERATIONS.md`를 참고하세요.

## Slack에서 사용하기

호스트 연결 확인:

```text
/codex-ping
```

정상 응답은 다음 형태입니다.

```text
pong from <BOT_HOSTNAME> (uptime <seconds>s)
```

새 Codex 작업 시작:

```text
README의 세팅 절차를 점검해줘
```

특정 workspace에서 새 작업 시작:

```text
--workspace C:\workspace\repo-a README를 최신화해줘
--cd=C:\workspace\repo-a cargo test 실패 원인을 찾아줘
/codex --workspace C:\workspace\repo-a README를 최신화해줘
/codex --cd=C:\workspace\repo-a cargo test 실패 원인을 찾아줘
```

첫 top-level DM 메시지는 그 자체가 parent message가 되고, Codex 결과는 그 message의 thread에 게시됩니다. `/codex` 요청은 호환을 위해 parent message를 만들어 같은 방식으로 결과를 게시합니다. 이후 같은 작업을 이어가려면 그 thread 안에서 일반 메시지로 답하면 됩니다.

```text
좋아. 이제 실패한 테스트만 고쳐줘
```

제약:

- workspace 선택은 새 session을 시작할 때만 가능합니다.
- 등록되지 않은 thread reply나 DM 바깥 메시지는 기존 Codex session을 재개하거나 새 session을 만들지 않습니다.
- `CODEX_OUTPUT_MAX_CHARS`보다 긴 결과는 `codex-output.txt` file upload로 게시됩니다.

## 운영 smoke test

각 host-specific Slack app마다 최소한 다음을 확인한 뒤 v1 ready로 봅니다.

```powershell
cargo fmt --all
cargo test
cargo build
```

Slack/서비스 쪽 manual smoke:

1. host service를 시작하고 service manager에서 running 상태를 확인합니다.
2. service restart 후 같은 `QUEUE_DB_PATH`로 다시 연결되는지 확인합니다.
3. `/codex-ping`이 기대한 `BOT_HOSTNAME`과 uptime을 반환하는지 확인합니다.
4. `/codex <harmless prompt>`가 하나의 parent thread와 결과 reply를 만드는지 확인합니다.
5. 같은 thread의 후속 메시지가 같은 Codex session을 resume하는지 확인합니다.
6. 긴 출력을 만들어 external file upload가 thread에 붙는지 확인합니다.
7. service를 멈춘 상태에서 Slack이 어떤 실패 신호를 보이는지 기록합니다.
8. 로그와 공유 산출물에 token, 개인 경로, session dump가 없는지 확인합니다.

Slack presence나 bot green-dot 상태는 workspace마다 다를 수 있으므로 readiness 근거로 단정하지 않습니다. 자세한 운영 체크리스트는 `docs/OPERATIONS.md`에 있습니다.

## 공개 문서

- `docs/ARCHITECTURE.md`: v1 runtime shape, state model, security boundary
- `docs/OPERATIONS.md`: host-local service 등록 원칙과 manual smoke checklist

Agent instructions, repo-local skills, TODO/WBS/ticket, reference notes, and historical planning docs may exist locally but are not required for public runtime setup.

## 보안 규칙

다음 값은 커밋하지 않습니다.

- 실제 Slack bot token, app token, signing secret
- 실제 Slack team/user ID
- Codex auth/profile 경로
- host-local workspace 절대경로
- runtime SQLite DB
- service 파일의 실제 계정명/경로
- 로그, Slack event dump, Codex session dump

`.env.example`에는 placeholder만 둡니다. 실제 `.env`는 host-local 비공개 파일로 관리합니다.
