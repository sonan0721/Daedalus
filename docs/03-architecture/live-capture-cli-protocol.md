# Live capture의 protocol 2와 CLI 파일 전달

상태: P02.3a.4 source 착수 전 계약 · renderer a.2·host a.3 인수 대기 · 2026-10-18

## 만든 이유와 실제 선행 조건

AI가 같은 project session의 명령 결과와 실제 pixels를 연결하려면 요청뿐 아니라 PNG를 언제 읽고 언제 ACK하는지도 정해야 한다. [전체 capture](live-capture-and-artifacts.md)와 [host artifact](live-capture-host-artifacts.md) 계약을 기존 JSONL adapter에 적용하는 마지막 경계를 고정한다. Luna의 읽기 전용 CLI source 감사와 root의 runtime/CLI 대조를 반영했다. 이 문서는 실제 wire operation·파일 전달·GPU 검증 성공의 근거가 아니다.

현재 runtime은 `SessionOperation::CaptureFrame`과 일반 ticket을 제공하며 CLI도 compact capture 결과를 변환한다. 그러나 wire enum에는 capture operation이 없고 `live_capture`는 false다. host의 capture builder/status/artifact lookup은 아직 a.3 구현 전이다. metadata serializer가 있다는 이유로 기능을 켜지 않는다.

`project_view.rs::run`은 `--asset-activation`일 때만 `project_view/activation.rs`의 재사용 `NativeProjectHost`를 시작한다. 첫 a.4는 이 경로를 사용한다. 기본 viewer·headless serve·batch `project capture`를 같은 live session 경로로 보고하지 않는다. 기본 viewer의 host 통합은 후속 제품 경로로 추적하며, 첫 capture 인수만으로 P02/D1을 닫지 않는다.

## Feature와 준비 상태

기존 CLI Cargo feature `capture`는 offscreen renderer 지원 의미를 유지한다. additive `live-capture = ["window", "daedalus-project-host/capture"]`를 두고 host-backed viewer가 해당 빌드에서 `with_live_capture`를 명시적으로 사용한다. SDK의 기본 host options는 capture off를 유지한다. 별도 CLI runtime flag를 추가하지 않으며 compiled feature가 없는 경로의 기존 실행 의미를 보존한다. 실제 feature 이름은 a.3 host manifest와 통합 시 대조한다.

operations 목록은 attached capture extension을 표시할 수 있지만 `live_capture: true`는 실제 host/renderer handshake가 Ready이고 해당 세션과 binding이 유효할 때만 반환한다. compiled flag·창 생성·이전 offscreen PNG만으로 true를 만들지 않는다. 초기 Starting과 Unsupported/Detached를 구분하고 호출자는 capabilities/status를 다시 읽을 수 있다. 기본 viewer에는 임의의 두 번째 capture actor를 붙이지 않는다.

## 요청과 기존 ticket

기존 protocol 2 envelope의 Submit 안에 아래 operation 하나를 추가한다.

```json
{"protocol":2,"request_id":"capture-1","request":{"op":"submit","operation":{"op":"capture_frame","at_least_tick":"1","timeout_ms":"5000"}}}
```

| Field | 계약 |
| --- | --- |
| `at_least_tick` | optional decimal-string u64. 생략하면 기존 actor dispatch 시점의 tick floor. 과거 pixels 조회나 implicit Step이 아님 |
| `timeout_ms` | optional decimal-string u64, 1..=30000. 생략하면 runtime 기본 5000ms. admission 때 시작한 deadline을 polling/dedup이 연장하지 않음 |

unknown field·잘못된 decimal·0/초과 timeout은 enqueue 전 `invalid_request`다. `command_id`, output path, dimensions, format, raw bytes/base64를 받지 않는다. 초기 형식은 실제 surface physical size의 PNG RGBA8/sRGB다. 실제 source generation/digest와 publication은 기존 actor/renderer가 bind하며 wire caller가 지정하지 않는다.

`Submitted → Poll → Ack/Forget`와 request_id dedup·기존 ticket token을 재사용한다. 같은 retained request_id/operation은 원래 ticket을 반환하고 payload 변경은 기존 collision이다. Capture는 normal ticket credit을 쓰고 reserved Step slot을 침범하지 않는다. 별도 CLI capture ledger·Engine·source reload·authority를 만들지 않는다.

## 성공 metadata와 실제 파일

기존 `CaptureFrameOut`의 session/tick/publication/authority snapshot/generation/source digest/scene revision/present sequence·actual view/projection/physical size·surface format/encoding·PNG format/length/SHA를 유지한다. artifact의 full `{session, client, sequence}`는 모두 기존 DecimalU64 decimal string이며 표시·상관관계 metadata다. 직렬화한 ID는 lookup 권한이 아니다.

성공 Poll은 terminal `CaptureFrameResult::artifact()`의 **실제 strong capability**를 attached host control의 `try_acquire_capture_artifact`에 넘긴다. host/session/ID와 weak interest의 실제 identity가 맞는 lease를 받은 경우에만 `artifact.local_spool_path`를 추가한다. ID로 파일명을 조합하거나 arbitrary path를 확인해 성공으로 대체하지 않는다. immutable path getter만 사용하며 이 단계에서 open/read/hash/PNG copy를 수행하지 않는다.

기존 bounded `TicketEntry`에 첫 검증된 host artifact lease를 보관한다. 이후 Poll은 같은 binding을 재사용해 registry lookup과 PNG 복제를 반복하지 않는다. `poll_ticket`의 mutable 접근과 feature-gated optional lease는 기존 adapter 안에서 처리한다. 새로운 독립 registry를 CLI에 만들지 않는다. UTF-8·응답 크기 제한 안에서 정확한 local path를 표현할 수 없으면 typed Poll 오류로 남기며 lossy path나 잘린 path를 성공으로 반환하지 않는다.

