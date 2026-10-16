# P03.3b.2 동일 authority의 CPU 세대 commit

상태: native headless 계약 동결·CPU/CI 통과·A-042 뒤 native-only 회복·clean package 전체 인수 대기 · 2026-10-17

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

start error variant는 `Preparation(NativeProjectPreparationStartError)`와 `Session(SessionControlStartError)`이며 원인을 보존한다. shutdown error의 unit variant는 `WorkerPanicked`/`ActorFailed`이며 기존 actor report의 termination을 덮지 않는다. 오류 관찰 전에도 두 소유자의 stop/cleanup을 요청하고, 아직 reclamation이 진행 중이면 완료를 주장하지 않는다. `Stopped`는 actor 종료와 worker/cache/project 회수가 모두 끝난 경우다. Drop은 stop만 요청하며 join하지 않는다.

`SessionControlService`와 `SessionControlClient`에 `asset_activation_enabled(&self) -> bool`을 추가한다. 기본/기존 factory는 false, 새 factory만 true다. public `SessionControlConfig` 필드를 늘리거나 기존 constructor signature를 바꾸지 않는다. Wasm factory는 기존 native-unavailable 의미로 실패하고 content 비교기 Wasm compile을 native service 지원으로 해석하지 않는다.

기존 client의 feature 사전 검사 경계에는 `SessionSubmitError::AssetActivationUnavailable` unit variant를 추가해 ticket 효과 전에 거부한다. actor 쪽 `SessionRequestError::AssetActivationUnavailable`와 wire code는 같으며 권한 없는 client의 기존 CapabilityDenied 의미도 유지한다.

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

`SessionOperation::AssetActivation(AssetActivationOperation)`과 `SessionRequestResult::AssetActivation(AssetActivationRequestResult)`를 추가한다. request result는 `Status(Box<AssetActivationStatus>)` 또는 `Command(AssetActivationCommandResult)`다. command result는 private field와 `command_id() -> CommandId`, `observed_tick() -> Tick`, `outcome() -> &AssetActivationCommandOutcome` getter를 제공한다. `AssetActivationCommandOutcome` variant는 `StageAccepted { activation: ActivationId, candidate: PreparationId }`, `CommitApplied { activation: ActivationId, generation: u64 }`, `CancelAccepted { activation: ActivationId }`다. ticket admission은 actor 적용보다 앞서며 StageAccepted는 Staged가 아니다. CommitApplied는 actor source swap을 마쳤다는 뜻이고 화면 표시를 뜻하지 않는다.

operation/result/outcome은 기존 preparation과 같은 `Clone + Debug + PartialEq`를 제공한다. Status는 기존 inspect capability, Stage/Commit/Cancel은 mutate capability를 사용한다. 새로운 `SessionCapabilities` field/독립 권한 ledger는 추가하지 않는다.

## bounded status와 오류

metadata 타입은 `Clone + Debug + PartialEq`와 private field/read-only getter를 사용한다. payload를 넣거나 Deserialize로 직접 effect를 만들지 않는다.

| 타입 | getter |
| --- | --- |
| `AssetActivationStatus` | `session()`, `state()`, `active_generation()`, `active_source_digest()`, `current_activation() -> Option<&AssetActivationSummary>`, `latest_failure() -> Option<&AssetActivationFailure>`, `memory() -> &AssetActivationMemoryStatus` |
| `AssetActivationSummary` | `id()`, `candidate()`, `expected_active_generation()`, `source_digest()`, `committed_generation() -> Option<u64>` |
| `AssetActivationFailure` | `id()`, `code() -> &str`, `message() -> &str`, `compatibility() -> Option<&AssetReplacementCompatibilityError>` |
| `AssetActivationMemoryStatus` | `active_cooked_accounted_bytes()`, `candidate_cooked_accounted_bytes()`, `retired_cooked_accounted_bytes()`, `total_cooked_accounted_bytes()`, `max_cooked_accounted_bytes()` as u64; `retired_generation_count() -> usize` |

`AssetActivationState`는 `Idle`, `StagePending`, `Staging`, `Staged`, `CancelRequested`, `Cancelled`, `Superseded`, `Committed`, `Failed`, `Stopping`, `Stopped`다. 마지막 summary/failure 한 개만 유지하고 과거 operation 목록을 늘리지 않는다. commit의 actor identity가 worker의 promotion 처리보다 먼저 보이더라도 status의 active generation/source는 실제 actor commit과 일치해야 한다. retirement는 별도 memory 장부로 드러낸다.

