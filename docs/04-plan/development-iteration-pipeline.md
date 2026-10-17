# 개발 반복 작업의 공통화와 자동화

상태: 실제 중복 감사·적용 진행 중 · 2026-10-17 · A-039/A-041

## 목적

사용자의 최적화 요구는 엔진 실행 속도와 CI 시간에 한정되지 않는다. 엔진과 두 게임을 만드는 동안 반복되는 작업·연결 코드·수동 확인·재작업 자체를 줄인다. [전체 제품 목표](product-delivery-and-acceptance.md)는 계속 진행 중이며, 이 문서나 자동화 도구가 생겼다고 엔진 인수를 통과시키지 않는다.

최적화는 `실제 반복 지점 → 공통 책임/계약 → 구현 → 실패·무효화 검증 → 반복 비용 비교` 순서로 진행한다. 한 번만 쓰는 작업을 위해 범용 플랫폼을 새로 만들거나, 필수 기능·회귀·visual 검증을 생략하는 방향으로 범위를 줄이지 않는다.

## 반복 작업과 공통 경계

| ID | 반복되는 작업 | 공통화할 책임 | 정확성 조건 | 현재 |
| --- | --- | --- | --- | --- |
| DEV01 | 별도 fixture/control/window마다 state·command·결과를 연결 | 공개 project lifecycle와 한 control service, CLI/MCP는 adapter | 같은 project/session·권한·tick·오류 의미, command 적용을 직접 state 변경으로 우회하지 않음 | P01.1 project/session·JSONL 확인, native owned-ticket service kernel 검증. native project view/protocol 2 연결 검증, legacy credit lease 공통화 로컬 검증, MCP/bridge attach 미완료 |
| DEV02 | Rust/C#·JSON schema·binding을 수동으로 맞춤 | version 있는 단일 schema/등록 정보와 생성/검증 가능한 SDK 계약 | ABI layout·정수 정밀도·오류·capability 일치, stale generated file 검출 | 제안, 생성 도구 미구현 |
| DEV03 | 에셋을 매번 전부 변환·업로드 | dependency graph 기반 incremental import/cook·content hash·last-good generation | source뿐 아니라 importer/settings/target/dependencies 변경도 무효화, 취소·손상 결과를 cache hit로 채택하지 않음 | P03.1·model/sprite/audio·P03.3a persistent CPU 후보를 PR14까지 인수. warm decode/leaf cook0, read/parse는 남음. b.2 CPU 교체·combined72 검증, test-only wait 경계 수정 후 최종 인수 대기. render/audio 전환 후속 |
| DEV04 | 수동 저장→build→실행→창 조작→screenshot 확인 | 동일 session의 edit/apply→wait→inspect/capture→assert→trace 재생 | 실제 정상 입력 경로, 적용 tick과 표시 frame 연결, timeout/실패는 명시 | native window/protocol-2 Step·상태·presented report 확인, live capture/조건/trace 후속 |
| DEV05 | 실행 경로별 별도 scene/asset 검증·오류 처리 | content/project 공통 validation·한 loaded-project 모델 | 위치 있는 진단·원자적 patch·동일 지원 component 정책, fake fallback 없음 | 공통 resolver/catalog/CLI loader와 model·sprite·audio 실제 소비 인수. 실행 중 patch 후속 |
| DEV06 | 매번 환경·명령·전체 검사를 수동 조립 | 도구 탐지·focused checks·gate checks·machine-readable report | 실제 선행/영향 graph, 실패 전파, 필수 gate의 검사 생략 금지 | assets/audio/preparation/activation CPU62·양 package CI 확인. A-042 뒤 native-only19/clean package combined72 회복. 새 CI의 test wait 가정 수정 후 최종 CI/PR 대기. 통합 환경 진단/전체 gate 자동화 후속 |
| DEV07 | 개발 빌드·패키지·게임 export를 따로 관리 | versioned build manifest와 공유 package/export backend | source/SDK/asset/target identity·hash·clean/dirty·archive smoke | CLI package만 구현 |
| DEV08 | 재생성·reload 때 resource 변환·scene 복사를 반복 | 불변 asset handle·generation별 CPU/GPU resource cache·dirty extraction | revision/stale·GPU lifetime·예산·취소·이전 정상 resource 유지 | 불변 mesh·model texture/sprite·native GPU cache·dirty instance/material 검증. 실행 중 세대 전환·skin 후속 |

위 표는 backlog다. `제안/미구현`을 기존 기능으로 홍보하지 않는다. 현재 코드 근거와 우선순위는 readiness 감사에 보완한다.

### 실패 뒤 반복 범위 줄이기

P03.3b.2의 실제 native audio 실패에서는 이미 생성·검증한 archive의 같은 extracted CLI/hash를 유지하고 기존 helper의 `--audio-native`만 분리 실행했다. 전체 assets/window/prepare/activation을 다시 실행하지 않고 같은 10초 종료 실패를 재현했다. 다음은 [test-only native 진행 진단](../03-architecture/native-audio-progress-diagnostics.md)으로 position/state·stream 오류를 관측한다. 원인을 모른 채 전체 suite/CI를 반복하거나 timeout을 늘리지 않는다. production 변경이 생기면 영향받는 통합·배포 경로를 다시 검증하며 이전 binary의 결과를 새 binary에 전용하지 않는다. 이는 실제 반복 범위 감소이며 wall-time 개선 배율이나 전체 DEV06 완료는 아니다.

### CLI의 연결을 SDK에서 재사용하기

source 감사 결과 project_view가 입력·snapshot 추출·window·actor의 조합을 직접 소유한다. [native project host](../03-architecture/native-project-host.md)는 이 조합을 공개 engine library로 옮기고 CLI/추후 MCP를 adapter로 만드는 구체안이다. [staged renderer](../03-architecture/staged-render-asset-generation.md)는 실제 entity binding의 resource-only 입력을 기존 planner에 투영하고, 작은 업로드와 기존 화면을 한 submit으로 조합하는 안이다. game/CLI마다 별도 lifecycle·cache·staging planner를 다시 작성하지 않게 한다. 아직 구현/속도 향상 결과는 아니다.

기존 Rust type의 설계 probe에서는 renderer를 공유 cache로 한 번 빌드하고 누락 assertion 보완은 같은 rlib에 rustc --test를 직접 적용했다. 새 cold workspace/Cargo rebuild 없이 실제3건을 다시 확인했다. [작업 기록](../06-worklog/2026-10-17-native-host-design.md)에 source/hash·실패 상태·근거 한계를 보존한다. 이 일회성 probe를 새 상시 smoke framework로 늘리지 않는다.

