# 같은 실행 세션의 비동기 frame capture

상태: P02.3a 구현 전 계약·runtime/renderer 독립 bridge 감사 반영 · 2026-10-18 · DEV04/DEV06

후속 [요청·이미지·artifact 소유권 계약](live-capture-ownership.md)은 ACK/reader·timeout·GPU map·worker/file 수명의 독립성, concrete capability와 weak interest, coordinator unwind 밖의 backstop을 고정한다. public signature를 source와 대조한 뒤 runtime foundation부터 구현하며 아직 live_capture capability를 켜지 않는다.

## 목적과 현재 차이

AI가 `수정 → 적용 확인 → 실제 frame 관찰 → assertion`을 같은 실행 세션에서 반복하게 한다. 현재 `latest_frame`은 metadata이며 PNG가 아니다. batch `project capture`와 창 종료 후 capture는 별도 render다. 이를 live capture로 부르지 않는다. 현재 protocol 2는 `live_capture: false`이며 아래 기능을 구현·실측한 뒤에만 바꾼다.

별도 CLI ticket ledger나 새 Engine을 만들지 않는다. [공통 service](session-control-service.md)의 submit/poll/ack/forget 수명에 renderer-neutral capture 요청과 bounded handoff를 추가한다. runtime은 wgpu/window/PNG encoder를 의존하지 않는다. 첫 범위는 native packet project window·PNG 한 형식이다. MCP resource와 web readback·영상은 후속이다.

## Capture가 뜻하는 frame

요청은 **window가 reliable handoff를 등록한 뒤의 첫 eligible render/copy 제출과 present 호출**을 선택한다. 이는 service admission보다 뒤지만 routing 동안 이미 지나간 frame까지 첫 frame이라고 보장하지 않는다. wgpu의 present()는 성공 여부를 반환하지 않으므로 성공 scanout을 판정하는 표현으로 쓰지 않는다. optional `at_least_tick`은 최소 tick이며 과거 tick의 pixels를 복구하는 기능이 아니다. Manual session에서도 capture가 tick을 몰래 진행하지 않는다. caller는 필요한 mutation/Step을 먼저 확인한다. deadline 전에 조건에 맞는 presentation이 없으면 typed timeout이다.

요청은 session과 source generation을 bind한다. active source가 달라졌으면 새 generation을 조용히 캡처하지 않고 StaleGeneration으로 실패한다. packet sequence rollback/session mismatch는 기존 규칙을 유지한다. renderer residency pressure가 이전 ready packet을 그린 경우 capture identity는 incoming candidate가 아니라 `render_packet_view`가 **실제 그린 packet**이다. 조건을 만족하지 않으면 그 frame은 선택하지 않는다.

결과는 session/tick/packet sequence/project generation/source digest, **present_serial**, effective local camera와 tagged perspective/orthographic projection(정사영 vertical_size 포함), 실제 surface physical width/height, color format과 PNG metadata를 포함한다. present_serial은 같은 packet을 다른 camera로 여러 번 그려도 구분하는 window-local monotonic counter다. checked 증가·고갈 실패를 정의하며 packet sequence로 대체하지 않는다. physical 크기는 선택한 frame 시점의 값이고 requested logical 크기나 이후 resize 값이 아니다.

copy는 그 frame의 scene·loading overlay를 모두 그린 뒤 해당 acquired surface texture에서 present 전에 기록한다. 같은 queue에서 draw→copy가 순서대로 제출되면 별도 submission도 허용하며, 불필요하게 기존 내부 submit을 전부 refactor하지 않는다. draw/copy 제출을 확인하고 present()를 호출한 frame만 선택 기록에 확정한다. present() 자체는 unit 반환이므로 OS presentation 성공을 확인했다고 표현하지 않는다. GPU map/PNG 성공을 surface scanout 증명으로 확대하지 않는다. 선택된 frame의 camera/projection/크기/identity는 이후 frame에 덮어쓰이지 않는다. [P03.2b 계약](sprite-runtime-and-orthographic-view.md)의 실제 PacketViewSubmission/PresentedView를 재사용하며 incoming/source initial camera에서 다시 추측하지 않는다. runtime 결과의 view DTO는 renderer-neutral 값으로 변환하고 runtime에 wgpu 의존성을 넣지 않는다. P03.2b 표시 metadata는 이미 구현됐지만 이 후속 live readback 연결은 구현 전이다.

