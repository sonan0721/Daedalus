# P03.3a 비동기 에셋 후보 준비 작업 기록

상태: Rust298·Python61·strict Clippy·managed·Metal9·CLI CPU50/native60 통과, clean package/CI 대기 · 2026-10-16 · P03.3/D1 미통과

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

## worker source 리뷰와 focused 검사 준비

worker-local ready 소유권·atomic stop·cache drop 뒤 Stopped·panic 기록 뒤 resume_unwind/actual join 확인을 source에 반영했다. optional candidate/failure metadata가 모든 결과 enum의 크기를 키우지 않게 status variant만 box로 두고, 불필요한 private10-argument constructor/lint allow는 named-field 초기화로 바꿨다. public port/상태 getter와 wire 형식은 유지한다.

worker/port 테스트9개는 source에 마련했으나 아직 Cargo로 실행하지 않았다. validation-before-I/O·missing/file root의 side-effect 거부·metadata lock 중 비대기 stop·panic join·남은 catalog quota0의 pre-read/epoch 미소비·cache 재사용/one-latest/실제 회수·invalid epoch/session을 포함한다. 특히 실제 import **후** publish 전 gate로 success/error를 각각 멈추고 cancel→새 desired→release하여 stale 결과를 버리는 두 테스트를 추가했다. import 시작 전 gate만으로는 이 경계를 증명하지 못한다.

CLI 전체 파일이 끝날 때까지 기다리지 않고 worker+actor runtime source부터 동결해 focused Cargo 검사를 진행할 계획이다. 그동안 CLI-only 작업은 계속할 수 있지만 컴파일 중인 runtime 파일은 수정하지 않는다. 단일 shared target과 명시적 lease를 유지하며 새 cold checkout을 만들지 않는다.

## Python helper의 독립 focused 결과

Rust source-only 작업 중 Python 담당에게 stub-process regression만 실행하는 lease를 별도로 부여했다. 새 실제 CLI 기능은 아직 호출하지 않았다. 첫25개 run은 Busy retry stub이 protocol envelope를 잘못 읽어 `KeyError: operation`으로1error였고 ignored `p03-asset-preparation-python-unit-01.log`에 보존했다. 수정 뒤 같은 focused suite25개가 통과했다(`...unit-02.log`,3.714초). 이 raw run은 보조 도구 검증이며 엔진의 Prepare나 native 표시 성공을 뜻하지 않는다.

helper source 리뷰에서 invalid candidate의 **pre-effect** 거부를 pre-ticket 거부와 구분하고, actor terminal Busy에는 이전 ticket ACK→새 transport request/same caller ID의 제한된 재시도를 추가했다.32KiB는 owned metadata cap이며 encoded JSON cap1MiB와 다르다. 나머지 terminal 오류를 retry로 숨기지 않는 regression과 기존 timeout/child cleanup을 유지한다. 실제 source freeze·runtime/CLI compile·실제 persistent process smoke는 다음 실행 근거로 남긴다.

## 첫 runtime focused build

Python 담당의 source/process lease를 회수하고 worker/lib·actor runtime source를 동결한 뒤 worker 담당에게 `cargo test -p daedalus-runtime --all-features --locked project_preparation`의 독점 shared-target lease를 부여했다. CLI source-only 작업은 별도 crate의 파일에서 계속한다.

attempt01은 테스트 실행 전에 컴파일 실패했다. actor의 cfg attribute가 return expression 뒤에 붙은 분기·새 테스트의 `super` import와 non-test 빌드에서 cfg로 사라진 test-gate phase type을 발견했다. raw는 ignored `p03-preparation-worker-test-01.log`에 보존했다. 각 담당자가 자기 파일을 수정한 뒤 다시 동결했고, 다음 실행은 새 attempt 파일을 사용한다. 이 실패를 worker9개 테스트의 실패/통과 수로 바꾸지 않는다.

