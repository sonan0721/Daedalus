# P03.3b.3.1 Runtime-neutral project generation consumer

상태: runtime 경계 구현 계약 동결·구현 전 · 2026-10-17 · 선행 b.2/PR16 인수

## 목적과 범위

[native host](native-project-host.md)가 같은 authority의 CPU source를 받아 renderer를 준비하고 실제 표시를 보고하는 유한 경계다. runtime은 wgpu/renderer/window type을 import하지 않는다. 기존 headless factory·Stage/Commit/replay 의미는 보존한다. 이 slice의 테스트는 trusted consumer의 보고를 실제 actor/worker에 연결하는 것이며 GPU upload나 화면 통과의 근거가 아니다.

PR16의 head523757a·CI synthetic8d5bbf1·main360ceb6은 tree57d87c2ab924e5efd2690313df7330e0eab1f0db로 같다. native 회복·combined72·최종 다섯 CI·원본 실패는 [선행 기록](../06-worklog/2026-10-17-native-host-design.md)에 연결한다. 전체 b.3/P03/D1/D2/D3는 미완료다.

## factory와 source lease

| API | 동결한 의미 |
| --- | --- |
| 기존 `NativeProjectActivationService::start_headless` | 동작/API 보존. render consumer를 만들지 않으며 CPU compatible이면 기존 Staged |
| `start_with_render_consumer(options, config)` | 같은 service와 non-Clone `ProjectGenerationConsumerPort`의 tuple 반환. 기존 start error 재사용. 초기 import 한 번·동일 source Arc를 actor/worker/initial lease에 연결. startup 전 consumer 등록, late attach 없음 |
| `try_take_initial()` | `Result<Option<ProjectSourceLease>, ProjectConsumerError>`. 한 번의 실제 initial payload; 이후 None. session/generation1/digest이며 PreparationId/ActivationId 없음 |
| `try_take_candidate()` | 같은 Result/Option. authored-compatible Stage의 실제 source lease 한 개. pending payload capacity1. 없어도 실패/가짜 후보를 만들지 않음 |
| lease의 `identity()/with_source()/is_current()/source_pin()` | non-Clone·Send lease. borrowed callback은 `&PreparedProject`만 제공하며 Arc getter 없음. currentness는 checkpoint 관측이지 callback 전체의 원자적 보장 아님 |

`ProjectSourceIdentity`는 Initial 또는 Candidate 의미를 구분한다. Candidate에는 실제 SessionId·PreparationId·ActivationId·expected/proposed generation·source digest가 모두 들어간다. proposed는 checked expected+1이며 digest는 최대71byte의 기존 SHA-256 표현이다. caller 보고는 shared copy 전에 크기를 검사한다. source pin은 **표준 `Arc<dyn Send + Sync>`**로 반환하며 private 값이 실제 PreparedProject Arc를 보유한다. host는 이를 renderer-local opaque pin에 넣는다. runtime-defined type을 renderer가 import하거나 새 marker crate/Any/downcast를 추가하지 않는다. source pin은 renderer registry/backing을 역으로 소유하지 않는다.

source pin/lease가 남아 있으면 기존 preparation worker의 canonical Arc try_unwrap이 실패하므로 실제 source charge도 유지된다. 빈 scene도 같은 pin/소유권을 갖는다. 신뢰된 Rust caller가 borrowed project를 임의로 Clone하는 것까지 sandbox로 제한하는 API가 아니다. 엔진 내부는 independent leaf를 장부 밖으로 clone하지 않는다.

lease의 currentness cell은 작은 독립 atomic 상태다. shared payload slot에 저장된 lease가 다시 port/shared registry 전체를 소유하는 Arc cycle을 만들지 않는다. source marker도 PreparedProject만 pin한다. stop/actor failure에서는 아직 take하지 않은 initial/candidate payload와 ACK 대기 pin을 해제해야 하며, Port 객체 자체가 살아 있다는 이유로 source worker가 영구 Stopping에 남지 않는다. 외부가 실제 take한 lease/pin은 해제까지 정직하게 Stopping을 유지한다.

