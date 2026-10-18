# Live capture host·encoder·artifact reader 계약

상태: P02.3a.3 header 동결·source 착수 승인, a.2 로컬 인수 통과 · 2026-10-18

## 만든 이유

[runtime a.1](../06-worklog/2026-10-18-live-capture-foundation.md)은 요청과 image lease, [renderer 계약](live-capture-renderer-return.md)은 실제 readback과 queue fence를 다룬다. 이 둘만으로는 PNG/파일·ACK 뒤 reader·coordinator panic 중 transfer를 안전하게 연결할 수 없다. 특히 기존 CPU owner는 frame/stage 준비와 반환 GPU owner Poll도 수행하므로 PNG encode·파일 I/O를 그 loop에 직접 넣으면 기존 기능의 진행을 지연시킨다. source 착수 전에 이 세 번째 경계를 고정한다.

Luna의 읽기 전용 감사는 `host.rs::NativeProjectHost::start`·`run_coordinator`·`CoordinatorState`·`CoordinatorGeneration`, `cpu_worker.rs::run_owner`·`poll_gpu_owner`·stop predicate를 대조했다. 새 source나 성능/PNG/장치 검증 결과는 아니다. a.2의 실제 API/장치 인수 뒤에만 a.3 코드를 작성한다.

## Additive host API

현재 default factory/options·private builder·기존 `run_view`·input policy를 유지한다. capture opt-in은 같은 service의 `start_with_render_consumer_and_capture`를 사용한다. 새 Engine·actor·ticket ledger·preparation cache를 만들지 않는다.

```rust
NativeProjectHostOptions::with_live_capture(self) -> Self
NativeProjectHostControl::try_capture_status()
    -> Result<NativeProjectCaptureStatus, HostControlError>
NativeProjectHostControl::try_acquire_capture_artifact(
    &self, capability: &CaptureArtifactCapability,
) -> Result<NativeCaptureArtifactLease, NativeArtifactAccessError>

NativeCaptureArtifactLease::local_spool_path(&self) -> &Path
NativeCaptureArtifactLease::open(&self)
    -> Result<NativeCaptureArtifactReader, NativeArtifactIoError>
NativeCaptureArtifactLease::copy_to(&self, writer: &mut impl Write)
    -> Result<(), NativeArtifactIoError>
```

이는 source 착수용 역할/signature 제안이며 정확한 type/enum·getter는 담당자가 consumer에게 먼저 공유한다. `capture` feature는 기존 `window`와 optional direct `png=0.18.1`, `sha2=0.10.9`를 활성화하는 안이다. 두 crate는 현재 lock graph에 이미 있으며 새 버전을 추정 추가하지 않는다. CLI feature/package 연결은 a.4에서 별도로 동결한다.

status는 Starting/Ready/Unsupported/Detached와 작은 작업 phase·byte count·cleanup 상태만 담는다. 실제 renderer handshake를 반영하며 초기 flag만으로 Ready를 만들지 않는다. path·image payload·GPU object를 넣지 않는다. capture를 요청하는 경로는 기존 SessionOperation/RequestTicket이다.

## 한 요청의 durable transfer

1. capture ingress cell1개와 `capture_handoff_open` OOB stop fence를 coordinator/CPU owner 시작 전에 만든다. worker는 이 flag가 열려 있거나 ingress가 차 있으면 stop 완료를 보고하지 않는다.
2. coordinator는 runtime handoff에서 얻은 요청을 다른 fallible 작업 없이 즉시 **iteration catch 바깥의 persistent state**에 설치한다. stack local에만 두거나 다음 routing 계산 뒤 보관하지 않는다.
3. 요청의 session/generation/digest를 실제 `CoordinatorGeneration`과 맞추고 그 record의 `ManagedStageKey`를 사용한다. last-presented clone·initial camera·generation 숫자를 stage key로 추측하지 않는다. 아직 해당 source를 선택할 수 없으면 원래 source/deadline을 유지해 bounded 대기하거나 typed failure다.
4. CPU owner ingress로 원래 request를 이동한다. 거부/Busy에는 같은 소유권을 보존한다. worker는 곧바로 catch 바깥의 `PendingCapture`로 이동한 뒤 renderer 요청을 제출한다.
5. stop/panic은 handoff detach→**coordinator가 이미 take한** request의 durable transfer/확정 처분→open fence 해제 순서다. flag를 먼저 지워 worker가 종료된 뒤 request를 넘기는 경로를 만들지 않는다.

