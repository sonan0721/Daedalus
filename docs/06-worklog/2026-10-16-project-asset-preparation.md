# P03.3a 비동기 에셋 후보 준비 작업 기록

상태: content memory summary focused 검증 통과·native/control API 동결·worker/adapter 구현 중 · 2026-10-16 · P03.3/D1 미통과

## 이 기록을 만든 이유

[실행 계약](../03-architecture/project-asset-preparation.md)은 AI가 같은 살아 있는 세션에서 에셋 후보를 준비·조회·취소할 때의 worker·cache·payload·예산·종료 책임을 고정한다. 이 기록은 source 감사·API 동결·Luna 구현과 실제 검사를 구분한다. CPU-prepared만으로 화면·audio의 active generation이 바뀌었다고 보고하지 않는다.

## 확인한 선행 gate

[PR13](https://github.com/sonan0721/Daedalus/pull/13)의 다섯 CI, 두 native packaged asset+offline audio37건, Linux linkage와 main `0b98805986f17598ccd2fca7dfaf5109f2f2cf7c`의 검증한 head 동일 tree를 확인했다. [audio 기록](2026-10-16-imported-audio-runtime.md)에 실제 PCM·native·same-window·clean archive identity와 실패를 보존했다. P03.2만 인수했으며 전체 P03/D1/D2/D3는 열려 있다.

새 branch는 `feat/project-asset-preparation`, 첫 계약 commit은 `2ffa4ed`다. read-only 감사·조정자 리뷰 뒤 `4c4a5dc`에서 native factory/owner/one-shot port와 공통 operation/result/wire API도 동결했다. worker/types와 actor/CLI의 dependent source 작업 전에 crate-private 오류·constructor/getter signature를 추가 공유한다.

## source 감사와 구현 선택

Luna의 read-only 감사와 조정자 리뷰는 TaskPool의 단일 completion receiver를 다른 asset consumer와 공유하면 기존 terrain 결과를 가로챌 수 있음을 확인했다. project worker/cache owner 하나를 별도로 두는 안을 선택했으며 새 Engine/권한 world를 만들지는 않는다. FileCookCache의 lock은 catalog CachePin으로도 유지되므로 초기 load와 이후 준비가 같은 cache owner를 사용해야 한다.

현재 CookedBytes는 imported payload·legacy mesh의 vertex/index charge와 catalog/binding metadata 추정의 장부다. parsed manifest/scene JSON·추가 legacy geometry 복제 backing·caller의 명시적 clone·foreign parser 내부 heap·전체 RSS는 다 세지 않는다. 후보 하나와 active 하나의256MiB 제안은 이 논리적 두-catalog 장부 범위다. decoded base 이미지와 cooked는 겹치므로 더해서 독립 총량처럼 쓰지 않는다. 최초 감사 당시 scratch 사용량만 있고 high-water가 없었으며 아래 구현에서 실제 계측을 추가했다.

첫 content 작업은 public opaque `AssetImportMemorySummary`, private report field/public `memory()` getter, 실제 CookedBytes/DecodedImageBytes/scratch peak·captured source identity 합계를 추가한다. 기존 public AssetImportMetrics literal·source/cook identity·cache format은 유지한다. source bytes·PCM/image를 복사하거나 별도 estimator를 다시 만들지 않는다.

## Luna xhigh 파일 소유권과 검증 순서

- content 담당: `crates/daedalus-content/src/assets/mod.rs`, `types.rs`, `tests.rs`와 report construction에 필요한 `project.rs`. 기존 private BudgetState를 읽는 `cache.rs`의 테스트2곳도 소유권을 확장해 수정했다. memory summary만 구현하며 runtime/manifest/lock은 건드리지 않는다.
- worker 담당: 새 runtime `project_preparation` module/types와 `lib.rs` export. 공통 signature를 먼저 전달하고 worker/cache owner·bounded payload 수명·취소/종료를 구현한다.
- control/CLI 담당: runtime `session_control.rs`와 CLI Rust adapter/테스트를 소유한다. 같은 actor/replay와 opt-in factory를 연결하며 worker나 `lib.rs`를 중복 구현하지 않는다. 작은 독립 작업으로 cook 출력에 기존 memory summary를 싣는다.
- smoke 담당: content source를 동결한 담당자에게 기존 `scripts/smoke_project.py`와 해당 Python regression만 새로 배정한다. opt-in `--asset-preparation`을 사용하고 CLI 담당은 이 파일을 수정하지 않는다. 정확한 wire example을 공유한 뒤 dependent assertions를 작성한다.
- 조정자: MD·통합/source 리뷰·Cargo/target owner 인계·실제 검증·Git/gh를 담당한다. content focused 검증 종료 뒤 Cargo를 회수했으며 worker/adapter는 source-only로 진행한다.

준비 worker는 큰 private candidate 하나를 worker/result 전체에서 보유하고 다음 준비 전 worker에서 이전 ready를 해제한다. 정상 ticket의 ACK는 candidate payload 회수 근거가 아니다. 이번 하위 단계의 제어 결과에는 요약만 싣고 activation/export lease는 후속 계약으로 남긴다.

## content memory summary 구현·실패·실행 근거

`AssetImportMemorySummary`는 private field/public getter와 camelCase Serialize를 사용하고 AssetImportReport가 보유한다. CookedBytes/DecodedImageBytes는 기존 current ledger, scratch peak는 실제 accepted scoped reservation의 동시 high-water, input 합계는 이미 captured한 unique source map에서 읽는다. metadata charge 이후 report를 만들고 별도 payload clone·hash·estimator를 추가하지 않았다.

첫 content 컴파일은 `cache.rs`의 기존 내부 테스트2곳이 `BudgetState.0`에 접근해 E0609로 실패했다. 이 최초 터미널 출력은 raw 파일로 저장하지 못했으므로 진단 요약으로만 남긴다. 공개 API에 영향 없이 BudgetState에 이름 있는 field를 유지하고 두 private 테스트를 `.current`로 맞췄다. 테스트를 통과시키기 위해 production cache 동작을 바꾸지 않았다.

| 검사 | 실제 결과/범위 | 근거 |
| --- | --- | --- |
| Luna content focused test |74통과, 실패0. empty catalog·actual model+PNG cold/warm·source byte 합계·reported cooked보다1작은 limit 거부·scratch overlap/drop/exact limit 검사 | `/tmp/p03-asset-memory-content-tests.log` |
| Luna content strict Clippy | all-target/all-feature, warnings denied 통과 | `/tmp/p03-asset-memory-content-clippy.log` |
| 조정자 독립 content focused test |74통과, 실패0; 실행5.84초. 컴파일 시간을 포함한 speedup 측정 아님 | ignored `dist/project-evidence/p03-preparation-memory-content-root.log` |
| content Wasm library check | `wasm32-unknown-unknown --locked` 통과; 실제 browser runtime/renderer 인수 아님 | ignored `dist/project-evidence/p03-preparation-memory-wasm.log` |

`project cook`도 `report.memory()`를 그대로 직렬화한다. 초기 새 CLI 테스트의 E0277(`CookFailure`에 Debug 없음)와 수정 후 E0433(테스트 import 누락)은 각각 ignored `p03-preparation-memory-cook-tests-initial-failure.log`와 `...second-failure.log`에 보존했다. production 오류 trait을 바꾸지 않고 테스트 scope를 수정했다.

그 다음 focused run은 작업자 보고 기준1통과/2실패였다. 기존 timestamp-only temp-root 이름 충돌로 cache lock Busy가 발생했고, 새 검사는 cold scratch188과 warm scratch132를 같다고 가정해 실패했다. 해당 raw 파일은 passing rerun으로 덮어썼으므로 **이 부분은 작업자 tool 관찰의 진단 요약이고 보존 raw 근거가 아니다**. 테스트 root에 process-local atomic suffix를 더하고 cold cook과 fresh content report를 비교했다. source 수는 임의1 대신 실제 report의3개 source와 대조한다. warm의 scratch가 cold와 같거나 항상 작다는 production 가정을 추가하지 않았다.

최종 `cargo test -p daedalus-cli --locked project_assets::loader_tests`는3통과/실패0이며 ignored `p03-preparation-memory-cook-tests.log`에 남았다. source-only→조정자 compile 오류 전달의 두 왕복 뒤 작업자에게 이 focused command의 독점 target lease를 주고, 통과 후 root가 회수했다. 이후 `cargo build -p daedalus-cli --features native-audio,window --locked`도 통과했다(빌드 로그9.27초, 개발 속도 개선 배율 아님).

## 조정자의 실제 공개 WAV CLI 검증

기존 [공개 원본 provenance](../02-research/asset-validation-provenance.md)의 두 alias/단일 Kenney WAV project를 그대로 사용했다. 새 debug binary SHA-256은 `69a6566797c74635e2727a27163b837a98a9a99d75b95012ea8193ab8aeac4dc`다. 빌드 당시 기반 head는 `4c4a5dc`이고 content summary/CLI wiring의 미커밋 변경을 포함했다. worker/actor 기능이 들어간 binary는 아니다.

```sh
target/debug/daedalus project cook dist/qa-assets/audio-runtime-project --cache-dir dist/project-evidence/p03-memory-wav-cache.Vcisfs
```

새 빈 cache root의 첫 결과와 재사용 결과는 각각 ignored `p03-preparation-memory-wav-cold.json`, `...warm.json`이다. 두 process 모두 exit0이고 입력 digest는 `sha256:bc40583411a4e055250b5ce127e3c32c2eea0846af80e07e5850c24bd7f64d4a`다.

| 항목 | cold | warm |
| --- | --- | --- |
| declared/imported audio aliases / unique cooked audio |2/1|2/1|
| cooked-accounted bytes / catalog metadata bytes |16,001 /5,187|16,001 /5,187|
| captured unique source 수/bytes |3/5,955|3/5,955|
| scoped parser/cache scratch peak bytes |21,628|37,840|
| audio cook / cache write / cache hit |1/1/0|0/0/1|
| source reads / container parses |3/1|3/1|

두 출력 모두 `memory.uniqueSourceCount == sources.length`, `memory.capturedSourceBytes == sum(sources.byteLen)`를 실제 JSON에 대해 확인했다. warm은 cook/write를 재사용했지만 read/parse는 남으며, 이 입력에서는 cache 검증을 포함한 scratch peak가 오히려 크다. elapsed 비교나 일반적인 speedup을 주장하지 않는다. 이16,001bytes 장부는 후속 audio runtime의21,512bytes Frame bank와 다른 scope이므로 서로 대신 쓰거나 전체 RSS로 해석하지 않는다. 이미지가 없는 입력의 decoded base image0은 실제 값이다.

live Prepare/Cancel·worker 수명·새 전체 통합·clean package·CI는 아직 검증하지 않았다. 이전 단계의 CPU281·Python56·Metal9·smoke46을 새 worker 기능의 통과 근거로 사용하지 않는다.

content summary와 작은 CLI cook wiring은 `f132506`으로 commit/push했다. 이 source milestone에 worker/actor/자동 smoke 구현을 포함했다고 보고하지 않는다. 이후 worker·control·smoke는 disjoint source-only 작업 중이며 root가 Cargo를 소유한다.

후속 source 리뷰에서는 stop의 blocking mutex·shared state가 가진 마지막 candidate backing·cache drop 전 Stopped 게시가 계약을 어길 수 있음을 찾아 수정 요청했다. stop은 atomic signal/notify, 큰 ready payload는 worker-local owner, 실제 Drop 후 terminal/상태 전이로 다룬다. worker panic을 catch해 join 성공처럼 숨기지 않으며 callback/actor에서 join하지 않는다. 이 시점의 발견은 리뷰 결과이며 수정 코드의 실행 통과를 뜻하지 않는다.

worker/CLI/Python을 연결하기 전에 protocol result의 snake_case·decimal IDs·content summary의 camelCase·정확한 오류 code를 MD에 동결했다. 라이브러리 DTO에 별도 raw-number/camelCase 자동 wire를 추가하지 않고 control adapter의 exhaustive 변환을 사용한다. 문서 checker와 해당 regression8개는 통과했다(ignored `p03-preparation-docs.log`, `p03-preparation-doc-tests.log`). worker/제어 기능의 통과 근거와는 구분한다.

worker의 syscall/parser 취소는 협력적이고 captured files는 디스크의 동일 순간 snapshot이 아니다. 실제 source/input hash·candidate epoch·stale/cancel/failure와 마지막 정상 active generation을 별도로 관찰해야 한다. 일반 source watch·GPU/audio 교체·최초 loading UI·전체 P03.3 인수는 아직 구현하지 않았다.
