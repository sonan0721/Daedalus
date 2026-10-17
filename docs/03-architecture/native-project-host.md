# Native project host: 엔진 라이브러리와 CLI의 조합 경계

상태: b.2 인수·runtime consumer 계약 동결·renderer/host API 최종 리뷰 전 · 2026-10-17

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

첫 view는 one-shot이다. 창을 닫으면 consumer도 Closed가 되며 reopening을 광고하지 않는다. 같은 authority의 read/Step 등 허용 operation은 whole-host `request_stop` 전까지 계속 가능하지만 새 Stage/Commit은 runtime의 consumer-closed 오류로 거부한다. CLI는 창 결과를 얻은 뒤 whole stop을 요청하고 nonwaiting 종료 진행을 관찰한다. view close와 whole-host stop을 같은 성공 상태로 합치지 않는다.

작은 status/outcome은 session·active generation/digest·activation/CPU owner/view lifecycle·논리 장부·bounded resource counters·latest accepted publication과 last actual-present identity/view·최대 유한 failure code/message를 구분한다. publication/authority sequence와 tick은 각각 보존한다. `PacketPresentationWindowOutcome`이나 final packet Arc를 public outcome으로 반환하지 않는다. window interval samples 등 기존 유한 report를 재사용하되 source/이미지/instance 배열을 넣지 않는다.

첫 host slice의 `start`는 현재 native 준비 factory처럼 **동기 initial load**다. 호출 전에 창이 생겼다고 하거나 async로 문서화하지 않는다. initial source를 두 번 읽지 않으려면 runtime의 additive render-consumer factory가 exact initial source lease를 제공해야 한다. 이후 교체는 같은 session 안에서 background 준비한다.

동기 초기 CPU 로드는 D1의 로딩 UI 요구를 면제하지 않는다. 후속 P02/P08 bootstrap은 아직 project가 없는 실제 Starting 상태에서 UI/입력을 표시하고, 준비 후 첫 실제 publication을 받도록 설계·검증해야 한다. 초기 packet을 임의로 꾸며 project load 성공으로 처리하지 않는다.

첫 GPU 준비에도 old-ready packet이 없다는 별도 경계가 있다. b.3 구체안은 같은 device의 clear-only/기존 loading overlay draw와 bounded initial stage chunk를 한 submit으로 묶는 additive packet-window bootstrap을 포함한다. 현재 `run_packet_windowed_presented`는 실제 initial packet을 요구하므로 새 경로는 아직 구현되지 않았다. 공통 WindowApp의 no-packet legacy Scene 분기가 이미 packet bootstrap을 지원한다고 설명하지 않는다. CPU initial load 이전에 창을 띄우는 전체 bootstrap과, load 이후 첫 GPU 자원을 나눠 올리는 이 경계는 구분한다.

bootstrap surface present는 game presented/ACK가 아니다. actor의 generation1/source는 이미 initial publication으로 존재할 수 있으나 view는 Starting·presented 없음이다. initial device-ready 뒤 실제 최신 SessionFrame에서 첫 packet을 추출하고 정상 draw/present 경계에서만 initial identity의 ACK를 보낸다. candidate/activation ID를 만들거나 초기 source를 다시 Commit하지 않는다. startup 실패도 candidate stage 실패로 위장하지 않고 host 초기 준비 실패/종료로 보고한다.

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

frame request도 latest-only snapshot 한 개, CPU 작업 중 한 개로 제한한다. coordinator는 snapshot 참조/identity를 전달하며 큰 extraction/validation을 실행하지 않는다. 교체된 pending snapshot/packet의 마지막 free는 CPU owner에 맡긴다. candidate prepare에는 frame 우선권과 유한 단위의 취소/checkpoint를 두어 큰 prepare가 입력/새 frame을 무기한 막지 않게 한다. 현재 renderer의 새 packet validation은 submit 경로에서 실행된다. 이를 이미 worker에서 한다고 보고하지 않는다. 후속 opaque CPU-validated input은 실제 packet/검증 limits와 묶고 기존 검사 로직을 재사용해야 하며, public legacy API의 검증을 boolean으로 건너뛰는 경로는 만들지 않는다. 정확한 type/retained validation 비용은 API 동결 대상이다.

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

## CLI에서 공통 library로 옮길 책임

현재 `project_view.rs`의 BindingMap/collect_bindings·latest_frame/submit_wait·InputTicket와 native 입력/ticket 진행은 host로 옮긴다. run_window/coordinate는 host의 typed coordinator·CPU owner·window lifecycle로 나누며 무거운 prepare/extract를 작은 입력 loop에 남기지 않는다. CLI의 ProjectViewArgs/validate_args, ProtocolController/StdioTransport·wire DTO/JSONL, build_report/emit_report와 파일 출력은 adapter로 남긴다. protocol의 Close는 host control handle에 연결하며 별도 Engine을 시작하지 않는다.

옮긴 코드의 동작을 복사한 두 번째 구현을 유지하지 않는다. 다만 P03.3c 이전의 기존 audio-view 경로는 지원을 잃지 않도록 별도 유지하고, 공통 입력/제어 helper 재사용 범위를 명시한다. 새 activation+audio 조합은 준비를 생략한 성공이 아니라 효과 전 unsupported 진단이다.

비장치 integration test는 새 crate의 public API만 import해 외부 temporary project start→attach/read/Step→status→stop/실제 shutdown을 수행한다. daedalus-cli를 dependency로 삼거나 CLI module을 test에 include하지 않는다. 후속 window hardware 검사는 실제 present→view close→같은 authority Step/read와 Stage/Commit 거부→whole shutdown을 확인한다. 이 최소 SDK 시험은 전체 C# SDK/AI 제작 제품 인수를 대신하지 않는다.

## 착수 전 남은 검증과 작업 분할

1. b.2 실제 native/PR 인수는 main360ceb6에서 확인. 과거 native 실패 원인 미확정은 보존.
2. runtime initial/candidate port·failure/cancel/ACK·Queued/Applied/Busy는 consumer 계약으로 동결. 구현/실제 검증은 후속.
3. renderer의 combined stage+old-ready draw·partial resource completeness·CPU leaf/frame registry와 수치 동결.
4. existing type의 Send/auto-trait compile probe3건은 확인. transient frame accounting·window-close 후 실제 device retirement owner는 미검증. probe를 production 기능 통과로 쓰지 않는다.
5. Luna xhigh에 runtime/renderer/host/CLI 파일 소유권과 단일 Cargo lease를 나눠 단계별 구현. 기존 helper/프로토콜/service를 확장하고 새 smoke framework를 만들지 않음.

그 뒤 실제 같은 session의 source edit→Stage→Commit→현재 pose publication→실제 픽셀/present ACK→old CPU/GPU 회수를 검증한다. Rust SDK가 host를 소비하는 최소 통합도 확인해 CLI 안에서만 동작하는 구현을 통과시키지 않는다. P03 전체와 D1/D2/D3는 별도 최종 인수까지 계속 미완료다.
