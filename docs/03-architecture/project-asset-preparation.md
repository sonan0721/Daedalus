# P03.3a 실행 중 에셋 후보의 비동기 준비

상태: memory/native/control API source 리뷰 후 동결·하위 구현 진행 중. P03.2c PR13/CI/main 인수 확인 · 2026-10-16

## 목적과 제품 경계

AI가 PNG·glTF/GLB·atlas·WAV 원본을 바꾸고도 프로젝트를 매번 종료·재시작해야 하는 반복을 줄인다. 첫 하위 단계는 **같은 살아 있는 세션에서 다음 후보를 준비·조회·취소하는 경로**다. 실제 active generation 교체·표시·audio bank 전환은 P03.3b 이후이며 이번 단계의 CPU-prepared를 reload 완료로 보고하지 않는다. P03.3/P03/D1/D2/D3는 계속 열려 있다.

P03.2c의 clean archive/다섯 CI/두 package smoke/Linux linkage/main 동일 tree를 [PR13 작업 기록](../06-worklog/2026-10-16-imported-audio-runtime.md)으로 확인했다. 이 초안의 수치/API 동결·source 리뷰가 다음 코드 작업의 선행 조건이다. 원본 포맷과 importer 지원 범위도 넓히지 않는다.

## 현재 코드에서 확인한 문제

- `PreparedProject`는 불변 source/catalog·validated model/sprite/camera binding을 가진다. `load_project_with_assets`는 동기 I/O/import/cook와 협력적 취소를 사용한다.
- `ProjectSession`은 최초 source와 StableId→EntityHandle을 고정하며 프레임 generation은1이다. native coordinator도 첫 digest를 고정한다. 세션 재시작은 entity/session identity와 현재 state를 보존하는 reload가 아니다.
- `Engine.tasks()`의 TaskPool은 하나의 completion receiver를 공유한다. 기존 LiveSession은 자신에게 없는 completion ID를 unexpected로 다룬다. 별도 asset consumer가 같은 receiver를 drain하면 terrain 결과를 가로챌 수 있다.
- FileCookCache의 단독 owner lock은 catalog CachePin으로도 유지될 수 있다. candidate마다 캐시를 새로 열면 active 준비 데이터와 충돌한다.
- 실제 window present와 actor의 `latest_frame`은 다른 관찰이다. CPU 준비·actor commit·GPU ready·실제 present를 같은 상태로 합치지 않는다.

## 선택한 worker와 cache 소유권

native project당 preparation worker 하나를 common session service 옆에 둔다. 별도 Engine/ECS/authority는 만들지 않는다. content importer·resolver·cache·ImportCancellation은 그대로 재사용하고, 동기 filesystem/parse/decode/cook만 worker에서 수행한다. window callback·authority tick·normal 요청 처리에서 load나 worker join을 호출하지 않는다.

worker가 concrete FileCookCache 하나를 직접 만들고 모든 준비에 순차 사용한다. host-neutral CpuCookCache trait에 Send를 일괄 추가하거나 unsafe Send를 붙이지 않는다. root/cache 경로는 process-local owner 설정이며 JSONL 요청이 임의 경로를 바꾸지 못한다.

초기 load도 같은 worker/cache owner를 사용하는 native factory를 우선한다. initial PreparedProject의 cache pins는 다른 Arc를 만들기 전에 기존 consuming detach API로 해제한다. CPU 데이터는 자체 backing을 소유하며 cache 삭제와 별개다. 기존 ephemeral loader에서 이어 붙이는 경우에는 pins 해제·이전 cache drop 이후에만 worker가 캐시를 한 번 연다. 그 사이 다른 process가 lock을 잡으면 명시적 Busy/startup error다. memory cache로 조용히 바꾸지 않는다.

기존 `start/start_shared/start_shared_with_audio`와 public config struct literal을 보존한다. 새 opaque options/factory를 additive로 제공하며 기본 세션에는 worker가 생기지 않는다. native worker를 사용할 수 없는 byte-only/browser 경로는 capability를 사실대로 보고한다. 이 native 구현으로 Web Worker/fetch가 완성됐다고 표시하지 않는다.

최초 source 준비를 기다리는 시작 비용은 별도 startup 결과다. 기존 창이 아직 없는 최초 로딩의 UI·device staging은 P08/후속 startup 계약이며 이번 reload 준비로 처음부터 loading UI가 완성됐다고 하지 않는다.

