# Managed packet window: stage·frame·실제 present 경계

상태: source·실제 same-window 교체 metadata·PR20/다섯 CI/package 인수 · 2026-10-18 · live pixels/전체 통합 인수 전

## 목적과 선행 조건

[Native host](native-project-host.md)가 같은 authority/device에서 source 교체를 제어할 수 있도록 기존 WindowApp을 확장한다. 기존 packet 창은 initial RenderPacket을 요구하고 final_packet Arc를 결과에 남긴다. 새 경로는 실제 initial **resource-only PacketStageInput**으로 시작하고 큰 CPU/GPU owner는 [owner 수명 계약](native-renderer-owner-lifecycle.md)에 따라 반환한다. CLI/게임마다 새로운 event loop·surface·camera·upload planner를 구현하지 않기 위한 문서다.

초기 source load와 initial CPU prepare는 첫 host slice에서 창을 만들기 전에 수행한다. 따라서 여기서의 bootstrap은 **GPU 준비 중 clear/loading**이며 CPU load 전의 D1 loading UI는 별도 미완료다. 초기화 지연을 숨기거나 가짜 initial frame을 만들지 않는다. 기존 public 함수·message enum·outcome·WindowOptions 필드/default는 보존한다. 새 options가 WindowOptions를 감싼다.

## additive public API

| symbol | 계약 |
| --- | --- |
| `managed_packet_window_channel()` | non-Clone client와 move-only server를 반환. payload sync_channel capacity는 1로 고정 |
| `ManagedPacketWindowOptions` | `window: WindowOptions`, `packet_limits`, `asset_limits`, `initial_stage_limits`. managed wrapper가 bootstrap loading을 선택하며 legacy default는 변경하지 않음 |
| `run_packet_windowed_managed(initial, server, options) -> ManagedWindowRun` | 호출 thread의 OS event loop에서 실행. top-level Result로 owner를 잃지 않음 |
| `ManagedWindowRun` | `outcome: Result<ManagedWindowOutcome, ManagedWindowError>`, `status: ManagedWindowStatus`, `gpu_owner: WindowGpuOwner`. 모든 성공/오류/unwind 반환에 owner 존재 |
| `ManagedPacketWindowInput::BeginStage { key, input, limits }` | 새 후보의 실제 canonical StageInput. runtime type을 renderer에 전달하지 않음 |
| `ManagedPacketWindowInput::Frame { key, frame }` | 실제 SessionFrame에서 CPU owner가 만든 `Arc<CpuFrameHandle>`. UI 재검증 map 생성 없음 |
| `client.try_send(input)` | std TrySendError의 Full/Disconnected에서 원래 소유 payload를 그대로 반환. enqueue 성공은 device stage/Commit/present 성공이 아님 |
| `client.control()` | cloneable `ManagedPacketWindowControl`. OOB signal·유한 status만 소유, source/frame/packet/device의 strong/weak roots 없음 |
| `client.try_stage_event()` / `try_presented()` / `try_status()` | 작은 metadata만 nonwaiting 조회. contention/empty/closed 구분, blocking lock·채널 wait 없음 |

`ManagedStageKey`는 opaque u64 local key다. Initial은 1, candidate는 checked_next로 2부터 증가한다. host가 단독 발급하고 runtime Initial/Candidate identity와 별도 map으로 묶는다. 준비 ID·activation ID·generation·CPU generation ID·GPU PacketStageId와 혼합하지 않는다. Deferred 재시도에는 같은 key를 사용하며 동시에 두 stage를 접수하지 않는다. 그 외 새 후보는 이전 key보다 커야 한다. sequence exhaustion은 wrap 없이 유한 오류다.

options/key/frame identity 검사는 효과 전에 한다. WindowApp의 packet 유무로 mode를 추정하지 않고 private Legacy/Packet/Managed mode를 사용해 initial frame이 없는 동안 legacy Scene 분기로 진입하지 않는다. 신규 module·helper는 window.rs와 그 하위 module에 두며 renderer init/source와 상호 소유권을 침범하지 않는다.

## 취소와 두 종류의 event

control은 `request_view_close`, `request_host_stop`, `request_cancel_through(key)`를 제공한다. close/stop atomic과 별도 monotonic cancellation watermark를 사용한다. cancel-through는 그 key 이하의 pending/준비 stage를 무효화하며 이미 committed active frame을 지우지 않는다. admission 전·bounded upload pump 사이·Ready 게시 직전에 확인한다. payload queue가 가득 차도 동작한다. host supersession은 기존 key를 취소한 뒤 더 큰 새 key를 전달한다.

stage event는 capacity1의 **비병합** 슬롯으로 Ready/Failed/Cancelled/Deferred를 전달한다. Deferred는 try_begin_stage가 입력을 소비한 Busy/일시 capacity 거부를 뜻하며 host는 동일 CPU generation의 `stage_input(id)`를 다시 요청한다. import/prepare/validation을 UI나 coordinator에서 반복하지 않는다. 영구 budget/config 오류를 Busy로 재시도하지 않는다.

host가 event를 복사하기 전에는 새 BeginStage를 dequeue/admit하지 않고 슬롯을 덮어쓰지 않는다. 이미 게시한 Ready 이후 늦은 취소는 Ready를 지우지 않는다. 이것은 그 시점의 device 준비 사실이며 runtime currentness가 stale 결과를 거부한다. host는 복사한 metadata를 runtime Queued→Applied 또는 stale/closed 거부까지 별도로 유지한다. renderer는 actor 상태를 알지 않는다.

