# Slack Codex

Slack Codex는 개인용 Slack 워크스페이스에서 로컬 Codex 작업을 비동기로 실행하고, 결과를 Slack thread로 돌려받기 위한 로컬 브리지입니다.

## 현재 상태

이 저장소는 v1 구현 전 초기화 단계입니다. 공개 저장소에는 Rust 바이너리 골격, 공개 가능한 아키텍처 요약, 환경변수 예시만 둡니다.

## v1 원칙

- Slack DM을 원격 작업 콘솔로 사용합니다.
- 세션 경계는 `Slack thread = Codex session`으로 고정합니다.
- 호스트는 봇 DM으로 수동 선택합니다. 자동 fallback은 v1 범위가 아닙니다.
- 공개 저장소에는 코드와 일반화된 문서만 둡니다.
- 토큰, 로컬 경로, SQLite DB, 로그, Codex session 출력물은 커밋하지 않습니다.

## 개발 시작

```powershell
Copy-Item .env.example .env
cargo fmt --all
cargo test
cargo build
```

현재 바이너리는 bootstrap placeholder입니다. Slack Socket Mode, Codex 실행, SQLite 상태 저장은 작은 수직 단위로 구현합니다.

## 공개 문서

- `docs/ARCHITECTURE.md`: v1 아키텍처와 보안 경계

Agent instructions, repo-local skills, TODO, WBS, tickets, reference notes, and historical planning docs are local-only and ignored by git.

## 공개 저장소 보안 규칙

실제 Slack 토큰, app token, signing secret, team/user ID, 개인 workspace 경로, Codex profile 경로, `sessions.db`, 로그, Slack event dump, Codex session dump는 저장소에 넣지 않습니다.