## 후보 ID·상태와 공통 제어

후보 ID는 현재 session과 단조 증가/non-reused epoch다. JSON은 기존 정수 정책대로 decimal string을 사용한다. epoch overflow·foreign session·만료된 candidate는 효과 전에 오류로 처리한다. 처음부터 무한 history를 보관하지 않는다.

| 상태 | 실제 의미 |
| --- | --- |
| Idle | initial project만 있고 Prepare를 아직 수락하지 않음. candidate ID·running/pending/ready는 None |
| Pending | 최신 요청 metadata가 bounded slot에 있고 이전 작업 반환을 기다림 |
| Preparing | worker가 해당 epoch의 입력을 실제 읽고 import/cook 중 |
| CpuPrepared | immutable PreparedProject를 worker의 private ready slot에 보유. 아직 active/display/audio 교체 아님 |
| CancelRequested | 취소 flag/desired epoch가 바뀜. parser/read가 끝났다는 뜻 아님 |
| Cancelled/Superseded | 실제 작업이 반환하고 해당 결과의 채택을 거부함 |
| Failed | typed importer/cache/budget 오류. active source·world·device는 그대로 |
| Stopping/Stopped | admission 종료와 취소 요청 / 실제 worker exit를 구분 |

service actor가 desired epoch와 명령 결과를 소유하고 worker가 cache·현재 import·private candidate payload를 소유한다. actor는 payload 없는 bounded completion/status만 비차단 조회한다. command dispatcher·정상/Step credit·result ticket·ACK/forget·오류 encoding은 기존 SessionControlService/protocol2를 재사용한다.

첫 명령은 **Prepare**, **Cancel**, **Status**, bounded candidate metadata 조회다. Activate/Reload 완료를 흉내 내는 명령은 제공하지 않는다. Prepare의 terminal 결과는 epoch를 가진 요청 수락이며 실제 CpuPrepared와 구분한다. Cancel terminal은 요청 수락이고 실제 Cancelled와 구분한다.

Prepare/Cancel은 side effect이므로 world/audio와 같은 session-wide caller CommandId high-water/FIFO replay를 사용한다. 같은 retained ID의 재시도는 새 epoch/import를 만들지 않는다. 효과 전에 replay 저장 공간을 확보하고 결과를 먼저 보관한 뒤 ticket에 게시한다. replay eviction·pending duplicate·cross-kind 최초 outcome 의미도 기존 계약대로다. 별도 reload replay cache나 attachment-local ID issuer를 만들지 않는다.

조회에는 active generation/source identity와 desired/running/ready epoch, 마지막 compact failure, per-invocation import metrics, candidate source/catalog digest를 구분한다. source/asset 목록은 기존 count/byte/page 상한을 따른다. JSON/ticket/replay에 PreparedProject·PCM·texture bytes를 복사하지 않는다. CpuPrepared에는 activation/presentation 미완료를 명시한다.

## 한 작업·최신 요청·payload 보유

동시에 importer를 하나만 실행한다. pending은 최신 metadata 요청 하나로 교체하며 무제한 FIFO를 만들지 않는다. newer epoch는 현재 ImportCancellation predicate를 취소하고 pending을 교체한다. 실제 이전 load가 반환하기 전 새 load를 겹쳐 시작하지 않는다. cancellation은 OS read·외부 parser call을 강제 중단하지 않는다.

미채택의 큰 PreparedProject는 **worker/result를 통틀어 하나만** 보유한다. worker가 CpuPrepared를 private slot에 보관하고 actor에는 요약만 보낸다. 다음 요청은 worker가 이전 private candidate를 해제한 뒤 시작한다. normal ticket의 ACK로 payload 예산을 해제했다고 거짓 계산하거나 raw Arc clone을 정상 결과에 노출하지 않는다. 큰 payload의 마지막 해제도 actor tick/window callback 대신 worker에서 처리한다.

worker completion은 epoch를 다시 비교해 stale 성공/실패를 active 또는 최신 ready로 게시하지 않는다. stale 작업이 이미 완료한 atomic cook-cache entry는 다음 준비에서 재사용할 수 있지만 active generation을 바꾸지는 않는다. 요청·completion handoff는 bounded mailbox이며 actor의 try entry가 잠금을 얻지 못하면 Busy를 반환한다. 전체 구현을 lock-free/wait-free라고 부르지 않는다.