기존 `AssetPreparationStatus`의 active_generation/active_source_digest도 commit 후 실제 actor identity로 갱신한다. startup1을 계속 반환하지 않는다. 다음 후보 준비는 이 현재 source와 모든 retained charge를 기준으로 하며 activation status·ProjectInfo·새 published frame의 source identity를 대조할 수 있어야 한다. worker가 retired backing을 늦게 회수하는 것은 identity를 과거로 돌려보낼 이유가 아니다.

status logical cap은 기존 preparation과 같은32KiB, wire encoded-result는 기존1MiB다. failure code64/message2048 UTF-8 bytes와 기존 bounded StableId 진단만 보관한다. 큰 JSON/source path/payload를 오류에 복사하지 않는다. compatibility failure code는 `incompatible_topology`, 기타 비동기 실패는 기존 bounded capacity/internal 의미를 유지한다.

공통 request error에 다음 의미를 추가하고 CLI 연결 시 이름을 그대로 사용한다: `asset_activation_unavailable`, `invalid_activation`, `activation_expired`, `activation_not_staged`, `active_generation_mismatch`, `activation_sequence_exhausted`, `project_generation_exhausted`, `activation_capacity`. 기존 `busy`, `invalid_candidate`, `candidate_expired`, `candidate_not_active`, `preparation_stopped`, replay/권한 오류는 재사용한다. new enum variant의 Rust 이름은 해당 PascalCase인 unit variant다. capability 없음은 unavailable이고 해당 factory에서 지원하지 않는 consumer 조합을 수락한 것으로 표시하지 않는다.

## Stage와 Commit의 순서

1. Prepare/Cancel/Status는 기존 의미를 유지한다. CPU-valid 후보와 authored-compatible 후보를 혼동하지 않기 위해 비교는 **Stage worker 작업**에서 수행한다.
2. Stage actor는 ID/session/current desired와 expected generation·bounded slot/replay/result 여유를 먼저 확인한다. 효과 전 Busy/invalid/capacity는 caller ID를 소비하지 않는다. 수락 때 activation sequence와 StagePending metadata만 게시하며 결과 receipt를 고정한다.
   pending/running Prepare는 Busy, 실제 ready가 없는 취소/실패 후보는 candidate_not_active다. 같은 candidate가 이미 StagePending/Staging/Staged이면 다른 CommandId의 Stage도 effect-free Busy다. 동일 CommandId는 공통 replay가 원래 receipt를 반환하며 두 번째 activation/lease를 만들지 않는다. 취소로 backing이 사라진 후보를 다시 Stage하려면 새 Prepare가 필요하다.
3. 같은 preparation worker가 ready payload를 빌려 현재 active source와 공통 비교기를 실행한다. 별도 importer/cache/스레드를 만들지 않는다. 비교/오류 반환 뒤 current desired·cancel·stop·expected active를 재확인하고 capacity-one private handoff에 non-Clone candidate lease를 넘긴다. 오래된 성공/오류는 최신 status를 덮지 않는다.
   actor가 새 Stage를 worker에 알린 뒤 이전 결과를 무조건 drain하면 빠르게 완료한 새 결과까지 버릴 수 있다. stale 정리는 새 수락 결과를 잃지 않는 순서 또는 sequence/epoch 필터로 수행한다. 효과 전 Busy/거부 때 기존 staged 상태를 먼저 지우지 않으며 actor는 worker 완료를 기다리지 않는다. private gate로 실제 새 결과가 먼저 queued되는 순서를 강제해 Staged/Commit 진행을 검증한다.
4. actor가 유효한 handoff를 받으면 Staged다. payload는 actor-private이고 공통 control client/status에 노출하지 않는다. active Engine/source는 아직 그대로다. invalid topology는 마지막 정상 source를 보존하고 bounded Failed를 기록한다.
5. Commit은 Staged·동일 candidate/activation·expected generation·current desired·retirement 여유·u64 증가 가능 여부를 다시 검사한다. 작은 shared-state transaction/try-lock으로 source swap과 generation/ledger identity를 연계한다. lock 확보 실패 뒤 swap부터 하는 순서를 금지한다.
6. crate-private `ProjectSession::replace_project_source(&mut self, Arc<PreparedProject>) -> Arc<PreparedProject>`는 project field만 교체한다. constructor/spawn/Step/게임플레이 schedule을 실행하지 않고 Engine·StableId↔handle map·실제 transform/revision·camera pose를 보존한다. displaced source는 미리 확보한 retirement slot에 전달한다.
7. 같은 공통 replay ledger에 CommitApplied를 기록한다. ACK/attach 교체 후 동일 CommandId 재시도가 generation을 다시 증가시키지 않는다. post-effect actor panic은 기존 OutcomeUnknown 계약이며 게임 state를 되감지 않는다.
8. 새 Prepare는 pre-commit Stage/lease를 supersede할 수 있다. Cancel은 pre-commit 작업을 취소하고 실제 payload release 전 charge를 유지한다. 이미 commit한 activation의 Cancel은 activation_not_staged로 거부하며 source rollback을 하지 않는다. 기존 preparation Cancel도 handed-off candidate를 무시하지 않는다.

