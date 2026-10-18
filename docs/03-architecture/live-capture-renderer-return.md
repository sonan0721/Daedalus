# Live capture renderer의 반환 슬롯과 GPU 완료 경계

상태: P02.3a.2 renderer 로컬 인수 통과 · 실제 범위는 [작업 기록](../06-worklog/2026-10-18-live-capture-renderer.md#a2-잔여-검증과-로컬-인수) · 2026-10-18

## 만든 이유

[전체 capture 계약](live-capture-and-artifacts.md)과 [runtime 소유권](live-capture-ownership.md)만으로는 UI panic 직전의 Buffer를 누가 정리할지 충분히 정하지 못했다. readback handle을 반환하기 전에 UI stack이 unwind하면 버퍼가 그 스레드에서 마지막으로 해제될 수 있다. 또한 map 오류·unmap·Rust Buffer handle 해제는 제출한 GPU 복사 완료의 증거가 아니다. 같은 acquired frame을 관찰하면서 이 두 수명을 지키기 위한 구체적인 renderer→host 반환 경계를 고정한다.

Luna의 세 차례 읽기 전용 감사와 root의 `window.rs`·`packet_renderer.rs`·`staging.rs`·host `cpu_worker.rs` 대조를 반영했다. 이 문서는 제안/구현 승인 기준이며 실제 COPY_SRC·GPU readback·PNG 성공 근거가 아니다. runtime a.1의 source/API 검증을 먼저 마쳐야 a.2 구현에 착수한다.

## 기존 경로를 확장하는 API

기존 managed channel, options, input policy와 실행 함수를 유지한다. capture opt-in channel은 기존 window client/server에 별도 capture endpoint를 더한다. renderer는 runtime을 의존하지 않으며 opaque renderer key와 renderer-owned generation/view DTO를 사용한다.

```rust
managed_packet_window_capture_channel()
    -> (ManagedPacketWindowClient, ManagedCaptureClient, ManagedPacketWindowServer)

ManagedCaptureClient::try_status() -> Result<ManagedCaptureState, EndpointError>
ManagedCaptureClient::try_submit(request)
    -> Result<(), /* original owned request + typed rejection */>
ManagedCaptureWorker::try_take_return()
    -> /* Empty | Busy | Submitted(readback) | Cleanup(cleanup)
          | Failed { key, failure, cleanup } */
```

이 header는 역할을 동결하는 표기다. 정확한 Rust type 이름·visibility·error shape는 구현 소유자가 source 작성 전 consumer에게 공유한다. 공개 request에는 scalar key·generation identity·tick floor·deadline·작은 취소 신호만 담는다. runtime ticket/responder/image lease·artifact capability·파일 path는 host에 남긴다.

## 2026-10-18 source 착수용 header 동결

runtime a.1은 `f3880d7`에서 로컬 통합 인수했다. Luna가 정확한 renderer header를 공유하고 root가 아래 additive 경계를 검토한 뒤 a.2 source 작성에 착수한다. 이름이 source에 생기는 것과 실제 GPU 인수는 구분한다.

| Public type/API | 동결한 의미 |
| --- | --- |
| `ManagedCaptureKey` | NonZeroU64, 채널별 성공 admission 뒤 strictly increasing, Busy/거부는 원래 key 비소비 |
| `ManagedCaptureSource::new` / `from_presented` | host가 실제 stage routing에서 얻은 stage key/session/generation/digest로 checked 생성. 기존 `valid_source_digest`와 같은 optional `sha256:` 규칙, exact bytes 보존. private `ManagedGenerationIdentity::new`를 추측해 호출하거나 첫 present까지 source bind를 늦추지 않음 |
| `ManagedCaptureRequest::new` | validated source·resolved Tick floor·deadline을 받아 `Result<Box<Self>, RequestError>` 반환. 한 번 할당한 Box를 `try_submit` rejection tuple로 그대로 반환하며 Busy 재시도 중 재할당하지 않음 |
| `ManagedCaptureClient` | readiness 전용 `try_status() -> Result<State, EndpointError>`, 원래 request 보존 submit, keyed cancel, idempotent OOB detach. ReadyInfo는 format/max getter 제공 |
| `register_worker` | 창 시작 전 non-Clone worker 한 번 등록. 중복은 AlreadyRegistered. durable worker 참조가 수락보다 먼저 존재해야 함 |
| `ManagedCaptureWorker` | `try_take_return() -> Result<Option<Return>, WorkerError>`와 keyed cell progress. readiness와 queue/map progress를 혼동하지 않음 |
| `ManagedCaptureReturn` | Submitted(readback) / Cleanup(cleanup) / Failed { key, failure, cleanup }. pre-copy 실패도 같은 cleanup handle로 return 소비·실제 Released를 마감한다. 한 return reserve이며 큰 inline payload는 쓰지 않음 |
| `ManagedCaptureReadback` | actual view/format getter, borrowed `try_read_rgba8()`는 Pending(None)/Busy/typed production failure/성공 pixels. 성공 후 재호출은 InvalidState. `into_cleanup(self)`가 같은 cell을 옮겨 실패에도 정리 소유권 보존 |
| `ManagedCaptureReadback::selected_frame` | 선택된 실제 `CpuFrameHandle`의 작은 Weak 참조. host가 readback/read/cleanup 전에 실제 publication binding을 찾는 용도. serialized scalar에서 권한을 만들지 않음 |
| `ManagedCaptureCleanup` | borrowed `try_finish()`는 실제 Released 뒤에만 true. Pending/Busy/DevicePollFailed 등을 성공으로 바꾸지 않음 |
| `ManagedCapturePixels::into_parts` | key·actual view·format·RGBA Vec를 move해 host가 전달 때문에 큰 복사를 한 번 더 하지 않음 |

Cancel Accepted는 CopyMayBeSubmitted 전이다. AlreadySubmitted는 cleanup 요청이며 successful pixels/rollback 보장이 아니다. Unknown은 현재 key와 일치하지 않아 영향이 없다. surface format4개·typed unsupported·source/timeout/크기/copy/map/poll 실패·Busy/poison/wrong-key를 구분한다.

worker Drop과 readback/cleanup handle Drop은 abandon 신호만 보낸다. after-close GPU owner fallback은 worker endpoint가 detach되고 별도 single-consumer 상태로 **outstanding handle이 없음을 확인했을 때만** Buffer를 정리한다. client/callback/renderer도 Arc를 보유하므로 Arc count를 reader 존재 판정으로 쓰지 않는다. live handle이 있으면 drain은 Pending이다.

CPU checkpoint→window 연결/main-thread opt-in harness 순서로 구현한다. macOS event loop를 보통의 parallel Cargo test thread에서 실행하지 않는다. root가 source/CPU 검사를 확인한 뒤 실제 창 proof를 요청하며, 이 header 동결 자체는 장치 인수가 아니다.

Ready는 실제 surface의 COPY_SRC와 허용 format 확인 뒤 한 번만 게시한다. format과 최대 축/pixel ceiling을 담고, resize한 현재 크기로 덮어쓰지 않는다. 실제 physical 크기는 선택된 `ManagedPresentedView`에서 고정한다. 미지원이면 normal presentation을 유지하며 Unsupported로 보고한다.

## 예약된 return cell

수락은 단일 request·단일 return slot·단일 buffer cleanup record를 함께 예약한다. CPU owner worker가 coordinator와 독립적으로 endpoint/cell 소유권을 유지하도록 **창 시작과 request 수락 전에** 등록한다. Busy/Detached/용량 거부는 원래 owned request를 반환한다. frame/stage lane과 capture의 예약은 독립이다.

cell은 한 key, 고정 상태/metadata, map·queue 완료 atomic flag, poison-recoverable `Mutex<Option<Buffer>>`를 갖는다. Buffer는 cell 안에 두고 CPU owner만 take/drop한다. UI의 handle·PacketRenderer drain pin은 작은 cell/state 참조만 보유한다. private cleanup 경로는 UI에서 최종 buffer 해제나 파일 정리를 하지 않는다. 공개 endpoint를 쓰는 consumer도 이 durable owner·반환 owner 수명 계약을 따라야 하며 임의 스레드의 handle Drop을 cleanup 완료로 간주하지 않는다.

UI는 buffer 생성 **전에** poison-recoverable guard를 try-lock으로 얻고, 그 guard를 유지한 채 cell을 Installing으로 전환한다. 같은 guard 안에서 `create_buffer`를 호출하고 반환값을 다른 fallible 작업 없이 예약한 cell에 즉시 설치한 뒤 guard를 놓는다. 생성 후 lock을 다시 얻는 형태는 WouldBlock 때 local Buffer가 UI에서 해제되므로 금지한다. worker는 이 짧은 create/install 구간 동안 Busy를 관측한다. copy 인코딩은 cell의 buffer를 빌리며 두 번째 buffer clone을 UI-local owner로 남기지 않는다. 모든 UI cell 접근은 try-lock/원래 request 보존/cleanup 신호 경로를 쓰고 blocking mutex 대기나 GPU Wait를 넣지 않는다.

UI의 작은 unwind guard는 정상 commit 전 Drop에서 CleanupRequired만 표시한다. 버퍼 이동·unmap·마지막 해제를 수행하지 않는다. guard 뒤 cell은 실패에도 worker가 접근할 수 있다. poisoning은 worker가 복구해 실제 정리를 계속한다. 일반 job/coordinator panic은 이 계약으로 회수하며 process abort·전체 owner thread의 회복 불가능한 중단까지 보장하지 않는다.

## 실제 frame 선택과 제출 fence

1. 현재 실제 draw의 `PacketViewSubmission`을 검증한다. 이전 ready packet을 그렸다면 그 실제 identity를 쓴다. request의 generation/tick 조건이 맞지 않으면 선택하지 않는다.
2. 실제 크기·aligned row·raw/PNG ceiling과 제출 counter/용량을 checked arithmetic으로 확인한 뒤만 buffer를 만든다. 예산이 부족하면 정상 rendering을 유지하며 bounded deferral/typed failure를 택한다.
3. scene/overlay draw 뒤, 같은 acquired surface texture에서 copy를 같은 queue에 제출한다. 별도 순서 있는 submission은 허용한다. 제출 직전에 CopyMayBeSubmitted로 표시해 submit 직후 panic을 보수적으로 처리한다.
4. copy submission의 retirement는 기존 bounded submission serial·`queue.on_submitted_work_done` watermark 경계에 연결한다. map 결과와 별도다. callback은 작은 flag만 바꾼다.
5. `present()`를 호출하고 반환한 뒤에만 exact view/format·present sequence와 Submitted를 게시한다. 이는 draw/copy 제출과 present 호출이며 GPU 완료·OS scanout 보장이 아니다. map은 아직 Pending일 수 있다.
6. post-submit/pre-present panic, map 등록 오류나 map 실패는 성공 PNG가 아니다. 같은 cell의 Cleanup으로 돌린다. 제출 여부가 불확실하면 가능한 copy 뒤의 queue fence를 설치해 확인할 때까지 cell·image work lease를 유지한다. 기존 frame draw의 더 이른 serial을 copy 완료로 재사용하지 않는다.

return slot은 이미 예약됐으므로 성공 제출 후 임의 큐 포화로 DTO를 버리지 않는다. 제출 fence도 request당 유한하다. UI가 닫힌 뒤 fence 설치가 필요하면 반환된 GPU owner가 같은 queue에서 후속 fence를 진행한다. device/poll 오류로 실제 완료를 확인하지 못한 상태는 실패/Pending owner로 보존하며 time limit이나 Buffer Drop만으로 Released를 위조하지 않는다.

## Worker와 owner drain

Submitted/Cleanup handle은 cell/key를 가리키는 move-only 작은 handle이다. Drop은 abandon 신호만 보내며 실제 Buffer·BufferView·큰 RGBA backing을 소유하지 않는다. worker의 실제 map/read/cleanup 경계에는 nonwaiting poll과 bounded try-lock을 제공한다. 큰 row copy·swizzle은 해당 API를 호출하는 CPU owner worker에서만 수행한다.

창이 열려 있을 때 Device Poll 소유자는 event loop다. `about_to_wait`에서 pending map/cleanup/deadline을 nonwaiting Poll로 진행하고 bounded wake를 사용한다. worker는 callback의 Release/Acquire 상태를 관찰한다. native BufferView는 worker에서 생성·사용·해제하며 다른 스레드로 전달하지 않는다.

worker는 BufferView drop→unmap을 수행하고, copy의 queue fence 완료도 확인한 뒤 cell에서 Buffer를 take/drop한다. 이 두 조건 뒤에만 Released다. return/result가 소비되고 cleanup도 Released여야 다음 renderer admission을 허용한다. runtime의 artifact/reader retention은 그 뒤에도 image slot을 유지할 수 있다.

cell은 실제 selected `Arc<CpuFrameHandle>` 하나도 함께 유지한다. UI가 pin하는 frame은 검증한 actual `PacketViewSubmission::packet()`과 같은 packet 객체여야 하며 incoming candidate를 대신 pin하지 않는다. pin은 최초 설치 뒤 교체하지 않고, 기존 buffer처럼 worker의 실제 read/cleanup/fallback release에서 take/drop한다. 다음 reserve는 이전 buffer와 frame pin이 모두 없어야 한다. UI unwind·handle Drop은 pin을 해제하지 않는다. readback 자체는 immutable view와 Weak만 갖기 때문에 Drop에서 큰 frame backing의 마지막 해제를 하지 않는다.

host는 `selected_frame` Weak와 성공 enqueue 전에 등록한 bounded frame→opaque publication binding을 대조하고, 실제 `ProjectPublicationIdentity`를 pending capture에 복사한 **뒤** pixels read/cleanup을 진행한다. 이 시점까지 cell의 strong pin이 binding을 살린다. 픽셀 성공 뒤에는 이 pin이 풀리므로 Weak를 나중에 lookup하거나 PNG encode 완료 때 최신 identity로 대신하지 않는다. readback의 expired/missing/nonmatching Weak는 typed stale/unknown-publication failure이며 scalar를 조합해 opaque token을 새로 만들지 않는다. 자세한 host metadata 수명은 [host 계약](live-capture-host-artifacts.md#실제-publication의-상관관계와-수명)에 둔다.

창 종료 뒤에는 기존 `ManagedWindowRun → GpuOwnerReturnSlot → cpu_worker::poll_gpu_owner → WindowGpuOwner::try_drain_after_window`로 Poll 소유권을 넘긴다. 같은 CPU worker가 반환 owner를 poll하고 pending cell을 정리한다. PacketRenderer drain은 capture cell/fence가 남아 있으면 Pending이다. 정상 창 종료나 facade Drop을 미리 Stopped로 보고하지 않는다.

host a.3는 기존 CPU worker의 per-iteration catch 바깥에 `{runtime CaptureRequest, renderer key, return cell/handle}`을 둔다. coordinator는 routing만 맡는다. encoder subjob은 RGBA만 받으며 runtime responder/work lease·GPU buffer를 소유하지 않는다. subjob panic이면 durable record가 ArtifactFailed와 후속 cleanup을 마감한다. artifact/reader/file 수명은 [별도 ownership 계약](live-capture-ownership.md)을 따른다.

## Source 경계와 인수 기준

| Source | 구현 책임 |
| --- | --- |
| renderer `window/managed.rs` | additive channel·readiness·요청/반환 예약·작은 opaque handle |
| renderer `window.rs` | actual format/COPY_SRC·같은 frame 선택/copy/present·UI guard·about_to_wait pump |
| renderer `packet_renderer.rs` | checked capture submission/fence·완료 관측·기존 retirement와의 정합성 |
| renderer `packet_renderer/staging.rs`, `gpu_owner.rs` | capture cell/fence가 실제 Released될 때까지 owner drain Pending |
| host `cpu_worker.rs` 및 후속 capture worker | durable request·map/read/unmap/final buffer drop·artifact cleanup |

CPU 시험은 별도 lane 포화/Busy·거부 ownership, one-shot Ready, limit/alignment/format·serial 고갈, post-submit panic/cleanup state와 잔류 owner를 검증한다. 실제 GPU 시험은 같은 surface의 known pixels·PNG decode·RGBA/BGRA와 manual-sRGB·overlay·resize·old-ready packet·반복 capture·close와 map/queue/worker 정리까지 확인한다. synthetic CPU metadata나 별도 offscreen PNG는 이 gate를 대신하지 않는다. 실제 창/host/protocol 연결은 a.3/a.4의 후속 인수다.

## Renderer 단독 native harness의 범위

macOS main-thread event loop용 opt-in example `managed_live_capture`를 별도 파일로 둔다. `--out-dir <새 디렉터리> --scenario capture|close-held|worker-drop`를 명시해 실행하며, 기존 파일을 덮어쓰거나 기본 CLI를 활성화하지 않는다. renderer의 현재 dependency와 공개 content/CPU owner API로 작은 결정적 mesh fixture를 검증·준비하고, 공개 snapshot DTO를 `PacketCpuOwner::extract_validated`에 넣어 실제 frame handle을 얻는다. private handle을 만들거나 renderer에 runtime dependency를 추가하지 않는다. 이 snapshot은 fixture이며 실제 `SessionControlService` authority 대조는 a.3/a.4에 남는다.

기본 scenario는 실제 initial stage의 source와 capture Ready를 확인해 첫 요청을 수락시킨 뒤 game frame을 게시한다. 첫 game draw의 loading overlay와 이후 반복 capture의 차이를 실제 pixels로 대조하며, 반환된 view/source/tick은 항상 actual selected frame에서 읽는다. 후속 요청은 실제 presented view에서 source/tick을 얻는다. CPU worker에서 반환 pixels를 PNG로 저장·decode해 크기·색·metadata를 대조한다. `close-held`는 반환 handle을 유지한 채 창을 닫고 GPU owner를 worker로 넘겨 첫 drain이 Pending임을 확인한 뒤 read/cleanup을 마감한다. `worker-drop`은 endpoint·handle Drop 순서와 최종 fallback drain을 확인한다. GPU Poll·PNG/file I/O는 지정한 owner/worker에서만 수행하며, UI callback에서 join/Wait/readback/파일 작업을 하지 않는다.

출력은 fixture임을 표시하고 scenario·실제 surface format/physical size·선택 frame identity/view·raw RGBA와 PNG hash·색 sample·decode 결과·drain 관측을 기록한다. source·실행 파일·raw log hash는 실행 시 별도로 고정한다. 실제 장치에서 선택되지 않은 surface format, 아직 주입하지 않은 resize/old-ready/panic 경로를 성공으로 채우지 않는다. 이 첫 harness 자체로 전체 a.2를 닫지 않는다.

관련 pinned wgpu Buffer/Device/types 1차 자료와 기존 source 제한은 [전체 계약의 자료](live-capture-and-artifacts.md#확인한-1차-자료)에 남겼다. channel/readback source의 lib-only compile checkpoint는 [작업 기록](../06-worklog/2026-10-18-live-capture-renderer.md)에 남기며, 실제 surface copy 연결과 GPU 인수 전이므로 a.2·상위 P02.3a·D1은 미통과다.


## a.2 잔여 검증의 구현 계약: resize·old-ready·실제 GPU 정리

2026-10-18 첫 native checkpoint 뒤의 보완이다. library의 실제 copy/readback helper를 다시 구현하지 않고 native example과 private adapter test가 같은 경로를 사용한다. 다음 항목의 실행 성공 전에는 a.2를 체크하지 않는다.

### 실제 창 resize control

`ManagedPacketWindowControl::try_request_resize(width: u32, height: u32) -> Result<(), ManagedResizeRequestError>`를 additive로 제공한다. error는 Busy/Closed/InvalidSize다. width/height는 physical pixel이며 zero 또는 초기화 뒤 실제 device의 최대 축 초과는 InvalidSize다. 실제 한도를 아직 게시하지 않은 Starting과 slot try-lock 경합은 Busy다. 기존 channel/options/run signature는 그대로 둔다.

slot은 작은 `Mutex<Option<(u32,u32)>>` 하나이며 최신 요청이 이전 미처리 요청을 대체한다. payload/GPU/source를 담지 않고 caller/UI 모두 blocking lock을 기다리지 않는다. 이 API의 Ok는 최신 resize 요청 접수이며 적용 ACK가 아니다. close가 우선하며 닫힘과 경합한 요청은 적용되지 않을 수 있다. 요청 ID나 per-request ledger는 만들지 않는다.

window event loop가 기존 cadence에서 slot을 가져가 `Window::request_inner_size(PhysicalSize)`를 호출한다. pinned winit0.30.12 source의 `window.rs:764` 문서에 따르면 Some(actual size)는 즉시 적용/거부된 실제 크기이고 별도 Resized event가 없을 수 있다. None이면 실제 크기는 나중 Resized event로 온다. **Some의 actual size와 실제 event 모두 같은 reconfigure helper를 사용하며 요청 좌표를 실제 크기로 간주하지 않는다.** event counter는 실제 event만 세고, 최종 성공 증거는 실제 다음 `ManagedPresentedView`·PNG physical 크기다. OS가 크기를 거부/조정한 경우도 요청 자체를 성공 capture로 표시하지 않는다. 기존 zero-size configure 회피와 device-limit 검증은 유지한다.

example `resize`는 첫 actual capture 뒤513×257 같은 odd physical 크기를 요청한다. 실제 다음 present를 기다린 뒤 capture해 row padding 제거·PNG decode·view/projection/physical size를 대조한다. ReadyInfo의 최대 크기는 negotiated ceiling이며 resize 때 current size로 덮어쓰지 않는다.

### Old-ready의 실제 source 선택

example `old-ready`는 실제로 다른 geometry/resource set을 갖는 두 번째 PreparedProject/CPU generation을 만들고, 유효한 create/upload quota로 여러 stage chunk를 발생시킨다. 첫 active frame을 계속 표시하면서 candidate를 준비한다. 실제 capture의 source/tick/view/selected-frame Weak는 old active frame과 같아야 하며 incoming candidate를 pin하면 실패다. 후보가 준비 중이었는지는 실제 stage progress와 관측 구간으로 기록하고, 이미 Ready이나 미commit인 경우를 Preparing 증거로 바꾸지 않는다. sleep 기간만으로 GPU 상태를 가정하지 않는다. 새 frame의 present를 막는 기존 generation ACK backpressure도 재사용할 수 있다. 후보 cancellation과 실제 owner drain까지 마감한다.

### GPU format·failure 회귀

새 private adapter test는 실제 CPU frame·GPU texture·buffer·copy submission·map callback과 새 managed capture helper를 사용한다. 네 mapped format(RGBA/BGRA·Unorm/sRGB)의 실제 texture-copy 결과를 known bytes/색과 대조한다. Unorm의 manual-sRGB draw/clear가 필요한 경우 실제 renderer의 같은 색 변환 경로도 검사한다. CPU byte test만으로 actual manual-sRGB draw를 인수하지 않는다. 실제 adapter가 지원하지 않는 format은 명시적으로 구분하고 지원 성공으로 채우지 않는다. native surface가 협상한 format과 adapter-backed texture test는 별도 증거다.

`#[cfg(test)]` private one-shot interruption은 실제 queue.submit 직후 callback 등록 전, 실제 map 등록 후 같은 지점에서 unwind를 일으킨다. guard Drop은 durable cell을 보존하고 worker/반환 owner가 실제 queue-tail fence·map callback을 진행한 뒤만 buffer/frame pin을 해제한다. Empty/Drop/timeout을 GPU 완료로 쓰지 않는다. 일반 renderer package/CLI에는 fault setting API를 노출하지 않는다. 실제 map-error 경로는 실제 validation-safe failure를 주입할 수 있을 때 별도 검사하며 metadata flag 변경을 실제 adapter error로 표시하지 않는다.

source 소유권은 D가 renderer library·private GPU test·manifest/export, C가 기존 example 시나리오다. public resize header를 먼저 공유하고 benchmark source probe를 복원한 뒤에만 두 source writer를 시작한다. Cargo는 한 담당만 소유하며 두 파일이 동결된 뒤 focused CPU/strict→private ignored GPU→main-thread native 순서로 확인한다. 기존 capture/close-held/worker-drop도 관련 source 변경 후 필요한 범위에서 회귀한다. full host/CLI/package 인수는 a.3/a.4에 남는다.


추가 source 경계: 기존 offscreen 생성자는 Rgba8UnormSrgb를 고정하므로 실제 Unorm shader/clear 경로 검증에는 format 선택이 필요하다. D가 `renderer.rs`에 `#[cfg(test)] pub(crate)` format-selecting blocking constructor 하나를 추가하는 범위까지 소유한다. 기존 adapter/renderer 초기화 경로를 재사용하고 pipeline/shader 구현을 복제하지 않는다. production public 생성자의 기본 format·signature·런타임 동작은 유지하며 이 test helper를 renderer의 새 공개 format 선택 기능으로 계산하지 않는다. native surface의 실제 협상 증거와도 구분한다.


old-ready 관측이 오래된 cached status를 새 결과로 오인하지 않도록, after-capture stage bracket은 같은 coherent `ManagedWindowStatus`의 `last_presented.present_sequence >= captured.present_sequence`를 만족할 때까지 기다린다. stage key/session/source도 일치해야 한다. window는 capture copy/present 뒤 staging과 last-presented를 status에 함께 게시하므로, 이 조건 없이 이전 pending summary를 다시 읽은 것만으로 capture 시점의 pending을 증명하지 않는다. 구현·기록에 이 최신성을 명시하며 status Busy를 성공이나 Ready 되돌리기로 숨기지 않는다.


2026-10-18 후속 결과: 위 잔여 계약의 CPU104·실제 adapter GPU2·기존 생성자 GPU1·Metal native5 scenario를 통과해 a.2를 로컬 인수했다. 상세 source/binary/raw hash·기기/format 범위·미검증 제한은 작업 기록에 둔다. 이 문서 안의 구현 전/미통과 문장은 각 당시 순서의 기록이며 현재 host a.3 source 착수의 선행 renderer gate는 충족됐다. a.3/a.4와 상위 제품 게이트는 계속 열려 있다.
