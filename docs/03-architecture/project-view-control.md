# 외부 project 실행 창과 JSONL protocol 2

상태: P02.2a 구현 계약·미구현 · 2026-10-16

## 목적과 범위

외부 project를 한 번 읽고 같은 권한 session을 사람이 보는 창과 AI 제어가 함께 사용한다. 매 실행 경로마다 project를 복제하거나 fixture용 state/command 처리를 다시 만드는 반복을 줄이는 DEV01/DEV04/DEV05 작업이다. [공통 service](session-control-service.md)와 [packet renderer](renderer-resource-reuse.md)를 연결하는 CLI adapter이며 새 simulation/renderer를 만들지 않는다.

`daedalus project view <directory>`는 기존 `window` feature 아래 추가한다. 기본 clock은 realtime이며 `--clock manual`로만 명시적 Step 모드를 선택한다. `--stdio-control`, 선택적인 `--controlled-entity <StableId>`, 크기·시간/프레임 제한·report 경로를 제공한다. 기존 top-level fixture `view`, protocol-1 `project serve`와 batch `project capture` 의미는 유지한다. 빈 project는 빈 scene이며 임의 terrain/entity를 주입하지 않는다. live capture·MCP·gameplay action·UI tree 지원은 이 단계의 기능이 아니다.

## 불변 source와 실제 tick 0

CLI가 `Arc<PreparedProject>` 한 개를 보유하고 additive `ProjectSession::new_shared`·`SessionControlService::start_shared`에 전달한다. 기존 by-value constructor는 Arc로 감싸 같은 구현에 위임한다. actor와 renderer 준비가 같은 불변 source를 공유하며 수십 MiB mesh를 복제하지 않는다. 현재 `ProjectSession::project() -> &PreparedProject`의 호출 형태·반환형은 유지하되 Arc 역참조 때문에 const 함수 표시는 제거한다. 아직 공개 release 전인 alpha API의 작은 source 호환 변경으로 기록한다.

service startup 성공 전에 현재 ECS의 tick-0 snapshot/frame을 게시한다. 별도의 current-state extraction은 clock 증가·begin_tick·command 적용·gameplay schedule·revision 변경을 실행하지 않는다. 기존 tick snapshot 추출의 entity/retention/sequence 한도를 재사용하고 실패를 typed startup error로 돌려준다. direct ProjectSession 생성자가 즉시 snapshot을 만드는 것으로 의미를 몰래 바꾸지 않는다. Step/bootstrap tick으로 초기 표시를 위조하지 않는다.

CLI는 bounded ProjectInfo page로 실제 stable ID→EntityHandle binding을 얻고 같은 source Arc로 ProjectRenderResources를 준비한다. startup에는 유한 deadline이 있고 Busy만 한도 안에서 재시도한다. renderer prepare/extract는 실제 frame session/sequence/generation/source digest를 보존한다. 이전 frame Arc를 불필요하게 보유하지 않는다. packet은 불변 geometry handle과 작은 instance 데이터만 보유한다.

## 스레드·소유권과 표시

OS/window는 main thread, service actor는 authority, presentation/control coordinator는 별도 worker다. coordinator가 service owner와 client·ticket book을 관리한다. window callback은 bounded input/message try-send·local camera·GPU 제출만 처리하며 actor·stdin/stdout·worker 완료를 기다리지 않는다. presentation은 최신 complete frame을 coalesce하고 bounded packet channel이 차면 최신 한 candidate만 유지한다. resource 오류는 Abort로 전파하며 오래된 identity를 새 화면처럼 표시하지 않는다.

window와 packet backend는 기존 ApplicationHandler의 event/input/resize/camera/overlay/종료를 공유한다. 정상 `close`는 PacketWindowMessage::Close로 전달해 CloseRequested를 기록한다. window가 닫히면 coordinator stop→service pending 결과 마감→actor/presentation worker 회수→report 순서로 종료한다. window/render 실패는 nonzero exit다. report는 source/session, 실제 authority 종료 tick, accepted와 presented identity, 사용한 camera/physical 크기, 자원 통계와 control detach 사유를 구분한다. surface present 호출 성공은 GPU 완료/display scanout 증명이 아니다.

## Protocol 2 envelope와 결과

`--stdio-control`에서만 stdout은 UTF-8 JSONL 응답이다. 요청은 `protocol: 2`, ASCII 비제어 문자 1~128 byte `request_id`, 명시적 operation을 가진다. 알 수 없는 version/field/operation·잘못된 target·비유한 숫자는 구조화한 오류다. tick/revision/session/client/sequence/command ID와 handle의 정수 필드는 decimal string으로 encode/decode해 JavaScript 정밀도를 잃지 않는다. offset/limit/count/크기는 고정 상한 내 JSON number다. native와 wire DTO를 검증하되 protocol 1의 DTO를 바꾸지 않는다.

| 요청 | 의미 |
| --- | --- |
| capabilities | 실제 protocol/clock·native/transport 한도·지원 operation, 미지원 live capture/MCP를 반환 |
| submit | project_info/get_transform/list_transforms/set_transform/spawn_transform/status/step을 SessionOperation으로 제출하고 ticket receipt를 즉시 반환 |
| poll | 이 attachment의 ticket에서 Queued/AuthorityAccepted/Terminal·native result/typed error 조회. terminal 실패를 적용 성공으로 표시하지 않음 |
| ack | terminal ticket/result reader 해제. 아직 terminal이 아니면 오류 |
| forget | 결과 소유권 포기. accepted mutation의 취소·rollback을 뜻하지 않음 |
| latest_frame | 같은 service의 complete snapshot identity metadata. presented 화면이나 image/capture 결과가 아님 |
| close | bounded window Close 요청 수락. 실제 종료는 최종 report/exit로 확인 |

