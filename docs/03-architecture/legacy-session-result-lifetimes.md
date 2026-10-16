# Legacy LiveSession의 결과 수명과 공통 credit

상태: P02.1c 구현·로컬/CI 검증·PR9 병합 · 2026-10-16 · DEV01

## 이 문서를 만든 이유

project service는 공통 `CreditPool`/`CreditLease`를 사용하지만 기존 진단용 `LiveSession`은 atomic count를 수동 증감한다. 제출 실패·결과 소비·종료마다 반환 코드를 중복 유지한다. 이 차이를 제거하되 fixture의 terrain 생성·초기 entity·delta 이동을 일반 project service에 끼워 넣지 않는다. [P02 service](session-control-service.md)의 작은 accounting primitive만 재사용한다.

read-only 코드 감사는 별도 정확성 문제도 찾았다. actor loop가 queue에서 꺼낸 command가 panic으로 결과를 만들지 못하면 남은 queue drain에 그 ID가 없다. 현재 count만 반환하는 수정으로는 “수락한 모든 command의 최종 결과” 계약을 복구할 수 없다. 잠재적으로 적용한 command를 `Stopped`로 위조하지 않고 unknown outcome을 명시한다.

## 유지하는 경계와 공개 변화

`LiveSession`은 진단용 fixture adapter다. `ProjectSession`/`SessionControlService`의 새 mode가 아니며 empty project·실제 tick 0 계약을 바꾸지 않는다. 단일 FIFO result receiver, `try_result()`의 Busy/Poisoned/Disconnected, public `LiveCommandResult`의 by-value·Clone·serde 형태, 기존 queue/result capacity와 정상 종료 동작을 유지한다. native ticket·ACK·reader Arc 의미를 이 API가 제공한다고 표시하지 않는다.

failure-only `LiveCommandOutcome::OutcomeUnknown` variant를 추가한다. 구체 원인은 session termination/report에서 확인한다. 기존 외부 Rust exhaustive match는 새 variant를 처리해야 한다. 아직 공개 release 전의 alpha API 변화로 기록하며 기존 variant의 serialize 이름과 의미를 바꾸지 않는다. stats에는 unknown 수를, 최종 report에는 result byte limit와 종료 시 usage를 추가해 근거를 남긴다. 기존 CLI adapter의 wildcard 처리는 새 성공으로 계산하지 않는지 검토한다.

## 예약과 소유권

| 상태 | lease 소유자 | 다음 상태·해제 |
| --- | --- | --- |
| 제출 후보 | 호출 stack | ID 소진·queue full/disconnect·이미 stopping이면 drop으로 반환 |
| 수락한 일반 command | private command envelope | actor가 effect 전에 unresolved table에 등록 |
| out-of-band Stop | admission gate가 보호하는 단일 ID+lease slot | stop drain이 같은 gate에서 take |
| 처리 중 command | bounded actor ID→lease table | 확정 outcome에 한 번만 이동 |
| 미소비 terminal | private leased result의 outbox/channel | `try_result` dequeue 또는 shutdown drain에서 반환 |
| 반환한 public result | caller의 by-value 자료 | engine credit과 별개, caller의 임의 clone을 계수하지 않음 |

`result_capacity`는 그대로 1~4096이다. ticket마다 typed result·metadata의 보수적 **8 KiB**를 예약하고 `checked_mul(result_capacity, 8192)`로 byte ceiling을 구한다. 최대 32 MiB이며 새 public config를 만들지 않는다. 별도 native service의 32 MiB와 같은 shared allocation 하나라고 주장하지 않는다. primitive를 공유하는 각 session의 독립 예산이다.

`Rejected(String)`의 engine-retained UTF-8 payload는 최대 **4096 bytes**다. 문자 경계에서 bounded 새 문자열로 복사하고 큰 기존 allocation을 버린다. 길이만 truncate하고 큰 capacity를 남겨 예산을 지킨 것처럼 표시하지 않는다. 결과 표에 원문 무제한 진단을 별도 보관하지 않는다. error 생성의 일시 allocation·allocator padding·caller 복제·전체 RSS를 정확히 계수하는 계약은 아니다. truncation 여부를 알아볼 수 있는 suffix도 한도에 포함한다.

공통 credit은 nonwaiting reserve/RAII release를 사용한다. actor unresolved 수·outbox·channel의 총 accepted 결과는 기존 result capacity 안이다. 같은 lease를 단계별로 복제해도 credit을 일찍 반환하지 않으며 마지막 engine 참조가 사라질 때 회수한다. 수동 release/fetch_sub 중복은 제거한다. FIFO reliable 결과는 frame coalesce처럼 덮어쓰지 않는다.

