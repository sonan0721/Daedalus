# Native project host: 엔진 라이브러리와 CLI의 조합 경계

상태: runtime/renderer library 인수·부분 초기화 owner 계약 동결·host/window 연결 전 · 2026-10-17

## 목적과 현재 사실

CLI가 가진 project 실행·입력·snapshot 추출·창 연결을 게임별로 다시 구현하지 않도록 공통 engine library로 만든다. [staged renderer](staged-render-asset-generation.md)와 [headless CPU activation](headless-asset-generation.md)의 사이를 연결한다. CLI에만 있는 기능을 공개 엔진 SDK라고 보고하지 않는다. P03.3b.2는 PR16/main360ceb6에서 인수했다. [runtime consumer 계약](project-generation-consumer.md)은 첫 코드 작업을 동결하며 renderer/host는 아래 최종 검토를 거친다. 이 문서가 b.3 구현/제품 통과 증거는 아니다.

현재 `daedalus-cli/src/project_view.rs`가 binding 수집, bounded input/packet 전달, snapshot→packet, actor 호출과 종료를 조합한다. `project_view_control.rs`의 JSONL·wire ID·stdin/stdout worker는 transport adapter다. `daedalus-renderer/window.rs`는 OS event loop·surface·한 PacketRenderer/device를 소유한다. runtime은 renderer에 의존하지 않는다. 이 분리를 보존한다.

## crate와 책임의 구체안

| 계층 | 책임 | 의존/경계 |
| --- | --- | --- |
| `daedalus-project-host` | native project/session/view lifecycle, typed input relay, publication 추출, CPU 준비 owner, consumer handoff·상태·종료 조합 | runtime/content/core/renderer를 소비. runtime에 renderer 역의존을 만들지 않음 |
| runtime | 기존 Engine·authority actor, source 준비/cache worker, Stage/Commit/replay, source lease·bounded consumer metadata | GPU object나 renderer type을 보유하지 않음 |
| renderer | CPU render 자원·불변 packet, device cache·bounded stage+draw·completion, 실제 present 경계 | runtime type을 읽거나 authority를 변경하지 않음 |
| CLI | 옵션 검증, 프로젝트 경로 전달, JSONL codec/transport, report 파일 저장·JSON serialization | 같은 host/session client에 attach. 별도 source load/두 번째 actor/device를 만들지 않음 |

프로젝트 filesystem resolver/import/cache는 이미 content/runtime의 책임이며 CLI 전용으로 옮기지 않는다. native library 사용자가 CLI process 없이도 같은 실행 기능을 소비해야 한다. C#·MCP의 전체 attach/SDK는 별도 제품 항목이며 이 Rust host 경계만으로 구현됐다고 하지 않는다. crate 이름은 위와 같이 채택하고 renderer의 default features는 끈다. host 기본 feature는 비장치 SDK 경계, native 창은 opt-in `window`다. activation-enabled host의 audio/native-audio는 P03.3c 전에는 제공하지 않으며 CLI의 기존 non-activation audio 경로를 보존한다.

## 외부 facade와 startup

외형은 `NativeProjectHostOptions`, `NativeProjectHost`, `NativeProjectViewOptions`, compact `NativeViewOutcome`으로 좁힌다. start/attach_control/run_view/request_stop/try_shutdown 책임을 분리한다. attach는 기존 `SessionControlClient`를 반환해 모든 frontend가 한 authority를 사용한다. run_view는 macOS의 OS event loop 제약을 따라 호출 thread에서 실행하며 별도 임의 window thread를 만들지 않는다. 정확한 renderer 입력/반환 type과 library 파일 소유권은 연결 API 동결 뒤 배정한다.

`run_view(&mut self, options)` 동안 같은 host borrow로 `request_stop(&self)`를 호출할 수 없으므로 미리 얻는 cloneable `NativeProjectHostControl`을 둔다. handle은 out-of-band `request_stop()`, `request_view_close()`, nonwaiting `try_status()`만 담당하며 작은 atomic/상태 metadata를 보유한다. source/packet/prepared roots를 넣지 않는다. control/input/packet queue가 가득 차도 close/stop은 관측한다. 새 일반 controller framework를 만들지 않는다.

source 감사상 SessionControlClient에는 owner stop API가 없다. 첫 host 구현은 host와 장수 coordinator가 `Arc<NativeProjectActivationService>`를 내부에서만 공유한다. 공개 control handle은 service/ControlShared의 strong/weak handle을 받지 않고 host-only atomic signals·작은 status만 보유한다. coordinator가 whole-stop을 관측해 기존 service의 request_stop을 호출하고 private Arc를 반환한다. 2026-10-17의 기존 artifact rustc probe에서 service의 Send+Sync와 private Arc의 Send를 확인했다. 이는 해당 Mac feature 조합의 compile 근거이며 actual host 구현·종료 검증은 아니다. 임의 두 번째 actor나 UI mutex 대기로 우회하지 않는다.