wire caller는 ACK 전에 파일을 읽거나 자신의 디렉터리로 복사하고, PNG decode·크기·byte length·SHA를 metadata와 대조한다. ACK는 runtime ticket과 CLI의 artifact lease를 함께 해제한다. Forget/detach도 client interest를 해제하되 GPU/encoder/file 작업의 완료나 rollback을 의미하지 않는다. ACK/Forget/transport detach 뒤 serialized path의 존재를 보장하지 않는다. native SDK에서 ACK 뒤에도 읽어야 하면 host의 capability-bound reader/lease를 별도로 유지한다.

## 오류와 transport 종료

admission 오류는 기존 `capture_unavailable`, `capture_starting`, `capture_unsupported`, `capture_detached`, `capture_capacity`와 권한·queue/result capacity·stop 매핑을 유지한다. accepted terminal 오류는 기존 `CaptureFailure` 코드로 반환하며 PNG 성공 metadata/path를 붙이지 않는다.

artifact lookup Busy는 retryable `capture_artifact_busy`이고 원래 terminal ticket을 보존한다. foreign/session mismatch·expired/not-ready binding·표현 불가능한 path는 구체적인 typed Poll 오류로 구분한다. 정확한 host error variant와 CLI code 매핑은 a.3 public header를 인수한 뒤 source 전에 대조한다. Poll 오류를 runtime terminal 결과나 ACK 성공으로 바꾸지 않는다.

stdin EOF는 기존 transport detach이며 창을 즉시 닫지 않는다. stdout queue Full/BrokenPipe도 기존 detach 경계를 따른다. tickets/CLI leases가 해제된 뒤에도 host의 work lease·readback·encoder/spool cleanup은 계속된다. 출력 worker는 raw bounded JSON bytes만 소유하고 host/ticket/GPU/file lease를 잡지 않는다. 따라서 detach 전 큐에 있었지만 아직 flush되지 않은 artifact 응답은 나중에 전달돼도 path 유효성을 보장하지 않는다. writer를 기다려 파일을 영구 보유하거나 창 callback/host shutdown에서 blocked stdout join을 하지 않는다.

Close는 새 제출 차단과 view-close 요청이다. 창 종료·coordinator/CPU owner·runtime service·encoder·spool의 실제 정리까지 기존 host shutdown을 진행한다. timeout terminal이나 stdout 종료만으로 image slot 회복/Stopped를 보고하지 않는다. native reader가 남거나 실제 I/O 정리가 실패하면 해당 수명을 보존하고 상태/오류를 보고한다.

## Source 분담과 인수

| Source | 범위 |
| --- | --- |
| CLI `Cargo.toml`, release/local build profiles | 새 feature와 실제 packaged binary의 backend 포함 여부 |
| `project_view_control.rs`, `project_view_control/tests.rs` | strict wire parse·operation/capabilities·기존 ticket의 lease·Poll/path/ACK/Forget·typed 오류 |
| `project_view/activation.rs` | 같은 host의 capture opt-in·control attachment·종료/report 연결 |
| `project_view.rs` | 필요한 validation/help만 조정. 기본 viewer를 조용히 다른 actor로 대체하지 않음 |
| `scripts/smoke_project.py`, `scripts/test_smoke_project.py` | 기존 same-window/process helper에 opt-in capture assertions 추가, 외부 전달 binary 사용 |

CPU 검사는 timeout/default/upper bound·bad decimal/unknown field·권한/Starting/Unsupported·dedup/collision·normal/image capacity·실제 capability 경로·ACK/Forget/detach와 no-base64를 확인한다. host registry 자체의 reader-after-ACK와 work/file 회수는 a.3 검사를 재사용하며 serializer만 시험해 대체하지 않는다.

실제 Mac smoke는 기존 imported model/sprite·asset activation 창의 같은 invocation을 확장한다. 초기 capture, mutation→Step와 generation commit 뒤 capture, PNG decode/known pixels/length/SHA·actual source/publication/authority/view 대조, ACK 뒤 다음 capture, no-eligible-frame timeout·pending close·막힌 stdout/EOF와 실제 정리를 확인한다. 기존 `pixel_capture: None`을 실제 증거로 바꿀 때만 보고서를 갱신한다. audio는 기존 별도 sub-smoke를 유지한다. 현재 금지된 `--asset-activation --audio`를 이 작업에서 허용하거나 동일 session의 audio 증거로 합산하지 않는다. 별도 batch render나 metadata-only fixture는 live capture 검증을 대신하지 않는다.

source freeze 뒤 relevant workspace/managed/Python 검사, 실제 Mac 창 검증, 두 native package와 다섯 CI job·검증 head/main tree를 확인한다. publish/tag는 이 작업에 포함하지 않는다. a.2/a.3가 통과하기 전 a.4 source를 시작하지 않으며, a.4 이후에도 P02의 MCP/input/assert/trace와 D1의 다른 기능 게이트는 열려 있다.

현재 source 확인: `session_control.rs::SessionOperation/try_submit`, `session_capture.rs`의 deadline/opaque capability, CLI `WireOperation/ProtocolController/TicketEntry/capture_frame_out`, `project_view.rs::run`, `project_view/activation.rs::run`, host `NativeProjectHost::start/control`, 기존 `smoke_project.py`의 activation window sub-smoke. 이는 로컬 pinned source 감사이며 미래 host API의 구현 성공을 뜻하지 않는다.