이 fence는 host가 가져간 요청의 소유권만 다룬다. 현재 runtime port는 Detached 뒤 take를 거부하며 unclaimed slot이 비었다는 별도 ACK는 없다. 가져가지 않은 요청은 GPU/file 작업을 시작하지 않은 runtime 소유다. actor의 Detached 관측·failure drain/retry·shutdown drain이 이를 마감하고, 전체 host shutdown은 runtime service도 별도로 확인한다. CPU worker의 stop fence가 port empty ACK나 전체 service Stopped를 기다리게 만들면 불필요한 API와 종료 의존 순환이 생길 수 있어 그렇게 하지 않는다.

`PendingCapture`는 runtime request/responder·독립 work lease·renderer key·readback/cleanup handle·encoder 상태·재시도할 compact completion을 소유한다. coordinator나 encoder job 하나의 unwind로 이 record가 먼저 떨어지지 않는다. 원래 boxed renderer request도 Busy 재시도에 그대로 보존한다.

## CPU owner와 단일 encoder/I/O subworker

capture opt-in 때만 **capacity1 job/result lane을 가진 encoder/I/O subworker 하나**를 만든다. 일반 job pool이나 두 번째 actor가 아니다. session image slot1개와 같은 부하 상한이며, 기존 CPU owner의 frame/stage/GPU Poll을 blocking 파일 작업에서 분리하기 위한 것이다.

| 경계 | 책임 |
| --- | --- |
| coordinator | runtime ingress와 실제 generation/stage routing, 작은 status |
| 기존 CPU owner | runtime cancellation/deadline 관측·renderer return/fence·worker-side map/view/rowcopy/unmap/final buffer drop·completion retry·GPU owner drain |
| encoder/I/O subworker | 전달받은 RGBA의 PNG encode·증분 SHA-256·spool 생성/완결/삭제·job panic 정리 |
| registry/reader | immutable file binding과 weak interest·work lease, 검증된 native 읽기 수명 |

`ManagedCapturePixels::into_parts()`의 Vec를 move해 encoder job으로 넘긴다. 전달만을 위한 최대32MiB 복사를 추가하지 않는다. encoder의 per-job catch 바깥에 partial/final spool cleanup 상태를 둔다. job이 panic해도 partial 제거와 typed failure를 마감한다. runtime request/work lease는 CPU owner의 durable record에 계속 남는다. 전체 owner thread/process abort까지 회복한다고 선언하지 않는다.

responder Busy에는 같은 compact completion을 보유하고 재시도한다. encode/hash/file을 다시 시작하지 않는다. timeout/abandon/stop이 먼저 terminal이어도 이미 제출한 GPU work·encoder·파일 정리 전 work lease를 반환하지 않는다. encoder result lane도 포화 시 원래 결과/파일 소유권을 보존한다.

## 실제 publication의 상관관계와 수명

runtime `ProjectPublicationIdentity`는 실제 actor의 `SessionFrame`에서만 얻는다. 현재 CPU worker는 이 token과 추출한 `Arc<CpuFrameHandle>`을 함께 coordinator에 전달하지만, 기존 present ACK deque는 ACK 뒤 예전 항목을 제거한다. readback/PNG가 늦게 끝나면 최신 ACK만으로 캡처 원본을 찾을 수 없으므로 해당 deque를 capture history로 사용하지 않는다.

host는 성공 enqueue할 각 frame에 `{actual ProjectPublicationIdentity, ManagedStageKey, Weak<CpuFrameHandle>}` metadata binding을 **전송 전에** 예약한다. bounded registry의 limit은 실제 CPU frame slot 한도와 맞추며, expired Weak만 제거한다. live binding을 eviction하거나 무제한 history를 만들지 않고 자리가 없으면 원래 frame을 보존해 defer한다. runtime frame/source payload Arc를 이 registry에 넣지 않는다. 이는 기존 실제 frame object의 metadata 상관관계이며 ticket/authority ledger가 아니다.