host try_shutdown은 종료 신호를 보낸 뒤 finished coordinator만 join한다. 그 전에는 Stopping이고 private Arc가 실제 반환된 뒤 `Arc::get_mut`으로 기존 service try_shutdown을 호출한다. CPU owner/device 반환·drain은 별도 진행이며 unfinished join을 하지 않는다. coordinator panic도 finished 뒤 관측/유한 failure를 기록하고 host가 가진 service로 stop·cleanup polling을 계속한다. view-close만으로 coordinator를 종료해 whole-stop 경로를 잃지 않으며 authority Step/read는 유지한다. 이 설계는 작은 control capability를 추가하려고 runtime의 큰 queue/frame state를 일시 upgrade하는 새 경로를 피하기 위한 내부 소유권 선택이다.

첫 view는 one-shot이다. 창을 닫으면 consumer도 Closed가 되며 reopening을 광고하지 않는다. 같은 authority의 read/Step 등 허용 operation은 whole-host `request_stop` 전까지 계속 가능하지만 새 Stage/Commit은 runtime의 consumer-closed 오류로 거부한다. CLI는 창 결과를 얻은 뒤 whole stop을 요청하고 nonwaiting 종료 진행을 관찰한다. view close와 whole-host stop을 같은 성공 상태로 합치지 않는다.

작은 status/outcome은 session·active generation/digest·activation/CPU owner/view lifecycle·논리 장부·bounded resource counters·latest accepted publication과 last actual-present identity/view·최대 유한 failure code/message를 구분한다. publication/authority sequence와 tick은 각각 보존한다. `PacketPresentationWindowOutcome`이나 final packet Arc를 public outcome으로 반환하지 않는다. window interval samples 등 기존 유한 report를 재사용하되 source/이미지/instance 배열을 넣지 않는다.

CLI 연결의 첫 compact publication 관측은 `NativeProjectHostStatus::last_queued_publication() -> Option<&ProjectPublicationIdentity>`로 구체화한다. 기존 runtime의 실제 publication token을 재사용하고 window Frame payload의 try_send가 성공한 뒤에만 갱신한다. 이는 ingress enqueue 사실이며 UI dequeue/accept·draw·present를 뜻하지 않는다. non-window/아직 전달 전은 None이다. 별도의 UI-accepted 관측은 이 getter로 구현됐다고 하지 않는다. `NativeProjectHost::shutdown_report() -> Option<&SessionShutdownReport>`는 기존 private service의 bounded 보고를 빌려 반환하고, 미완료에는 None을 유지한다. source/snapshot/packet root가 없는 기존 type을 재사용한다.

첫 host slice의 `start`는 현재 native 준비 factory처럼 **동기 initial load**다. 호출 전에 창이 생겼다고 하거나 async로 문서화하지 않는다. initial source를 두 번 읽지 않으려면 runtime의 additive render-consumer factory가 exact initial source lease를 제공해야 한다. 이후 교체는 같은 session 안에서 background 준비한다.

구체 startup 순서는 pure options/limits 검증 → signals/유한 slots 할당 → 기존 render-consumer factory → host가 service/port 즉시 보유 → private coordinator와 단일 CPU owner 시작이다. coordinator는 run_view 전부터 OOB stop을 service에 전달한다. `start -> Result<NativeProjectHost, StartError>`의 Err는 service 반환 전 검증/factory 실패까지만 쓴다. factory는 기존 동기 partial cleanup을 수행하므로 start 전체를 nonwaiting API라고 하지 않는다. service가 생긴 뒤 spawn 등의 실패는 consumer close/service stop과 Failed status를 가진 host를 Ok로 반환해 caller가 실제 try_shutdown을 계속할 수 있게 한다. Ok는 Ready/Running 보장이 아니며 status를 확인한다. 살아 있는 service/thread를 잃는 plain late Err를 만들지 않는다.

initial CPU prepare는 같은 owner에서 한 번 수행하고 InitialPreparing status로 노출한다. 첫 run_view는 실제 initial StageInput이 준비될 때까지 **창 생성 전에** 기다리며, 이미 실행 중인 coordinator의 whole-stop이 cooperative cancel을 요청할 수 있다. 이후 managed 창에 실제 initial resource input만 주고 첫 frame은 GPU Ready 이후 실제 SessionFrame에서 추출한다. 이 startup 대기는 현재 additive API의 명시적 제한이며 D1의 CPU load 전 loading UI gate를 닫지 않는다. view 준비/실행 실패는 host를 소비하지 않는 typed outcome으로 남긴다.

