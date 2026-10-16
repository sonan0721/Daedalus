# 같은 project 세션의 실행·제어·표시 service

상태: P02/DEV01 다음 구현 계약 초안·미구현 · 2026-10-16

## 목적과 실제 기준선

외부 project를 창에서 실행하고 CLI/MCP/SDK가 동일한 authority를 관찰·조작하게 한다. 엔진을 transport마다 새로 만드는 반복, 별도 command 의미, 장면별 fixture 분기를 공통 session service로 줄인다. [project lifecycle](project-lifecycle.md)은 저작 자료 준비/동기 실행 계층이고, 이 문서는 그 위의 지속 실행·제어·표시 계층이다.

현재 `ProjectSession`은 실제 Engine과 stable ID map을 소유하지만 공통 live actor가 없다. JSONL control은 `&mut Engine`을 직접 제어한다. 기존 `LiveSession`은 bounded queue/result credit/latest mailbox/stop·join을 구현했으나 한 demo entity와 terrain job의 actor다. bridge는 별도 Engine과 atomic transform batch 의미를 가진다. `view --capture-output`은 창 종료 뒤의 별도 offscreen render다. 어느 경로도 같은 live project의 MCP/입력/표시 frame capture 완성을 증명하지 않는다.

구현 전에 공통 public API와 소유 파일을 Luna xhigh 담당자에게 공유한다. 아래 API 이름은 후보이며 실제 type 서명과 error enum을 먼저 합의한 뒤 dependent adapter 코딩을 시작한다. 새 빈 service interface나 기존 fixture의 이름 변경을 P02 통과로 보고하지 않는다.

## 엔진과 실행 영역의 소유권

| 영역 | 소유·역할 | 기다리면 안 되는 대상 |
| --- | --- | --- |
| source preparation | bounded project source/asset 준비 | window callback에서 파일/decoder 완료 |
| session actor | 하나의 ProjectSession/Engine·clock·command·gameplay callback·권한 상태 | renderer·네트워크 write·capture encode·UI event |
| SessionClient | 제한된 요청 제출·자기 결과 조회·불변 최신 frame 조회 | simulation tick/queue capacity를 UI callback에서 blocking wait |
| presentation bridge | 같은 prepared generation과 snapshot의 projection·resource 갱신 계획 | 장기 source 작업·GPU readback/file write |
| OS/window/render | main-thread event·camera·GPU 제출·제한된 capture 요청 | simulation join·asset cook·CLI stdin |
| transport adapter | JSONL 또는 MCP frame parsing/응답·deadline·local capability | OS event loop에서 I/O |
| coordinator | startup·호스트 수명·창 종료 후 cooperative stop/join·최종 보고 | callback 안 join |

actor는 session마다 Engine을 정확히 하나 만든다. project의 stable ID·authored source revision/digest와 runtime entity handle/revision/tick은 구분한다. renderer는 live ECS를 직접 읽지 않는다. 이후 C# module host/Rust gameplay는 같은 actor의 runtime API를 소비하고 자기 권한 world를 하나 더 만들지 않는다. 외부 callback의 메모리/CPU 사용을 엔진이 강제 sandbox한다고 주장하지 않는다.

기존 LiveSession의 공통 admission/result credit/frame mailbox/clock/shutdown을 먼저 추출할 수 있는지 검토한다. terrain generation·initial demo entity·R/arrow 의미는 fixture adapter에 남긴다. 호환 facade와 기존 검증은 보존하되 fixture 가정을 공통 project service의 필수 필드로 옮기지 않는다. migration 중 임시 중복이 필요하면 파일과 제거 작업을 명시하고 DEV01을 미완료로 유지한다.

## public API와 clock 계약

후보는 `SessionHost::start(prepared, engine_config, clock_mode, limits)`와 attach한 `SessionClient`다. Host만 join handle을 소유하고 Client clone이 host lifetime이나 종료 권한을 암묵적으로 갖지 않는다. UI에서 Client를 drop할 때 blocking join하지 않는다. startup 검증·실패는 아직 event loop를 돌리기 전 coordinator에게 반환한다.