A-042 사용자 가청 보고 뒤에는 보존 binary/hash를 유지한 native-only19건으로 먼저 회복을 확인했다. 전체 combined72는 최신 clean archive의 extracted binary에서 한 번 수행할 계획이다. 기존 binary의 전체 suite→문서 commit→새 package의 전체 suite라는 중복을 만들지 않는다. 이 선택은 검사 대상 source/artifact의 정확성을 유지하는 실행 순서 개선이며 아직 전체 장치 인수나 속도 개선율을 뜻하지 않는다.

그 계획의 c9f5dee clean archive는 실제 combined72를 통과했다. 동시에 CI는 audio/world duplicate replay 테스트의 잘못된 대기 가정을 찾았다. AuthorityAccepted snapshot에 Terminal이 이미 있다고 가정하지 않고, 각 assertion이 요구하는 phase를 기존 bounded wait helper로 명시한다. production 상태 전이를 합쳐 테스트에 맞추거나 CI를 무조건 재시도하지 않는다. test-only 수정 뒤에는 runtime/workspace·정확한 source의 CI를 확인하고, native 검증 재사용은 production/helper/에셋 미변경과 새 clean package의 실행 bytes·notice/inventory hash 일치를 먼저 증명한 경우에만 검토한다. source 이름만 같거나 과거 성공했다는 이유로 다른 bytes의 인수를 생략하지 않는다.

## 이번 코드 감사에서 확인한 반복

- CLI `control.rs`의 `Operation`과 native bridge `lib.rs`의 `Operation`, managed `NativeEngineSession.cs`의 수동 JSON field 구성은 겹치는 DTO를 별도로 유지한다. stdio request ID/protocol과 bridge atomic batch는 의도적인 차이이므로 envelope까지 하나로 억지 통합하지 않는다.
- CLI `view.rs`는 mesh 변환을 일부 cache하지만, frame마다 RenderObject를 clone하고 renderer의 `flatten_scene`·vertex/index buffer 생성 경로가 geometry를 다시 처리한다. source project/asset identity를 먼저 만들고 그 위에 transform-only extraction·resource cache를 연결한다.
- 현재 window QA는 R/키/drag/resize/close·이미지·report 대조를 매번 조립한다. 정상 입력 trace·tick/frame capture correlation을 공통 control로 제공한 뒤 물리 OS 검증을 짧은 별도 검사로 유지한다.
- `TESTING.md`·`managed/README.md`·managed workflow의 .NET/native library/profile 준비는 반복된다. 향후 작은 공통 smoke entrypoint로 줄일 수 있으나 첫 우선순위는 project/service 경로다.
- 초기 감사 당시 `scripts/package_cli.py`와 release workflow는 이미 같은 backend를 사용했다. 새 packaging 도구로 중복 구현하지 않는다. 그때 없었던 importer/cook는 이후 P03.1에서 구현·검증했으며 watcher는 계속 미구현이다.

이 감사는 read-only이며 새 기능·속도 향상 시험 통과가 아니다. 우선순위는 project/common service → source/geometry 재사용 → shared schema/DTO 순서다. project 선행 계약은 [별도 명세](../03-architecture/project-lifecycle.md)에 두었다.

## 계측과 효과 판정

반복 루프별로 `source/build commit·입력 규모·cold/warm·환경·변경 종류·실행 명령·wall time·재컴파일/recook 항목 수·수동 단계 수·오류/회귀·출력 identity`를 남긴다. cache hit만 세지 않고 잘못된 reuse와 불필요한 invalidation을 함께 확인한다. 한 번 관찰한 시간으로 일반적인 개선 배율을 보장하지 않는다.

에셋 container의 ImportKey는 source/parser/settings/실제 dependency identity를 사용한다. LeafCookKey는 해당 geometry/image/audio 출력이 소비한 descriptor/payload·cooker/layout/settings/target만 사용한다. GLB 전체 hash를 geometry key에 넣으면 embedded image 변경에도 geometry를 다시 cook하므로 두 key를 분리한다. PNG base decode는 role/mip variant와 분리해 공유한다. timestamp만으로 성공 판정을 내리지 않는다. cache entry는 검증 후 원자적으로 게시하고 동일 project의 미완료 세대와 섞지 않는다. 정확한 경계는 [P03.1 계약](../03-architecture/asset-import-foundation.md)을 따른다.

SDK 생성은 Rust의 모든 내부 type을 그대로 export하는 방식으로 만들지 않는다. 외부 API schema·소유권·수명·오류를 먼저 정하고, 생성 산출물의 conformance 시험을 유지한다. 초기 구현을 시작하기 전에 공개 ABI와 code generation의 변경 범위를 문서화한다.

AI 루프는 GUI automation만 반복하는 방식에 의존하지 않는다. CLI/MCP가 engine control service를 통해 상태/UI tree·행동·조건·capture를 다루고, 실제 window·정상 device input·audio는 별도 현실 검증 근거로 유지한다. screenshot만 정상이어도 world/server state가 틀리면 실패다.

## 지금 적용한 항목과 다음 순서

기존 작업의 API-first 분담·단일 Cargo.lock 소유·공유 target cache·focused→통합 검사·CI Cargo cache는 유지한다. 이것만으로 개발 프로세스 최적화를 완료했다고 보고하지 않는다.

다음 우선순위는 외부 project의 lifecycle·공통 loaded-session 경로다. 이를 통해 게임마다 별도 fixture와 CLI branch를 만드는 반복을 먼저 없앤다. 이후 그 경로에 asset cook·SDK·오토파일럿을 연결하며 실제 반복 비용을 기록한다. UI·network 등 미구현 제품 기능을 자동화 문서로 대체하지 않는다.

## DEV06 첫 자동화 계약: 외부 CLI project smoke

반복적인 JSON 조립·임시 project 작성·명령 결과 대조를 하나의 CPU smoke로 묶는다. `scripts/smoke_project.py --cli <executable>`는 이미 빌드한 binary를 받으며 Cargo·GPU·.NET 설치나 빌드를 자체 실행하지 않는다. 엔진 저장소 밖의 임시 작업 directory에 빈 project와 작은 authored mesh project를 만들고 종료 시 정리한다. 게임 예제나 fixture 파일을 저장소에 생성하지 않는다.

검사 범위는 init→빈 project 검증→덮어쓰기 거부, 실제 mesh project의 validate/run identity 일치, 하나의 `project serve` process에서 authored ID 조회→권한 transform 명령→step→현재 revision/transform→snapshot→현재 project tick 대조, stale revision의 미적용, unsupported component의 명시적 실패다. handle은 조회 응답에서 얻고 임의 session/index를 고정하지 않는다. JSONL 응답의 protocol/request ID/성공 여부를 확인하고, command 수락과 tick 적용 결과를 구분한다.