actual-present는 stage와 별도의 latest-only metadata 슬롯이다. 같은 committed generation 안에서 최신 값으로 합칠 수 있지만 미관측된 이전 generation의 ACK를 다음 generation으로 덮어쓰지 않는다. host는 복사한 값을 actor Applied까지 유지한다. slot 여유가 없으면 새 generation frame의 game present를 보류하고 기존 화면은 유지한다. status/progress coalescing이 이 두 필수 event를 지우지 않는다.

## bootstrap·active·candidate와 실제 표시

1. Initial input으로 stage를 접수한다. 아직 CpuFrameHandle이 없으므로 기존 combined `render_cpu_frame_with_stage(None, Some(stage))`가 clear/loading과 bounded chunk를 같은 submit에 넣는다. 임시 유효 view는 clear 제어에만 쓰고 게임 view로 보고하지 않는다.
2. GPU 초기 upload serial이 실제 완료된 Ready event를 host에 보낸다. 이는 source Commit이나 화면 ACK가 아니다. host가 실제 최신 SessionFrame을 CPU owner에 전달한다.
3. matching key·generation/digest·publication/snapshot sequence·tick을 가진 실제 CpuFrameHandle만 첫/후속 게임 draw로 사용한다. UI는 active latest frame과 candidate stage를 별도로 보유한다. 후보 준비 동안 최신 active pose와 로컬 camera/input 처리를 유지한다.
4. successful submit 이후 `SurfaceTexture::present()`를 실제 호출한 경계에서만 checked game present sequence/event를 기록한다. bootstrap surface present 수는 별도이며 game ACK는 0이다. scanout/compositor 표시의 증명이라고 하지 않는다.

present metadata는 key, session, generation, digest, publication sequence, authority snapshot sequence, tick, checked present sequence, physical width/height, **실제 submission RenderView**와 그 view/physical aspect로 기존 renderer 수식이 계산한 view_projection `[f32; 16]`을 포함한다. host는 이 실제 행렬을 runtime ProjectPresentedView에 복사하며 renderer camera 수학을 재구현하거나 표시용 view에서 다른 aspect로 추측하지 않는다. publication과 authority snapshot sequence가 같은 값이라고 가정하지 않는다. WindowInputSink와 기존 local camera/input/report/frame-interval 표본을 재사용한다.

digest는 진단용 message와 달리 원본 그대로 복사하는 식별자다. 현재 content의 canonical 표기는 `sha256:<64 lowercase hex>`로 71바이트이며 기존 renderer는 prefix 없는 hex도 검증한다. UI identity·stage summary·present·host summary에서 64바이트로 잘라서는 안 된다. 허용 형식/길이는 기존 검증 계약으로 제한하고, 실제 PreparedProject/CPU frame에서 metadata까지 정확히 같은 digest가 유지되는지 검사한다.

GPU Busy는 acquired surface frame을 버리고 present/ACK하지 않는다. Timeout은 재시도, Lost/Outdated는 재설정하며 실제 draw/present 전에는 ACK하지 않는다. OOM/device 오류·unwind는 bounded failure와 보존된 owner를 반환한다. stage 실패/취소가 기존 ready generation을 표시 가능한 상태에서 훼손하지 않는다.

## 종료·오류와 파일 소유권

ViewClosed/Escape, HostStop, TimeLimit, FrameLimit과 Failed를 metadata에서 구분한다. view-close는 one-shot consumer 종료이며 authority Step/read를 중지하지 않는다. whole-stop만 전체 host를 닫는다. renderer는 이 구분을 signal/status로 알리고 runtime을 직접 조작하지 않는다.

app을 catch 바깥에 유지하고 owner를 먼저 설치한다. validation/EventLoop 생성 실패도 NoDevice owner를 반환한다. 내부 unwind/failure를 수집한 뒤 UI의 frame/packet/queued aliases → SurfaceState → Window 순서로 정리하고 owner를 run 결과로 이동한다. 초기/완성 owner의 마지막 drop을 UI에서 하지 않는다. UI에서 drain loop나 unfinished join을 실행하지 않는다. bounded failure는 engine 코드·유한 메시지만 가지며 panic payload 복사는 하지 않는다.

Luna A는 window.rs·window 하위 module/테스트만 소유한다. Luna C가 renderer/gpu owner·lib.rs export를 단독 소유하며 API를 공유한다. root가 문서·리뷰·Git과 검증 lease를 관리한다. source/manifest/CLI/runtime를 이번 window task에 섞지 않는다.

## 검증과 남는 제품 gate

CPU tests는 exact payload의 Full/Disconnected 반환, key overflow/Deferred retry, OOB close/stop/cancel, stage/present/status 독립성, zero bootstrap ACK, mode/legacy defaults, generation 전환과 sequence/currentness 거부를 확인한다. 실제 GPU tests는 partial owner와 기존 stage 경계를 재사용한다.

실제 native 창의 bootstrap→real frame, old pose during upload, view/physical dimensions, surface Busy/Timeout 무ACK, 오류/내부 panic의 owner 반환과 Surface/Window 정리 순서는 후속 host/CLI hardware 검증에서 실행한다. libtest thread에서 macOS main event loop를 임의로 만들거나 새 smoke framework를 추가하지 않는다. deterministic seam으로 확인하지 못한 surface 오류는 미검증으로 남긴다. 모든 CPU 검사가 통과해도 실제 창·SDK·same-session source 교체·retirement와 P03/D1/D2/D3는 별도 인수 전 미완료다.