attempt02는9개 통과/실패0이었다. 이어진 strict Clippy attempt01은 test module 뒤의 Shared impl 배치와 actor 테스트 helper의 중첩 if를 지적했다(`p03-preparation-worker-clippy-01.log`). 각 담당자가 자기 파일에서 고쳤고 lint를 숨기는 allow를 추가하지 않았다. 최종 worker attempt03도9개 통과/실패0(0.08초), all-target/all-feature runtime Clippy attempt02도 warnings denied로 통과했다. raw는 각각 `p03-preparation-worker-test-03.log`, `p03-preparation-worker-clippy-02.log`다. 이 runtime filtered 검사는 actor 전체 행동·CLI·workspace·native 표시를 실행한 결과가 아니다.

Cargo를 회수한 뒤 control/CLI 담당에게 다음 focused lease를 넘겼다. default/audio feature 경로·CLI parser/DTO·actor Busy/재시도·Step/audio 동시 진행·post-effect panic 등 남은 source/검증을 수행하며 root는 실제 CLI·hardware·packaging을 이 lease와 동시에 실행하지 않는다. worker 담당은 그동안 frozen Python 새 경로의 read-only 교차 리뷰를 맡는다.

CI 담당은 기존 `.github/workflows/release.yml`의 같은 staged-binary step만 `Verify packaged CLI asset, audio, and preparation smoke`로 바꾸고 `--asset-preparation`을 더했다. diff check/YAML syntax 검사만 통과했으며 workflow를 dispatch하거나 tag/Release를 만들지 않았다. 실제 양쪽 CI 통과는 아직 없다.

## 교차 리뷰 후 source 보완

read-only Python 리뷰는 replay 뒤 남은 old ready ID만 보는 검사의 허점을 찾았다. 같은 receipt뿐 아니라 `cpu_prepared`·ready current·desired의 동일 warm ID·running/pending None을 함께 검사하도록 고쳤다. future rejection의 caller ID 재사용을 검증한 이름은 그 범위만 가리키고, native 이름도 CPU 후보와 active view의 session/source 공유를 뜻하도록 바꿨다. candidate 자체가 표시됐다고 주장하지 않는다. 수정 후 py_compile/diff check만 통과했으며 마지막25개 unit run 이후의 작은 assertion 변경은 전체 Python 단계에서 다시 확인한다.

held-worker Step/audio 검증을 위해 기존 cfg(test) gate의 RAII guard에 crate-private 제어 seam을 추가했다. source 리뷰에서 타임아웃 assert가 mutex를 poison시킨 뒤 Drop에서 재차 panic할 수 있는 경로도 찾아, timeout panic 전 lock 해제·poison을 허용하는 release로 바꿨다. 실제 프로세스 abort를 관찰했다고 주장하지 않으며 production API/worker 동작을 바꾸는 기능이 아니다.

runtime/CLI compile attempt03은 새 gate helper가 `MutexGuard` return type을 쓰는데 조정자가 이전 unused-import 진단만 보고 제거를 지시해 실패했다. raw는 `p03-preparation-runtime-cli-allfeatures-03.log`다. import를 복구했고 현재 cfg(test) 사용과 normal-lib 진단을 기준으로 담당자가 정리한다. 재작업 원인과 절차 수정은 [개발 반복 기록](../04-plan/development-iteration-pipeline.md)에 남긴다.

worker의 syscall/parser 취소는 협력적이고 captured files는 디스크의 동일 순간 snapshot이 아니다. 실제 source/input hash·candidate epoch·stale/cancel/failure와 마지막 정상 active generation을 별도로 관찰해야 한다. 일반 source watch·GPU/audio 교체·최초 loading UI·전체 P03.3 인수는 아직 구현하지 않았다.

## actor/CLI 집중 검사와 실패 분류

control 담당의 all-feature attempt01은 held-worker 검사의 ticket ACK에 mutable binding이 없어 E0596으로 컴파일 실패했다. attempt02는 capability getter가 service에만 있고 실제 protocol adapter가 쓰는 client에 없어 E0599로 실패했다. 두 곳 모두 source를 수정했고 raw는 각각 ignored `p03-preparation-runtime-cli-allfeatures-01.log`, `...02.log`다. service와 client의 additive bool 조회는 같은 actor capability를 읽으며 public config literal을 바꾸지 않는다. attempt03의 import 회귀는 위 기록과 같다.

