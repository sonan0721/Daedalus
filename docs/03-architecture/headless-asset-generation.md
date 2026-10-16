# P03.3b.2 동일 authority의 CPU 세대 commit

상태: native headless 선행 계약 동결·구현/검증 전 · 2026-10-16

## 목적과 제한

[호환성 비교기](asset-replacement-compatibility.md) 다음으로, 준비한 project를 **같은 Engine과 현재 entity state**에 적용하고 이전 CPU backing을 worker에서 회수하는 경계를 먼저 구현한다. [전체 activation](project-asset-activation.md)의 source 소유권과 replay를 GPU/audio보다 먼저 검증하는 내부 선행 단계다. CLI 전용 두 번째 실행기를 만들지 않는다.

이번 factory는 renderer/audio가 연결되지 않은 native headless 제어 세션만 제공한다. 렌더·음향 세대 교체는 뒤의 consumer handshake를 연결한 뒤에 지원한다. headless commit이 성공해도 actual presentation·audio bank 선택·P03.3b/c 전체·D1 인수는 아니다. 기존 창/audio/preparation-only factory는 그대로 동작하며 새 activation mutation은 capability 없음으로 거부한다. 일반 라이브러리 사용자가 frame을 임의로 외부에 그리는 것까지 자동 탐지한다고 주장하지 않는다.

## 공개 Rust 경계

새 opaque `NativeProjectActivationService`는 다음 API를 제공한다. `PreparedProject`/Arc getter·공개 candidate lease·직접 mutation port는 제공하지 않는다.

| 메서드 | 결과와 의미 |
| --- | --- |
| `start_headless(NativeProjectPreparationOptions, SessionControlConfig)` | `Result<Self, NativeProjectActivationStartError>`. 기존 worker/cache와 actor를 각각 하나씩 시작. 초기 source 대기는 startup이며 UI callback에서 호출하지 않음 |
| `session(&self)` | `&SessionControlService`. 같은 actor에 기존 typed client를 attach |
| `request_stop(&self)` | actor와 worker에 nonblocking stop 요청 |
| `try_shutdown(&mut self)` | `Result<NativeProjectActivationShutdown, NativeProjectActivationShutdownError>`. 이미 finished인 thread만 join. `Stopping`/`Stopped` enum으로 진행을 구분 |
| `shutdown_report(&self)` | `Option<&SessionShutdownReport>`. actor가 실제 끝나고 join한 뒤의 기존 report |

start error는 기존 preparation start 또는 session start 오류를 원인으로 보존한다. shutdown error는 `WorkerPanicked`/`ActorFailed`를 구분하며 기존 actor report의 termination을 덮지 않는다. 오류 관찰 전에도 두 소유자의 stop/cleanup을 요청하고, 아직 reclamation이 진행 중이면 완료를 주장하지 않는다. `Stopped`는 actor 종료와 worker/cache/project 회수가 모두 끝난 경우다. Drop은 stop만 요청하며 join하지 않는다.

`SessionControlService`와 `SessionControlClient`에 `asset_activation_enabled(&self) -> bool`을 추가한다. 기본/기존 factory는 false, 새 factory만 true다. public `SessionControlConfig` 필드를 늘리거나 기존 constructor signature를 바꾸지 않는다. Wasm factory는 기존 native-unavailable 의미로 실패하고 content 비교기 Wasm compile을 native service 지원으로 해석하지 않는다.

공통 operation/result 경계는 다음과 같다. 아래 native 정수 타입과 기존 decimal-string wire 표현은 별개다.

```rust
pub enum AssetActivationOperation {
    Status,
    Stage {
        command_id: Option<CommandId>,
        candidate: PreparationId,
        expected_active_generation: u64,
    },
    Commit {
        command_id: Option<CommandId>,
        activation: ActivationId,
        expected_active_generation: u64,
    },
    Cancel {
        command_id: Option<CommandId>,
        activation: ActivationId,
    },
}
```

