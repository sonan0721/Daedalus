# Live capture renderer의 반환 슬롯과 GPU 완료 경계

상태: P02.3a.2 구현 전 계약, source feasibility 감사·root 검토 반영 · 2026-10-18

## 만든 이유

[전체 capture 계약](live-capture-and-artifacts.md)과 [runtime 소유권](live-capture-ownership.md)만으로는 UI panic 직전의 Buffer를 누가 정리할지 충분히 정하지 못했다. readback handle을 반환하기 전에 UI stack이 unwind하면 버퍼가 그 스레드에서 마지막으로 해제될 수 있다. 또한 map 오류·unmap·Rust Buffer handle 해제는 제출한 GPU 복사 완료의 증거가 아니다. 같은 acquired frame을 관찰하면서 이 두 수명을 지키기 위한 구체적인 renderer→host 반환 경계를 고정한다.

Luna의 세 차례 읽기 전용 감사와 root의 `window.rs`·`packet_renderer.rs`·`staging.rs`·host `cpu_worker.rs` 대조를 반영했다. 이 문서는 제안/구현 승인 기준이며 실제 COPY_SRC·GPU readback·PNG 성공 근거가 아니다. runtime a.1의 source/API 검증을 먼저 마쳐야 a.2 구현에 착수한다.

## 기존 경로를 확장하는 API

기존 managed channel, options, input policy와 실행 함수를 유지한다. capture opt-in channel은 기존 window client/server에 별도 capture endpoint를 더한다. renderer는 runtime을 의존하지 않으며 opaque renderer key와 renderer-owned generation/view DTO를 사용한다.

```rust
managed_packet_window_capture_channel()
    -> (ManagedPacketWindowClient, ManagedCaptureClient, ManagedPacketWindowServer)

ManagedCaptureClient::try_status() -> ManagedCaptureState
ManagedCaptureClient::try_submit(request)
    -> Result<(), /* original owned request + typed rejection */>
ManagedCaptureClient::try_take_return()
    -> /* Empty | Busy | Submitted(readback) | Cleanup(cleanup) | Failed(key, reason) */
```

이 header는 역할을 동결하는 표기다. 정확한 Rust type 이름·visibility·error shape는 구현 소유자가 source 작성 전 consumer에게 공유한다. 공개 request에는 scalar key·generation identity·tick floor·deadline·작은 취소 신호만 담는다. runtime ticket/responder/image lease·artifact capability·파일 path는 host에 남긴다.

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

관련 pinned wgpu Buffer/Device/types 1차 자료와 기존 source 제한은 [전체 계약의 자료](live-capture-and-artifacts.md#확인한-1차-자료)에 남겼다. 새 renderer 구현이 없으므로 a.2·상위 P02.3a·D1은 미통과다.