registry의 durable 참조는 CPU owner가 시작부터 보유하고 coordinator는 작은 공유 참조로 등록한다. coordinator의 per-iteration unwind/종료가 이미 renderer에 보낸 frame의 binding을 지우지 않는다. window 전송 전에 예약을 마치고, Busy면 같은 frame/binding을 보존해 재시도한다. source/정상 Frame lane과 동일하게 이 공유 경계도 nonwaiting try-lock을 사용한다.

renderer cell은 actual drawn packet과 대조한 `Arc<CpuFrameHandle>` 하나를 capture read/cleanup까지 유지한다. public readback은 이 frame의 작은 Weak를 제공한다. CPU owner는 readback을 받으면 pixels read/cleanup **전에** host registry의 Weak와 pointer identity를 대조하고 session·stage key·publication/authority sequence·tick·generation/digest도 검증한다. 맞는 실제 opaque publication을 단일 pending capture에 복사한 뒤 read/PNG 작업으로 진행한다. lookup Busy는 같은 readback/원래 작업을 유지하며 재시도한다. cell strong pin 때문에 lookup 동안 원본 frame과 live binding이 사라지지 않는다.

worker가 GPU/map/BufferView·buffer와 frame pin을 실제로 해제하면 frame binding은 이후 expired Weak로 정리할 수 있다. pending capture는 이미 복사한 작은 actual publication token을 terminalization까지 보유하므로 그 뒤 present ACK나 새 frame/세대가 진행돼도 바뀌지 않는다. renderer handle은 frame backing을 직접 소유하지 않아 Drop에서 큰 backing을 해제하지 않는다. missing/expired/mismatched binding은 typed stale/unknown-publication 실패이며 최신 status·요청 metadata·scalar를 조합한 token으로 성공을 위조하지 않는다.

CPU 회귀는 capture frame 뒤 더 최신 frame/present ACK가 진행돼도 원래 opaque token을 유지하는지, lookup Busy·capacity·stale/foreign frame과 pin release 뒤 metadata 회수가 되는지 확인한다. 실제 host 인수는 캡처 pixels의 actual view/publication/authority와 이 token을 대조한다. 이 bridge는 source 감사에서 발견한 설계 보완이며 아직 구현 성공 근거가 아니다.

## Bounded PNG와 spool

입력은 실제 view의 width/height·format·checked row/raw size를 사용한다. [전체 ceiling](live-capture-and-artifacts.md)은 축8192·8,388,608pixels·raw reserve128MiB·PNG40MiB·작업/retained 합계1개다. Renderer가 이미 sRGB-encoded한 native UNORM bytes에 gamma 변환을 한 번 더 하지 않는다. 출력은 RGBA8/sRGB PNG이며 색 metadata도 기록한다.

PNG writer는 write 중 encoded40MiB 상한을 적용하고 같은 실제 bytes로 SHA-256·length를 증분 계산한다. 큰 Vec를 모두 만든 뒤만 한도를 검사하거나 hash를 위해 완성 파일을 다시 읽지 않는다. 인코더 내부 allocator/driver/OS cache 전체 RSS까지 정확히 제한한다고 주장하지 않는다.

spool은 host가 만든 private 임시 directory의 fresh file이며 create-new로 생성한다. request가 임의 output path나 overwrite를 지정하지 않는다. partial은 완성 artifact로 공개하지 않는다. PNG writer finalization·파일 완결 뒤 immutable binding과 registry를 먼저 게시하고 성공 completion을 보낸다. 파일 오류/인코더 오류·취소·panic은 typed failure와 실제 cleanup으로 처리한다.

registry lookup·coordinator·window callback·CPU owner Poll에서 file open/read/rename/delete를 수행하지 않는다. encoder/I/O subworker가 파일을 정리하고 성공한 제거 뒤 work lease를 반환한다. 제거 실패를 성공으로 바꾸거나 slot만 먼저 풀지 않는다. 아직 남은 자원/오류는 status/shutdown에 보고하며 OS I/O의 강제 시간 상한은 보장하지 않는다.

## Artifact와 reader 수명

registry는 full opaque CaptureArtifactId·session/host binding·weak interest·independent work lease·immutable spool metadata를 보유한다. 성공 뒤 runtime request의 strong capability를 registry에 계속 넣는 cycle을 만들지 않는다.