target은 stable ID 또는 session/index/generation handle 중 정확히 하나다. command_id는 request_id와 별개이며 native replay/high-water 규칙을 그대로 적용한다. manual mutation은 AuthorityAccepted 후 Step이 있어야 적용되고 submit/poll이 Step을 숨겨 실행하지 않는다. Step은 reserved admission을 유지하며 realtime에서 거부한다. Busy는 finite retry가 가능하지만 QueueFull/ResultCapacity를 숨겨 반복하지 않는다.

ticket token은 session/client/sequence를 포함하는 opaque string이며 이 coordinator의 book과 정확히 대조한다. 다른 client/session token은 거부한다. book은 native client 기본 최대 8개(일반 최대 7개+reserved Step 1개) 이내이며 일반 요청으로 Step 여유를 점유하지 않는다. ack/forget 때 receipt도 제거한다. retained submit request_id의 같은 payload 재전송은 같은 ticket을 반환하고 다른 payload는 collision 오류다. ack/forget 후 transport replay 보장은 끝나며 재실행 방지가 필요한 mutation은 명시 command_id를 사용한다. poll 등의 correlation ID를 무제한 ledger에 쌓지 않는다.

## 유한 process I/O와 detach

첫 portable CLI 구현은 bounded blocking stdin reader와 stdout writer를 각각 process-owned thread로 둔다. 입력은 newline 전에 64 KiB를 검사하고 초과·request queue 16개 포화 때 control을 detach한다. 응답은 serialize 중 1 MiB 상한을 적용하고 output queue는 4개다. writer의 현재 응답·coordinator의 한 encoding scratch·reader의 한 line은 별도 유한 메모리다. 이 transport 임시 메모리를 native 32 MiB result budget에 포함한 것처럼 표시하지 않는다. 결과가 wire 한도를 넘으면 response_too_large이며 전체 JSON allocation을 먼저 만들지 않는다.

EOF·oversize·request/output queue 포화·broken stdout은 protocol만 detach하고 창을 유지한다. detach는 새 protocol admission을 막고 book/result 참조를 포기한다. 이미 actor에 수락한 command를 되돌리거나 미적용이라 단정하지 않는다. diagnostics는 stderr, 최종 근거는 report 파일에 남긴다.

blocking reader/writer는 사용자 pipe가 닫히지 않으면 portable std API만으로 취소할 수 없다. 창 종료 시 두 thread를 join하지 않으며 service owner/Engine/packet/frame/result lease를 이들 thread에 넣지 않는다. thread는 bounded raw line/encoded response·I/O handle만 소유하고 CLI process return 때 종료된다. 이는 CLI process에 한정한 수명 계약이며 embedded library·서버의 cancellable transport로 재사용하지 않는다. actor/coordinator/presentation 등 엔진 worker는 함께 detached 처리하지 않는다. fully cancellable native transport는 후속 adapter다.

## 입력 경계

WASDQE·pointer camera와 Escape/resize/focus는 공통 window 경로다. controlled entity가 없으면 camera-only이며 첫 entity를 임의 선택하지 않는다. 지정 ID가 없으면 startup 오류다. 첫 entity 조작은 arrow press별 작은 translation 명령을 정상 query→expected revision→SetTransform으로 보낸다. bounded observer가 press/release를 drop할 수 있어 held movement의 권한 상태로 사용하지 않는다. 누락·충돌·포화는 진단하고 continuous action system 지원이라 표시하지 않는다. UI callback에서 재시도 대기하지 않는다.

manual 모드의 F10은 명시 Step 1 요청이다. 자동 tick/implicit apply는 없고 realtime에서는 Step을 보내지 않는다. 입력 ticket도 별도 client/유한 credit로 보유·회수해 protocol Step reserve를 빼앗지 않는다. 이 제한된 조작은 연결 검증이며 정식 gameplay action·AI device injection은 P02.3 후속이다.

## 검증과 완료 조건

Luna xhigh가 runtime shared source/tick-0와 CLI adapter/test를 작성하며 renderer 담당자와 파일 소유권·API·단일 Cargo.lock/target 소유를 맞춘다. 새 의존성 없이 기존 std/serde 경로로 시작한다. focused 검사 후 stable source에서 workspace·managed·패키지 회귀를 수행한다.

- 실제 tick 0: authored transform/handle/source 동일, schedule/clock/revision/queued command 미실행, 첫 Step은 tick 1.
- source Arc 공유, 기존 by-value/프로토콜1/fixture 동작 유지, startup 실패·snapshot/entity 한도.
- 같은 process의 submit→poll authority→manual Step→terminal→get/latest_frame 대조. wrong-session/revision/clock/version·중복 receipt·ack/forget·queue/result/line/output 한도.
- EOF/blocked stdin/blocked stdout·invalid input·window Close·renderer/startup 실패가 UI/엔진 종료를 멈추지 않음. 실제 child는 finite timeout/cleanup 적용.
- 실제 외부 project Metal 창의 authority 변경·accepted/presented identity·resource write 계측. native key/focus/resize/close와 state 대조는 CPU 시험과 별도.

live capture·MCP/SDK·UI/action/reload/server 등 P02 전체 인수는 열려 있다. 새 창/JSONL 연결만으로 P02/D1 또는 3D·2D 게임 목표를 완료 처리하지 않는다.