## Capability와 실제 GPU 경로

surface capabilities의 COPY_SRC와 지원 color format을 조회한다. 첫 범위는 RGBA8/BGRA8의 선형/sRGB native surface이며 PNG sRGB RGBA8로 swizzle한다. 현재 renderer는 non-sRGB UNORM target에도 shader/overlay의 manual_srgb 경로로 이미 sRGB-encoded bytes를 쓴다. worker에서 다시 linear→sRGB 변환하면 이중 인코딩이므로 기존 bytes를 보존한다. 실제 source format·render encoding·output color space를 metadata로 기록하고 sRGB/native-linear fallback 모두 known-color 회귀로 확인한다. surface 설정은 지원할 때만 RENDER_ATTACHMENT|COPY_SRC다. 지원하지 않는 adapter/format이면 명시 CaptureUnsupported이며 검은 이미지·새 offscreen render로 바꾸지 않는다. 후속 portable copyable render tap은 별도 설계/검증한다.

GPU 초기화 전에는 Starting, 초기화 뒤에는 실제 limits/format을 가진 Ready 또는 Unsupported로 handshake한다. CPU-only service와 capture sink 없는 session은 unsupported다. 하나의 session에는 presentation sink 하나만 bind하며 session identity가 다른 completion을 거부한다. capability 응답은 실제 handshake 상태를 반영한다.

살아 있는 window callback에서 `PollType::Wait`·GPU 결과 대기·PNG encode·hash·파일 I/O·thread join을 금지한다. GPU copy 제출 후 `map_async`를 요청하고 event loop에서 nonwaiting `Poll`로 진행시킨다. surface timeout/최소화로 새 frame이 없어도 pending map/취소와 deadline 처리는 redraw 성공에만 의존하지 않는다. about_to_wait의 pending-capture pump와 다음 deadline을 고려한 bounded WaitUntil wake로 진행시키며 busy spin을 만들지 않는다. callback은 작은 완료 flag/try-send만 수행한다. native poll은 callback body가 끝날 때까지 반환하지 않으므로 callback 안에서 큰 row 복사도 하지 않는다.

wgpu Buffer clone은 native Send+Sync지만 BufferView를 thread 사이로 넘기지 않는다. 별도 bounded encoder worker가 clone한 Buffer에서 BufferView를 생성·사용·drop하고 mapped buffer를 읽어 row padding 제거/BGRA 변환·PNG·파일 쓰기·hash를 처리하고 view drop→unmap을 수행한다. mapped buffer를 다음 GPU copy에 재사용하지 않는다. window는 renderer owner 상태/작은 completion만 확인한다. 기존 synchronous capture의 alignment/format/PNG 검증 책임은 가능한 private helper로 재사용하되 blocking wait 함수를 live 경로에서 호출하지 않는다.

## 공통 ticket과 handoff

| 영역 | 책임 |
| --- | --- |
| service admission/actor | 기존 normal ticket/1 MiB result reserve, 별도 image credit, session/generation/clock/deadline 검증, pending capture·native terminal 결과 |
| renderer-neutral request/completion endpoints | 별도 capacity-1 reliable handoff, 같은 request identity·취소 상태·한 번만 완료하는 responder |
| native project host coordinator | 같은 service↔managed packet window routing, 새 ticket cell/authority를 만들지 않음 |
| CLI adapter | 기존 JSONL codec·ticket 결과·artifact capability 전달. 별도 source load/capture actor/packet coordinator를 만들지 않음 |
| main/window | capability handshake, 다음 eligible frame 선택, exact metadata·GPU copy/map, nonwaiting 완료 통지 |
| bounded worker/artifact owner | pixels/PNG/hash/파일·byte limits·비동기 cleanup, 작은 metadata 결과 |