`try_acquire_capture_artifact`는 try-lock 안에서 session/host·ID·`CaptureInterestWeak::matches`의 실제 interest identity를 검증한다. 작은 immutable binding과 strong capability를 가진 lease만 만들고 lock을 놓는다. file open은 하지 않는다. foreign capability·NotReady·Expired·Busy를 구분한다.

`open()`은 lock 밖의 명시적 I/O다. 반환 reader는 Read/Seek wrapper와 **strong capability/binding**을 함께 보유하며 bare std File만 반환하지 않는다. ACK 뒤에도 reader가 있으면 파일/image slot을 유지한다. `copy_to`는 전체 I/O 동안 lease를 borrow한다. reader/lease Drop은 spool 삭제를 하지 않는다.

`local_spool_path`는 검증된 lease의 경로 getter다. CLI가 같은 실제 파일을 전달하기 위해 두 번째 PNG copy를 만들지 않게 한다. path를 복사한 값이나 wrapper를 거치지 않은 raw OS file handle은 native reader lease를 새로 만들지 않는다. wire caller는 ACK 전에 읽거나 자신의 경로로 복사한다. 그 뒤의 파일 존재/수명은 보장하지 않으며, tracked native 읽기는 capability-bound reader를 사용한다.

마지막 strong interest가 사라지고 GPU/map/encoder/file 작업이 모두 끝나면 encoder/I/O worker가 spool을 제거한 뒤 registry entry/work lease를 해제한다. native reader가 영원히 남으면 shutdown/image slot 회복도 늦어질 수 있다. facade/coordinator Drop이 살아 있는 reader를 무효화하거나 정리 완료를 위조하지 않는다.

## Shutdown과 인수

기존 CPU worker stop predicate에 ingress fence·pending capture·encoder job/result·artifact cleanup을 더한다. 반환 `WindowGpuOwner`의 fence/drain도 기존 경로로 진행한다. encoder join handle은 기존 CPU worker lifecycle이 소유한다. pending capture·registry·cleanup이 없어진 뒤에만 encoder stop을 요청하고, 계속 Poll하며 `is_finished()` 뒤에만 join한다. host는 기존 CPU worker와 runtime service의 실제 종료를 모두 확인한다. encoder의 예상 밖 종료나 파일 제거 실패는 work lease를 보존한 cleanup failure이며 성공 종료가 아니다.

facade Drop은 stop 요청만 하고 window callback에서 wait/join/I/O를 하지 않는다. `try_shutdown`은 reader-held/cleanup 상태 동안 Stopping이며 finished worker만 join한다.

필수 CPU 회귀는 default unavailable·real readiness 전달·정확한 generation→stage routing·coordinator take/transfer panic·stop race·Busy completion 시 encode1회·write 상한/실패·partial 삭제·weak cycle 부재·각 strong owner 단독 retention·reader-held shutdown이다. 실제 PNG는 decode/hash/색/identity를 검사하고, 실제 GPU/host 연결은 a.2/3 native proof와 a.4 같은 창의 mutation→Step→capture→ACK→다음 capture로 확인한다. source 제안이나 CPU synthetic metadata를 실제 PNG/장치 검증으로 계산하지 않는다.

이번 문서는 host 구현 승인이 아니다. a.2 미통과, a.3/a.4·상위 P02.3a·D1/D2/D3도 미통과다.


## a.3 소유자 사이의 header·실패 순서 보완

2026-10-18 현재 source 대조에서 확인한 연결 조건이다. 구현 승인은 여전히 a.2 뒤이며, 이 보완은 세 파일을 병렬 작성할 때 필수 routing/metadata가 빠져 다시 작성하는 반복을 줄이기 위한 것이다.