모든 subprocess/대기에는 유한 timeout을 둔다. 실패 시 원인과 비정상 응답을 진단하고 child를 정리하며 nonzero exit를 반환한다. 성공은 실행한 검사·binary identity·경과 시간을 machine-readable JSON으로 기록한다. 이것은 CPU 제작 경로의 회귀 검사이며 UI·GPU·gameplay SDK·배포·제품 gate 통과를 뜻하지 않는다.

로컬 검증과 native package 검증은 같은 smoke 함수를 재사용한다. 기존 archive 무결성·version/inspect 검사는 유지하고, 추출한 binary로 project smoke를 추가한다. release workflow는 이미 package backend를 호출하므로 별도 workflow/중복 smoke를 만들지 않는다. stub 기반 package unit test는 실제 process를 성공으로 위조하지 않도록 기존 mock 경계를 명시적으로 조정하고, 실제 binary smoke는 별도로 실행한다. CLI/source 변경이 없는 경우 이 자동화 추가 때문에 Rust workspace 전체를 다시 빌드하지 않는다.

실제 첫 결과: `python3 scripts/smoke_project.py --cli target/debug/daedalus` 한 번으로 18개 계약 검사를 통과했다. 조정자의 같은 Mac warm 관찰은 0.093575초이며 executable SHA-256은 worklog와 ignored JSON에 기록했다. 반복 수동 fixture/JSONL 조립을 한 invocation으로 묶은 근거다. 과거 수동 wall time과 통제된 비교 측정은 없으므로 속도 향상 배율을 주장하지 않는다. Python regression 41건은 별도 통과했다. GPU/GUI/.NET 검사를 이 CPU helper로 대체하지 않는다.

## DEV01/DEV08 후속 설계

P02.2의 실제 적용 결과는 `project view`/protocol 2가 같은 session을 쓰고 source Arc·mesh handle을 공유하며, window redraw는 geometry를 재변환/업로드하지 않는 것이다. explicit native smoke는 기존 `scripts/smoke_project.py --cli ... --view` 한 invocation으로 CPU 18개+native 4개 검사를 수행한다. 최초 연결 run은 20개 통과·4.261689초였고, EOF/blocked stdout 추가 후 같은 entrypoint가 22개 통과·8.832935초를 기록했다. 두 실행은 검사 범위가 달라 속도 비교가 아니며, 이전 수동 루프와 통제된 wall-time 비교가 없어 배율을 주장하지 않는다. 첫 GPU 표시 전에 여러 snapshot이 합쳐질 수 있어 이 한 실행의 instance-write 1회를 transform dirty-update의 증거로 쓰지 않는다. 별도 실제 키 입력 run과 GPU library 시험이 변경 행/세대 재사용을 확인했다. native actor stop을 last frame 존재와 분리하고, Close는 별도 bounded intent를 우선 전달해 caller의 반복 Close 조립을 줄였다. DEV01 전체·DEV04 live capture/trace·DEV08 asset reload는 계속 미완료다.

P02.2 adapter 다음 DEV06 작업은 기존 `scripts/smoke_project.py`의 opt-in `--view` 확장이다. 기본 CPU 18-check/package 검증 의미를 유지하고 빌드·GPU 초기화를 자동 강제하지 않는다. native viewer 모드는 같은 외부 임시 project·binary hash·유한 JSONL 읽기·cleanup을 재사용하며 protocol 2 manual submit→authority poll→Step→terminal→get/latest_frame과 report를 대조한다. 실제 표시의 확인은 window가 time limit으로 종료한 report의 presented identity와 state를 비교하고, 별도 짧은 Close 요청 사례로 정상 종료를 검사할 수 있다. latest_frame metadata만 보고 presented/capture 성공이라 판정하지 않는다. stderr는 임시 파일로 흘려 pipe deadlock을 피하고 모든 대기/child 종료에는 deadline을 둔다. 실제 GPU/display가 필요한 opt-in이며 CPU CI/package의 기존 smoke를 이 모드로 바꾸지 않는다. 이 문단은 착수 전 계약을 보존한 것이며 실제 native 결과는 아래 후속 결과와 worklog에서 구분한다.

P02의 [공통 service](../03-architecture/session-control-service.md)와 [ADR-0006](../05-decisions/0006-owned-session-control-results.md)은 typed 요청·owned 결과·clock·포화/종료를 먼저 고정했다. 계약 감사에서 Manual Step admission deadlock, client-held frame retention, 유한 replay의 의미를 보완한 뒤 Luna xhigh가 runtime/world를 구현했다. root는 backing 회수 수정 후 workspace CPU 157건을 통과했고 기존 control/managed/fixture window는 별도 회귀로 확인한다. actor panic 때 in-flight ticket이 남는 문제와 중복 ticket의 종료 통계도 regression으로 막았다.

당시 P02.1의 공통화는 native service와 legacy mailbox/stop까지였다. legacy count-only result guard/receiver, JSONL·visible project·MCP·SDK attach는 남아 있다. 따라서 DEV01 전체 완료나 제작 속도 배율을 주장하지 않는다. 새 frontend는 이 kernel을 사용하고 adapter-specific envelope·동기/비동기 수명만 추가하는 것이 다음 검증 대상이다.

같은 작업에서 테스트의 정상 admission/frame 조회가 합법적인 Busy를 곧바로 실패 처리하는 경로를 점검했다. 정상 성공 검사는 기존 bounded retry helper(제출 2초·frame 조회 3초)를 사용하고 의도한 Busy/ResultCapacity/권한/clock 오류 검사는 그대로 실패를 대조한다. 실제 flaky 발생률을 측정한 것은 아니며 무조건 retry로 오류를 숨기지 않는다. 마지막 helper 수정은 test-only라 focused 23건을 재검증하고 GPU/package 검사를 불필요하게 반복하지 않았다.

별도 [renderer resource 재사용](../03-architecture/renderer-resource-reuse.md) 문서는 로컬 source 감사에서 확인한 per-frame flatten/buffer 생성과 project entity별 mesh 복제를 없애기 위한 다음 계약이다. CPU mesh handle/instance 갱신·GPU cache·in-flight retirement를 함께 다루고 upload count/bytes로 검증한다. native packet library와 실제 project window의 reuse 검증을 추가했으며, asset cook/reload나 전체 DEV08 완료를 뜻하지 않는다.

## 다음 공통화의 구현 전 경계

P02.1c는 [legacy 결과 수명](../03-architecture/legacy-session-result-lifetimes.md)의 CreditPool/RAII를 사용해 제출·소비·실패마다 수동 반환하는 중복을 줄인다. fixture actor/일반 project의 의미는 그대로 나눈다. pop 직후 panic의 lost ID는 단순 accounting 변경과 별개로 Unknown terminal을 검증한다. 공통 credit 구현과 failure/종료 회귀는 root workspace CPU 200건·기존 project CPU18·실제 loading window 종료로 확인했다. 코드량·개발 시간 개선율을 측정한 것은 아니며 전체 DEV01 완료도 아니다.