## 보고의 접수와 actor 반영

| API/type | 의미 |
| --- | --- |
| `try_report_stage(identity, outcome)` | outcome은 Ready 또는 Failed. exact current/taken candidate만 허용. initial/future/foreign/stale/conflicting 보고 거부 |
| `ProjectConsumerStageFailure` | CpuPrepare/DevicePrepare phase·최대64byte code·최대2048byte message. shared copy 전에 길이 검증, 큰 source/path payload 없음 |
| `ProjectConsumerReportProgress::Queued` | capacity-one report slot에 접수됨. actor 반영/Commit 권한이 아직 아님 |
| `ProjectConsumerReportProgress::Applied` | 같은 내용이 실제 actor에 반영됨. 이미 반영된 같은 보고 재시도도 idempotent |
| `ProjectConsumerError` | Busy/Closed/Stopped/ForeignSession/StaleIdentity/ConflictingReport/InvalidReport/Poisoned의 typed 단위 오류. Busy는 효과 전 거부이며 같은 identity 재시도 가능 |

ready/failure terminal slot은 한 개이며 Queued→Applied를 보존한다. 다른 identity가 다음 Stage를 수락할 때만 이전 terminal metadata를 교체한다. 새 caller ID/replay framework를 만들지 않는다. 같은 identity의 Ready와 Failed를 바꾸는 재시도는 idempotent가 아니며 거부한다. 취소/stop/supersession으로 무효화되면 늦은 Queued 보고도 Staged 권한을 얻지 못한다.

render-consumer mode에서는 compatible Stage result를 받은 뒤에도 기존 AssetActivationState의 **Staging**을 유지한다. 별도 consumer status가 CPU handoff/WaitingDevice/Ready/Failed/Closed를 구분한다. Ready가 actor에 Applied되어야 Staged/Commit 가능하다. 기존 headless 상태·CLI enum에 새 variant를 억지 추가하지 않는다. Commit 전 ready는 host가 complete upload ranges·required pipelines·final upload serial 완료와 첫 committed frame reservation을 확인했다는 trusted 보고다. runtime이 GPU를 직접 검증하는 것은 아니다.

## 실제 publication의 present ACK

`ProjectPublicationIdentity`는 `SessionFrame`에서만 생성하는 opaque 작은 값으로 session/tick/publication sequence/authority snapshot sequence/generation/digest/scene revision을 복사한다. snapshot/source Arc를 보유하지 않고 raw scalar constructor를 공개하지 않는다. 이 값은 실제 게시 frame의 identity이며 GPU 제출/표시 증거 자체는 아니다.

host는 actual present 뒤 `ProjectPresentedFrame`을 구성한다. 필드는 publication identity·consumer 내 checked 단조 present_sequence·runtime-neutral view metadata다. `ProjectPresentedView`는 physical width/height와 실제 사용한16float view-projection을 보유하며 양수 크기/finite 값을 검증한다. 생성/보고는 trusted host 경계다. 같은 publication을 다른 local camera로 다시 그려도 present_sequence로 구분한다. renderer type/serialized renderer JSON을 runtime에 넣지 않는다.

`try_ack_presented(presented)`도 Queued/Applied를 반환한다. 별도 latest-only ACK slot은 현재 committed generation 안에서 더 최신 present_sequence로 coalesce할 수 있다. actor는 실제 publication token의 session·current generation/digest를 확인한다. publication/authority sequence·tick은 현재 게시 값보다 앞설 수 없고 마지막 Applied 값보다 되돌아갈 수 없다. present_sequence도 단조 증가해야 한다. 이전/다른 generation에 Applied였다는 이유로 새 generation의 ACK로 취급하지 않는다. 정확한 duplicate는 Applied, 이미 더 최신 ACK가 적용된 같은 generation의 이전 보고는 그 watermark로 충족될 수 있으나 latest view를 되돌리지 않는다. 충돌하는 같은 present_sequence는 거부한다.