동기 초기 CPU 로드는 D1의 로딩 UI 요구를 면제하지 않는다. 후속 P02/P08 bootstrap은 아직 project가 없는 실제 Starting 상태에서 UI/입력을 표시하고, 준비 후 첫 실제 publication을 받도록 설계·검증해야 한다. 초기 packet을 임의로 꾸며 project load 성공으로 처리하지 않는다.

첫 GPU 준비에도 old-ready packet이 없다는 별도 경계가 있다. b.3 구체안은 같은 device의 clear-only/기존 loading overlay draw와 bounded initial stage chunk를 한 submit으로 묶는 additive packet-window bootstrap을 포함한다. 현재 `run_packet_windowed_presented`는 실제 initial packet을 요구하므로 새 경로는 아직 구현되지 않았다. 공통 WindowApp의 no-packet legacy Scene 분기가 이미 packet bootstrap을 지원한다고 설명하지 않는다. CPU initial load 이전에 창을 띄우는 전체 bootstrap과, load 이후 첫 GPU 자원을 나눠 올리는 이 경계는 구분한다.

bootstrap surface present는 game presented/ACK가 아니다. actor의 generation1/source는 이미 initial publication으로 존재할 수 있으나 view는 Starting·presented 없음이다. initial device-ready 뒤 실제 최신 SessionFrame에서 첫 packet을 추출하고 정상 draw/present 경계에서만 initial identity의 ACK를 보낸다. candidate/activation ID를 만들거나 초기 source를 다시 Commit하지 않는다. startup 실패도 candidate stage 실패로 위장하지 않고 host 초기 준비 실패/종료로 보고한다.

### managed window의 additive 연결 경계

source 리뷰에서 기존 `run_packet_windowed_presented`는 필수 initial packet을 요구하고 결과의 `final_packet` Arc를 유지하며, 실제 present metadata에는 authority snapshot sequence가 없음을 확인했다. 새 연결은 기존 `WindowApp`/surface/input/camera/event loop를 managed mode로 확장한다. 기존 public packet 함수·message·outcome의 동작과 struct literal은 보존한다.

managed 시작 입력은 실제 initial `PacketStageInput`과 유한 options/limits이며 **initial CpuFrameHandle은 요구하지 않는다**. 실제 GPU Ready 전에는 bootstrap clear/loading만 표시한다. 이후 첫/후속 frame은 실제 SessionFrame에서 추출한 `Arc<CpuFrameHandle>`이고, resource stage 입력과 구분한다. UI는 최신 active frame과 candidate 준비를 따로 보관해 로딩 중에도 최신 authority pose를 그린다. CPU validation map을 UI에서 다시 만들지 않는다.

payload 전달은 capacity1의 typed 경계로 초기/후보 StageInput·최신 frame을 순서/현재성 검증 후 접수한다. 큐 포화 시 소유권은 sender에게 반환되며 host는 canonical CPU roots를 유지한다. close/whole stop/cancel-through는 이 큐의 여유에 의존하지 않는 별도 atomic 신호다. ready/failure/deferred stage event와 last actual-present metadata는 각각 유한 슬롯에 보존해 하나가 다른 필수 ACK를 덮어쓰지 않는다. host는 renderer-local key와 GPU stage ID를 runtime의 실제 Initial/Candidate identity에 연결하며 renderer가 runtime ID/type을 import하지 않는다. 정확한 additive window API는 [managed 창 계약](managed-packet-window.md)을 따른다.

managed window의 metadata는 session/generation/digest·publication sequence·authority snapshot sequence·tick·실제 physical dimensions/view와 checked present sequence를 함께 보존한다. UI는 `surface.present()` 경계 뒤에만 presented를 기록한다. Busy로 acquired frame을 버린 시도·bootstrap clear는 actual game present 횟수/ACK에 포함하지 않는다. compact host 결과에는 packet/frame/source Arc를 담지 않는다.

window 내부 finalizer는 작은 결과를 복사한 뒤 UI의 CPU-frame/packet aliases, SurfaceState, Window를 순서대로 해제하고 device owner를 항상 소유권을 돌려주는 run 결과로 반환한다. host가 private capacity1 disposition 경계로 worker에 전달한다. 완료된 PacketRenderer뿐 아니라 packet 준비 전의 Renderer와 device 생성 뒤 부분 초기화 owner도 구분한다. handle 미반환과 device 소유 이후 실패를 구분하는 정확한 owner phase·in-place 초기화·paired create·반환 규칙은 [owner 수명 계약](native-renderer-owner-lifecycle.md)으로 동결했다. native host의 같은 CPU owner/backstop이 반환을 수신해 실제 nonwaiting drain과 CPU registry 회수를 끝내며 UI에서 unfinished worker를 join하지 않는다. public 실패가 발생했다고 초기화된 device를 UI에서 곧바로 최종 drop하는 반환형을 만들지 않는다. managed window/host 연결 API는 착수 전에 별도 동결한다. 아직 구현/실제 창·오류·panic 검증을 통과하지 않았다.