capture는 engine mutation이 아니므로 AuthorityAccepted/world CommandId를 위조하지 않는다. native ticket은 terminal 전 Queued이며 renderer handoff/selected/mapping/encoding은 별도 capture status로 진단한다. 제출/완료 channel과 pending table은 coalesced Present channel과 분리한다. frame traffic이 capture·Close 완료를 덮어쓰지 못한다.

RequestCell의 terminal publication은 현재 actor에서 mutex를 사용한다. renderer callback이 직접 이를 호출하지 않는다. callback/worker의 bounded completion을 actor가 받아 같은 cell을 마감한다. queue full/Busy 때 무한 대기·hidden retry를 하지 않으며 예약으로 보장한 한 completion slot과 명시 실패를 사용한다.

공개 handoff의 첫 API 형태는 기존 start/by-value 동작을 보존하는 `start_shared_with_capture_handoff(...) -> (SessionControlService, CaptureHandoff)`다. CaptureHandoff는 Starting/Ready/Unsupported/Detached의 동적 상태와 nonwaiting request 수신을 제공한다. CaptureRequest는 native ticket ID·session/generation·tick floor/deadline·취소 상태·한 번만 완료하는 responder를 가진다. responder의 private capacity-1 completion은 actor가 poll한다. registry/worker는 renderer-neutral artifact handle과 metadata를 반환하고 runtime이 wgpu/Buffer/PNG를 직접 소유하지 않는다. 실제 public type signature·caps/예산 검증·single binding은 담당자가 consumer에게 공유·검토한 뒤 source를 작성한다.

위 factory 이름은 최초 조사 때의 예시이며 구현된 API가 아니다. 2026-10-17의 [native project host](native-project-host.md) 조합에서는 기존 activation/render-consumer factory가 같은 service/initial source를 소유한다. capture를 붙이려고 별도 SessionControlService/Engine을 시작하지 않는다. 이 기존 factory에 renderer-neutral handoff를 additive하게 붙이는 정확한 반환 type·소유권은 코드 착수 전에 다시 동결한다. public host control의 compact status에 GPU object나 image payload를 넣지 않는다.

attach 시점의 고정 권한과 GPU의 동적 readiness를 혼동하지 않는다. capture submit은 Starting/Unsupported/Detached를 명시 거부하고 Ready일 때만 예약한다. Ready 이후 detach/close와 제출이 경쟁하면 pending ticket을 반드시 typed failure로 마감한다.

첫 정책은 session당 capture **1개**다. normal ticket 여유를 소비하고 reserved Manual Step lane은 소비하지 않는다. 추가 capture는 ImageCapacity이며 ACK/reader·GPU/worker 소유권 반환 뒤 회복한다. 기존 normal service 결과나 Step의 32 MiB 예산을 이미지가 제한한다고 표시하지 않는다.

## 예산·artifact·수명

초기 ceiling은 최대 **8,388,608 physical pixels**, 각 축 최대 **8192**, staging/raw/encoder의 보수적 capture reserve **128 MiB**, encoded PNG/spool **40 MiB**, pending/active/retained capture 합계 **1개**다. default deadline 5초·최대 30초다. actual physical 크기가 ceiling을 넘으면 allocation 전에 CaptureTooLarge로 실패하며 window를 임의 축소하지 않는다. width×height×4·256-byte row alignment·staging size·PNG append는 checked arithmetic이다. encoder output은 write 중 한도를 적용하고, 완성된 큰 Vec 뒤에 검사하는 방식은 쓰지 않는다. 이 수치는 engine-owned 책임의 ceiling이며 driver memory/PNG 내부 allocator·전체 RSS의 정확한 계수나 1ms 처리 보장이 아니다.