| Private 경계 | 고정할 소유권/호출 조건 |
| --- | --- |
| `RoutedCapture` | 원래 `Box<CaptureRequest>`와 coordinator가 실제 generation record에서 찾은 `ManagedStageKey`를 함께 전달. ingress Busy는 같은 struct/Box 반환. CPU가 generation 숫자로 stage key를 재구성하지 않음 |
| `CaptureIngress` | 시작 전 만든 공유 slot1·OOB transfer fence. `try_send/try_take`는 nonwaiting. `close_transfer_fence`는 coordinator가 take한 요청의 transfer/typed 처분 뒤만 호출. `is_drained`는 fence closed AND slot empty이며 lock Busy는 false |
| `CapturePublicationRegistry` | 실제 `PacketCpuOwner::status().frame_slots_limit`을 한도로 사용. frame 송신 전 same pointer/opaque identity/key를 idempotent 등록, expired Weak만 prune. selected lookup은 readback read/cleanup 전에 수행하고 Busy는 원래 handle 보존 |
| `EncodeJob` | actual publication·**actual present_sequence**·view·format/encoding·ID·weak interest·독립 work lease와 move된 RGBA Vec. strong request/capability/responder를 encoder에 넘기지 않음 |
| encoder client/owner | capacity1 job/result·Starting/Ready/Stopping/Stopped/Failed 상태. submit Busy는 같은 job/Vec 반환. request_stop은 OOB, owner try_finish는 is_finished 뒤만 join |
| immutable artifact binding | ID/path/완결 metadata만 공유. registry는 weak interest+work lease, public lease/reader는 strong capability+binding. 서로 강한 참조 cycle을 만들지 않음 |

encoder spawn caller는 thread 생성만 하며 spool directory create/open/delete는 encoder worker의 비동기 bootstrap에서 수행한다. renderer Ready와 encoder의 실제 Ready를 모두 확인한 뒤만 runtime Ready를 게시한다. bootstrap I/O 실패는 capture Failed·handoff detach로 보고하고 unrelated Unsupported 사유나 성공으로 바꾸지 않는다. host/CPU/window callback에서 bootstrap 파일 작업을 대신하지 않는다.

capture 모드의 renderer channel은 `NativeProjectHost::start`에서 CPU worker spawn 전에 만든다. 그 시점에 capture worker를 등록하고 capture client/worker를 CPU owner에 이동한다. 일반 window client/server는 host가 보유하다 기존 `run_view(&mut self)`에서 한 번 소비한다. 채널 생성만으로 장치나 Ready가 생기는 것은 아니며 실제 창/장치 handshake를 기다린다. default capture-off 경로는 기존처럼 `run_view`에서 일반 채널을 만든다. 창을 한 번도 열지 않은 capture host도 stop 때 미사용 endpoint와 encoder를 정리해야 하며, 새 actor·device·늦은 worker-binding 채널을 추가하지 않는다.

partial/unpublished 파일은 실패 시 interest가 살아 있어도 삭제한다. CPU의 pending request 자체가 strong capability를 보유하기 때문에 weak interest 소멸만 기다리면 실패 cleanup이 자기 자신을 기다릴 수 있다. encoder는 partial path·work lease·정리 상태를 per-job catch 바깥에 설치하고, 실제 삭제 뒤만 work lease를 해제한다. 성공해 공개된 파일만 weak interest 소멸을 기다린다. responder Busy는 같은 completion을 재시도하며 다시 encode하지 않는다. responder Closed/timeout 뒤 request를 처분해도 독립 cleanup work lease는 실제 GPU/파일 정리까지 남는다.

host startup 오류에도 fence가 열린 채 CPU owner를 남기지 않는다. coordinator spawn 이전 실패는 transfer를 소유한 startup guard가 detach/이미 취득한 요청 처분/transfer-fence close를 마감한다. encoder thread 생성 뒤 다른 초기화가 실패한 경우에도 encoder stop·private spool 정리 ownership을 남긴다. 이 경계는 constructor 성공 경로와 별도로 회귀한다. source/actual publication 비교는 public session/generation/digest getter를 사용하는 host의 한 helper로 통일하며 runtime private matcher의 visibility를 추측해 호출하지 않는다.


2026-10-18 승인 갱신: [a.2 실제 인수](../06-worklog/2026-10-18-live-capture-renderer.md#a2-잔여-검증과-로컬-인수)를 확인했으므로 위 보완 header와 소유권으로 a.3 source 작성을 승인한다. 이는 실제 host PNG/reader/shutdown 성공 또는 a.3 완료가 아니다. 과거 본문의 미승인/대기 문장은 당시 계약 작성 순서의 기록이다.