`ActivationId`는 private session/sequence와 `from_parts(SessionId,u64)`, `session()`, `sequence()`를 가진 `Clone + Copy + Debug + Eq + Hash + PartialEq` 타입이다. sequence0/foreign/future는 invalid, 이전에 만료한 ID는 expired다. 준비 epoch·activation sequence·committed generation·CommandId는 서로 다른 식별자다. 모두 checked 증가하고 wrap/reuse하지 않는다.

`SessionOperation::AssetActivation(AssetActivationOperation)`과 `SessionRequestResult::AssetActivation(AssetActivationRequestResult)`를 추가한다. request result는 `Status(Box<AssetActivationStatus>)` 또는 `Command(AssetActivationCommandResult)`다. command result는 private field와 `command_id()`, `observed_tick()`, `outcome()` getter를 제공한다. outcome은 `StageAccepted { activation, candidate }`, `CommitApplied { activation, generation }`, `CancelAccepted { activation }`다. ticket admission은 actor 적용보다 앞서며 StageAccepted는 Staged가 아니다. CommitApplied는 actor source swap을 마쳤다는 뜻이고 화면 표시를 뜻하지 않는다.

## bounded status와 오류

metadata 타입은 `Clone + Debug + PartialEq`와 private field/read-only getter를 사용한다. payload를 넣거나 Deserialize로 직접 effect를 만들지 않는다.

| 타입 | getter |
| --- | --- |
| `AssetActivationStatus` | `session()`, `state()`, `active_generation()`, `active_source_digest()`, `current_activation() -> Option<&AssetActivationSummary>`, `latest_failure() -> Option<&AssetActivationFailure>`, `memory() -> &AssetActivationMemoryStatus` |
| `AssetActivationSummary` | `id()`, `candidate()`, `expected_active_generation()`, `source_digest()`, `committed_generation() -> Option<u64>` |
| `AssetActivationFailure` | `id()`, `code() -> &str`, `message() -> &str`, `compatibility() -> Option<&AssetReplacementCompatibilityError>` |
| `AssetActivationMemoryStatus` | `active_cooked_accounted_bytes()`, `candidate_cooked_accounted_bytes()`, `retired_cooked_accounted_bytes()`, `total_cooked_accounted_bytes()`, `max_cooked_accounted_bytes()` as u64; `retired_generation_count() -> usize` |

`AssetActivationState`는 `Idle`, `StagePending`, `Staging`, `Staged`, `CancelRequested`, `Cancelled`, `Superseded`, `Committed`, `Failed`, `Stopping`, `Stopped`다. 마지막 summary/failure 한 개만 유지하고 과거 operation 목록을 늘리지 않는다. commit의 actor identity가 worker의 promotion 처리보다 먼저 보이더라도 status의 active generation/source는 실제 actor commit과 일치해야 한다. retirement는 별도 memory 장부로 드러낸다.

status logical cap은 기존 preparation과 같은32KiB, wire encoded-result는 기존1MiB다. failure code64/message2048 UTF-8 bytes와 기존 bounded StableId 진단만 보관한다. 큰 JSON/source path/payload를 오류에 복사하지 않는다. compatibility failure code는 `incompatible_topology`, 기타 비동기 실패는 기존 bounded capacity/internal 의미를 유지한다.

공통 request error에 다음 의미를 추가하고 CLI 연결 시 이름을 그대로 사용한다: `asset_activation_unavailable`, `invalid_activation`, `activation_expired`, `activation_not_staged`, `active_generation_mismatch`, `activation_sequence_exhausted`, `project_generation_exhausted`, `activation_capacity`. 기존 `busy`, `invalid_candidate`, `candidate_expired`, `preparation_stopped`, replay/권한 오류는 재사용한다. new enum variant의 Rust 이름은 해당 PascalCase다. capability 없음은 unavailable이고 해당 factory에서 지원하지 않는 consumer 조합을 수락한 것으로 표시하지 않는다.

## Stage와 Commit의 순서