attempt04는 CLI40통과/1실패였다. 실패한 protocol 검사는 caller ID `9223372036854775809`로 Prepare한 뒤 Cancel에 `2`를 사용해 공통 high-water 정책의 `replay_window_expired`를 받았다. 이 실행에서 terminal Busy를 관찰했다고 해석하지 않는다. Cancel을 다음 ID `9223372036854775810`으로 고쳤고, attempt05는 CLI41개·runtime89개·runtime integration2개가 모두 통과했다. CLI/runtime focused 범위이며 workspace 전체 결과가 아니다.

오디오 없는 no-default attempt01도 CLI31개·runtime77개·integration2개가 통과했다. 다만 audio-only 시험 seam의 dead-code 경고가 남아 cfg를 실제 사용 feature에 맞추는 중이다. Busy는 비대기 port의 정상 일시 오류일 수 있으므로, 별도 test helper는 이전 terminal ticket ACK 후 같은 caller ID/새 transport request를 한 deadline 안에서만 재시도한다. 그 외 오류를 성공 재시도로 숨기지 않는다. default/all-feature 최종 정적 검사와 실제 process 검증은 이 변경 후 따로 기록한다.

held-worker 검사는 import gate가 닫힌 동안 정상 credit을 포화시키고 reserved Step이 실제 tick1까지 진행하며, 같은 actor의 offline audio Play/Render16이 실제 비영 PCM을 반환하는지 확인했다. post-effect actor panic은 OutcomeUnknown으로 끝난다. 이것은 worker와 권한/audio 진행의 결정적 CPU 검사이며 frame-rate·실제 loading UI·GPU/audio candidate 교체를 증명하지 않는다.

worker 담당의 최종 read-only 교차 리뷰도 caller-ID/epoch의 효과 후 증가·retained replay 우선·startup Arc 일치와 actor 내 session bind·post-import stale 성공/실패 폐기·worker-local backing/cache 해제·stop/panic 관찰·유한 진단을 대조했고 그 범위의 남은 구체 결함을 찾지 않았다. 이 리뷰에서는 명령을 실행하거나 파일을 수정하지 않았으며 독립 인간 승인이나 전체 제품 검증으로 계산하지 않는다.

control 담당의 최종 all-feature attempt07은 CLI41·runtime89·integration2 통과, no-default attempt03은31·77·2 통과다. strict all-target Clippy는 all-feature attempt04와 no-default attempt03이 warnings denied로 통과했다. 첫 all-feature Clippy의 large-enum 경고는 adapter status DTO도 box로 바꿔 해결했다. ticket count getter를 test-only로 한 변경은 window production에서도 쓰여 Clippy attempt03에서 E0599로 실패했고, 실제 사용에 맞춰 `cfg(any(test, feature = "window"))`로 고쳤다. no-default 첫 Clippy의 unused getter도 같은 feature 경계로 해결했다. 앞선 실패 파일은 덮어쓰지 않았다.

최종 format/diff check와 `cargo build -p daedalus-cli --features native-audio,window --locked`가 통과했고 source를 동결해 root가 Cargo를 회수했다. build raw는 ignored `p03-preparation-runtime-cli-native-window-build-01.log`, binary SHA-256은 `d62426e35e94e4af16365050c6239144dab18331cd0dc4b043f6302893121792`다. 이 binary는 head `8d1e5fd` 기반의 아래 미커밋 worker/adapter 변경을 포함한다. `p03-preparation-terminal-busy-helper-01.log`의 Cargo1개는 통과했지만 바깥 zsh wrapper가 예약 변수 `status`에 대입해 종료 오류를 냈다. Cargo 실패로 바꾸지 않으며 shell 변수에는 task 전용 이름을 사용한다.

root의 전체 Python discovery는61개/8.401초, 문서 checker도 통과했다(`p03-preparation-python-all-root-01.log`, `p03-preparation-docs-root-02.log`). 여기에는 마지막 Python source 감사 뒤의 assertion 변경도 포함한다. 실제 CLI·전체 workspace·hardware·clean package·CI 결과는 다음 기록으로 남긴다.

## 조정자의 실제 persistent CLI 검사

```sh
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --audio --asset-preparation
```

