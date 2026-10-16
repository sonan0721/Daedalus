# 엔진 제품 준비도 재감사

상태: 전체 목표 진행 중·D1 엔진 인수 미통과 · 2026-10-16

## 만든 이유와 기준선

사용자가 작은 checkpoint 종료와 엔진 전체 완료를 혼동한 보고를 지적했다. `bbccc56`의 실제 공개 API를 Luna xhigh가 read-only로 감사하고 조정자가 문서와 대조했다. 이전 테스트·Mac 창·package 근거는 유효하지만, AI-driven 게임 제작을 끝까지 수행할 수 있다는 증거는 아니다. [제품 인수 계약](../04-plan/product-delivery-and-acceptance.md)을 상위 완료 기준으로 사용한다.

## 실제 코드와 남은 연결

| 기능 | 현재 근거 | 제품 기능으로 부족한 부분 |
| --- | --- | --- |
| project lifecycle | CLI `inspect/run/step`, scene validate/patch, hardcoded live `view` | 사용자 project create/load/build/run·asset resolver·일반 scene 적용 없음 |
| AI 제어 | `crates/daedalus-cli/src/control.rs` JSONL entity/transform/step/snapshot | visible project와 한 세션 제어·MCP·조건/입력/UI/capture trace 없음 |
| scene/render/asset | content scene schema/atomic patch, renderer CPU triangle/scalar material | texture/model/sprite import/cook/load·일반 project scene 실행 없음 |
| 물리·audio | Jolt/Rapier bodies/step/query, Kira PCM/service/offline | ECS·gameplay·scene lifecycle·character/collision/audio 이벤트 연결 미완료 |
| gameplay SDK | C# host·transform API·module 교체·Rust builder 등록 | 외부 game module의 UI/physics/audio/action·watch/build·live project 연결 미완료 |
| UI·animation | animated loading overlay | widget·텍스트/한글 IME·focus·일반 UI·3D skin/2D animation 없음 |
| network/save | 메모리 내 command·dedup·revision | dedicated server/client·replication·AOI·30명·durable world/player save 없음 |
| streaming/simulation | 단일 청크 generate/mesh 취소·stale 검사, CPU fluid/light/path | 이동 기반 다중청크·수정/충돌/조명/복제/저장 연결·VRAM residency 없음 |
| 배포 | macOS/Linux CLI archive와 실제 Mac 실행 | game export·SDK distribution·HTML/Wasm export 없음 |

source 근거는 `crates/daedalus-cli/src/{main,control,view}.rs`, `crates/daedalus-content/src/{scene,voxel,tile,navigation}.rs`, `crates/daedalus-renderer/src/{scene,renderer,window}.rs`, `crates/daedalus-physics/src/`, `crates/daedalus-audio/src/service.rs`, `managed/Daedalus.ManagedSdk/IGameplayModule.cs`다. 이 표는 code audit이며 새로운 실행 시험 통과를 뜻하지 않는다.

## 가장 먼저 해결할 의존성

외부 project를 열고 실행하는 공통 lifecycle이 없다. 따라서 AI가 실제 게임의 scene/code/asset을 수정하고 같은 실행 세션을 관찰·제어하는 제작 경로가 연결되지 않는다. 다음 구현은 별도 fixture 추가가 아니라 public project/content/session 경계를 만든다. transform만 로드하는 하위 단계가 생겨도 P01 전체나 D1을 닫지 않는다.

모든 제품 gate는 미통과다. 하위 구현의 실제 상태와 필수 미완료를 [구현 체크리스트](../04-plan/implementation-checklist.md)에 유지하며, D1 이후에만 별도 3D 게임, 그 인수 뒤 별도 2D 게임을 제작한다.

## 반복 개발 최적화

사용자의 추가 정정은 개발 과정의 반복을 줄이라는 요구다. [반복 파이프라인](../04-plan/development-iteration-pipeline.md)에 code/schema binding·scene 검증·asset 변환·AI 수정/실행/관찰·검증/배포의 공통 경계를 두었다. 구현과 정확성·효과 근거가 없는 항목은 backlog로 남긴다.