P02.3a의 [live capture 계약](../03-architecture/live-capture-and-artifacts.md)은 동일 ticket→render handoff→실제 frame PNG→artifact lease로 AI 관찰 경로를 만든다. image bytes는 별도 budget이며 blocking batch capture로 대체하지 않는다. 아직 설계 리뷰 중이다. 기존 smoke backend를 확장하고 필요한 실제 GPU/OS 검증을 유지해 새로운 자동화가 검증을 생략하지 않도록 한다.

P03.1은 네 포맷마다 별도 project reader·cache·CLI를 만드는 반복을 줄이는 구현이다. native/byte resolver→bounded 공통 context→포맷 driver→immutable catalog를 한 pipeline으로 만들고 CLI cook은 adapter만 둔다. 새 smoke framework 대신 기존 helper의 opt-in `--assets`를 사용한다. 실제 공개 원본 확인은 별도 [출처 기록](../02-research/asset-validation-provenance.md)에 두며 CI smoke에 네트워크 의존을 추가하지 않는다.

로컬 source 고정 후 실제 debug CLI의 CPU+assets31건과 native22건이 통과했다. 공개 원본5개 asset/8개 source에서 같은 Cube2개 ID는 geometry1회 cook/key를 공유했고, warm은 PNG decode/geometry/image/atlas/audio cook과 cache write가 모두0이었다. image-only·node/material·geometry·atlas/WAV 수정과 두 cache corruption 경로는 같은 offline helper가 대조한다. color 해석은 공통 decoder에 명시 policy를 전달하며, standalone과 glTF 의미가 다른 경우를 cache identity로 구분하고 no-op은 공유한다. 포맷별 별도 decoder를 만들지 않았다.

실제 공개 입력의 단1회 debug 관찰은 cold0.34초/warm0.21초다. 두 실행 모두 source read8·container parse7·range hash2448 bytes가 남았고 전체 source는583,313 bytes다. 따라서 모든 제작 비용 제거·일반적인 속도 개선 배율·P03.4 인수로 해석하지 않는다. 초기 parser/IR 준비와 별도 process의 source 검증은 남은 비용이며, 다음 실제 consumer·in-session generation/reload 단계에서 영향과 비용을 구분한다. 전체 결과·binary hash·command는 [작업 기록](../06-worklog/2026-10-16-asset-import.md)을 따른다. 이후 clean package/PR10 CI까지 인수했으나 DEV03/DEV05 전체 완료는 아니다.

## P03.2a의 반복 제거와 로컬 근거

[모델·텍스처 실행 계약](../03-architecture/model-texture-runtime.md)은 다음 중복과 실패를 source 감사로 확인해 구현 대상으로 고정했다. 공통 loader·packet·stats 변환·cache pin 해제·기존 opt-in smoke로 구현했고 로컬 CPU/GPU/native를 검증했다. clean package/extracted 검사와 PR11 다섯 CI/main 동일 tree까지 인수했다.

| 현재 반복 또는 장애 | 이번 공통 책임 | 효과를 확인할 근거 |
| --- | --- | --- |
| CLI마다 legacy/asset-aware loader를 선택하고 오류·지원 범위를 따로 맞춰야 함 | mode만 다른 공통 asset-aware 준비 helper | 같은 source digest·component 지원·진단과 기존 CPU/assets smoke 회귀 |
| window packet과 batch capture의 legacy RenderScene 분기가 나뉨 | 같은 ProjectRenderResources/RenderPacket/PacketRenderer | 실제 GPU texture 픽셀·동일 authority/projection identity, 별도 importer/renderer 없음 |
| 창이 가진 catalog pin 때문에 별도 CLI가 같은 disk cache를 계속 Busy로 만남 | eager owned IR의 명시적 storage-pin detach | project를 유지한 채 cache reopen, pinned clone은 계속 차단, pixels/geometry 유효 |
| model 확장·image hash/geometry pack을 redraw마다 새로 처리할 위험 | 준비 시 고정 mapping·불변 handle·device resource cache | 반복 frame·transform-only에서 geometry/texture upload0, 필요한 dirty slot write만 증가 |
| 실제 GPU 회귀마다 subprocess·project JSON·deadline·cleanup을 새로 조립 | 기존 smoke helper의 opt-in model mode | CPU18/assets31/native22 의미 유지, GPU 미사용 기본 mode와 실패 cleanup 회귀 |

새 renderer·framework·cache를 추가한 개수보다 실제로 공유되는 경로와 제거된 작업을 평가한다. 별도 process의 source read/parse는 이 변경만으로 없어지지 않으며 후속 in-session generation 작업에서 측정한다. 효과가 없는 범용화는 진행률로 계산하지 않는다.

실제 반복 GPU 검사에서 같은 packet 재표시는 geometry/texture/sampler/pair/instance/material 증가0이었다. authority transform만 변경한10draw는 instance write10회만 증가했고 geometry/texture/material과 장치 resource 생성은 증가하지 않았다. 공개 Cube 두 entity도 geometry upload1회/1296 bytes와 texture upload1회/1,398,100 bytes를 공유했고12초 native redraw1409회 동안 geometry/texture/instance/material upload 수가 더 증가하지 않았다.

같은 smoke helper의 `--view --models`는24건/11.015236초로 통과했다. viewer ready 뒤 별도 capture를 실행하고 viewer 생존 확인→authority set/step/query→실제 presented packet 대조까지 자동화해 cache 잠금 회귀를 검사한다. GPU frame capture와 별도 batch process의 의미를 섞지 않는다. 기존 실패/timeout/cleanup 회귀를 포함한 Python54건도 통과했다. 이 값은 현재 검사 실행의 관찰이며 이전 작업 대비 개발 시간 개선율은 측정하지 않았다. 실제 source/binary와 command는 [작업 기록](../06-worklog/2026-10-16-model-texture-runtime.md)에 둔다.

## P03.2b 착수 시 반복 제거 계약

[스프라이트·정사영 계약](../03-architecture/sprite-runtime-and-orthographic-view.md)은 model의 texture/cache/submission 경로와 기존 authority native helper를 재사용한다. slot당16-byte UV row와88-byte unit quad1개를 선택해 region마다 geometry key/upload/entry를 늘리지 않는다. 실제표시 RenderView/PacketViewSubmission은 창·capture·후속 AI 관찰이 camera/projection을 다시 추측하는 중복을 줄인다. 입력 없는 local override가 authority camera를 가리는 source 문제도 함께 다룬다. 이는 구현 대상으로 고정한 내용이고 actual sprite counter/제작시간 효과는 아직 검증하지 않았다.