미래 consumer가 payload를 인수하는 순간에는 별도 owned lease와 generation-wide 예산을 함께 넘기는 계약이 필요하다. P03.3a에서는 외부 제어에 payload를 공개하지 않으며 candidate 준비를 위한 새 public API가 무제한 PreparedProject export 통로가 되지 않게 한다.

## 예산과 계측 범위

P03.1의 source·decoded/cooked·metadata·cache 상한은 후보마다 그대로 적용한다. CookedBytes에는 imported payload·legacy mesh의 vertex/index byte charge·catalog/binding 추정치가 포함된다. 그러나 parsed manifest/scene JSON·추가로 복제된 legacy mesh backing·모든 allocation을 완전히 세는 장부는 아니다. active PreparedProject와 uncommitted candidate가 동시에 존재하므로 단일 catalog128MiB를 전체 runtime memory 한도로 표현하지 않는다. private ready slot과 running import를 겹치지 않는 수명 규칙을 고정한다.

| 항목 | 기본값/하드 상한 | admission과 의미 |
| --- | --- | --- |
| importer 실행 | 1/1 | 취소 중 parser 반환을 기다리며 두 번째 load를 겹치지 않음 |
| latest pending | 1/1 | epoch/명령 metadata만 보유. 미리 읽은 source/PreparedProject 없음 |
| uncommitted candidate payload | 1/1 | running+private ready 전체에서 하나. 다음 준비 전에 worker에서 이전 ready 해제 |
| 후보 cooked-accounted | 기존 ImportLimits, 최대128MiB | 기존 CookedBytes 장부를 그대로 사용. 전체 PreparedProject heap 또는 exact unique Arc bytes가 아님 |
| active+candidate catalog cooked-accounted | 최대256MiB, 낮출 수 있음 | active accounted를 먼저 차감하고 effective candidate limit을 `min(import limit, remaining)`으로 낮춤. remaining0/active가 한도를 넘으면 I/O 전 Capacity 오류 |
| source input | 파일32MiB/합계64MiB, unique1,026개 이하 | native resolver/import의 기존 상한. captured graph와 실제 read metrics를 기록 |
| decoded base image | 최대64MiB | cooked와 일부 중복되는 별도 sub-limit, 서로 배타적 합계가 아님 |
| scoped parser/cache scratch | 기존 최대128MiB+64KiB | 엔진이 예약한 동시 scratch와 peak. foreign parser의 내부 allocator 전체가 아님 |
| normal result/replay | 기존 service 계약 유지 | payload 없는 상태/요청 결과만 보유. 별도 무한 lane/history 없음 |

새 additive public `AssetImportMemorySummary`는 private field와 public accessor를 갖는다. 기존 공개 field를 가진 `AssetImportMetrics`에 필드를 추가해 외부 literal을 깨지 않는다. private field를 가진 `AssetImportReport`에 summary와 public `memory()` accessor를 추가해 runtime crate가 읽을 수 있게 한다. 실제 BudgetState current/peak에서 값을 만들며, 현재 존재하지 않는 scratch high-water 계측은 scoped reservation 함수에 추가해야 한다. `cooked_accounted_bytes`는 catalog metadata 추정치를 이미 포함하고 `decoded_base_image_bytes`와 단순 합산하지 않는다. source identity 목록에서 unique source 수/소비 byte 합계도 대조한다. disk cache occupancy를 retained CPU memory로 쓰지 않는다.

memory summary 단독 하위 API는 source 감사와 리뷰 후 다음과 같이 동결한다. 다른 worker/control API의 검토와 독립적으로 이 content 하위 작업만 구현할 수 있다.

| 공개 API | 반환/의미 |
| --- | --- |
| `AssetImportReport::memory()` | `&AssetImportMemorySummary`. asset-aware report에 항상 존재 |
| `AssetImportMemorySummary::schema_version()` | `u32`, 첫 schema1 |
| `cooked_accounted_bytes()` | `u64`, 완료 직전 기존 CookedBytes 사용량. catalog metadata 추정 포함 |
| `decoded_base_image_bytes()` | `u64`, 기존 DecodedImageBytes 사용량. cooked와 중복 가능 |
| `parser_scratch_peak_bytes()` | `u64`, 이번 context의 accepted scoped reservation 최대 동시 사용량 |
| `unique_source_count()` | `usize`, 실제 captured source identity 수 |
| `captured_source_bytes()` | `u64`, 위 unique source의 byte_len 합계 |