## Stop·실패·종료

제출 gate에서 ID와 lease를 함께 가진 Stop slot을 게시한다. actor가 stop drain에서 같은 gate로 admission을 닫고 queue와 Stop slot을 회수한다. ID만 atomic에 먼저 보이거나 lease만 남는 경쟁을 허용하지 않는다. 명령 queue가 차도 result credit이 남으면 Stop을 받을 수 있고, result 한도까지 찬 경우 `LiveCommand::Stop`은 기존 backlog 오류를 반환한다. 별도 `request_stop()`은 결과/credit 없이 항상 협조 종료 신호를 설정한다.

정상 종료는 기존 queue-only/미적용 결과의 `Stopped`와 이미 확정된 Applied/TerrainReady/Superseded/Rejected를 보존한다. result channel disconnect·outbox 폐기·session drop·startup/ID/send 실패는 private envelope drop으로 예약을 반환한다. shutdown report로 plain result를 이동할 때도 credit을 반환하며 actor/worker join은 UI callback 밖에서 수행한다.

actor의 runtime failure/panic 때는 effect 전에 등록한 unresolved ID를 모두 회수한다. 적용 여부를 증명하지 못하는 active command는 **OutcomeUnknown**으로 한 번만 마감한다. 아직 queue에만 있던 ID는 적용되지 않았음을 확인할 수 있어 Stopped다. 이미 확정한 결과는 덮어쓰지 않는다. failure 후 새 final simulation tick을 실행해 결과 의미를 바꾸지 않는다. terrain task 취소·worker 회수는 계속 수행하고, stale completion이 이미 terminal인 ID에 두 번째 결과를 만들지 않게 한다.

unwind 경계에서 잃을 수 있는 popped envelope, engine step 뒤 report 처리 전의 적용 창을 실제 test hook으로 검증한다. RAII의 memory 회수와 semantic terminal 보장은 서로 다른 검사다. catastrophic allocation/process abort까지 결과 전달을 보장하는 명세는 아니다.

## 구현과 인수

Luna xhigh가 `crates/daedalus-runtime/src/live_session.rs`와 필요할 때 `session_primitives.rs`만 소유한다. 공개 service·renderer·CLI protocol·Cargo manifest/lock은 바꾸지 않는다. CLI match에 실제 문제가 있으면 먼저 영향과 별도 소유권을 공유한다. root는 Markdown·리뷰·통합 검사를 담당한다.

- count/byte ceiling·4096-byte UTF-8 경계·ID/send 실패의 RAII rollback.
- outbox/channel에 남은 결과는 charged, dequeue/report drain 뒤 반환, FIFO/Busy 호환.
- command queue full에서 credit이 남은 Stop, result full에서 별도 request_stop, Stop ID/lease drain 경쟁.
- receiver disconnect·폐기·정상 stop·terrain supersession/cancel의 무중복 terminal과 credit 0.
- pop 직후 panic·실제 engine step 직후 panic·정상 stop의 final tick 적용 직후 panic의 exact accepted ID→Unknown, 실패 후 추가 tick 없음.
- focused runtime tests/strict Clippy 후 workspace·기존 project CPU smoke·legacy 실제 loading/종료 회귀.

P02.1c는 구현과 위 증거를 통과한 뒤에만 닫는다. P02.2는 이미 별도로 통과했고, live capture/조건·MCP/SDK·UI/action/reload/server와 P02 제품 gate·D1/D2/D3는 계속 미완료다.

## 로컬 구현 근거

Luna xhigh의 구현은 live_session.rs 한 파일이며 공통 primitive 자체를 중복 작성하지 않았다. focused runtime 65 unit+2 integration, root workspace all-feature CPU 200건·hardware 3건 기본 ignored·check/fmt/strict Clippy를 통과했다. public API barrier race는 accepted ID와 terminal ID 집합의 일치·무중복·회수를 확인하는 coverage이며 formal race proof가 아니다. root의 외부 project CPU18 smoke와 실제 Metal loading window 종료도 통과했고 default 128 ticket은 1 MiB byte limit·종료 credit 0을 보고했다. 정확한 binary/명령/현재 PR 상태는 [작업 기록](../06-worklog/2026-10-16-product-readiness-audit.md)에 둔다.