clock mode는 `Manual`과 `Realtime`을 구분한다. Manual은 명시한 bounded step만 실행하고 realtime은 fixed clock의 bounded catch-up을 사용한다. realtime 세션에 manual step을 몰래 더하지 않는다. pause는 tick boundary에서 적용하며 요청 수락/실제 pause tick을 구분한다. clock 전환을 지원한다면 pending apply tick·단계 결과 의미를 별도로 시험한다.

최초 작업은 현재 지원된 project transform/mesh로 한정하되 공통 typed operation은 실제 ECS를 사용한다. project info/page, get/list transform, revisioned set, manual step 또는 realtime status, latest frame, bounded result polling, stop의 소유권을 정한다. 입력/action·wait condition·reload/capture는 뒤 항목의 실제 구현 전 capability를 false/unsupported로 반환한다.

operation DTO는 transport envelope와 분리한다. 단순 공통 DTO를 재사용하려고 JSONL의 request ID와 bridge의 atomic batch를 같은 envelope에 강제로 넣지 않는다. 기본값·상한·error 의미와 wire compatibility를 검증하며 C# 수동 JSON을 향후 schema/SDK generation에 연결한다. Rust private ECS layout을 외부 ABI로 노출하지 않는다.

## admission·상관관계·유한 결과

세 단계는 별개다: `service queue admission`, `authority command acceptance`, `Applied/Rejected` terminal outcome. service가 queue에 넣은 시점에 world 변경/적용 tick을 성공으로 단정하지 않는다. JSONL의 기존 `submission: accepted`는 authority가 WorldCommand를 수락한 의미를 유지한다. Manual 모드의 set 요청을 Applied까지 동기 기다려 같은 I/O loop가 다음 step을 읽지 못하는 deadlock을 만들지 않는다.

transport의 문자열/숫자 request ID는 adapter-local이다. core correlation은 session·attachment·monotonic service request ID와 world CommandId로 추적한다. 서로 다른 adapter가 같은 request ID를 써도 다른 결과를 가져갈 수 없다. core가 생성하는 world command ID는 session 전역 allocator와 기존 reserved-half 규칙을 지킨다. caller ID의 중복 의미와 권한도 유지한다.

여러 client가 하나의 global result receiver를 경쟁 소비하는 구조는 금지한다. attachment별 bounded result queue 또는 요청별 owned ticket을 사용하고, 소유 client만 자신의 completion을 소비한다. read query 응답과 command acceptance/outcome의 필요한 credit을 admission 전에 예약한다. mutation의 terminal outcome은 latest frame처럼 덮어쓰지 않는다. unread result를 무한 HashMap으로 보관하지 않는다.

P02.1의 결과 선택은 아래 owned ticket 계약으로 고정한다. manual set→step의 wire 호환, realtime의 비동기 terminal outcome, 복수 attachment, attachment 종료/timeout 뒤 pending command 결과 회수, 종료 시 unread 결과 보고를 모두 만족해야 한다. 구현 편의 때문에 한 client의 polling이 다른 client 결과를 훔치거나 대기 중 write가 완료된 척 응답하면 실패다.

queue·client 수·요청 bytes·page·tick count·pending mutation·reliable result bytes를 제한한다. Busy/QueueFull/ResultCapacity/Stopped/CapabilityDenied/ClockModeMismatch는 별개 오류다. stop 신호는 포화된 일반 command queue와 독립적으로 전달할 수 있어야 한다. 취소 불가/이미 적용된 요청은 실제 상태를 반환한다.

## P02.1 고정 API와 owned ticket

Luna xhigh의 read-only 제안을 검토한 초기 계약이다. 소유 구현은 `daedalus-runtime` 안에 두고 renderer/JSON parser/transport를 runtime dependency로 추가하지 않는다.