summary는 Clone/Debug/PartialEq/Serialize를 제공하고 JSON report의 additive `memory` 아래 camelCase 필드로 기록한다. public constructor나 mutable field는 제공하지 않는다. legacy의 unavailable은 기존 `PreparedProject::asset_import_report() == None`로 나타낸다. report가 있는 empty catalog도 실제 ledger를 반환하며 임의0이나 missing으로 바꾸지 않는다. source/catalog/cook key·저장 cache format·기존 metrics 필드는 그대로다. 계측을 위해 PCM/image/geometry를 clone·재해시하거나 두 번째 byte estimator를 만들지 않고 기존 BudgetState와 이미 수집한 source identities를 사용한다.

`project cook`의 기존 bounded 요약에도 같은 `report.memory()`를 additive `memory`로 직렬화한다. library report·CLI·향후 preparation status가 각자 별도 메모리 계산기를 만들지 않는다. 기존 metrics/counts와 CPU-only runtime-consumer 의미는 유지한다.

첫 status는 진행 중인 외부 parser의 live byte counter를 만들어 채우지 않는다. running epoch와 active cooked 장부는 알려진 값으로 표시하고 candidate의 최종 cooked/input/scratch summary는 준비가 반환한 뒤에만 Some으로 보고한다. 아직 없거나 legacy인 값은 unavailable/None이며0과 구분한다. summary는 captured 입력과 엔진의 예약 장부이고 외부 allocator를 관측한 값이 아니다.

native factory가 만든 initial project의 새 summary를 사용해 active 장부를 초기화한다. legacy/외부 prepared 값에 summary가 없으면 unknown을0으로 처리하지 않고 preparation 기능의 startup unsupported/계측 불가 오류로 구분한다. 기본 기존 service는 계속 그 prepared 값을 사용할 수 있다. active/candidate가 같은 cook key를 참조하더라도 이 논리적 두-catalog 장부는 보수적으로 각각 charge하며 cross-generation pointer dedup을 구현했다고 표시하지 않는다.

| ledger | 측정할 자료 | 포함했다고 주장하지 않을 범위 |
| --- | --- | --- |
| retained active/candidate | 기존 cooked-accounted 장부·catalog/binding metadata 추정과 owner/slot 수 | scene/manifest JSON의 미계측 heap·추가 legacy geometry 복제 backing·caller가 명시적으로 복제한 자료 전체·정확한 unique backing 계측 |
| preparation input/transient | source bytes·decoded PNG/audio·변환 scratch의 기존 budget | 외부 parser 내부 allocator/전체 process RSS의 완전 계측 |
| control/result | 요청·status/page·ticket·replay의 bounded count/bytes | asset payload를 작은 status 크기로 대체 회계 |
| cache | 기존 concrete cache limits·정확한 owner lock·entry validation | cache hit를 runtime consumer activation으로 해석 |

renderer GPU active/candidate/staging/in-flight/retired, audio Frame bank·old voices/callback backing은 P03.3b/c에서 별도 합산한다. CPU 후보 하나의 성공으로 device residency admission을 통과시키지 않는다. native preparation worker1개는 기존 Engine CPU budget과 구분해 진단에 표시한다.

## 입력 identity와 동시 편집

PreparedProject는 차례로 읽은 exact bytes/dependency identity의 captured graph다. 디스크의 동일 순간 atomic snapshot이 아니다. source hash·candidate digest는 소비한 입력을 설명한다. mtime·watcher debounce·double-stat을 atomic transaction 증명으로 쓰지 않는다.

명시적 다음 Prepare 요청 또는 watcher의 후속 dirty epoch가 들어오면 최신 후보로 다시 준비한다. candidate의 manifest/scene이 달라도 이번 단계는 CPU-valid 여부만 보고하며 기존 authority를 변경하지 않는다. 후속 Activate는 topology·authored transform 정책과 expected active generation을 별도로 검사해야 한다. 게임 state를 source 값으로 조용히 초기화하지 않는다.