구현 리뷰에서 정적인 binding map/StableId 복사와 painter sort를 tick마다 반복하는 경로를 찾아, 준비된 generation의 Arc backing을 extract/validation이 공유하도록 바꿨다. 공통 camera JSON 변환과 model/sprite batch-capture 검증 helper도 각각 한곳에서 사용한다. GPU pixel reference와 실제 native smoke는 목적이 달라 하나로 대체하지 않는다. 첫 workspace CPU259건은 통과했으며 최종 GPU/native/package 결과는 후속이다.

PNG alpha 감사는 같은 readback/row-unpadding/encoder에 private 표현 policy와 in-place 변환을 추가하도록 계약했다. 별도 sprite PNG writer·추가 RGBA 배열·GPU intermediate를 만들지 않는다. 불변 데이터 공유의 retained CPU metadata도 별도8 MiB 사전 한도로 제한한다. 공통화 때문에 수명/메모리 비용을 숨기거나 잘못된 결과를 재사용하지 않도록 해당 오류·GPU regression을 추가한다. 새 native `--sprites` opt-in은 기존 임시 project/권한 제어/timeout/cleanup을 사용하며, 코드 공통화가 확인됐다는 이유만으로 개발 시간 개선율을 만들지 않는다.

실제 후속 결과는 CPU264·Metal9·debug 통합 smoke39/14.288578초와 clean archive의 같은 smoke39/14.522841초다. 공개 원본의 네 sprite도 unit geometry1개/88 bytes·image1회/135,168 bytes를 공유했고8초 native939 present 동안 추가 geometry/texture/instance/material 업로드가 없었다. alpha255는 기존 byte를 보존하고 부분 alpha만 공통 backing 안에서 변환한다. 서로 다른 범위·profile의 단1회 관찰로 speedup을 주장하지 않는다. 실제 실패·수정·source/binary identity는 [작업 기록](../06-worklog/2026-10-16-sprite-runtime.md)을 따른다.

이번 smoke의 float64 literal과 f32 wire 기대값 불일치도 DEV02의 후속 conformance 대상으로 기록한다. SDK/schema 생성 시 float의 실제 표현·정밀도와 JSON round-trip을 명시해 각 adapter가 잘못된 exact 비교를 반복하지 않도록 한다. 현재는 작은 fixture 기대값을 고쳤으며 범용 SDK 생성기를 구현한 것은 아니다.

## P03.2c에서 줄일 개발 반복

[imported audio 계약](../03-architecture/imported-audio-runtime.md)은 다음 네 경로를 새 기능 착수 전에 공통 책임으로 고정했다. 계약 commit은 `b1b50e8`이며 아래 결과는 아직 검증 전이다.

| 반복 위험 | 구현할 공통 경로 | 결과 판정 |
| --- | --- | --- |
| native 창과 headless 테스트가 별도 JSONL·ticket·Close 구현을 유지 | renderer-neutral protocol2와 같은 SessionControlService. owner만 창 Close 또는 actor 종료로 변환 | 동일 operation/result·capability·재시도와 EOF/blocked output 회귀 |
| AudioAsset→임시 sample Vec→Frame 변환을 load/play마다 반복 | borrowed PCM의 unique LeafCookKey당 한 bank 변환, voice는 Frame Arc 공유 | alias 수와 unique conversion 수·payload bytes·여러 Play 후 불변 counter |
| audio마다 또 다른 command replay/클라이언트 수명 구현 | 기존 actor의 bounded world/audio 공통 high-water·FIFO 결과 | ACK/detach/cross-kind/eviction 후 중복 Play·offline clock 진행0회 |
| 장치 의존 테스트와 CPU/package fixture를 각각 수동 조립 | 기존 offline asset generator·bounded subprocess/JSONL·cleanup과 opt-in audio smoke | 동일 추출 binary의 offline 검사, 명시적 native 실제 출력 검사, 실패 cleanup |

작업 분담도 manifest 먼저 고정→단일 Cargo.lock/target owner→source-only 병렬 구현→focused→통합→hardware→clean archive 순서로 유지한다. 루프별 wall time·변환/재생/clock counter와 source/binary hash는 실행 후 [작업 기록](../06-worklog/2026-10-16-imported-audio-runtime.md)에 남긴다. 재사용한 함수 수나 CI cache만으로 개발 시간 향상을 주장하지 않는다. C# SDK/schema 생성·source reload·AI live capture 같은 아직 없는 제작 기능도 이 공통화로 완료 처리하지 않는다.

이 작업에서 종료된 작업자에게 단순 메시지만 보내 수정이 시작되지 않는 대기도 확인했다. 단순 메시지 전송과 새 작업 dispatch를 구분하고, 후속 수정의 담당자가 종료 상태이면 명시적으로 재활성화한 뒤 수신·source freeze를 확인한다. 실제 P03.2c volume 시험 수정은 작업자 상태 조회 후 후속 작업으로 다시 배정했다. Cargo 소유권은 그대로 유지했다. 대기 시간의 비교 계측은 없으므로 절감 분수/배율을 만들지 않는다. 별도 orchestration framework를 추가하는 대신 기존 작업 배정 기능을 올바르게 사용한다.

실제 구현 후 CPU281·Python56·Metal9·단일 debug 통합 smoke46/14.966788초와 공개 WAV offline/native를 확인했다. protocol2 DTO/ticket/replay·bounded subprocess/PCM16 작성 helper·package smoke를 재사용했다. 두 alias의 Frame 변환은1회/21,512bytes이고 ACK 후 Play/Render 재시도는 voice/clock을 추가하지 않았다. low-level reap은 호출별 stopped-index Vec 대신 미리 확보한 free-slot storage를 직접 사용한다. 별도 allocator profile이나 이전 수동 작업과 통제된 시간 비교는 없어 speedup 배율을 주장하지 않는다.

QA에서도 관찰 경계를 재사용한다. 첫 sprite native 회귀가 initial frame coalescing 때문에 실패했고 published snapshot 조회는 실제 present ACK를 대신하지 못했다. 중복 polling·sleep을 추가하지 않고 최종 native actual view와 기존 ordered Metal dirty-write 시험으로 근거를 나눴다. device 없는 CPU24(audio 포함)와 실제 native1을 opt-in으로 결합하며, package CI도 기존 asset step에 audio를 붙인다. clean archive·CI/main 결과는 아직 대기이며 DEV06/07 전체 완료는 아니다.

