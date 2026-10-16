# 개발 반복 작업의 공통화와 자동화

상태: 실제 중복 감사·적용 진행 중 · 2026-10-16 · A-039/A-041

## 목적

사용자의 최적화 요구는 엔진 실행 속도와 CI 시간에 한정되지 않는다. 엔진과 두 게임을 만드는 동안 반복되는 작업·연결 코드·수동 확인·재작업 자체를 줄인다. [전체 제품 목표](product-delivery-and-acceptance.md)는 계속 진행 중이며, 이 문서나 자동화 도구가 생겼다고 엔진 인수를 통과시키지 않는다.

최적화는 `실제 반복 지점 → 공통 책임/계약 → 구현 → 실패·무효화 검증 → 반복 비용 비교` 순서로 진행한다. 한 번만 쓰는 작업을 위해 범용 플랫폼을 새로 만들거나, 필수 기능·회귀·visual 검증을 생략하는 방향으로 범위를 줄이지 않는다.

## 반복 작업과 공통 경계

| ID | 반복되는 작업 | 공통화할 책임 | 정확성 조건 | 현재 |
| --- | --- | --- | --- | --- |
| DEV01 | 별도 fixture/control/window마다 state·command·결과를 연결 | 공개 project lifecycle와 한 control service, CLI/MCP는 adapter | 같은 project/session·권한·tick·오류 의미, command 적용을 직접 state 변경으로 우회하지 않음 | P01.1 project/session·JSONL 확인, native owned-ticket service kernel 검증. native project view/protocol 2 연결 검증, legacy credit lease 공통화 로컬 검증, MCP/bridge attach 미완료 |
| DEV02 | Rust/C#·JSON schema·binding을 수동으로 맞춤 | version 있는 단일 schema/등록 정보와 생성/검증 가능한 SDK 계약 | ABI layout·정수 정밀도·오류·capability 일치, stale generated file 검출 | 제안, 생성 도구 미구현 |
| DEV03 | 에셋을 매번 전부 변환·업로드 | dependency graph 기반 incremental import/cook·content hash·last-good generation | source뿐 아니라 importer/settings/target/dependencies 변경도 무효화, 취소·손상 결과를 cache hit로 채택하지 않음 | 미구현 |
| DEV04 | 수동 저장→build→실행→창 조작→screenshot 확인 | 동일 session의 edit/apply→wait→inspect/capture→assert→trace 재생 | 실제 정상 입력 경로, 적용 tick과 표시 frame 연결, timeout/실패는 명시 | native window/protocol-2 Step·상태·presented report 확인, live capture/조건/trace 후속 |
| DEV05 | 실행 경로별 별도 scene/asset 검증·오류 처리 | content/project 공통 validation·한 loaded-project 모델 | 위치 있는 진단·원자적 patch·동일 지원 component 정책, fake fallback 없음 | 제한된 root mesh project의 공통 parser·불변 PreparedProject·native/byte adapter 검증. 일반 importer/실행 중 patch 후속 |
| DEV06 | 매번 환경·명령·전체 검사를 수동 조립 | 도구 탐지·focused checks·gate checks·machine-readable report | 실제 선행/영향 graph, 실패 전파, 필수 gate의 검사 생략 금지 | 외부 project CPU18과 opt-in native4 smoke를 같은 backend에서 재사용. 통합 환경 진단/전체 gate 자동화 후속 |
| DEV07 | 개발 빌드·패키지·게임 export를 따로 관리 | versioned build manifest와 공유 package/export backend | source/SDK/asset/target identity·hash·clean/dirty·archive smoke | CLI package만 구현 |
| DEV08 | 재생성·reload 때 resource 변환·scene 복사를 반복 | 불변 asset handle·generation별 CPU/GPU resource cache·dirty extraction | revision/stale·GPU lifetime·예산·취소·이전 정상 resource 유지 | 불변 mesh handle·native GPU cache·dirty instance/material 검증, cook/reload·texture/skin 후속 |

위 표는 backlog다. `제안/미구현`을 기존 기능으로 홍보하지 않는다. 현재 코드 근거와 우선순위는 readiness 감사에 보완한다.

## 이번 코드 감사에서 확인한 반복

- CLI `control.rs`의 `Operation`과 native bridge `lib.rs`의 `Operation`, managed `NativeEngineSession.cs`의 수동 JSON field 구성은 겹치는 DTO를 별도로 유지한다. stdio request ID/protocol과 bridge atomic batch는 의도적인 차이이므로 envelope까지 하나로 억지 통합하지 않는다.
- CLI `view.rs`는 mesh 변환을 일부 cache하지만, frame마다 RenderObject를 clone하고 renderer의 `flatten_scene`·vertex/index buffer 생성 경로가 geometry를 다시 처리한다. source project/asset identity를 먼저 만들고 그 위에 transform-only extraction·resource cache를 연결한다.
- 현재 window QA는 R/키/drag/resize/close·이미지·report 대조를 매번 조립한다. 정상 입력 trace·tick/frame capture correlation을 공통 control로 제공한 뒤 물리 OS 검증을 짧은 별도 검사로 유지한다.
- `TESTING.md`·`managed/README.md`·managed workflow의 .NET/native library/profile 준비는 반복된다. 향후 작은 공통 smoke entrypoint로 줄일 수 있으나 첫 우선순위는 project/service 경로다.
- `scripts/package_cli.py`와 release workflow는 이미 같은 backend를 사용한다. 이를 새 packaging 도구로 중복 구현하지 않는다. importer/cook/watcher는 현재 없으므로 cache 최적화를 구현됐다고 보고하지 않는다.

이 감사는 read-only이며 새 기능·속도 향상 시험 통과가 아니다. 우선순위는 project/common service → source/geometry 재사용 → shared schema/DTO 순서다. project 선행 계약은 [별도 명세](../03-architecture/project-lifecycle.md)에 두었다.

## 계측과 효과 판정

반복 루프별로 `source/build commit·입력 규모·cold/warm·환경·변경 종류·실행 명령·wall time·재컴파일/recook 항목 수·수동 단계 수·오류/회귀·출력 identity`를 남긴다. cache hit만 세지 않고 잘못된 reuse와 불필요한 invalidation을 함께 확인한다. 한 번 관찰한 시간으로 일반적인 개선 배율을 보장하지 않는다.

에셋 cache의 후보 key는 `source bytes + importer/version + settings + target/format + transitive dependency hashes + schema/toolchain 필요분`이다. timestamp만으로 성공 판정을 내리지 않는다. cache entry는 검증 후 원자적으로 게시하고 동일 project의 미완료 세대와 섞지 않는다.

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