## 종료와 실패

admission을 닫고 desired epoch를 취소한 뒤 worker가 실제 반환하는지 관찰한다. UI/actor에서 blocking join을 호출하지 않는다. non-cooperative read/parser가 남으면 Stopping과 cache lock 보유를 보고하고 실제 종료 전 캐시를 다시 열지 않는다. owner Drop의 취소 요청과 실제 join 성공을 구분한다. hard-bounded parser 종료를 보장하지 않는다.

원본 오류·cache Busy·allocation/budget·worker panic·handoff saturation은 typed 실패다. cache corruption은 기존 검증→폐기/재쿠킹 정책과 counter를 유지하며 복구 불가일 때 실패한다. 이미 존재하는 active world와 마지막 화면/voice를 없애지 않는다. 이번 단계에서 device를 바꾸지 않으므로 renderer/audio rollback을 구현했다고 하지 않는다. worker panic 후 재시작·cache lock 재획득은 실제 종료와 owner 회수를 확인한 별도 명령이어야 한다.

## Native owner·port API 동결

실제 worker는 runtime library의 `project_preparation` module에 둔다. CLI-private 구현이나 public generic port trait만 제공해 SDK 사용자가 engine 기능을 다시 작성하게 하지 않는다. 다음 additive API를 채택한다. 새 type의 field는 private다.

| API | 동결한 의미 |
| --- | --- |
| `NativeProjectPreparationOptions::new(root)` | process-local PathBuf. default cache는 project의 `.daedalus/cache`, import/cache 기존 defaults와 catalog 합산256MiB |
| `with_cache_root`, `with_import_limits`, `with_cache_limits`, `with_max_catalog_accounted_bytes` | builder는 Self 반환. 경로는 trusted owner 설정, 수치 validation은 start에서 기존 validator/하드 상한 재사용 |
| `NativeProjectPreparationService::start(options)` | `Result<Self, NativeProjectPreparationStartError>`. worker 안에서 cache open/initial import, pins detach 후 source를 공유. 초기 실패는 실제 worker 종료를 정리하고 반환. startup wait는 UI callback에서 호출하지 않음 |
| `initial_project(&self)` | `&Arc<PreparedProject>`, 최초 불변 source. candidate payload getter 아님 |
| `take_control_port(&mut self)` | `Result<ProjectPreparationPort, ...>`, non-Clone opaque port를 한 번만 인수 |
| `request_stop(&self)` | 비대기 취소 요청. cache 해제/join 성공 아님 |
| `try_shutdown(&mut self)` | `Result<PreparationWorkerShutdown, ...>`; Stopping/Stopped. 종료 확인 후 join·실패 관찰. 실행 중이면 owner가 handle/cache 수명을 유지 |
| `SessionControlService::start_shared_with_preparation(project, config, port)` | 원래 actor를 한 번만 생성. source와 port initial Arc의 ptr identity를 spawn 전에 검증 |
| `start_shared_with_preparation_and_audio(project, config, port, audio_options)` | audio feature의 additive 조합. 기존 private startup 하나로 합류하며 별도 actor 없음 |
| `SessionControlClient::asset_preparation_enabled()` | bool. 기존 public SessionCapabilities/config/limits field를 늘리지 않음 |

Native service/port Drop은 stop을 요청한다. actor가 port를 drop하면 worker에도 stop이 전달되고, 외부 Native owner가 실제 worker join을 관찰한다. Native owner를 버려도 실행 중 thread closure가 cache owner를 유지한다. 다른 service에 port를 복제·재부착할 수 없고, source mismatch/두 번째 take는 명시적 오류다. native factory가 없는 target에서는 기능 unavailable이며 filesystem worker를 시작하지 않는다. 이번 변화로 runtime/browser 전체 target 지원을 인증하지 않는다.

port의 효과 함수는 crate-private다. `initial_project()`와 `try_prepare(epoch: u64)`, `try_cancel(epoch: u64)`, `try_status(session: SessionId)` 및 stop 신호를 actor에서만 사용한다. epoch issuer는 actor 한 곳이고 worker는 전달된 epoch를 검증한다. 일반 SDK 사용자가 port를 통해 replay/capability를 우회할 mutation API를 얻지 않는다.

