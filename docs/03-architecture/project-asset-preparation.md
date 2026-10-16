# P03.3a 실행 중 에셋 후보의 비동기 준비

상태: memory summary 하위 API 동결·native/control API 검토 중. P03.2c PR13/CI/main 인수 확인 · 2026-10-16

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

P03.1의 source·decoded/cooked·metadata·cache 상한은 후보마다 그대로 적용한다. source 감사는 CookedBytes에 payload와 catalog/binding 추정치가 포함되지만 parsed manifest/scene JSON·legacy mesh 자료·모든 allocation이 들어가지는 않음을 확인했다. active PreparedProject와 uncommitted candidate가 동시에 존재하므로 단일 catalog128MiB를 전체 runtime memory 한도로 표현하지 않는다. private ready slot과 running import를 겹치지 않는 수명 규칙을 고정한다.

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

첫 status는 진행 중인 외부 parser의 live byte counter를 만들어 채우지 않는다. running epoch와 active cooked 장부는 알려진 값으로 표시하고 candidate의 최종 cooked/input/scratch summary는 준비가 반환한 뒤에만 Some으로 보고한다. 아직 없거나 legacy인 값은 unavailable/None이며0과 구분한다. summary는 captured 입력과 엔진의 예약 장부이고 외부 allocator를 관측한 값이 아니다.

native factory가 만든 initial project의 새 summary를 사용해 active 장부를 초기화한다. legacy/외부 prepared 값에 summary가 없으면 unknown을0으로 처리하지 않고 preparation 기능의 startup unsupported/계측 불가 오류로 구분한다. 기본 기존 service는 계속 그 prepared 값을 사용할 수 있다. active/candidate가 같은 cook key를 참조하더라도 이 논리적 두-catalog 장부는 보수적으로 각각 charge하며 cross-generation pointer dedup을 구현했다고 표시하지 않는다.

| ledger | 측정할 자료 | 포함했다고 주장하지 않을 범위 |
| --- | --- | --- |
| retained active/candidate | 기존 cooked-accounted 장부·catalog/binding metadata 추정과 owner/slot 수 | scene/manifest JSON의 미계측 heap·legacy geometry·caller가 명시적으로 복제한 자료 전체·정확한 unique backing 계측 |
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

## 구현 전 동결할 API·파일 책임

조정자는 수치 ledger와 public operation/result/accessor를 source 감사 후 확정한다. Luna xhigh 담당끼리 runtime worker·actor/replay·CLI/smoke 파일 소유권을 나누고 manifest/lock owner를 먼저 지정한다. shared target/Cargo owner 하나를 유지한다. watcher dependency·범용 plugin registry·추가 ECS는 이번 코드에 넣지 않는다.

## 하위 gate 검증

- 실제 PNG/glTF/atlas/WAV project의 Prepare→CpuPrepared·metrics/source/catalog identity와 기본 render/audio 계속 실행을 같은 persistent session에서 관찰한다.
- worker를 importer 경계에서 제어해 one-running/one-latest, supersede/cancel, stale 성공/실패, private ready 해제 순서를 결정적으로 검사한다. 임의 sleep으로 경쟁을 통과시키지 않는다.
- ACK/detach/reconnect 후 retained Prepare/Cancel replay가 epoch·import를 중복 생성하지 않고 eviction/cross-kind 결과가 기존 world/audio와 일치한다.
- candidate payload가 ticket/status에 새어 나오지 않으며 여러 조회/ACK로 큰 backing이 누적되지 않는다. 기본 CPU/normal 포화에서도 reserved Step이 진행한다.
- 같은 cache root의 두 owner는 Busy, 한 worker의 반복 준비는 정상 재사용, unchanged warm은 decode/cook0이며 read/parse가 남으면 그대로 기록한다.
- 실제 요청 취소와 actual worker exit·cache lock 회수를 구분한다. non-cooperative read stub에서는 Stopping을 유지한다.
- default/audio/전체 feature CPU·strict Clippy/Python, 기존 real Metal/visible/control/audio 회귀, clean extracted package와 PR/CI/main을 인수한다.

P03.4 비용 표는 기존 smoke helper에 opt-in preparation 검증을 더해 source/binary hash·operator steps·wall time·read/parse/decode/cook/bytes·epoch·cancel/stale·active identity를 기록한다. 실제 화면·audio 교체 비용은 후속 b/c가 연결된 뒤 계측한다. 숫자나 테스트 개수로 엔진 제품 완료율을 만들지 않는다.