초기 generation1은 이미 실제 source publication이며 candidate/Commit을 만들지 않는다. initial GPU ready는 host-local이고 첫 실제 game present ACK 전까지 consumer는 Starting/WaitingPresentation이다. bootstrap clear/loading 화면은 game ACK가 아니다. 새 Commit은 현재 committed generation의 필요한 ACK가 Applied되지 않았다면 효과 전 ActivationBusy다. Stage/Prepare·일반 authority Step/입력은 독립적으로 진행할 수 있다.

Commit 후 old source의 ACK 대기 pin 한 개를 runtime이 유지한다. matching new-generation ACK가 actor에 Applied될 때만 이 pin을 반환한다. actual old GPU serial·CPU leaf aliases의 pins는 host가 별도로 유지하므로 ACK만으로 source charge가 사라지지 않는다. source swap/ACK/actual reclamation은 독립 상태다. post-Commit 표시 실패/consumer closure는 committed source를 되감지 않는다.

## 상태·한도·종료

port의 `try_status()`는 small consumer status를 nonwaiting으로 읽는다. initial view 상태·현재 stage identity/outcome/report progress·마지막 Applied present·bounded failure/closed 상태만 포함한다. 전체 논리 DTO는 기존32KiB result ceiling 안에서 검사한다. public control 결과/JSON에 source Arc/lease를 넣지 않는다. 이 첫 slice는 port와 같은 actor의 getter이며 CLI/MCP wire 추가는 후속 adapter다.

pending lease1·terminal report1·present ACK1·ACK 대기 old-source pin1을 둔다. status/slot 접근은 try-lock이며 큰 작업을 lock 안에서 실행하지 않는다. stop/consumer-closed/currentness 표시는 out-of-band atomic/기존 wake 경계로 전달하고 queue의 여유를 기다리지 않는다. actor가 control queue 포화 중에도 이 신호/보고를 처리해야 한다. stage request마다 thread를 만들지 않는다.

port Drop은 기다리지 않고 Closed를 표시한다. 아직 commit하지 않은 candidate는 취소/실패로 수렴하며 새 Stage/Commit은 기존 AssetActivationUnavailable 등 기존 typed session 오류로 효과 전 거부한다. 일반 Step/read는 살아 있는 authority에서 계속 가능하다. Closed/stop은 불가능해진 ACK 대기 pin을 abandoned로 반환할 수 있으나 presented 성공을 기록하지 않는다. host의 GPU/CPU pins는 실제 drain/회수까지 남는다. actor/worker panic·startup 실패에서도 기존 canonical worker backstop과 source charge를 유지한다. try_shutdown은 실제 join 가능 상태만 관측하고 held pin이 있으면 Stopping이다.

## 구현 소유권과 필수 검증

Luna xhigh 한 명이 runtime의 새 private `project_generation_consumer.rs` 및 project_preparation/session_control/project_activation/lib를 소유한다. manifest/lock·renderer/CLI/CI는 수정하지 않는다. public surface가 위 계약과 다를 필요가 있으면 dependent 코딩 전에 조정자에게 알린다. 기존 headless/start/replay/comparator/worker cleanup 경로를 공유한다.

실제 파일/source와 actor/worker로 initial take once·동일 digest/no second load·Stage→WaitingDevice·ready Queued/Applied→Staged·same Engine Commit/current pose를 검사한다. stale/foreign/conflicting/duplicate reports·Busy effect-free·cancel/supersede/consumer Drop·stop/panic·report slot 포화와 reserved Step 진행·실제 SessionFrame token ACK·동일 publication의 다른 view·future/foreign ACK 거부를 포함한다. initial ACK 전 Commit과 committed 미표시 중 다음 Commit은 Busy다. old source는 ACK 전/held pin 중 계속 charged이고 실제 worker final drop 뒤 회복해야 한다. empty scene도 같은 경계를 검증한다.

headless 기존 회귀·default/all-feature runtime·strict Clippy/fmt와 적절한 workspace 통합을 수행한다. fresh exclusive `p03-consumer-*attemptNN` 로그를 쓰고 exact filter의 실제 실행 수를 확인한다. mocked GPU Ready/ACK를 이용한 이 runtime 테스트를 actual GPU/present 인수로 보고하지 않는다.