| public API | 의미 |
| --- | --- |
| `SessionControlService::start(prepared, SessionControlConfig) -> Result<Self, SessionControlStartError>` | actor thread 안의 한 ProjectSession. config는 EngineConfig·Manual/Realtime·validated limits |
| `attach()` / `attach_with_capabilities(capabilities)` | 각각 기본 trusted local client / host가 제한한 inspect·mutate·manual-step client 발급. 네트워크 인증 구현을 뜻하지 않음 |
| `SessionControlClient::try_submit(SessionOperation) -> Result<RequestTicket, SessionSubmitError>` | nonwaiting admission. ticket은 session·attachment·sequence identity를 갖고 Clone 불가 |
| `RequestTicket::try_state() -> Result<Arc<RequestProgress>, RequestReadError>` | 같은 요청의 읽기 전용 진행/결과. 결과 Vec 전체를 poll마다 복사하지 않음 |
| `RequestTicket::wait_for(WaitStage, Duration)` | control/coordinator에서만 유한 대기. AuthorityAccepted/Terminal 구분, timeout 뒤 ticket과 실제 작업은 유지 |
| `RequestTicket::acknowledge(&mut self)` | terminal일 때만 engine 보존 책임 해제. progress Arc를 보유한 consumer의 memory lease는 마지막 참조까지 계수 |
| `SessionControlClient::try_latest_frame()` | ticket과 독립적인 최신 불변 SessionFrame 조회. Client drop으로 host join하지 않음 |
| service `request_stop()` / `shutdown(self)` | 독립 stop / coordinator의 명시 join·bounded final report |

`RequestProgress`는 authority admission과 terminal `Result<SessionRequestResult, SessionRequestError>`를 구분한다. public field를 가변으로 열어 결과 identity를 바꾸지 않는다. admission에는 world CommandId, Enqueued/DuplicatePending/DuplicateCompleted 의미와 실제로 아는 apply tick만 둔다. terminal command에는 실제 CommandResult와 resolve tick을 기록하고, cached replay에 원래 tick 정보가 없으면 `None/unknown`을 사용한다.

`SessionOperation`은 ProjectInfo(page), SpawnTransform, GetTransform, ListTransforms(page), SetTransform(revision/command ID/optional apply tick), Step(count), Status다. get/set target은 authored StableId 또는 EntityHandle을 명시적으로 고를 수 있다. handle은 session/generation을 검사하며, source map에 없는 runtime-spawn entity를 임의 authored ID로 만들지 않는다. 기존 JSONL의 get `entity`/set `target`과 spawn을 adapter에서 손실 없이 매핑할 수 있어야 한다.

query는 Queued→Terminal, set은 Queued→AuthorityAccepted→Terminal이다. authority 거부도 해당 ticket의 명시적 terminal 오류다. duplicate pending은 모든 소유 ticket이 같은 실제 outcome을 받되 world 명령을 한 번만 적용한다. duplicate completed는 실제 cached CommandResult를 반환한다. 서로 다른 adapter의 auto ID allocator를 따로 두지 않는다.

ticket drop은 결과를 포기하는 의미이며 이미 authority가 수락한 명령을 취소하지 않는다. pending credit은 actor의 terminal 처리까지 유지한다. ack/drop된 ticket의 결과 Arc를 consumer가 보유하면 실제 engine-owned payload lease가 살아 있는 동안 byte credit을 성급히 반환하지 않는다. client detach 후에도 살아 있는 ticket의 읽기 권한은 유지할 수 있으나 새 제출은 거부한다. 초기 reconnect/resume API는 만들지 않는다.

| 초기 제한 | 기본값 | hard ceiling |
| --- | --- | --- |
| actor queue | 128 | 4096 |
| client attachment | 16 | 64 |
| client별 pending ticket | 8 | 256 |
| session 전체 pending ticket | 128 | 4096 |
| ticket result bytes | 1 MiB | 1 MiB |
| session result bytes | 32 MiB | 32 MiB |
| page entities | 1024 | 1024 |
| Manual step/request | 120 | 120 |
| retained frames | 3 | 64 |

