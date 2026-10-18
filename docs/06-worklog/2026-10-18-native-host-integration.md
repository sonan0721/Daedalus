# Native host와 실제 CLI 에셋 교체 통합

상태: P03.3b.3.3 통합 검증 중 · 2026-10-18 · D1/D2/D3 미완료

## 목적과 시작 상태

공통 host·managed window·CLI adapter의 개별 검사를 실제 외부 프로젝트의 같은 창/세션에서 연결한다. [이전 집중 근거](2026-10-17-render-generation.md)는 단일 모델 SDK 창과 CLI CPU/정적 검사까지이며, 혼합 에셋의 실제 CLI 교체 통과가 아니었다. [인수표](../04-plan/implementation-checklist.md)의 host/CLI·pixels/pose/ACK/회수 행을 닫기 전에 결함과 해결 근거를 기록한다. 전체 제품 목표와 이 하위 검증을 구분한다.

시작 source는 작업 branch `feat/native-project-host`의 `fce7cd3`이며 Git 작업 트리는 깨끗했다. 기존 debug CLI는 SHA-256 `ac6fcdea7adc9ccc96d3d7b5244fedf7821a432d9e8f63012d4db7e8c330ebcf`다. 기존 binary를 먼저 사용하고 이 검증을 위해 자동 재빌드하거나 새로운 게임 프로젝트를 만들지 않았다. 소스·스크립트는 검증 동안 동결했다.

## 실제 통합 첫 시도: 초기 device 준비 실패

```sh
python3 scripts/smoke_project.py --cli target/debug/daedalus --assets --view --models --sprites --asset-preparation --asset-activation
```

실행기는 `scripts/run_logged.py`와 고유 raw `dist/project-evidence/p03-host-native-cli-root-attempt01.log`다. exit1·14.395초, raw SHA-256 `c0c196e8a980bb74e9d7fa7a7809690bd614dbd88aad3a7c439cf422af4b417b`를 보존했다. 최종 JSON이 완성되기 전에 실패했으므로 앞서 실행됐을 수 있는 sub-smoke를 성공 건수로 계산하지 않는다. 이 시도는 native audio를 요청하지 않았다.

혼합 model/orthographic sprite fixture의 첫 actual-present 대기에서 host lifecycle이 Failed가 됐다. code는 `initial_device_prepare_failed`, message는 `packet stage generation or upload accounting overflowed`다. generation1/source digest `sha256:60e3fbc7a1d963cd33caf1603e20e7bd04b1f51262081b433e8fec433c511ee1`, last_presented/queued_publication은 null이었다. CPU status는 metadata5116byte·scratch peak12438byte·frame slots1/3이며 frame reserve16MiB였다. 후보 Stage/Commit에 도달한 실패가 아니다.

## 수정 전에 고정한 경계

- 기존 renderer resource planner와 checked accounting의 정확한 실패 지점을 먼저 찾는다. 오류 문자열만 보고 실제 정수 고갈이나 예산 부족이라고 단정하지 않는다.
- 단일 모델 창 통과를 model+sprite 초기 bootstrap 검증으로 대체하지 않는다. 혼합 fixture를 줄이거나 항목을 빼서 통과시키지 않는다.
- 실제 원인이 확인된 부분만 Luna xhigh가 수정하고 기존 CPU/GPU regression에 재현을 넣는다. 별도 importer·renderer·새 Engine·새 smoke framework를 만들지 않는다.
- queue·memory·deadline 한도를 늘려 증상을 가리지 않는다. 부분 GPU owner 보존·nonwaiting UI·same-device staging 계약을 유지한다.
- source 동결 후 관련 집중 검사→실제 CLI 재실행→전체 workspace/CI/package 인수로 진행한다. 실패 raw를 덮어쓰거나 실제 pixel 검증을 metadata로 바꾸지 않는다.

Luna의 첫 작업은 읽기 전용 진단이며 아직 root cause/fix 성공으로 보고하지 않는다. 후속 [live capture 계약](../03-architecture/live-capture-and-artifacts.md)도 읽기 전용 API 감사를 진행하지만, 이 결함을 남긴 채 capture 구현을 같은 source에 섞지 않는다.


## Source 진단과 수정 계약

Luna의 읽기 전용 진단과 root의 해당 source 대조는 `prepare_stage_create_batch`/`preflight_stage_create_batch`의 공유 texture-layout 중복 계산을 찾았다. ModelPipeline과 SpritePipeline은 각각3개의 pipeline을 만들고 layout이 없을 때1개를 더 만든다. 두 task를 같은 초기 상태에서 따로 예상하면8이지만 실제 첫 task 뒤에는 layout이 있으므로7개다. `create_stage_resources`의 실제7/계획8 불일치가 CounterExhausted를 반환한다. 단일 모델 SDK가 통과한 범위와 혼합 bootstrap 실패를 설명하는 source 원인이며 아직 수정 실행 성공은 아니다.

[Device 계약](../03-architecture/render-device-generation.md)에 batch-aware 공유 자원 계산을 먼저 명시했다. Luna xhigh는 `packet_renderer/staging.rs`의 공통 batch estimator/preflight와 기존 `staged_gpu_tests.rs`의 좁은 혼합 회귀만 소유한다. 필요하면 `renderer.rs`의 existing pipeline-call helper에 작은 내부 accessor만 제안한다. host/runtime/CLI·예산·timeout은 이 수정에 포함하지 않는다. 집중 source 동결 뒤 검사와 실제 CLI를 한 소유자가 순서대로 실행한다.


## 후속 live capture의 읽기 전용 감사

별도 Luna 감사는 existing activation factory·RequestTicket의 normal credit·managed actual view를 확인했다. root는 [capture 계약](../03-architecture/live-capture-and-artifacts.md)에 additive 같은-load factory·renderer의 runtime 독립성·host bridge pending/backstop·concrete image lease와 artifact reader·정확한 camera/projection/format 복사를 명시했다. runtime ACK DTO만으로는 tagged projection/camera 전체를 얻을 수 없음을 기록했다. source 구현과 capability 활성화는 하지 않았으며 이번 혼합 pipeline 수정과 별도 인수한다. 정확한 close/panic/backstop 소유권 signature는 다음 코드 착수 전 남은 동결 항목이다.