### 초기화 실패 경계의 source 감사

2026-10-17 감사에서 현재 window는 Renderer 생성 뒤 dimensions 검증·surface configure·PacketRenderer 변환을 수행하며, consuming 변환의 오류는 원래 Renderer를 반환하지 않는다는 것을 확인했다. 따라서 PacketRenderer/NoDeviceCreated 두 경우만 있는 반환형으로는 device 생성 후 실패를 정직하게 표현할 수 없다. WindowApp 밖의 catch_unwind도 충분하지 않다. 함수 안의 app이 먼저 unwind되어 device owner를 잃을 수 있기 때문이다.

새 managed 경로는 생성된 Renderer를 즉시 app의 owner slot에 넣고, borrowed packet 준비가 성공한 뒤에만 PacketRenderer로 승격하도록 설계한다. app이 살아 있는 범위에서 오류/unwind를 수집하고 surface/window 해제 뒤 소유권을 반환한다. 기존 consuming public API의 호환성은 보존한다. Renderer::from_adapter 내부에도 request_device 성공 뒤 layouts/pipelines/buffers 생성 구간이 있으므로, 이 구간은 별도의 recoverable partial-init owner가 필요하다. 단순 Device clone만으로 부분 GPU 자원의 마지막 해제 thread까지 보장했다고 하지 않는다.

부분 초기화 holder의 고정 필드·실패에도 owner를 반환하는 경계·finalizer 순서는 별도 owner 계약으로 동결했다. 새 type의 Send와 실제 일반 오류/주입 panic·반환/drain은 구현 뒤 검증한다. engine에 handle이 반환되기 전 backend 내부 실패와 engine이 이미 소유한 handle의 실패를 구분한다. 해당 시험을 통과할 때까지 host 종료 소유권 항목은 열린 상태다. 기존 staged-upload library 통과로 이 항목을 닫지 않는다.

## runtime-neutral consumer port 구체안

기존 `NativeProjectActivationService::start_headless`는 보존한다. 별도 render-consumer startup factory는 기존 actor/worker owner와 move-only single-consumer port를 함께 반환한다. startup 뒤 소비자를 붙여 이미 수락한 Commit 조건을 바꾸지 않는다. headless의 Staged 의미는 현재 CPU 호환성이고, render factory에서는 실제 consumer-ready까지 Commit을 막는다.

| 제안 type/operation | 의미 |
| --- | --- |
| `ProjectSourceIdentity::Initial` | 실제 SessionId·generation1·source digest. candidate/activation ID를 만들지 않음 |
| `ProjectSourceIdentity::Candidate` | session·실제 PreparationId/ActivationId·expected/proposed generation·digest를 묶음 |
| `ProjectSourceLease` | non-Clone·move-only source 접근. `with_source`의 짧은 borrowed callback, identity, cancellation/currentness 조회만 제공 |
| `source_pin(): Arc<dyn Send + Sync>` | private source marker를 표준 erased Arc로 반환. host가 renderer-local opaque pin에 넣으며 runtime type 의존/역방향 cycle 없음 |
| consumer의 initial/candidate take | initial은 한 번, pending candidate payload는 capacity1. control 결과·JSON에는 Arc/큰 source를 넣지 않음 |
| `try_report_stage(identity, Ready/Failed)` | final upload serial 완료 또는 bounded CPU/device failure를 제출. actor의 exact identity/currentness 검증 뒤에만 Applied |
| `try_ack_presented(ProjectPresentedFrame)` | 실제 SessionFrame에서 복사한 opaque publication identity·checked present sequence·neutral view metadata. 실제 present 호출 보고이며 scanout 증명은 아님 |

source pin이 남아 있으면 원래 lease를 drop해도 source charge를 반환하지 않는다. 기존 worker의 Sized PreparedProject canonical Arc가 `try_unwrap`될 때까지 active/candidate/retired 장부를 유지한다. borrowed Rust source는 신뢰된 SDK 경계이며 caller가 임의로 Clone/allocation하는 것까지 sandbox로 막는다는 뜻은 아니다.

ready/failure는 현재 stage의 유한 terminal metadata 한 슬롯, presented는 별도 latest-only metadata다. 접수 Queued와 실제 actor 반영 Applied를 구분한다. Busy는 효과 전 거부이며 같은 identity를 재시도할 수 있다. 이미 반영된 같은 결과는 idempotent이고 conflicting/foreign/stale identity는 거부한다. Stage 성공 뒤 cleanup이 새 worker 결과를 버렸던 실제 회귀를 반복하지 않는다. 상세 API/한도는 consumer 문서를 기준으로 한다.

