# Live capture host·encoder·artifact reader 계약

상태: P02.3a.3 구현 전 source 감사·root 설계, a.2 인수 대기 · 2026-10-18

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
5. stop/panic은 handoff detach→이미 얻은 request의 durable transfer/확정 처분→open fence 해제 순서다. flag를 먼저 지워 worker가 종료된 뒤 request를 넘기는 경로를 만들지 않는다.

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

기존 CPU worker stop predicate에 ingress fence·pending capture·encoder job/result·artifact cleanup을 더한다. 반환 `WindowGpuOwner`의 fence/drain도 기존 경로로 진행한다. facade Drop은 stop 요청만 하고 window callback에서 wait/join/I/O를 하지 않는다. `try_shutdown`은 reader-held/cleanup 상태 동안 Stopping이며 finished worker만 join한다.

필수 CPU 회귀는 default unavailable·real readiness 전달·정확한 generation→stage routing·coordinator take/transfer panic·stop race·Busy completion 시 encode1회·write 상한/실패·partial 삭제·weak cycle 부재·각 strong owner 단독 retention·reader-held shutdown이다. 실제 PNG는 decode/hash/색/identity를 검사하고, 실제 GPU/host 연결은 a.2/3 native proof와 a.4 같은 창의 mutation→Step→capture→ACK→다음 capture로 확인한다. source 제안이나 CPU synthetic metadata를 실제 PNG/장치 검증으로 계산하지 않는다.

이번 문서는 host 구현 승인이 아니다. a.2 미통과, a.3/a.4·상위 P02.3a·D1/D2/D3도 미통과다.