후속 clean archive `95de9b0`도 동일 helper의46건/15.094582초와 공개 WAV offline·native-view를 통과했다. same-window 음향 QA는 기존 public source·JSONL/ACK/timeout·window-report helper를 확장한 ignored 일회성 mode로 확인했다. 새 별도 protocol/게임 project를 추가하지 않았다. notice 누락은 실제 두 target graph를 먼저 검사해 macOS10/Linux공통2를 함께 보강했고 strict221/236dependency의 missing0을 확인했다. 실제 PR CI/main 인수는 아직 대기다. 구체 source/artifact hash와 첫 실패는 audio worklog를 따른다.

그 뒤 PR13의 다섯 CI·두 packaged assets/audio37건·Linux 동적 linkage와 main 동일 tree를 확인해 P03.2c를 인수했다. 이 결과로 DEV06/07 전체나 엔진 제품을 완료 처리하지 않는다.

## P03.3a에서 줄일 개발 반복

[비동기 준비 계약](../03-architecture/project-asset-preparation.md)은 에셋을 고칠 때마다 project load·service/장치 생성·session handle 취득을 반복해야 하는 현재 경로를 다음 대상으로 삼는다. 한 native worker/cache owner를 유지하고 기존 protocol2의 Prepare/Cancel/Status와 ticket/replay를 사용한다. CLI 전용 importer나 두 번째 authority를 만들지 않는다. 첫 단계는 CPU 후보 준비까지이며 화면/audio 전환과 자동 watcher는 후속이다.

메모리 계측도 importer의 기존 budget ledger와 captured input identities에서 한 번 만든다. library report·bounded CLI cook 출력·후속 preparation status가 같은 summary schema를 참조한다. cold/warm마다 payload를 다시 clone·hash하거나 별도 estimator를 유지하는 중복을 추가하지 않는다. CookedBytes는 전체 heap/RSS가 아니며 decoded image와 중복되는 범위를 명시한다.

구현 분담은 content memory summary → runtime worker/공통 type의 정확한 signature 공유 → actor/CLI adapter 순서다. worker와 actor를 병렬로 작성하기 전에 private 오류·constructor·getter까지 맞춰 컴파일 후 API 재작업을 줄인다. Cargo는 한 담당이 shared target을 사용하고 focused 검증 뒤 통합·장치·archive로 확장한다. 테스트 count와 API 공유 자체는 개발 속도 개선의 측정 결과가 아니다. 실제 반복 import의 read/parse/decode/cook·epoch·session/active identity·wall time과 실패는 [준비 작업 기록](../06-worklog/2026-10-16-project-asset-preparation.md)에 남긴다.

작은 cook-summary 테스트에서 두 번의 lexical compile 오류를 조정자 빌드→작업자 source 수정으로 왕복한 뒤 검증 소유권을 조정했다. source-only 병렬 작업 중에도 다른 Cargo 사용자가 없고 동시 source 변경을 멈춘 구간이면, 해당 Luna에게 **한정한 focused command와 shared-target lease**를 넘겨 자기 범위의 compile/test를 마치게 한다. 완료 뒤 source/Cargo를 명시적으로 회수하고 조정자는 실제 CLI/통합 동작을 검증한다. 하나의 Cargo owner·실패 raw log·독립 제품 검증은 유지하면서 단순 compiler feedback 전달을 줄이는 변경이다. 이전 대비 wall-time 개선은 측정하지 않았다.

worker와 actor가 같은 runtime crate를 공유하는 점도 build 순서에 반영했다. 두 runtime 파일을 먼저 동결해 worker-focused9개와 strict runtime Clippy를 통과시키는 동안 CLI-only source 작업은 계속했다. stub-process Python25개는 Rust source 작성 중 별도 lease로 끝냈다. 이후 runtime/CLI default·audio 조합으로 검증 범위를 넓힌다. 각 담당의 파일을 바꾸는 도중 같은 crate를 컴파일하거나 동시에 프로세스 부하를 더하지 않는다. 첫 실패는 고유 attempt 로그로 보존하고, 새 source 변경이 있는 경우에만 관련 검사를 반복한다. 이 실행 순서 변경의 wall-time 절감 배율은 측정하지 않았다.

후속 gate helper를 고친 뒤 이전의 unused-import 진단을 다시 대조하지 않아 조정자가 필요한 MutexGuard import 제거를 지시했고, runtime/CLI compile attempt03을 한 번 되돌렸다. 복구 뒤 진단과 현재 source/cfg의 대응을 확인하도록 정정했다. 작업 소유권 예외로 작은 수정을 다른 담당에게 넘길 때도 이전 warning을 그대로 적용하지 않고, 해당 심볼의 현재 사용 위치와 build feature를 확인한다. 이는 실제 추가 컴파일을 만든 조정 오류의 기록이며 개선 배율을 꾸미지 않는다.

다음 schema 작업의 실제 중복 후보도 남긴다. protocol2 capability는 현재 base/audio/preparation/조합의 네 operation 배열에 공통 이름을 반복하고, CLI의 feature 사전 검사는 serve/view/factory에 유사 분기가 있다. 새 기능마다 조합 배열·검사 branch를 늘리지 않도록 DEV02의 공통 등록 정보/검증 adapter에서 다룬다. 내부 enum·권한 검사와 wire를 한 선언으로 생성할 때 기존 feature-off 오류·decimal ID·상한·conformance를 유지해야 한다. 이번 준비 기능의 인수 직전에 검증 없는 범용 생성기를 덧붙이지 않으며, 이 항목을 이미 제거한 중복으로 계산하지 않는다.

첫 실제 preparation CPU smoke는 기존 helper 한 invocation으로49개/2.002032초에 통과했다. PNG·warm·node·atlas·WAV의 다섯 후보를 같은 service/cache owner에서 준비하고 epoch/replay/실제 취소를 관찰했다. unchanged warm은 read9/parse8이 남지만 decode·geometry/image/atlas/audio cook·cache write는 모두0이다. 매 수정마다 새 service/session을 만들지 않는 경로가 동작한다는 근거이며, 아직 표시/audio 전환이 연결되지 않아 완성된 edit→observe 루프나 개발 시간 개선율로 보고하지 않는다. binary/input/metrics는 [준비 worklog](../06-worklog/2026-10-16-project-asset-preparation.md)에 둔다.

실제 process 결과 리뷰 뒤 audio와 preparation이 별도 service로만 검사된 빈틈을 보강했다. 기존 ticket/audio helper의 composition 함수 하나를 serve/view에 함께 쓰고 자동 server command ID로 caller high-water 충돌을 피했다. Python만 바뀌었으므로 전체 Rust/GPU를 중복 실행하지 않고 Python61·preparation-only30·combined CPU50·native60을 확인했다. 새 검사도 기존 archive/CI staged binary helper에서 그대로 실행한다. 검사 수/모드가 달라 단1회 wall time을 속도 비교에 쓰지 않는다.

## DEV06/07: PR package cache 범위의 실제 반복 비용