host는 window의 present 관측을 받았다는 이유만으로 이전 generation의 source/ready pin을 먼저 반환하지 않는다. matching ACK가 actor에 수락될 때까지 같은 pending metadata를 유지한다. 같은 committed generation 내 더 최신 present로 coalesce할 수 있지만, 필요한 ACK를 잃은 채 다음 generation으로 넘어가지 않는다. actor ACK 수락·old GPU 마지막 serial 완료·실제 CPU alias 해제를 각각 확인한 뒤 retired source를 해제한다. 비어 있는 씬도 explicit generation/source pin을 유지하므로 mesh가 없다는 이유로 present 전 source가 회수되지 않는다.

취소·supersession·stop은 pending payload queue가 비어야만 전달되는 메시지로 구현하지 않는다. out-of-band stop과 최신 desired identity/cancellation 표시를 둬 CPU checkpoint와 device chunk 경계에서 읽는다. consumer 이탈도 actor가 관측해 준비가 영구 Staging에 남지 않도록 실패/종료로 수렴시킨다. 무효화 후 늦게 온 ready는 Commit 권한을 얻지 못한다. 이미 접수된 Commit 이후는 rollback 대신 committed/presented를 구분한다.

## thread와 ownership

| owner | 실행/보유 |
| --- | --- |
| UI/event-loop | OS input, surface acquire/present, 한 PacketRenderer/device, nonwaiting completion poll과 bounded upload+draw |
| host coordinator | typed 작은 input/control/identity 전달, actor ticket polling·bounded coalescing. 무거운 CPU prepare나 큰 final free를 맡기지 않음 |
| render CPU owner 1개 | candidate 자원 준비·cooperative 취소, snapshot→packet/CPU validation, actual backing registry와 final cleanup |
| 기존 authority actor | Engine/게임 state·command replay·source Commit·publication |
| 기존 preparation/cache worker | import/cook·authored 비교·source 장부·actual final source drop |

Stage마다 detached thread를 만들지 않는다. engine TaskPool의 shared completion receiver를 다른 poller가 몰래 가져가지 않는다. 이 host CPU owner는 별도 process worker budget/진단에 표시하며 Bevy/tasks와 합쳐 이미 모든 thread를 제한한다고 하지 않는다. 장치 callback/CLI I/O worker도 별도다. worker 수와 cache/queue budget은 서로 다른 단위다.

frame request도 latest-only 한 개, CPU 작업 중 한 개로 제한한다. coordinator는 작은 desired identity/추출 허용 신호를 전달하고 CPU owner가 같은 authority의 private attached client.try_latest_frame으로 실제 최신 SessionFrame을 읽는 방식을 우선한다. 그곳에서 opaque ProjectPublicationIdentity::from_frame과 CPU frame을 함께 만들고 snapshot을 해제한다. coordinator가 큰 snapshot payload를 최신 것으로 교체하며 마지막 free를 수행하거나 별도 frame mailbox를 복제하지 않는다. 어떤 이유로 payload 전달이 필요해도 교체된 pending snapshot/packet의 마지막 free는 CPU owner에 맡긴다. PR19의 PacketCpuOwner/CpuFrameHandle은 opaque CPU validation·정확한 limits/token·실제 backing registry를 제공한다. host는 이 검증을 같은 CPU owner에서 재사용한다. legacy packet submit의 validation은 보존하며 managed 경로에서 boolean skip/revalidation map 복제로 우회하지 않는다. prepare에는 cooperative 취소 checkpoint가 있지만 현재 synchronous prepare가 frame request를 선점한다고 주장하지 않는다. 실제 host 지연 계측 뒤 continuation 단위가 필요하면 후속 설계·검증한다.

CPU 전달은 prepare(실행1+pending1), latest-frame 요청1, stage-input 재투영 요청1과 각각의 유한 result slot을 분리한다. initial prepare는 병합하지 않으며 후보 pending 교체는 stale/currentness 확인 뒤 한다. 모든 요청/결과는 runtime identity와 CPU generation/publication을 명시한다. UI try_begin_stage가 Busy로 input을 소비하면 host는 같은 key/identity/CpuGenerationId를 유지하고 CPU owner의 `stage_input(id)`만 다시 요청한다. canonical Arc projection이며 source import/prepare/validation을 반복하지 않는다. renderer event를 복사한 뒤에도 coordinator는 stage/present 보고 metadata를 runtime Applied 또는 명시적 stale/closed 거부까지 유지한다.