PNG bytes/base64를 JSONL/native 1 MiB result에 싣지 않는다. 결과는 bounded metadata와 artifact capability다. file은 CLI가 생성한 전용 temporary spool 안에 fresh ID로만 만든다. request의 임의 파일 경로/덮어쓰기를 받지 않는다. partial file은 완료 artifact로 게시하지 않고 실패 때 제거한다. commit된 파일의 media type·byte length·SHA-256·resource ID/path를 기록한다. caller는 ACK 전에 읽거나 자신의 경로로 복사할 수 있다.

native artifact handle을 clone하면 같은 lease를 공유한다. ticket ACK와 마지막 native reader/handle 해제 후 파일은 cleanup 대상이 된다. metadata만 복사한 path는 lifetime capability가 아니다. wire caller는 ACK 후 파일 존재를 기대하지 않는다. Drop에서 동기 파일 삭제를 하지 않고 worker-owned bounded spool registry가 해제 상태를 확인해 정리한다. registry와 파일의 수/bytes도 capture ceiling에 포함한다. shutdown은 window loop 밖에서 worker를 회수하고 spool을 정리한다.

forget/EOF는 진행 중 capture의 rollback이 아니다. 관심이 사라진 요청은 가능하면 dispatch 전 취소하지만 GPU에 제출했으면 staging/job ownership이 끝나기 전 image credit을 반환하지 않는다. timeout terminal과 GPU 자원 회수도 별개다. result는 timeout이어도 submission/callback/worker가 가진 lease가 남으면 새 capture는 계속 ImageCapacity일 수 있다. image/result/artifact 예산을 이중 또는 조기 반환하지 않는다.

close/device/map/encoder/파일 오류는 명시 terminal failure이며 PNG 성공으로 위조하지 않는다. close는 새 admission을 막고 미선택 요청을 중단한다. submitted readback은 cancel/unmap·renderer teardown 경로로 해제하며 UI callback에서 flush/Wait/join하지 않는다. actor failure와 presentation sink disconnect도 pending ticket을 마감한다. shutdown report는 terminal·abandoned·아직 회수하지 못한 GPU/worker 자원을 구분한다. OS 파일 I/O나 driver teardown의 강제 시간 상한을 선언하지 않는다.

## 검증과 구현 순서

1. CPU service: sink 없는 unsupported·single binding·wrong session/generation·normal/Step/image reserve·deadline·forget/reader retention·duplicate completion·shutdown/actor failure를 검증한다.
2. renderer: alignment/BGRA/limits·실제 selected packet/used projection·같은 tick의 local pan/zoom·Busy old-ready view·present_serial·resize·copy/map failure와 bounded worker/artifact cleanup을 검증한다. 별도 requested real GPU 시험에서 sRGB/linear fallback의 known color·비검정 foreground·PNG decode·hash·반복 요청·계속되는 frame 제출을 확인한다.
3. 같은 실제 Mac project window에서 protocol 2 mutation→Step→capture ticket→poll→PNG decode/state/identity 대조→ACK→다음 capture를 실행한다. stale/too-large·close 중 capture·막힌 stdout·deadline과 resource 회복도 검사한다.
4. 기존 `smoke_project.py --view` process/project/JSONL/report helper를 확장한다. 이미 빌드한 binary를 받고 자동 재빌드하지 않으며 CPU18/default package 의미는 유지한다. physical OS 입력과 source asset 품질 시험은 별도다.

공개 API와 파일 소유권을 먼저 공유한 뒤 Luna xhigh가 runtime→renderer→CLI 연결을 구현한다. Cargo.lock/공유 target은 한 명씩 소유한다. root는 설계·리뷰·실제 Mac 검증을 맡는다. 아직 코드 착수/검증 전이며 이 문서로 live_capture capability나 P02/D1을 통과 처리하지 않는다.