2026-10-16 PR14 로그에서 macOS/Linux package가 모두 `Cache not found`였고 release compile은 각각2분47초/2분06초였다. `actions/caches` API snapshot은 package cache가 PR12/13/14 merge ref에만 있고 main에는 없음을 보였다. ignored `p03-preparation-ci-cache-audit-01.json`은8개 cache/10,240,376,390bytes를 기록한다. 용량 설정·과금은 바꾸지 않는다.

공식 문서상 PR merge-ref cache는 그 PR의 재실행에만 사용할 수 있고 다른 PR/main에서 복원할 수 없다. 현재 release workflow는 PR/tag/manual만 실행해, 저장 성공과 다음 PR 재사용이 달랐다. main의 trusted push/manual로 만든 cache는 후속 PR의 default/base branch 검색에서 사용할 수 있다. 접근일2026-10-16. [GitHub cache 범위와 검색 규칙](https://docs.github.com/en/actions/reference/workflows-and-actions/dependency-caching#restrictions-for-accessing-a-cache)

다음 좁은 개선을 채택한다. 기존 release workflow에 `push.branches: [main]`을 추가해 같은 native build/package/smoke/artifact 경로가 main cache를 공급한다. 새 workflow/job·dependency·토큰 권한·cache path·보호 체크를 추가하지 않는다. public Publish는 기존 tag-only 조건을 그대로 사용하고 branch/manual에는 실행하지 않는다. cache hit이어도 locked build·실제 smoke는 유지한다. main에서의 추가 build 비용과 후속 PR의 실제 restore/compile을 기록해 효과를 판단하며, cache 저장만 보고 시간 절감 배율을 만들지 않는다.

초기 공급은 현재 main `275ea360fb53dde3713aca84910781a4c895a8e4`의 기존 read-only [manual package run](https://github.com/sonan0721/Daedalus/actions/runs/38907971641)으로 시작했다. 아직 결과와 main cache 생성/다음 PR hit는 확인하지 않았다. source/설계→Luna의 최소 YAML 변경→syntax/기존 package unit→실제 main run/후속 PR 복원 확인 순서이며, 이 개선으로 DEV06/07 전체를 완료 처리하지 않는다.

후속 실제 결과: 해당 manual run은 macOS/Linux package와 각 staged CPU50이 모두 성공했고 Publish는 skipped였다. cache API에서 `refs/heads/main`의 macOS ID9050680369/249,846,600bytes, Linux ID9050710266/313,614,043bytes 생성을 확인했다. 두 binary hash는 PR14와 같았다. raw는 ignored `p03-preparation-main-seed-*-01.log`, `...main-cache-after-seed-01.json`, `...main-seed-jobs-01.json`이다. 아직 다음 PR의 실제 restore/compile 감소를 주장하지 않는다.

Luna의 workflow diff는 `push.branches`의 main 두 줄만 추가한다. 기존 Publish의 `push && refs/tags/v` 조건·read-only build·모든 job/검증은 그대로다. Ruby Psych YAML syntax·package unit14개/2.594초·diff check가 통과했다. PyYAML이 없어 이미 설치된 parser를 썼고 dependency를 추가하지 않았다. 이 세 검사의 raw 파일은 저장되지 않아 작업자 tool 응답의 관찰로 기록하며 사후 raw를 재현해 최초 run인 것처럼 쓰지 않는다. 현재 자동 main trigger 변경은 다음 PR 인수 전이다.

[PR15](https://github.com/sonan0721/Daedalus/pull/15)의 실제 후속 package 두 개가 main cache를 primary key로 복원했다. macOS release compile은37.16초, Linux는31.45초였으며 두 staged CPU50은 각각1.951949초/0.457183초에 통과했다. binary SHA-256도 PR14·main 초기 공급과 각각 동일했다. 직전 cache miss의167초/126초와 비교 가능한 compile 구간을 기록하지만 runner/image 변동을 통제한 반복 실험은 아니므로 일반적인 속도 개선율로 해석하지 않는다. cache restore 자체의 비용도 job 총시간에 포함된다. raw는 ignored `p03-preparation-cache-pr15-macos-01.log`, `...linux-01.log`다.

PR15의 다섯 체크 성공·Publish skipped를 확인했다. head `9c7e27103309840a99546baac8778dd54f42c128`, CI synthetic merge `a4fd0183bb5b3f65b23a7e15b80b7b40d8aac66c`, squash main `c2283cb686ff7eb5589d6e38f66d5e357993531e`의 tree는 `9d484f6be44d7ea1b4359eeb6328e675e6a28388`로 같다. main push가 실제 [run38909669190](https://github.com/sonan0721/Daedalus/actions/runs/38909669190)을 시작했으며 결과는 별도로 확인한다. source가 바뀌지 않은 doc/YAML 개선에 local Rust/Metal/package 전체를 중복 실행하지 않았고 실제 CI의 locked build/smoke는 유지했다. DEV06/07 전체와 엔진 제품 gate는 계속 열려 있다.

그 main push run도 두 package·두 staged CPU50·Linux linkage를 통과했고 Publish는 skipped였다. 양쪽 로그에서 실제 primary-key main cache restore를 확인했다. 따라서 이번 좁은 개선의 manual 초기 공급→후속 PR 복원→자동 main 실행은 검증했다. raw는 ignored `p03-preparation-cache-main-push-{jobs,macos,linux}-01.*`다. 공개 tag/Release는 만들지 않았다. 이후 dependency/toolchain key가 바뀌면 새 main 공급이 필요하며 cache eviction/runner 부하에 관계없이 항상 같은 시간을 얻는다는 보장은 없다.

## P03.3b에서 공통화할 반복

[호환성 계약](../03-architecture/asset-replacement-compatibility.md)은 content의 단일 비교기를 두고 consumer마다 authored field 판단·정렬/진단을 중복 구현하지 않도록 한다. 이 b.1 비교기는 content 로컬 검증을 통과했다. [headless 세대 계약](../03-architecture/headless-asset-generation.md)은 새 session 생성 대신 기존 worker·actor/replay·현재 Engine을 이어 쓰며 source 변경→Stage/Commit→같은 상태 조회를 한 실행에서 검증한다. authored topology 비교의 reference index만 한도 내에 만들고 payload 재복사/해시/parse는 하지 않는다. b.2 CPU 전환의 실제 첫 process 검증은 통과했으나 최종 통합/package 인수와 renderer/audio 전환은 남아 있다. 개발 시간 개선율을 주장하지 않는다.

CLI adapter는 실제 네 capability 배열의 base7 반복을 base/extension composer로 제거한다. 기존 feature-off 오류·네 조합의 operation 순서·새 headless-only 지원 경계를 함께 검사한 뒤에만 구현 결과로 기록한다. 범용 schema generator를 이번 선행 작업에 끼워 넣지 않고 source에서 확인한 작은 중복만 제거한다. runtime public type/getter·wire decimal ID를 먼저 고정해 별도 작업자가 private source를 추측하며 재작성하는 반복도 줄인다. compile/test 소유권과 focused→통합 순서는 유지한다.

첫 b.2 process-helper review는 feature별 수동 command-ID 번호대가 공통 high-water와 충돌하는 반복을 발견했다. 새 helper는 한 세션의 단일 증가 issuer와 명시적 replay ID 재사용으로 바꿨다. 실제 엔진 오류가 나기 전 source review에서 찾은 문제이며 최종 actual CLI 검증은 대기다. provider body를 기다리는 adapter source 대기도 줄여 동결한 공개 API로 먼저 작성하게 하고, 한정한 Python stub lease는 runtime source 작업 중 실행한 뒤 Cargo에 돌려줬다. Python28 통과는 transport 회귀 근거일 뿐 실제 세대 전환·개발 시간 개선율이 아니다. 구체 source/raw/수정 전후 범위는 [activation 작업 기록](../06-worklog/2026-10-16-project-asset-activation.md)을 따른다.

이후 실제 CLI opt-in은28개(CPU18+activation10)를 통과했다. PNG/model/atlas/WAV를 같은 session에서 순서대로 commit하고 현재 handle/pose/tick을 유지했다. helper의 잘못된 ProjectInfo 필드 assertion만 고친 재시도는 기존 binary를 그대로 사용해 불필요한 Cargo rebuild를 피했다. runtime source 변경 뒤에는 해당 runtime 검사만 먼저 수행하고, source freeze 후 CLI/전체 통합/장치/package를 순서대로 수행한다. 단순 raw/count/hash는 위 작업 기록을 기준으로 하고 설계·TESTING·체크리스트에 중복 복사하지 않는다. 서로 다른 범위의 실행 시간을 속도 향상으로 비교하지 않는다.

마지막 경계 검증에서는 한 작업자가 기존 runtime attempt 번호를 재사용해 앞선 raw 일부를 덮어썼다. 현재 raw를 다른 고유 prefix로 보존하고 손실 범위를 작업 기록에 명시했다. 이후 로그 생성은 고유 이름뿐 아니라 exclusive/noclobber로 기존 파일을 거부한다. 통과한 과거 검사를 다시 실행해 과거 원본처럼 보관하지 않는다. CLI 모든 기능 test/Clippy는 다음 workspace 모든 기능 검사에 이미 포함되므로 별도로 중복 실행하지 않고, default feature 경계는 별도 유지한다.

## P03.3b.3의 실행 연결 재사용

[ADR-0007](../05-decisions/0007-reusable-native-host-and-generation-consumer.md)은 각 frontend가 CLI의 프로젝트 load·actor·입력·packet·종료 코드를 다시 만드는 반복을 줄이기 위해 공통 native host library를 채택했다. 초기 source는 runtime worker가 한 번 읽은 실제 Arc를 [consumer port](../03-architecture/project-generation-consumer.md)로 전달한다. renderer가 다시 project를 load하거나 세대마다 device를 만드는 방식을 추가하지 않는다. CLI는 기존 protocol2/파일 출력 adapter를 유지하고 Rust SDK 통합 시험은 CLI 없이 같은 host를 소비한다. 이 공통 제작 경로는 현재 구현 중이며 아직 실제 edit→present 루프를 통과하지 않았다.

공통화 전 source 감사는 prepared model/sprite sidecar 두 map을 각각 전체 expanded instance 수로 예약하는 중복을 찾았다. 실제 각 binding 수로 예약·동일 estimator에 반영하는 작은 변경을 renderer CPU 작업에 포함한다. instance 최대16384와 metadata8MiB는 독립 한도이며 모든 조합이 들어간다는 보장이 아니다. wrapper가 아니라 실제 공유 leaf·validation normal map·capacity를 세고, 일시 pin 포화의 Busy와 개별 상한 초과의 영구 오류를 구분한다. 숫자를 줄였다는 이유만으로 제작 반복 시간이 개선됐다고 계산하지 않는다.

runtime source 작성 중에도 별도 renderer/host의 read-only 리뷰를 진행하되 Cargo/장치 lease는 한 명에게만 둔다. 초기 코드 리뷰에서 consumer close와 try-lock 경합 때 currentness·untaken payload 정리가 늦어질 수 있는 경계를 찾았고, 컴파일 전에 독립 atomic 관측과 기존 preparation worker의 최종 cleanup으로 조정하도록 작업을 배정했다. 실제 재현 테스트·결과는 후속 worklog에 기록한다. public signature와 표준 erased pin을 먼저 고정하고 dependent source를 이어 붙여 서로 private 구현을 추측하는 재작업을 줄인다. 절감 시간/배율은 측정하지 않았다.

PR17에서는 runtime source만 선택 commit해 clean CI가 기존 renderer와 전체 회귀를 검사하도록 했다. local renderer WIP는 보존했다. 실제 Mac 하드웨어 검증에는 CI가 만든 archive를 내려받아 exact checkout/dirty=false·checksum·binary hash를 확인하고 기존 helper를 그대로 썼다. 두 packaged CPU62와 downloaded binary의 native72를 확인했고 local release를 다시 컴파일하지 않았다. source가 다른 WIP 빌드로 검증 대상을 바꾸거나 새 smoke framework를 만드는 반복을 피한 절차이며 시간 절감 배율은 측정하지 않았다.

renderer source 리뷰에서는 미참조 legacy mesh의 불필요 stage 요청과 첫 frame의 expected bind group×draw 탐색을 찾았다. 실제 참조 집합과 private resource-set token으로 공통 CPU 검증을 재사용하는 계약을 먼저 수정했다. token은 작은 연결이며 GPU 초기화 완료 검증을 대신하지 않는다. source 구현·CPU/GPU 검사 전이므로 이를 이미 달성한 프레임 시간 개선으로 보고하지 않는다. 로그 wrapper의 zsh readonly 변수 실수는 AGENTS에 task-specific exit 변수·단일 exclusive descriptor 규칙으로 명시했다.

device production과 실제 GPU 시나리오 테스트를 한 작업자에게 직렬로 몰지 않도록, 새 test module 파일만 별도 Luna가 작성하고 device 담당이 parent 선언/기존 helper의 최소 visibility를 소유하도록 분리했다. CPU 담당의 실제 PreparedProject/validated-frame 경계를 공유하며 forged FrameHandle이나 별도 smoke framework는 만들지 않는다. compile은 관련 source가 모두 동결된 뒤 단일 owner가 맡아 작성 병렬화와 불안정한 동시 빌드를 구분한다. 실제 검증/효과는 후속 기록 대상이다.