source 분담 seam도 동결한다. `ProjectPreparationPort` type 자체는 public opaque이고 non-Clone이다. crate-private `matches_initial_project(&Arc<PreparedProject>) -> bool`로 spawn 전 pointer identity를 확인하며, `bind_session(SessionId)`는 actor가 실제 ProjectSession을 만든 뒤 startup 성공을 보내기 전에 한 번만 호출한다. 별도 session ID를 만들지 않는다. `try_status(session)`은 `Result<AssetPreparationStatus, ProjectPreparationPortError>`를 직접 반환해 actor에 또 다른 snapshot 변환기를 만들지 않는다. 상태의 current 판정은 같은 lock에서 수락한 desired/cancel metadata를 기준으로 한다.

`AssetPreparationCommandResult::new(CommandId, Tick, AssetPreparationCommandOutcome)`는 crate-private constructor다. port 오류는 Busy·Stopped·SessionMismatch·InvalidEpoch·NoActiveCandidate·AlreadyBound·AlreadyTaken·Capacity를 구분한다. actor는 자신의 epoch issuer로0/foreign/future/expired를 효과 전에 검사하며, worker의 pre-read capacity 거부도 ID를 소비하지 않는다. 이 내부 seam의 오류/DTO는 기존 request/replay가 요구하는 Clone/Debug/Eq 범위를 맞추되 public mutable field나 payload export를 추가하지 않는다.

## 공통 operation·결과·wire 동결

| runtime type/API | 형태·의미 |
| --- | --- |
| `PreparationId` | opaque session+epoch, Copy/Eq/Debug, `from_parts/session/epoch`.0·foreign·future는 요청에서 검사 |
| `SessionOperation::AssetPreparation(AssetPreparationOperation)` | `Prepare {command_id}`, `Cancel {command_id,candidate}`, `Status` |
| `SessionRequestResult::AssetPreparation(AssetPreparationResult)` | `Command(AssetPreparationCommandResult)` 또는 `Status(AssetPreparationStatus)` |
| `AssetPreparationCommandResult` | private fields/public `command_id/observed_tick/outcome` getters |
| `AssetPreparationCommandOutcome` | `PrepareAccepted {candidate}` / `CancelAccepted {candidate}`. worker 완료와 별개 |
| `AssetPreparationStatus` | public read-only getters `session/active_generation/active_source_digest/active_cooked_accounted_bytes/desired/running/pending/state/ready_candidate/latest_failure` |
| `PreparationCandidateSummary` | read-only `id/is_current/source_digest/catalog_digest/source_scene_revision/asset_count/entity_count/memory/metrics`. 목록·PCM·project Arc 없음 |
| `PreparationFailure` | read-only `id/code/message/source_id/path/diagnostic_truncated`. 마지막1개, code64/message2048/source_id4096/path4096 UTF-8 bytes 상한 |

새 타입/enum은 실험적0.1 additive API다. 기존 enum을 audio feature로 확장한 것과 마찬가지로 exhaustive match의 source 변경 가능성을 문서화한다. 기존 public literal config/metrics/capabilities를 바꾸지 않는다. 상태 getters의 optional ID/summary/failure는 값이 없을 때 None이고 임의 generation0/bytes0으로 바꾸지 않는다.

candidate의 `asset_count`는 legacy mesh descriptor도 포함한 `prepared.scene().assets.len()`, `entity_count`는 검증한 `prepared.entities().len()`이다. imported catalog digest/memory와 선언 수의 범위를 혼동하지 않는다.

상태의 `desired`와 실제 `running/ready` epoch는 다를 수 있다. newer Prepare가 수락된 직후 이전 ready의 큰 payload는 worker가 실제 해제하기 전까지 존재할 수 있다. ready summary의 `is_current`는 desired epoch·취소 상태와 함께 계산하며 stale 것을 activation-ready처럼 표시하지 않는다. worker가 그 payload를 해제하기 전에 retained byte/slot을0으로 보고하지 않는다. 새 import는 이전 ready를 worker에서 해제한 뒤 시작한다.