1. Prepare/Cancel/Status는 기존 의미를 유지한다. CPU-valid 후보와 authored-compatible 후보를 혼동하지 않기 위해 비교는 **Stage worker 작업**에서 수행한다.
2. Stage actor는 ID/session/current desired와 expected generation·bounded slot/replay/result 여유를 먼저 확인한다. 효과 전 Busy/invalid/capacity는 caller ID를 소비하지 않는다. 수락 때 activation sequence와 StagePending metadata만 게시하며 결과 receipt를 고정한다.
3. 같은 preparation worker가 ready payload를 빌려 현재 active source와 공통 비교기를 실행한다. 별도 importer/cache/스레드를 만들지 않는다. 비교/오류 반환 뒤 current desired·cancel·stop·expected active를 재확인하고 capacity-one private handoff에 non-Clone candidate lease를 넘긴다. 오래된 성공/오류는 최신 status를 덮지 않는다.
4. actor가 유효한 handoff를 받으면 Staged다. payload는 actor-private이고 공통 control client/status에 노출하지 않는다. active Engine/source는 아직 그대로다. invalid topology는 마지막 정상 source를 보존하고 bounded Failed를 기록한다.
5. Commit은 Staged·동일 candidate/activation·expected generation·current desired·retirement 여유·u64 증가 가능 여부를 다시 검사한다. 작은 shared-state transaction/try-lock으로 source swap과 generation/ledger identity를 연계한다. lock 확보 실패 뒤 swap부터 하는 순서를 금지한다.
6. crate-private `ProjectSession::replace_project_source(&mut self, Arc<PreparedProject>) -> Arc<PreparedProject>`는 project field만 교체한다. constructor/spawn/Step/게임플레이 schedule을 실행하지 않고 Engine·StableId↔handle map·실제 transform/revision·camera pose를 보존한다. displaced source는 미리 확보한 retirement slot에 전달한다.
7. 같은 공통 replay ledger에 CommitApplied를 기록한다. ACK/attach 교체 후 동일 CommandId 재시도가 generation을 다시 증가시키지 않는다. post-effect actor panic은 기존 OutcomeUnknown 계약이며 게임 state를 되감지 않는다.
8. 새 Prepare는 pre-commit Stage/lease를 supersede할 수 있다. Cancel은 pre-commit 작업을 취소하고 실제 payload release 전 charge를 유지한다. 이미 commit한 activation의 Cancel은 activation_not_staged로 거부하며 source rollback을 하지 않는다. 기존 preparation Cancel도 handed-off candidate를 무시하지 않는다.

candidate/staging은 하나의 backing/slot reservation이 이동하는 것이다. 같은 payload를 둘로 세지 않으며 별도 allocation이면 각각 charge한다. commit 뒤 다음 Stage는 retired slot/예산이 회복될 때까지 Busy/Capacity일 수 있다. receipt ACK가 공간을 돌려주지 않는다.

## worker 소유권과 종료

기존 worker startup을 legacy/consuming 두 내부 mode로 공유한다. legacy `NativeProjectPreparationService`와 public getter는 그대로 보존한다. consuming factory는 초기 source를 actor로 넘기고 worker의 reclamation 참조만 유지한다. startup port의 identity 확인은 private Weak 또는 동등한 단독 startup token으로 유지하고 bind 뒤 초기 strong 참조를 남기지 않는다. getter를 panic하게 만드는 외부 API 변경은 금지한다.

worker는 최대 active1·candidate/staged1·retired1의 project reclamation backing을 유지한다. 모든 engine-owned project의 마지막 큰 drop은 worker가 담당한다. 정상·취소·startup 실패·actor 실패·worker panic·owner Drop에서 같은 수명을 따른다. `Arc::try_unwrap` 실패의 원래 Arc와 byte reservation을 보존하고 재시도한다. `.ok()`/강한 참조 수의 단일 관찰로 최종 소유권을 버리지 않는다. [공식 Arc 계약](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.try_unwrap)

panic 시 reclamation registry가 먼저 unwind되어 actor가 마지막 소유자가 되지 않도록 registry/guard를 worker catch scope 밖에 둔다. fatal state를 게시해 actor가 종료/참조 해제를 진행하게 하고, worker에서 실제 backing/cache 정리를 마친 뒤 resume_unwind해 join 오류도 보존한다. worker poison 처리 중 동일 mutex를 다시 panic시키는 cleanup을 만들지 않는다. process abort/OOM/임의 외부 native allocator까지 복구하는 보장은 아니다.