동결한 debug binary에서49개가 모두 통과했다. 실제 process exit0·report status `passed`·관찰 wall time2.002032초이며 raw는 ignored `p03-preparation-cli-cpu-root-01.json`과 stderr 파일이다. GPU나 출력 장치를 열지 않은 CPU 검사다. 외부 임시 project의6개 descriptor/9개 captured source를 사용했고, 하나의 persistent preparation service에서 PNG→warm→glTF node→atlas→WAV 변경과 epoch1~5·retained command replay·future/foreign 거부·ready 취소 완료를 확인했다.

| 항목 | 실제 관찰 |
| --- | --- |
| active source | `sha256:78065cdcd28469c263b79db5977fb0364e83b36674a9a19bdc21cbe9f11dc089` |
| active generation / logical cooked bytes |1 /20,408; 후보 편집 후에도 유지 |
| PNG edit | decode1·image cook2·geometry/atlas/audio cook0 |
| unchanged warm epoch2 | read9·source4,119bytes·parse8·range hash204bytes; decode/모든 leaf cook/cache write0·cache hit10 |
| warm catalog / scratch / decoded base image |20,408 /2,524 /1,056bytes; scratch·decoded는 별도 독립 총량으로 합산하지 않음 |
| node edit | wrapper/catalog identity 변경, geometry/image/atlas/audio cook0 |
| atlas edit / WAV edit | 각 해당 leaf만1회 cook |
| replay / invalid / cancel | epoch2 중복 생성 없음; future/foreign 효과 없음; 취소 수락과 실제 Cancelled·ready 해제를 각각 확인 |

warm source digest는 `sha256:c701aa173614db1004d8a6b2d5b0f277f6e21734f7bd0b7d5159e4b562d0e8a8`, catalog digest는 `37c0872502b69cee16d1ecf9617e9552eb79b854f943adda881d4960fad3dc28`다. 실제 source를 다시 확인하는 비용은 남는다. 이 단1회2초 관찰을 이전 수동 개발 대비 개선 배율로 쓰지 않으며, 아직 실제 candidate 표시/audio 교체·workspace/hardware/package/CI 인수를 뜻하지 않는다.

## 전체 회귀와 장치 검사

root의 `cargo test --workspace --all-features --locked`는 CPU298개 통과/실패0, 장치 검사10개 기본 ignored다(`p03-preparation-workspace-test-root-01.log`). workspace all-target/all-feature strict Clippy도 warnings denied로 통과했다(`...workspace-clippy-root-01.log`). 별도 `cargo build -p daedalus-bridge --locked` 뒤 pinned .NET10.0.402 Release build는 경고/오류0, 실제 native self-test와 managed integration2개가 통과했다. ABI1·transform/revision/Step/snapshot·module migration/rollback/반복 ALC 회수를 확인한 기존 경로 회귀이며 managed preparation attach를 새로 구현한 결과가 아니다.

실제 CLI의 protocol1 serve와 control 없는 view에 `--asset-preparation`을 주면 각각 exit1·정확한 flag 오류를 반환했다. 입력은 없는 project path였고 호출 뒤에도 경로가 생성되지 않았다. raw는 `p03-preparation-v1-guard-root-01.log`, `...view-guard-root-01.log`다.

기존 renderer의 explicit ignored 검사도8개 library+1개 capture, 합계9개가 통과했다(`p03-preparation-metal-root-01.log`). capture report adapter는 Apple M3 Pro/Metal이다. 실제 GPU 색/alpha/공유 resource 회귀 근거이며 준비 후보를 활성화한 결과가 아니다. 통합 native smoke와 clean package/CI는 다음 결과로 분리한다.

첫 debug 통합 native 명령 `--assets --view --models --sprites --audio --audio-native --asset-preparation`은59개/26.421733초에 통과했다(`p03-preparation-cli-native-root-01.json`, stderr 파일). CPU 후보·실제 active present의 session/source가 같고 active generation1·authority tick1·actual present347frames를 확인했다. source digest는 `sha256:1bc9d8790510b700274fa2987ababe35a27a43a652f6edf0d5757f9aebc9740f`, worker 종료는 `stopped`다. 새 후보의 source가 initial과 같아도 CPU 후보 상태와 실제 active 표시를 구분한다. native audio는 실제 backend-reported Ended였고 speaker loopback/청취 측정은 아니다.