Cancel은 새 preparation epoch를 발급하지 않는다. 현재 desired의 pending/running/ready를 취소 표시하고 worker가 실제 반환/해제를 관찰한다. 이전 issued epoch는 expired, 이미 실패/취소된 current는 not-active, foreign/0/future는 invalid다. retained command replay는 이 새 효과 검사보다 먼저 원래 결과를 찾는다. actor의 queue/permission/port Busy·invalid 같은 pre-effect 거부는 caller ID를 소비하지 않고, 수락 후 import 실패는 accepted receipt를 바꾸지 않은 채 Status에 나타난다. 효과 후 actor panic은 기존 OutcomeUnknown이며 작업 미실행으로 단정하지 않는다.

protocol2의 submit operation은 `asset_preparation_status`, `asset_preparation_prepare {command_id?}`, `asset_preparation_cancel {command_id?,candidate:{session,epoch}}`다. 결과는 `{kind:"asset_preparation",data:{kind:"status"|"command",data:...}}`로 기존 ticket/poll/ack에 담는다. ID·command ID·tick·scene revision은 decimal string이다. bounded memory/metrics counters는 content summary의 numeric camelCase schema를 재사용하고 새 별도 estimator/JSON report를 복제하지 않는다. result serialization은 exhaustive match를 사용한다.

외부 wire의 정확한 spelling도 고정한다. status/command/candidate/failure의 field는 기존 control/audio와 같은 snake_case이며, nested `memory`/`metrics`만 기존 content camelCase schema를 그대로 쓴다. generation 역시 decimal string이다. state는 `idle/pending/preparing/cpu_prepared/cancel_requested/cancelled/superseded/failed/stopping/stopped`다. 대표 result body는 다음과 같다(기존 ticket envelope는 생략).

```json
{"kind":"asset_preparation","data":{"kind":"command","data":{"command_id":"101","observed_tick":"0","outcome":{"kind":"prepare_accepted","data":{"candidate":{"session":"7","epoch":"1"}}}}}}
```

status data의 field는 `session`, `active_generation`, `active_source_digest`, numeric `active_cooked_accounted_bytes`, optional `desired/running/pending`, `state`, optional `ready_candidate/latest_failure`다. candidate는 `id/is_current/source_digest/catalog_digest/source_scene_revision/asset_count/entity_count/memory/metrics`, failure는 `id/code/message/source_id/path/diagnostic_truncated`다. source digest는 PreparedProject의 digest이고 catalog digest와 바꿔 쓰지 않는다. runtime read-only DTO에는 이와 경쟁하는 자동 Serialize wire를 추가하지 않으며 CLI adapter 한 곳에서 정수 정책과 exhaustive 변환을 유지한다.

| 요청의 pre-effect 오류 | wire `code` |
| --- | --- |
| candidate0·foreign session·future epoch | `invalid_candidate` |
| 이전에 발급했지만 current가 아닌 epoch | `candidate_expired` |
| current가 이미 실패/취소되어 취소할 작업/ready가 없음 | `candidate_not_active` |
| catalog 합산 capacity 부족 | `preparation_capacity` |
| port의 짧은 try-lock contention | 기존 retryable `busy` |
| worker admission 종료 | `preparation_stopped` |
| preparation worker 없는 기본 service | `asset_preparation_unavailable` |
| monotonic epoch 공간 고갈 | `preparation_epoch_exhausted` |

AlreadyTaken/AlreadyBound/initial source mismatch는 startup 오류이며 foreign candidate 요청과 섞지 않는다. 기존 command ID/replay/권한 오류 code는 유지한다. 실제 parser/cache failure는 accepted receipt를 바꾸지 않고 status의 bounded `latest_failure`에 기존 importer code와 함께 기록한다. process smoke는 실제 ready를 취소해 결정적으로 해제 완료를 확인하고, 진행 중 비협력적 parser·stale 반환의 순서는 worker 경계의 주입 테스트에서 따로 검사한다.

capability 목록은 실제 연결 상태를 반영한다. worker를 켜지 않은 service는 명시적 unavailable이고 경로/asset payload를 요청 인자로 받지 않는다. 정상 inspect/mutate와 normal/Step ticket 예산, terminal을 AuthorityAccepted wait 충족으로 보는 기존 동작을 그대로 적용한다. 상태 복사 전에 count/byte를 검사하며 진단이 잘리면 truncated flag를 보존한다.