candidate/staging은 하나의 backing/slot reservation이 이동하는 것이다. 같은 payload를 둘로 세지 않으며 별도 allocation이면 각각 charge한다. commit 뒤 다음 Stage는 retired slot/예산이 회복될 때까지 Busy/Capacity일 수 있다. receipt ACK가 공간을 돌려주지 않는다.

commit의 작은 metadata 갱신 뒤 worker promotion이 아직 남아 있을 수 있다. 다음 Prepare가 도착해도 committed candidate를 stale ready처럼 폐기하거나 그 backing을 새 candidate로 덮지 않는다. promotion을 새 import/ready 폐기보다 먼저 처리하며 필요하면 Prepare에 효과 전 Busy를 반환한다. pre-commit candidate를 supersede한 경우도 actor/channel이 잡은 lease를 실제로 회수하기 전에는 해당 charge를 새 import의 여유로 쓰지 않는다.

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

## CLI/protocol2 adapter 계약

runtime API를 위대로 공유한 뒤 별도 Luna가 disjoint CLI source를 작성할 수 있다. native opt-in은 `project serve DIR --protocol 2 --asset-activation`이며 preparation도 함께 활성화한다. `--asset-preparation`을 함께 쓰는 것은 허용한다. protocol1·view·audio 조합은 source/cache/worker/device 시작 전에 명시적으로 거부한다. view가 지원하지 않는 flag를 단순히 무시하지 않는다. 기본 명령의 동작은 유지한다.

기존 protocol2에 `asset_activation_status`, `asset_activation_stage`, `asset_activation_commit`, `asset_activation_cancel`을 추가한다. Stage는 candidate, expected_active_generation, optional command_id; Commit은 activation, expected_active_generation, optional command_id; Cancel은 activation, optional command_id를 받는다. candidate는 기존 `{session, epoch}`, activation은 `{session, sequence}` object다. 모든 ID/expected generation/committed generation/tick/frame/authority snapshot sequence는 decimal string으로 encode/decode하며 unknown field·범위 밖·외국 session을 기존처럼 거부한다.

result는 기존 tagged operation result의 `asset_activation` 안에 `status` 또는 `command`를 둔다. status/summary/failure/memory의 JSON field는 위 native getter의 snake_case 이름이고 state·outcome·compatibility kind는 enum variant의 snake_case다. command result는 command_id, observed_tick, outcome이며 outcome payload는 위 Rust variant와 같다. memory byte/count는 기존 bounded memory summary와 같이 JSON number다. compatibility 오류는 kind와 optional entity_id/component_id/asset_id만 보내며 전체 authored JSON을 포함하지 않는다. latest_frame에는 authority_snapshot_sequence를 추가해 frame.sequence와 구분한다.

capabilities는 asset_activation_enabled와 네 operation을 실제 feature에 맞게 알린다. 기존 base7 이름을 audio/preparation 조합마다 네 번 복사한 배열은 이번 adapter에서 base7·audio extension5·preparation extension3·activation extension4의 고정 목록을 한 bounded composer로 합친다. 이전 네 조합의 순서/이름을 보존하고 최대19개의 이름을 한 번 조립한다. 새 범용 registry/schema framework는 만들지 않는다. feature 선언과 operation 이름의 실제 중복을 제거하는 좁은 DEV02 개선이며 unavailable/권한 검사를 생략하지 않는다.

종료는 같은 owner가 actor/worker를 멈추고 기존 유한2초 관찰을 사용한다. stderr report에 actor 종료와 generation worker stopping/stopped를 구분하며 stdout JSONL을 오염시키지 않는다. timeout을 성공한 회수로 바꾸지 않는다.

