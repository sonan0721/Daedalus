# Native live session 통합 계약

상태: 구현 착수 · 2026-10-16 · E01/E02/E03/E08

## 이번 체크포인트

CLI `view`로 실제 Mac 창을 열고 동일 엔진 세션의 ECS entity와 background voxel 생성·meshing 결과를 표시한다. WASD·mouse camera 조작, 화살표 entity 이동, R terrain 재생성, Escape 종료를 제공한다. 작은 engine validation scene이며 sample game이나 일반 scene importer로 표시하지 않는다. 기존 CPU CLI·managed bridge는 유지한다.

## 실행 영역과 자료 흐름

| 영역 | 소유한 데이터와 작업 | 전달 |
| --- | --- | --- |
| macOS main/window thread | winit event loop·local camera·wgpu surface·독립 loading 표시 | input observer try queue, owned scene 소비 |
| presentation bridge thread | CPU mesh 변환·최신 scene 구성·입력→runtime command 변환 | 유한 scene queue, main callback에서 계산·I/O 금지 |
| simulation thread | 실제 Engine·권한 ECS·fixed clock·명령 적용·snapshot 게시 | 유한 owned live state와 명령 결과 |
| bounded CPU pool | 실제 generate_chunk·greedy meshing·협조 취소 | generation/revision을 검사한 결과만 채택 |

renderer는 runtime을 의존하거나 live ECS를 읽지 않는다. runtime의 live session 모듈은 content의 CPU 자료구조만 의존하고 GPU/window는 소유하지 않는다. CLI가 두 계층을 연결한다. geometry는 제한된 청크·quad·byte 예산으로 처리하고 queue 포화 시 마지막 complete scene을 유지한다. loading·tick·scene은 일관된 presentation message로 전달한다.

## 과부하·명령·수명

world 이동은 simulation이 권한 command로 적용하고 실제 snapshot을 통해 화면에 반영한다. queue 수락과 tick 적용을 구분하고 결과 전달에도 상한을 둔다. 표시 state는 coalesce할 수 있지만 command 결과를 성공으로 조작하거나 조용히 버리지 않는다. 오래된 terrain generation은 취소·거부하고 이전 mesh를 유지한다. 동시 job·미수집 결과·state queue·mesh output을 모두 제한한다.

작은 청크가 너무 빨리 끝나는 경우 부하 검증은 실제 생성·meshing의 제한된 반복 횟수를 늘려 수행한다. 반복 수는 CLI에 명시하고 인위적 sleep을 실제 지형 처리 시간으로 표시하지 않는다. 반복 사이 취소를 확인한다. 무한 지형·거주성·전체 VRAM residency는 별도 작업이다.

window callback에서 simulation·worker·readback·파일 쓰기·thread join을 기다리지 않는다. 초기 adapter/device/surface 설정의 startup 비용은 측정 범위를 구분한다. 창 종료 뒤 bridge·simulation 협조 종료와 join을 수행하고 실패 원인·종료 시간을 보고한다. 자동 종료 시간 옵션은 검사에만 사용하며 기본 실행은 사용자 종료까지 유지한다.

## 관측과 검증

- CLI `view`는 window feature에서 제공하고 capability JSON에 실제 지원을 반영한다. 기본 CPU build는 GPU 없이 동작한다.
- 종료 report에 session/tick·entity 초기/최종 상태·applied/rejected command·terrain 완료/취소/stale·worker/queue 설정을 기록한다.
- window report는 실제 그린 frame 수·loading 중 frame/input·camera 변경·resize·frame interval 분포를 유한 표본으로 기록한다. CPU/present 간격이며 GPU 완료 시간이나 144fps 달성 인증으로 표시하지 않는다.
- CPU 시험: snapshot 반영·포화·stale generation·실제 meshing·작업 중 종료·명령 결과·invalid config. 기존 Rust/C# 회귀도 실행한다.
- 실제 Mac 시험: visible 창 확인 → loading 중 camera/entity 입력 → R 재생성 → resize → 종료. 스크린샷과 JSON report를 함께 확인한다. physical input을 실제로 수행하지 못하면 해당 게이트는 미검증으로 남긴다.
- 선택적 PNG는 창 종료 후 마지막 owned scene/camera로 캡처해 파일 I/O/readback을 interactive callback 밖에 둔다. window screenshot과 offscreen capture를 구분한다.

## CLI 배포와 작업 분담

native package는 window 기능을 포함하도록 확장하고 사용 안내·manifest feature·license inventory·PR matrix를 함께 갱신한다. Linux CI는 CPU package smoke이며 실제 window 검증은 Mac에서 수행한다. 현재 tag/public release는 발행하지 않는다.

| Luna xhigh 작업 | 파일 소유권 | 선행 결과 |
| --- | --- | --- |
| live runtime | `crates/daedalus-runtime/**` | live frame/command API를 CLI 담당자에게 공유 |
| window renderer | `crates/daedalus-renderer/**` | atomic presentation message·report API 공유 |
| CLI·package 통합 | `crates/daedalus-cli/**`, scripts/package·notice와 해당 tests, CI, Cargo.lock | 앞선 API를 소비하며 공통 파일은 단독 편집 |

조정자는 Markdown 계약·통합 리뷰·독립 검증·commit/PR를 담당한다. source 파일·test·CI는 모두 Luna xhigh가 작성한다. 구현 계약은 [실행 통합](runtime-control-and-presentation.md)과 [구현 체크리스트](../04-plan/implementation-checklist.md)를 구체화한다.