현재 ticket 하나의 결과 상한은 기존 고정1MiB이며 public limits의 가변 total byte 예산과 구분한다. bounded status의 최대 DTO/진단/digest 크기에 대해 공통 보수 상한32KiB를 두고 이 cap 안에 드는지 복사 전에 검사한다. CLI에 또 다른 추정기를 만들거나, 존재하지 않는 가변 per-ticket 설정을 추가하지 않는다. Prepare/Cancel receipt도 effect 전에 결과 공간을 확인한다.

## CLI·종료·파일 책임

CLI opt-in은 `project serve DIR --protocol 2 --asset-preparation`과 `project view DIR --stdio-control --asset-preparation`이다. protocol1 또는 control 없는 view의 flag는 load/worker/device 시작 전에 거부한다. `--audio offline/native`는 같은 factory/actor 조합을 사용한다. CLI는 Native owner·initial source·기존 service를 연결할 뿐 worker/import/cache 구현을 소유하지 않는다. old start/default 명령은 worker0개로 유지한다.

새 경로는 initial load를 기존 ephemeral loader로 먼저 수행하지 않는다. cache owner 하나를 계속 유지하므로 다른 process의 같은 cache-root cook/capture는 Busy일 수 있다. 기존 opt-in을 끈 native+batch 동시 smoke의 pin-detach 의미는 유지한다. 이 모드의 candidate QA는 같은 live session의 metadata/presentation report를 사용하며 batch capture를 live capture로 표시하지 않는다.

종료는 actor shutdown→worker stop 요청→owner가 실제 종료 관찰 순서다. UI loop가 끝난 뒤 CLI owner에서 최대2초 동안 짧게 `try_shutdown`을 관찰할 수 있다. 이 시간은 parser 강제 종료 deadline이 아니다. 남아 있으면 Stopping/미회수를 보고하고 background owner가 cache를 계속 소유함을 표시한다. 정상 cooperative 작업이 아직 스케줄되지 않았다는 이유로 한 번의 poll만 보고 shutdown 실패로 만들지 않는다. library Drop·process 종료·실제 worker join을 혼동하지 않는다.

Luna xhigh 파일 책임은 content memory summary, runtime worker/types+exports, actor/replay+CLI/DTO/smoke로 분리한다. runtime `lib.rs`는 worker 담당 한 명이 소유하고 control 담당은 필요한 export를 메시지로 요청한다. manifest/lock 변경은 현재 불필요하며 새 dependency를 임의 추가하지 않는다. shared target/Cargo owner 하나, source-only 병렬→focused→통합→hardware→clean archive 순서를 유지한다. watcher·범용 plugin registry·추가 ECS는 이번 코드에 넣지 않는다.

## 하위 gate 검증

- 실제 PNG/glTF/atlas/WAV project의 Prepare→CpuPrepared·metrics/source/catalog identity와 기본 render/audio 계속 실행을 같은 persistent session에서 관찰한다.
- worker를 importer 경계에서 제어해 one-running/one-latest, supersede/cancel, stale 성공/실패, private ready 해제 순서를 결정적으로 검사한다. 임의 sleep으로 경쟁을 통과시키지 않는다.
- ACK/detach/reconnect 후 retained Prepare/Cancel replay가 epoch·import를 중복 생성하지 않고 eviction/cross-kind 결과가 기존 world/audio와 일치한다.
- candidate payload가 ticket/status에 새어 나오지 않으며 여러 조회/ACK로 큰 backing이 누적되지 않는다. 기본 CPU/normal 포화에서도 reserved Step이 진행한다.
- 같은 cache root의 두 owner는 Busy, 한 worker의 반복 준비는 정상 재사용, unchanged warm은 decode/cook0이며 read/parse가 남으면 그대로 기록한다.
- 실제 요청 취소와 actual worker exit·cache lock 회수를 구분한다. non-cooperative read stub에서는 Stopping을 유지한다.
- default/audio/전체 feature CPU·strict Clippy/Python, 기존 real Metal/visible/control/audio 회귀, clean extracted package와 PR/CI/main을 인수한다.

P03.4 비용 표는 기존 smoke helper에 opt-in preparation 검증을 더해 source/binary hash·operator steps·wall time·read/parse/decode/cook/bytes·epoch·cancel/stale·active identity를 기록한다. 실제 화면·audio 교체 비용은 후속 b/c가 연결된 뒤 계측한다. 숫자나 테스트 개수로 엔진 제품 완료율을 만들지 않는다.