수치가 상한 안이라도 0/상호 모순인 config는 시작 전에 거부한다. 요청별 보수적 payload/metadata 예산을 실제 생성 전에 예약하며, source 문자열·StepReport/command 수·page 결과도 계산에 넣는다. query 처리 실패나 queue full은 모든 예약을 되돌린다. runtime-spawn entity 수도 기존 16,384 상한을 유지한다. 별도 frame/source/CPU pool 예산을 이 result budget 하나로 모두 제한했다고 주장하지 않는다.

world 목록에 offset page가 필요하면 `daedalus-world`의 기존 bounded extraction을 작은 공통 page helper로 확장한다. 전체 entity Vec를 만든 뒤 잘라내거나 runtime에서 ECS handle 해석을 재구현하지 않는다. mutable 목록 page에는 실제 읽은 authority tick을 기록하며 서로 다른 tick의 page를 일관된 한 snapshot이라고 표시하지 않는다. immutable source binding page와 runtime entity page를 구분한다.

Manual set은 AuthorityAccepted까지만 기다린 기존 wire 응답을 유지한다. Step은 실제 StepReport를 반환하고 pending mutation ticket도 같은 결과로 완료한다. realtime은 receipt의 terminal 상태를 읽을 수 있어야 하며 후속 adapter는 bounded CommandId→ticket map과 명시 result 조회/ack를 제공한다. core 단계에서 그 JSONL 기능까지 구현한 것으로 표시하지 않는다.

종료 때 queue-only는 StoppedBeforeAuthority, authority accepted지만 아직 tick에 적용 안 된 명령은 StoppedNotApplied로 마감한다. 이미 적용/거부한 명령을 stopped로 덮지 않는다. report는 마지막 authority tick·실제 termination·미확인/abandoned 결과 수를 유한하게 기록한다. 공통 primitives는 credit/terminal cell/latest frame/stop처럼 작게 추출해 기존 LiveSession에도 사용하고, fixture actor와 입력 의미는 유지한다. migration 중 남긴 중복과 후속 제거를 명시한다.

## snapshot·source·표시 identity

공통 frame은 session ID·tick·frame sequence·active project generation/source digest/revision과 `Arc<RenderSnapshot>`를 가진다. 한 fixture의 terrain 통계는 공통 필수 payload로 넣지 않는다. source metadata와 entity revision/state를 복사해 오래된 현재값으로 표시하지 않는다. inspect 결과의 읽은 tick과 presentation frame의 snapshot tick을 구분한다.

표시는 latest complete frame을 coalesce할 수 있다. consumer가 이전 Arc를 오래 보유할 때 snapshot/resource retention 예산을 드러낸다. simulation은 render 소비를 기다리지 않고 이전 정상 표시를 유지한다. frame drop/coalesce와 reliable result drop을 같은 정책으로 처리하지 않는다.

같은 source mesh는 불변 handle/generation으로 공유하고 transform-only tick마다 vertex/index를 clone·flatten·upload하지 않는 renderer 경로를 만든다. CPU projection과 GPU resident cache 둘 다 관찰한다. 동일 scene unchanged/transform edit/resource generation edit별 decode·mesh clone·GPU upload byte/count를 기록한다. CPU 변환 cache 하나로 GPU 재업로드까지 해결했다고 보고하지 않는다.

## visible project와 제어 adapter

후보 CLI는 `project view <directory>`이며 기존 `view` fixture는 진단용으로 보존한다. startup은 project load/검증→Host start→window/control attach 순서다. 빈 project는 빈 scene을 표시하고 실행 창에 default terrain/entity를 주입하지 않는다. camera 조작과 authored/gameplay entity의 action을 구분한다. 선택 대상 stable ID·action binding이 없는데 arrow key를 임의 첫 entity 이동으로 연결하지 않는다.