lease는 non-Clone이며 CPU prepare로 이동한다. coordinator는 consumer status에서 desired/current identity를 관측해 host-owned AtomicBool을 갱신하고 CPU checkpoint가 이를 읽는다. 완료 뒤 lease.is_current를 다시 확인한 뒤에만 handoff한다. exported lease 내부에 즉시 취소 shared cell이 있다고 가정하지 않으며 cancellation latency는 cooperative poll 간격과 현재 work 단위에 의존한다.

## publication과 표시 identity

runtime frame의 publication sequence와 authority_snapshot_sequence는 다르다. Commit은 같은 tick/snapshot으로 새 source publication을 만들 수 있다. 기존 renderer extract는 snapshot.sequence를 packet.sequence에 넣으므로 이를 그대로 쓰면 정상 세대 교체도 sequence 충돌이 된다.

기존 packet 생성자/extract는 유지하고 새 explicit publication extraction 경계를 추가한다. RenderPacket의 private optional authority-snapshot identity와 getter를 검토한다. legacy 임의 producer는 모르는 값을 None으로 표현하고 snapshot 관계를 만들어내지 않는다. host packet은 실제 SessionFrame의 두 sequence를 각각 전달한다. window transition의 publication 단조성·generation/digest·tick 검사를 느슨하게 풀지 않는다.

실제 present ACK는 session·project generation·digest·publication sequence·authority snapshot sequence·tick·실제 view metadata를 갖는다. actor commit, submission, GPU completion, surface present 호출, CPU backing 회수는 독립 상태다. consumer ready/present는 trusted host의 보고이며 runtime이 GPU나 compositor를 직접 검증한 것처럼 표현하지 않는다. live 조회는 같은 control service의 bounded 상태로 노출할 방향이며 정확한 wire DTO/capability 추가는 코드 전 동결한다.

## 큰 CPU backing과 종료

source pin만으로 복사된 legacy Mesh Vec·map·draw array·packet instances의 마지막 free thread를 보장할 수 없다. CPU owner는 **handoff 전에 실제로 공유될 각각의 backing Arc**를 typed registry에 등록한다. ProjectRenderResources wrapper/packet root의 refcount만 보지 않는다. RenderPacket은 Clone이며 실제 renderer에도 packet.clone 경로가 있어 root와 별개로 leaf가 남는다.

registry 범위는 legacy MeshResource, model draw templates, model/sprite maps·sprite order, per-packet instances와 `Arc<OnceLock<...>>` normal cache까지 포함한다. imported geometry/image 원본은 source/cooked 장부에서 한 번 세고 renderer-owned wrapper/metadata·복사본만 별도로 센다. registry가 자기를 다시 소유하는 pin은 금지한다. parent template/map을 먼저 회수하고 mesh/texture wrapper를 나중에 회수한다.

Sized Arc는 worker의 `try_unwrap`, unsized Arc slice는 worker에서 `get_mut`로 단독 소유와 Weak 부재를 확인한 뒤 drop하는 방안을 검토한다. strong_count 관측만으로 마지막 free를 허용하지 않는다. 엔진 내부 Weak/raw Arc 우회 경로를 제한하고 held alias는 정직한 Pending/Stopping과 charge로 남긴다. 초기/실패/panic/취소/연속 교체 모두 같은 backstop을 사용한다.

packet instances/normal cache는 source별 prepared CPU와 별도의 frame 비용이다. registry/queue가 이를 무제한 쌓지 않도록 per-frame/aggregate estimator와 capacity를 코드 전 정해야 한다. GPU cache/bookkeeping8MiB를 CPU 전체 budget이라고 재사용하지 않는다. canonical leaf 없이 wrapper만 유지한 상태로 registry 공간을 반환하지 않는다.

| CPU lane | source에서 확인한 비용과 구체 제안 |
| --- | --- |
| content/source | imported geometry/image 원본·mip는 기존 cooked/source charge에서 한 번. renderer Arc wrapper와 같은 byte를 중복 청구하지 않음 |
| prepared generation | 기존 metadata estimator의 per-item8MiB·두 generation16MiB 제안 유지. GPU stride를 더한 geometry_bytes를 CPU 복사량으로 부르지 않음 |
| legacy CPU copy | unique Mesh의 vertices/indices 실제 capacity×size_of 별도 합산. per-generation64MiB/aggregate128MiB 제안, 할당 전 peak와 미참조 handle의 임시 생존도 확인 |
| canonical frame | unique packet instances·OnceLock normal cache·추가로 retain하는 validation map·wrapper/문자열을 한 번씩 계산. 기본 **per-frame16MiB/aggregate48MiB·최대3 unique backing** 제안 |
| CPU scratch | 한 owner의 extraction/validation/prepare 계획 scratch16MiB 제안. snapshot index, Vec→Arc 겹침, ID/map/slot/resource/victim 계획 포함. upload packing scratch와 별개 |