stop은 import/Stage를 협조 취소한다. handoff/actor가 참조를 내려놓을 때까지 worker는 Stopping으로 회수하며 bounded timeout wait/notify를 사용한다. worker마다 별도 janitor나 request별 detached cleanup thread를 만들지 않는다. try_shutdown은 actor의 finished 확인→실제 join/report 보관→worker의 finished 확인/join 순서로 관찰할 수 있으며 어느 쪽도 UI/actor에서 blocking join하지 않는다. 임의로 finished=false인데 Stopped를 반환하지 않는다.

cooked/catalog 총 상한은 기존 옵션의 최대256MiB다. active+candidate/staged+retired가 함께 사용하며 source당 기존 최대128MiB도 유지한다. worker가 실제 final backing을 회수한 뒤에만 charge를 해제한다. source/catalog의 기존 logical scope와 scratch/decoded overlap·누락 heap 범위는 preparation 명세를 그대로 따르며 전체 RSS로 바꾸어 부르지 않는다. 이 단계에는 renderer prepared CPU/GPU/audio Frame allocation이 없다.

## frame identity

기존 `SessionFrame.sequence()`를 단조 증가하는 presentation publication 순번으로 사용하고 `authority_snapshot_sequence() -> u64` getter를 추가한다. 초기/일반 Step은 기존 숫자 흐름을 보존할 수 있지만 asset commit만으로 같은 snapshot을 새 generation/source에 연결할 때 frame sequence는 증가하고 authority snapshot sequence/tick/transform/revision은 유지한다. 중간 frame coalescing은 여전히 가능하다.

mailbox 포화로 즉시 게시하지 못한 경우 actor commit을 되감지 않는다. 실제 committed identity와 published frame identity를 구분하고 후속 actor 진행에서 최신 generation의 게시를 재시도한다. 반복 게시가 새 logical project payload를 복제하지 않는다. frame 독자가 source backing을 장부 밖으로 꺼낼 새 Arc getter는 추가하지 않는다.

렌더러의 explicit packet sequence·stage-only device 준비·actual-present ACK는 후속 계약이다. 이번 headless frame 재게시가 기존 창을 자동 지원하게 하는 것은 아니다.

## 구현과 검증 책임

Luna xhigh 한 명이 runtime의 `project_preparation.rs`, `session_control.rs`, `project.rs`, `lib.rs`와 필요하면 private `project_activation.rs`·그 tests만 소유한다. public API가 달라질 필요가 있으면 dependent 코딩 전에 조정자에게 알린다. manifests/lock·renderer/audio·CLI/scripts/CI는 별도 배정 전 수정하지 않는다. worker와 actor를 다른 작업자에게 나누어 private handoff를 추측하며 작성하지 않는다.

검증은 실제 project/source edit·공통 Prepare/Stage/Commit/Status로 같은 session/Engine/entity handle·바뀐 source/gen·현재 transform/revision/tick·snapshot/frame 순번을 대조한다. Stage-before-ready, foreign/future/expired, mismatch generation, topology 거부, cancel/supersede 뒤 stale 성공/실패, 공유 replay/ACK/eviction/교차-kind ID, saturated normal 요청 중 reserved Step을 검사한다.

lower budget·held private old reader·retirement 포화→거부→실제 worker release 뒤 회복, Stage 중 stop/owner Drop/start 실패/worker·actor panic의 실제 종료·drop thread·cache reopen을 검증한다. status cap과 feature-off/unavailable, 기존 preparation-only/default/audio 회귀도 검사한다. 기존 actor source swap만 검사하는 fake worker나 고정 Staged 응답을 쓰지 않는다.

focused default/audio feature→runtime 전체·strict Clippy→source freeze/조정자 review 순서다. Cargo owner 하나·shared target·고유 `dist/project-evidence/p03-generation-*-attemptNN.log`를 사용한다. CLI/protocol2·process smoke·통합/managed/실제GPU·clean package/PR 인수는 후속 adapter와 consumer 통합 시 실행한다. 이 내부 선행 단계만으로 전체 b/c를 체크하지 않는다.