stdin JSONL을 붙이는 경우 OS event loop와 별도 I/O thread를 사용하고 stdout에는 해당 protocol 응답만 출력한다. window/report 진단은 stderr 또는 명시 파일이다. window가 먼저 닫히는 경우에도 blocking stdin thread를 join하느라 프로세스가 종료되지 않는 문제를 해결해야 한다. fd-ready/비동기 transport 또는 명시적으로 종료 가능한 I/O 수명을 선택한다. timeout/EOF/창 종료의 host stop 정책도 표시한다.

MCP는 실제 protocol adapter이며 기존 JSONL과 같은 stdout에 혼합하지 않는다. version별 discovery/초기화·capability/tool schema·호출·실패·deadline/cancel·transport 종료를 공식 spec/고정 SDK와 검증한다. backend는 같은 Client를 사용하고 tool마다 새 Engine을 만들지 않는다. MCP 구현 전 placeholder tools를 지원 기능으로 나열하지 않는다.

bridge migration은 기존 C ABI·buffer token·capacity·atomic_commands의 preflight/commit/StepReport 의미를 유지한다. CLI service만 먼저 연결했으면 managed/bridge까지 같은 session에 attach했다고 보고하지 않는다. 기존 atomic batch를 비원자적인 request 반복으로 바꾸는 migration은 허용하지 않는다.

## MCP 공식 기준선 조사와 채택 조건

2026-10-16 공식 조사에서 [2026-07-28 versioning](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning)은 per-request metadata·`server/discover` 기반이며 `initialize` handshake가 없다. [2025-11-25 lifecycle](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle)은 `initialize`→`notifications/initialized`를 사용한다. 두 시대를 지원하면 각각 시험하고, legacy 초기화 시험만으로 최신 규격 준수를 주장하지 않는다. protocol의 stateless 요청은 엔진 world를 요청마다 재생성하라는 뜻이 아니다. 계속 살아 있는 SessionHost에 명시적 session/attachment를 연결한다.