renderer prepared-generation registry는 두 슬롯으로 active+candidate 또는 active+retiring을 보유하는 방향을 채택한다. cancelled/failed partial GPU 작업도 retiring 슬롯을 점유한다. old alias/serial이 남으면 다음 renderer prepare는 Busy다. runtime source의 active/candidate/retired 세 슬롯·cooked 합산 budget과 이 renderer의 두 슬롯을 같은 것으로 부르지 않는다. runtime source candidate가 준비됐어도 renderer 슬롯을 얻기 전에는 큰 prepared backing을 새로 만들지 않는다.

위 숫자는 실측/최종 계약이 아니다. 최초 리뷰의 frame8MiB 제안은 opaque validation을 보유할 때 추가 normal map 비용까지 검증한 수치가 아니어서 그대로 동결하지 않았다. 현재 source의 HashMap load-factor/bucket 논리 estimator·실제 capacity·checked size_of로 각 경로의 합산을 검증해야 한다. 이 논리 byte accounting은 allocator/RSS 상한이 아니다. prepared geometry64MiB/128MiB의 기존 GPU 장부와 별도 legacy CPU copy 장부는 같은 수치라도 의미를 혼합하지 않는다.

engine-held frame slot은 ready·새 current/committed frame·held previous presentation/outcome 등 최대3개의 unique backing으로 제한한다. 같은 backing의 clone은 byte/slot을 다시 세지 않고 회수를 막는 pin이다. 다른 backing을 추가할 여유가 없으면 effect-free Busy이며 기존 화면/입력/actor는 유지한다. candidate device-ready를 보고하기 전 첫 committed packet에 필요한 frame reservation도 확보하고, Commit 뒤 capacity 부족이 처음 드러나지 않게 한다. 한 frame/candidate 자체가 per-item 상한을 넘거나 최소 한 항목도 진행할 수 없는 config는 typed budget/config 오류다. 신뢰된 외부 SDK의 임의 clone/String copy를 sandbox로 제한한다는 뜻은 아니다.

종료는 먼저 stop/cancel 요청, small final metadata 복사, 큰 outcome/packet/device owner 해제·회수 진행, actor/CPU/preparation worker의 finished 관측 순서다. `PacketWindowOutcome.final_packet`을 잡은 채 source 회수를 기다리는 coordinator에 join하지 않는다. `try_shutdown`은 실제로 finished인 thread만 join하며 다른 owner가 남으면 Stopping이다. Drop은 stop 요청만 한다. [설계 probe](../06-worklog/2026-10-17-native-host-design.md)에서 기존 PacketRenderer/PacketViewSubmission/ProjectRenderResources의 Send·RenderPacket의 Send+Sync는 compile 확인했다. 이는 실제 GPU completion drain/마지막 device drop의 검증이 아니다. 그 retirement owner는 후속 장치·종료 검사까지 열린 상태이며 winit Window/Surface를 임의 worker로 옮기지 않는다.

device disposition slot은 run_view보다 오래 사는 private host/CPU cleanup 상태에 둔다. 첫 one-shot view에서는 `Arc<OnceLock<Mutex<Option<WindowGpuOwner>>>>`처럼 단일 publisher가 한 번 설치하고 worker만 get/try_take하는 유한 slot을 쓴다. Window를 만들기 전에 slot과 one-shot latch를 준비한다. UI는 Mutex를 획득하지 않고 owner를 감싼 새 Mutex를 OnceLock에 한 번 설치하며, worker는 get(nonwaiting) 이후 try_lock/take해 lock을 놓고 drain한다. worker나 다른 경로는 set/get_or_init/wait를 호출하지 않으므로 concurrent initializer가 없다. 이 ownership 조건을 코드/테스트로 유지하고 정상 반환 경합을 owner 유실·mem::forget으로 처리하지 않는다. 두 번째 view는 자원 생성 전에 거부한다.