이 실행의 audio-native 검사와 preparation 검사는 서로 다른 service였다. held-worker library 검사는 이미 같은 actor의 offline PCM/Step 진행을 증명했지만 실제 CLI 조합 factory의 process 근거를 더 분명히 남기기 위해 smoke 담당에게 작은 보완을 배정했다. `--audio --asset-preparation` 동시 선택 시 preparation process 자체에도 offline audio를 켜고, native view도 같은 조합을 사용하도록 한다. 기본 preparation-only mode는 audio feature를 새로 요구하지 않는다. 기존 bounded audio/JSONL helper를 재사용하고 자동 audio command ID로 preparation caller high-water를 건드리지 않는다. 이 보완의 테스트/실제 process 결과는 아직 대기다. Rust production source는 그대로 동결했으며 Python 수정만으로 workspace298·Metal9를 다시 실행하지 않는다.

Rust worker/actor/CLI와 정확한 공개 계약은 coherent source milestone `1cc69a6`으로 commit/push했다. 이 commit만으로 Python opt-in/새 packaged CI step·P03.3a 인수를 끝냈다고 보고하지 않는다. smoke 보완과 나머지 검증 문서는 후속 coherent commit에 묶는다.

## 같은 actor의 audio/preparation 실제 process 보완

담당자는 기존 v2 audio ticket helper를 사용하는 공통 composition 함수 하나를 CPU/native preparation 경로에 연결했다. caller ID를 보내지 않아 audio 명령은 server-generated 영역을 사용하고 기존 preparation caller high-water를 변경하지 않는다. Play→256 frames render→Stop→64 frames render/reap은 authority Step을 새로 넣지 않는다. preparation-only와 기존 native helper caller는 audio 기본값을 끈다.

focused Python25개/6.012초가 통과했고 raw는 `p03-asset-preparation-python-unit-03.log`다. 이어 root 전체 Python61개/6.896초와 문서 checker도 통과했다(`p03-preparation-python-all-root-02.log`, `...docs-root-03.log`). 최종 smoke source SHA-256은 `7049a01c9e3c0e4f8619fc8558938bf4769e9f758891a70c831c563c196bd31a`, regression source는 `f1f6421f75786e2331010dddf428460d46fc17f7dfade0183e2a5efae50b193e`다. Rust source나 dependency graph는 바뀌지 않았다.

root의 같은 `--assets --audio --asset-preparation` 실제 command는 **50개/1.026083초**에 통과했다(`p03-preparation-cli-cpu-root-02.json`, stderr 파일). 같은 session1/authority tick0에서 imported clip1개·Frame 변환1회/32bytes,48kHz stereo256frames/428개 nonzero samples·peak0.030517578을 관찰했다. 이어 Stop/render64 뒤 cumulative320frames·active voice0·Ended이고 source/active cooked20,408bytes·preparation Idle은 유지됐다. 이 검사는 실제 조합 factory/PCM 근거이며 준비와 mix의 결정적 overlap은 별도 held-worker library 검사로 증명한다.

audio 없는 `--asset-preparation` 실제 mode도30개/0.442016초에 통과했고 composition report는null이다(`p03-preparation-cli-preparation-only-root-01.json`). 위 두 실행의 wall time은 모드·입력이 달라 이전49개와 speedup 비교를 하지 않는다. native 조합·clean package·CI/main 인수는 아직 대기다.

보완한 native 전체 조합도 **60개/18.555073초**에 통과했다(`p03-preparation-cli-native-root-02.json`, stderr 파일). preparation 실행 창 자체의 session1/tick0에서 offline PCM256frames·nonzero samples428·peak0.030517578과 Stop/reap320frames·active voice0을 확인했고, 이어 같은 authority의 tick1/sequence2·active generation1이 실제341frames 표시됐다. source는 앞선 native 입력과 동일하다. worker는 실제 `stopped`, 별도 실제 native backend voice도 Ended였다. offline PCM·native backend 완료·실제 window presentation을 각각 구분하며 speaker loopback이나 candidate activation을 주장하지 않는다. clean archive/CI/main 인수는 다음 실행으로 남긴다.