Luna xhigh의 [rmcp 3.5.1 manifest](https://docs.rs/crate/rmcp/3.5.1/source/Cargo.toml)·[features](https://docs.rs/crate/rmcp/3.5.1/features) 확인은 edition 2024·MSRV 1.88·Apache-2.0을 기록했다. 첫 후보는 exact `=3.5.1`, defaults off, `server`/`transport-io`이며 typed macro handler를 쓰면 `macros`를 추가한다. HTTP/OAuth/client child-process 기능은 stdio server의 필수 의존성이 아니다. Tokio/SDK 전이 의존성·실제 lock·Rust 1.94 빌드·notice는 채택 시 검증한다. 현재 Cargo에 추가하거나 두 protocol과 호환 시험한 결과가 아니다.

[stdio 규칙](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports)에 따라 stdout은 UTF-8 newline-delimited JSON-RPC만 사용한다. structurally invalid/unknown tool의 protocol error와 정상 호출의 engine 실행 실패(`isError`)를 구분한다. 취소는 service wait/관찰에 전파하되 이미 수락한 world command를 되돌렸다고 응답하지 않는다. EOF/detach의 session stop 여부는 host 소유권으로 정하고 별도 MCP shutdown method를 발명하지 않는다.

첫 검증은 실제 CLI child의 discovery 또는 legacy initialize, tools/list/schema, valid/invalid call, engine error, cancel/deadline, EOF/exit와 모든 stdout line의 protocol 유효성을 확인한다. [공식 conformance 도구](https://github.com/modelcontextprotocol/conformance)의 server 실행은 HTTP URL을 받으므로 그것만으로 stdio 준수를 증명하지 않는다. [Inspector](https://modelcontextprotocol.io/docs/2026-07-28/tools/inspector) interoperability와 process-level suite를 구분한다. 선택한 외부 harness도 version을 고정하며 그 Node 실행 도구를 엔진의 V8/Chromium 의존성으로 포함하지 않는다.

## 입력·조건 대기·live capture

AI input은 정상 action/device/UI 경로로 주입하고 accepted/drop/focus release와 assigned tick을 기록한다. 현재 window observer가 입력을 drop할 수 있으므로 게임플레이 press/release 상태를 그 observer만으로 권한화하지 않는다. observer 포화/focus loss 때 held key가 영구 유지되지 않는 계약이 필요하다. editor camera와 game input capture 여부도 관찰 가능하게 한다.

condition wait는 유한 deadline·session/tick·검사 대상·cancel token을 가진다. authority condition과 frame presented condition을 구분한다. timeout을 조건 성립으로 바꾸지 않는다. manual mode에서 시간을 기다리기만 할지 explicit step을 수행할지 호출자가 명시한다.

live capture에는 선택한 complete presentation frame의 session/tick/sequence/source generation/camera/해상도를 넣는다. `at_least_tick` 요구를 만족하는 frame을 골라 결과 identity를 반환한다. 요청·readback buffer·encode 작업은 유한하고 render callback에서 CPU mapping/encoding/file write 완료를 기다리지 않는다. 화면에 제출한 frame과 offscreen 재렌더 방식은 구분하고 실제 방식·UI 포함 여부를 report한다.

창이 열린 상태에서 같은 session을 조작하고 capture/state/frame identity를 대조해야 P02의 그 항목을 통과한다. 종료 뒤 새 Renderer로 한 번 그린 이미지나 다른 batch `project capture` 결과를 live frame 증거로 대체하지 않는다. 실패한 capture에도 reason/deadline/현재 tick을 전달하며 가짜 검은 이미지를 성공 반환하지 않는다.

## 종료·실패·권한

Host shutdown은 stop 요청→새 admission 거부→pending command의 명시적 결과→작업 취소/회수→actor 종료→coordinator join과 report 순서다. renderer/control disconnect·panic·startup failure·출력 실패를 정상 종료와 구분한다. Drop은 nonblocking stop만 요청하고 명시 shutdown이 회수 결과를 보고한다. 협조하지 않는 foreign code를 강제로 안전 중지할 수 있다는 보장은 하지 않는다.

attachment는 inspect/input/mutate/manual-step/capture/shutdown capability를 구분한다. 최초 로컬 trusted adapter에서의 검증과 후속 원격 인증은 별개다. 엔진 제어 기능을 위해 임의 shell/file/script 권한을 자동 제공하지 않는다. protocol request ID를 보안 token처럼 취급하지 않는다.

## 구현 단계와 gate

- P02.1: generic session/control kernel·typed API·manual/realtime·여러 attachment와 reliable outcome·frame/stop를 실제 Engine으로 검증. fixture 공통 부분의 추출/호환 계획과 남은 중복을 기록한다.
- P02.2: 외부 `project view`·같은 session의 JSONL control·resource reuse·로딩/포화/창 종료·실제 Mac 입력과 state 대조. fixture 창으로 대신하지 않는다.
- P02.3: 동일 live session의 조건/입력/관찰/capture identity·실제 MCP adapter·재현 trace, managed/SDK attach와 atomic migration 회귀.
- P02 제품 gate: 위 연결과 UI tree/action·reload·server authority까지 [상위 인수표](../04-plan/product-delivery-and-acceptance.md)의 실제 제작 루프로 확인. 하위 kernel/창 추가로 전체 gate를 닫지 않는다.

회귀는 기존 P01.1 CPU smoke·bridge/C#·fixture view·renderer/CPU 제한을 유지한다. 새 검증은 같은 actor에 여러 client, 충돌 request ID, wrong-session/stale revision, acceptance와 적용 분리, queue/result 포화와 취소, frame retention/coalesce, manual/realtime 오용, pending 중 종료를 포함한다. 실제 창의 ongoing capture와 state/tick/sequence 및 upload count 증거는 별도로 남긴다. 현재 이 문서는 설계이며 위 검사가 통과했다는 보고가 아니다.