조사 근거는 [Rust 1.94.0 OnceLock 원본](https://raw.githubusercontent.com/rust-lang/rust/1.94.0/library/std/src/sync/once_lock.rs)이다(2026-10-17, S90). get은 대기하지 않지만 set은 다른 initializer와 경쟁하면 대기할 수 있다. 위 설계는 비공개 one-shot publisher로 그 경쟁을 배제하는 추론이며 OnceLock 자체나 host 전체가 형식적으로 wait-free라는 주장이 아니다. 실제 one-shot/반환/종료 시험은 별도로 남긴다.

반환 publication과 worker의 실제 수신·drain 완료를 별도로 추적한다. worker가 slot을 빈 상태로 poll한 직후 UI가 owner를 설치해도, 뒤늦게 returned 신호만 보고 worker가 종료해서는 안 된다. expected owner는 worker가 실제로 take한 뒤 drain/drop한 경우에만 종료 조건을 충족한다. 이 순서를 결정적으로 재현하는 CPU 시험을 포함한다. facade Drop 후에는 소비자가 사라진 진단 결과 slot이 cleanup thread를 영구 유지하지 않게 하며, 살아 있는 facade에는 오류를 보존한다.

CPU job의 catch는 CPU owner를 바깥에 보유한 범위에서 실행하며 job panic 뒤에도 owner가 device 반환 수신·drain/reclaim을 계속한다. run 결과는 UI가 surface/window를 해제한 뒤 그 slot에 이동한다. 실제 thread 자체가 backstop 밖에서 실패하면 Failed/Stopping과 retained disposition을 기록하고 정상 Drained/Stopped로 바꾸지 않는다. replacement reclaimer의 자동 복구는 첫 slice 범위 밖이며 테스트 없이 안전하다고 주장하지 않는다. host facade Drop 뒤에도 private cleanup 상태는 worker가 완료할 때까지 살아 있어야 한다. OOB control과 try_shutdown/status 경로는 blocking Mutex.lock으로 기다리지 않으며 metadata contention은 Busy/후속 갱신으로 남긴다.

## CLI에서 공통 library로 옮길 책임

현재 `project_view.rs`의 BindingMap/collect_bindings·latest_frame/submit_wait·InputTicket와 native 입력/ticket 진행은 host로 옮긴다. run_window/coordinate는 host의 typed coordinator·CPU owner·window lifecycle로 나누며 무거운 prepare/extract를 작은 입력 loop에 남기지 않는다. CLI의 ProjectViewArgs/validate_args, ProtocolController/StdioTransport·wire DTO/JSONL, build_report/emit_report와 파일 출력은 adapter로 남긴다. protocol의 Close는 host control handle에 연결하며 별도 Engine을 시작하지 않는다.

옮긴 코드의 동작을 복사한 두 번째 구현을 유지하지 않는다. 다만 P03.3c 이전의 기존 audio-view 경로는 지원을 잃지 않도록 별도 유지하고, 공통 입력/제어 helper 재사용 범위를 명시한다. 새 activation+audio 조합은 준비를 생략한 성공이 아니라 효과 전 unsupported 진단이다.

첫 additive CLI 전환은 explicit `--asset-activation` 경로이며 기존 non-activation/audio 경로는 보존한다. 이 한정 때문에 기존 non-activation packet coordinator 전체가 이미 제거됐다고 하지 않는다. protocol·diagnostic 입력 mapping은 adapter에 남기고 새 활성화 경로의 load/CPU/frame/stage/window/회수는 host만 담당한다. flag·report v2·Close/whole stop·파일 소유권은 [CLI activation 연결 계약](native-project-view-activation.md)으로 동결한다.

비장치 integration test는 새 crate의 public API만 import해 외부 temporary project start→attach/read/Step→status→stop/실제 shutdown을 수행한다. daedalus-cli를 dependency로 삼거나 CLI module을 test에 include하지 않는다. 후속 window hardware 검사는 실제 present→view close→같은 authority Step/read와 Stage/Commit 거부→whole shutdown을 확인한다. 이 최소 SDK 시험은 전체 C# SDK/AI 제작 제품 인수를 대신하지 않는다.

## 착수 전 남은 검증과 작업 분할

1. b.2 실제 native/PR 인수는 main360ceb6에서 확인. 과거 native 실패 원인 미확정은 보존.
2. runtime initial/candidate port·failure/cancel/ACK·Queued/Applied/Busy의 library 구현은 PR17에서 인수. 실제 renderer/window ACK 생성·host 전달은 후속.
3. renderer combined stage+old-ready draw·partial resource completeness·CPU leaf/frame registry의 library 경계는 PR19/mainc507835에서 인수. 실제 host 연결은 미완료.
4. 2026-10-17 새 public type의 auto-trait probe도 확인. PacketCpuOwner Send, CpuFrameHandle/PacketStageInput Send+Sync, PacketRenderer/Renderer Send와 service private Arc 전달이 해당 artifact에서 compile됐다. 실제 host frame 선점·window-close 후 device retirement·부분 초기화 실패 반환은 미검증이며 probe를 기능 통과로 쓰지 않는다.
5. Luna xhigh에 runtime/renderer/host/CLI 파일 소유권과 단일 Cargo lease를 나눠 단계별 구현. 기존 helper/프로토콜/service를 확장하고 새 smoke framework를 만들지 않음.

그 뒤 실제 같은 session의 source edit→Stage→Commit→현재 pose publication→실제 픽셀/present ACK→old CPU/GPU 회수를 검증한다. Rust SDK가 host를 소비하는 최소 통합도 확인해 CLI 안에서만 동작하는 구현을 통과시키지 않는다. P03 전체와 D1/D2/D3는 별도 최종 인수까지 계속 미완료다.