## 2026-10-17 managed window source 감사

Luna의 read-only 감사와 root의 기존 owner/표시 계약 리뷰를 반영했다. managed window는 실제 acquired SurfaceTexture에 draw하고 PacketFrameSubmission의 identity/view를 확인한 뒤 present()를 호출한다. ManagedPresentedView에는 실제 session/tick·두 sequence·generation/digest·present sequence·physical 크기·view/행렬이 이미 있지만 surface format이나 pixels는 없다. 현재 surface usage는 RENDER_ATTACHMENT뿐이며 Frame/BeginStage capacity1 lane에 capture를 섞지 않는다. capability를 확인한 별도 readback handoff가 필요하다.

현재 `render_packet_png`는 별도 offscreen texture를 만들고 `capture_texture_png`는 Wait·전체 RGBA Vec·파일 쓰기를 동기로 수행한다. 이 경로를 실제 창 capture라고 재명명하지 않는다. alignment/format/PNG 검증 helper만 분리 재사용하고 UI callback에는 blocking wait·큰 copy/encode·I/O를 가져오지 않는다. runtime에는 아직 Capture operation/result·handoff·image credit·artifact lease가 없다. 새 host/CLI의 view_status도 metadata만 제공하므로 live_capture=false를 유지한다.

창 종료 후 mapped buffer/encoder job이 남을 때의 GPU owner·callback·worker·artifact 회수 순서는 새 [owner 계약](native-renderer-owner-lifecycle.md)과 함께 동결해야 한다. source/frame pin과 image credit을 같은 예산으로 취급하거나 callback이 끝나기 전에 장치 종료 완료를 보고하지 않는다. 이 감사는 실제 capture 실행이나 OS 화면 픽셀 검증이 아니며 구현 착수/통과 근거로 세지 않는다.

## 확인한 1차 자료

2026-10-16에 설치된 upstream source와 현재 repository를 확인했다. docs.rs의 정확한 버전 page는 web 도구에서 열리지 않아 registry의 동일 pinned source로 대조했다. 최신 API를 이전 버전에 추정 적용하지 않았다.