기존 `smoke_project.py`에 CPU-only `--asset-activation` opt-in을 추가한다. 같은 authored project와 실제 service 한 개에서 live transform/Step→Prepare→Stage 실제 Staged 대기→Commit→source/generation/frame/state assert를 반복한다. PNG/model 또는 legacy geometry/atlas/WAV의 변경을 실제로 적용하고 동일 handle·revision/tick 보존, replay의 generation 증가0, topology 거부/취소/stale/expected-generation 거부 뒤 last-good을 확인한다. 별도 window/capture를 activation 실제 표시 근거로 붙이지 않는다. existing subprocess/JSONL/ticket/ACK/deadline/fixture 생성기를 재사용하고 stub tests는 transport/timeout/cleanup 회귀만 증명한다. 실제 CLI 실행과 구분한다.

새 process helper는 기존 smoke 결과에 별도 asset_activation 요약과 checks를 추가한다. 기존 preparation/audio/native mode의 의미와 counts를 조용히 교체하지 않는다. CLI 자체의 activation+audio/view 거부도 실제 invalid 옵션/없는 project 경로로 확인한다. CI/package step 확대는 local runtime/CLI 실제 smoke 인수 후 명세와 함께 별도 배정한다.

## 구현과 검증 책임

Luna xhigh 한 명이 runtime의 `project_preparation.rs`, `session_control.rs`, `project.rs`, `lib.rs`와 필요하면 private `project_activation.rs`·그 tests만 소유한다. public API가 달라질 필요가 있으면 dependent 코딩 전에 조정자에게 알린다. manifests/lock·renderer/audio·CLI/scripts/CI는 별도 배정 전 수정하지 않는다. worker와 actor를 다른 작업자에게 나누어 private handoff를 추측하며 작성하지 않는다.

검증은 실제 project/source edit·공통 Prepare/Stage/Commit/Status로 같은 session/Engine/entity handle·바뀐 source/gen·현재 transform/revision/tick·snapshot/frame 순번을 대조한다. Stage-before-ready, foreign/future/expired, mismatch generation, topology 거부, cancel/supersede 뒤 stale 성공/실패, 공유 replay/ACK/eviction/교차-kind ID, saturated normal 요청 중 reserved Step을 검사한다.

lower budget·held private old reader·retirement 포화→거부→실제 worker release 뒤 회복, Stage 중 stop/owner Drop/start 실패/worker·actor panic의 실제 종료·drop thread·cache reopen을 검증한다. status cap과 feature-off/unavailable, 기존 preparation-only/default/audio 회귀도 검사한다. 기존 actor source swap만 검사하는 fake worker나 고정 Staged 응답을 쓰지 않는다.

focused default/audio feature→runtime 전체·strict Clippy→source freeze/조정자 review 순서다. Cargo owner 하나·shared target·고유 `dist/project-evidence/p03-generation-*-attemptNN.log`를 사용한다. CLI/protocol2·process smoke·통합/managed/실제GPU·clean package/PR 인수는 후속 adapter와 consumer 통합 시 실행한다. 이 내부 선행 단계만으로 전체 b/c를 체크하지 않는다.

## 통합·배포 검증의 재사용

로컬 runtime/CLI 경계와 실제 process smoke를 통과한 뒤에만 기존 release workflow의 staged-binary smoke에 `--asset-activation`을 추가한다. `--assets --audio --asset-preparation --asset-activation` 한 invocation은 지원되는 sub-smoke를 순차 실행하는 도구다. preparation+offline audio 세션과 headless activation 세션은 별개이며, activation+audio/view 동시 지원을 의미하지 않는다. 각각의 session/binary/source identity와 실제 checks를 결과에 유지한다.

새 workflow/job·별도 package backend·장치 의존 CI를 만들지 않는다. 기존 archive 무결성/version/inspect 검증·Linux linkage·보호3개/제품5개 체크·tag-only Publish 조건을 그대로 유지한다. packaged step의 이름과 flag만 최소 확장하며 local source/clean archive와 두 CI staged binary의 실제 결과를 각각 확인한다. 이번 변경으로 tag/공개 Release를 발행하지 않는다.

CPU activation helper 안에서 protocol1과 activation+audio의 잘못된 옵션을 실제 binary/없는 directory로 검사한다. 기대하는 옵션 오류·nonzero 종료·미생성 directory/cache를 대조한다. view 거부는 window feature를 요구하므로 all-feature native binary의 별도 실제 검사로 확인한다. CPU-only activation helper에 window를 강제하거나 CLI의 generic unknown-command 오류를 view 사전 검사 성공으로 계산하지 않는다.
