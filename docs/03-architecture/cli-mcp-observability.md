# CLI·MCP·관측 가능성

상태: AI 친화적인 엔진 제어 계약 초안. 아래 engine 명령은 아직 미구현 · 2026-10-15

## 공통 제어 서비스

CLI와 MCP가 각각 ECS를 직접 만지는 별도 구현이 되지 않게, engine control service를 공통 경계로 둔다. 실행 창도 같은 상태·명령·진단 의미를 사용한다. UI·네트워크·저장과 일치하는 권한 command를 적용한다.

도구는 코드·자산·API 버전·현재 상태를 먼저 관찰하고, 명령을 제출하고, request ID로 완료·실패를 확인한 뒤 이미지·수치·진단으로 검증한다. 문자열 로그를 읽는 것만으로 성공을 추정하지 않는다.

## 명령군 제안

| 명령군 | 필요한 기능 | 완료 결과 |
| --- | --- | --- |
| inspect | 실행 모드·capabilities·버전·world/entity·asset 상태 | schema 있는 JSON·페이지 크기·snapshot tick |
| control | 시작·pause/step·종료·설정·안전한 world command | 수락/거부·request ID·적용 tick |
| reload | module/asset 변경·검증·staging·commit 상태 | 이전/새 generation·migration·진단 |
| capture | 고정 tick·camera·품질·이미지·연속 프레임 | 파일/참조·hash·manifest·실패 이유 |
| profile | trace·frame·queue·job·GPU·memory | 측정 환경·기간·분위수·원시 자료 |
| validate | 엔진 fixture·상태 invariant·시각 비교 | 기대값·실제값·재현 command·exit code |
| autopilot | 실제 입력·UI 조작·조건 대기·검증·capture 연결 | session·request ID·적용 tick·표시 frame·재현 trace |
| build/export | native package·web HTML/Wasm·asset cook | versioned manifest·파일·필수 배포 조건 |

실제 command spelling은 후속 API 명세에서 정한다. README에 현재 존재하지 않는 `engine-cli` 명령을 실행 가능한 기능으로 안내하지 않는다.

오토파일럿은 같은 세션의 플레이 제어와 새 C# 게임 로직 주입을 모두 다룬다. 기존 동작의 테스트 절차를 바꿀 때마다 게임 코드를 reload하지 않는다. 입력/action 경로·clock·취소·캡처·서버 권한의 세부 계약은 [실행 중 테스트](autopilot-and-live-testing.md)에 둔다. Lua VM은 이 기능의 필수 의존성이 아니다.

## 요청과 오류

각 요청은 schema/API version·request ID·deadline·권한·취소 가능성·작업 예산을 가진다. `Accepted`와 `Completed`를 구분하며 timeout 뒤 작업이 계속되는지 취소됐는지도 명시한다. 결과에는 오류 코드·원인·재시도 가능성·진단 위치를 담는다.

로컬 제어는 기본 최소 권한 경로를 검토한다. 원격 제어가 필요해지면 인증·명령 권한·전송 보안을 별도 설계한다. MCP 연결을 열었다는 이유로 임의 파일 접근·쉘 실행·불신 script 실행까지 허용하지 않는다.

## 관측 데이터

job·tick·frame·command·asset에 correlation ID를 부여한다. trace span에 enqueue/start/end·wait·cancel·bytes·revision을 기록한다. 평균 fps뿐 아니라 p50/p95/p99/p99.9·최대·예산 초과·queue age·snapshot age·GPU 지연·memory high-water를 수집한다.

diagnostic 수집 자체의 비용·buffer 상한·drop 정책을 둔다. frame thread가 로그 file flush를 기다리지 않게 한다. 오디오·GPU timestamp 등 플랫폼별 관측 가능성의 차이를 capabilities로 공개한다.

## 재현 manifest

엔진 commit·build profile·toolchain·의존성 lock·OS·GPU/backend·해상도·quality·seed·입력 trace·asset/script hash·warmup·시간·worker 수·환경 상태를 남긴다. 개인정보·사용자 파일 경로·계정 토큰을 공개 성능 자료에 포함하지 않는다.

이미지는 실제 GPU 경로의 근거이고, 상태 invariant는 시뮬레이션 정합성의 근거이며, trace는 지연 원인의 근거다. 각각을 연결해서 AI가 “보이는 화면만 정상”인 오류를 놓치지 않게 한다.