- [wgpu v27.0.1 Buffer source](https://github.com/gfx-rs/wgpu/blob/v27.0.1/wgpu/src/api/buffer.rs): `map_async` callback 진행·native polling thread·mapped buffer exclusive access. 로컬 crate `wgpu-27.0.1/src/api/buffer.rs`의 같은 문서 확인.
- [wgpu v27.0.1 Device source](https://github.com/gfx-rs/wgpu/blob/v27.0.1/wgpu/src/api/device.rs): `Poll`/`Wait`·WebGPU automatic polling. 웹 구현 통과 근거는 아니다.
- [wgpu v27.0.1 types](https://github.com/gfx-rs/wgpu/blob/v27.0.1/wgpu-types/src/lib.rs): SurfaceConfiguration의 보장 usage는 RENDER_ATTACHMENT. COPY_SRC는 실제 capability 검사 필요.
- 현재 `window.rs`는 RENDER_ATTACHMENT만 설정하며, `renderer.rs`의 기존 explicit capture는 blocking readback이다. 기존 source는 재사용 후보이지 live capture 구현 근거가 아니다.


## 2026-10-18 additive API와 의존성 경계

읽기 전용 source 감사는 정확한 기존 진입점을 확인했다. `NativeProjectActivationService::start_with_render_consumer(options, config)`가 service와 generation consumer port를 같은 initial PreparedProject에서 만든다. 이 API는 유지하고 capture opt-in sibling인 `start_with_render_consumer_and_capture`가 같은 private start_mode를 사용해 추가 runtime capture port를 반환하도록 설계한다. `NativeProjectHostOptions`의 private-field builder로 opt-in하며 capture를 위해 Engine·source resolver·preparation worker를 하나 더 만들지 않는다. 이름과 반환 type의 최종 source signature는 구현 소유자가 consumer에게 공유한 뒤 동결한다.

runtime은 `SessionOperation::CaptureFrame`과 기존 RequestTicket의 compact terminal 결과를 소유한다. native `at_least_tick`·validated timeout을 받는다. client queue admission에서는 ID·deadline·normal/image reserve를 고정하고, 실제 session/generation/digest는 actor가 handoff로 dispatch하는 한 turn에서 bind한다. 기존 latest-frame mailbox는 commit 뒤에도 이전 세대일 수 있어 source bind에 사용하지 않는다. 이 source 감사에 따른 시점 정정과 default tick floor는 [소유권 계약](live-capture-ownership.md)을 따른다. query와 같이 Queued→Terminal이며 CommandId·AuthorityAccepted를 위조하지 않는다. try_submit 시 readiness·capacity를 확인하더라도 actor/renderer의 최종 currentness 검사와 close 경쟁 처리는 생략하지 않는다. 기존 normal ticket credit과 단일 image credit은 독립이다. 마지막 terminal progress reader도 기존 normal credit을 보존한다.

renderer에는 runtime dependency를 추가하지 않는다. renderer-owned `ManagedCaptureRequest`/completion DTO는 scalar request key·core identity·조건·취소 상태와 실제 선택 metadata/readback 수명만 다룬다. runtime ticket/responder·artifact path·RequestCell을 받지 않는다. 기존 host coordinator가 runtime port와 renderer의 별도 capacity1 capture lane을 변환하고, pending record 하나에 runtime responder와 image lease를 유지한다. 이 record는 두 번째 ticket ledger가 아니라 두 기존 endpoint 사이의 한 요청 소유권이다.

`CaptureImageLease`를 runtime type인 채 renderer에 넘기는 signature는 채택하지 않는다. host의 pending/backstop은 renderer-owned readback의 실제 완료·unmap·encoder/file 정리 확인까지 runtime image credit을 유지해야 한다. timeout terminal·coordinator 종료·facade Drop으로 이 record가 먼저 사라지면 안 된다. 기존 host CPU worker/GPU owner 반환 backstop을 확장하는 정확한 closure/panic·completion 소비 순서를 구현 전 별도로 동결한다. generic trait의 임의 Drop이나 콜백에서 큰 backing의 마지막 해제를 수행하는 방식은 사용하지 않는다.

artifact는 host 소유 bounded registry와 worker가 관리한다. runtime 결과에는 concrete opaque capability와 metadata만 담고 임의 경로/PNG bytes를 넣지 않는다. host control의 capability 기반 reader 획득은 해당 session/registry를 검증하고 별도 reader lease를 유지한다. wire path가 필요하면 CLI가 이 안전한 실제 entry에서 metadata를 만들고 ticket ACK까지 capability를 보존한다. serialized ID/path만 복사한 값은 native lifetime authority가 아니다. 파일 삭제·PNG·hash·cleanup은 worker에서 수행하며 reader·GPU·spool이 실제 해제되기 전 image slot을 반환하지 않는다.

runtime `ProjectPresentedFrame`/view ACK DTO는 identity·physical 크기·view-projection을 제공하지만 camera pose와 tagged projection 전체는 없다. capture의 정확한 pose·perspective/orthographic 파라미터·surface format/encoding은 그 frame의 renderer `ManagedPresentedView`/SurfaceConfiguration에서 복사한다. 나중 host.last_presented를 읽어 pixels와 맞추지 않는다. ResidencyBusy 자체로 새 frame을 선택하지 않으며, 기존 경로가 실제 old-ready frame을 그린 경우에는 그 실제 identity가 요청 generation/tick 조건을 만족하는지 검사한다.

현재 혼합 pipeline bootstrap 결함의 실제 통합 인수 전에 capture 코드를 같은 변경으로 섞지 않는다. 이 절은 source 감사·적용 방향이며 API 구현·live_capture capability·실제 PNG 검증 통과가 아니다.
