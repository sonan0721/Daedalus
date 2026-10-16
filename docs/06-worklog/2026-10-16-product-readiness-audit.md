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

## 첫 구현 분담

`772ef64`에서 제품 인수·반복 파이프라인·ADR-0005·준비도 감사·project lifecycle 계약을 먼저 커밋했다. Luna xhigh에게 content/project, runtime/project, CLI/project와 renderer projection을 disjoint 파일로 맡겼다. 지원 API와 Cargo.lock 소유권을 먼저 공유하고, 명령별 project parser를 중복 작성하지 않도록 한다.

분담 시작 시점에는 새 project 구현의 실제 실행 검증 전이었다. 기존 fixture 결과를 새 project 기능의 근거로 재사용하지 않았다. 아래 결과는 후속 독립 검증이며, 전체 엔진 제품 gate는 계속 열어둔다.

## P01.1 실제 외부 project 연결

Luna xhigh의 content/runtime/CLI-projection 구현을 `af6a4ff`에 커밋했다. byte/native loader가 같은 parser를 사용하고 `PreparedProject`는 private field·읽기 전용 getter로 source identity와 내용을 묶는다. aggregate input bytes, 경로/symlink, hash와 geometry를 검증한다. runtime은 실제 Engine·stable ID map·정상 revision/tick command 경로를 사용한다. renderer는 runtime crate에 의존하지 않고 expected session·binding·snapshot을 검사한다.

리뷰에서 source 파일의 중복 byte 복사와 digest 중복 hashing, project_info의 전체 목록 복사, expanded geometry의 검사 전 복제, installed init의 build-tree 의존을 수정했다. duplicate handle alias와 작은 정상 scale의 잘못된 renderer 거부도 회귀 검사로 막았다. 이는 확인한 correctness/불필요 작업 제거이며 속도 배율이나 전체 파이프라인 최적화 완료를 뜻하지 않는다.

조정자는 같은 Mac의 pinned Rust 1.94.0에서 다음을 독립 실행했다.

| 검사 | 실제 결과 |
| --- | --- |
| `cargo fmt --all --check` | 통과 |
| `cargo check --workspace --all-features --locked` | 통과 |
| `cargo test --workspace --all-features --locked` | 128건 통과, hardware 2건 기본 ignored |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 통과 |
| 새 bridge build + pinned .NET 10.0.402 Release build | 통과, managed 경고/오류 0 |
| 실제 Rust bridge의 managed self-test·module lifecycle | self-test와 integration 2 scenario 통과 |
| 실제 외부 `project validate/run --ticks 2` | own mesh 1개·entity 1개, source scene revision 3·tick 2·동일 digest |
| 실제 한 `project serve` process의 set/step/get/snapshot | translation `[0,0,0]`→`[1,0,0]`, entity revision 0→1, tick 1·snapshot 일치 |
| 같은 process의 stale revision command | 수락 뒤 tick 2에서 `RevisionConflict`로 거부, translation/revision 유지·snapshot 일치 |
| 실제 `project capture` 513×289 | Apple M3 Pro/Metal, object 1·vertices 3·indices 3, authority/snapshot tick 2, PNG를 열어 authored triangle/material 확인 |

임시 project는 `/tmp/Daedalus-ProjectCLI-QA-20261016`에 있고 엔진 repository에 커밋하지 않았다. 입력 digest는 `sha256:e84c24a46282308076e042b8f7535f495f47f22b882f9edcbfbfc1387aa64ae7`이다. ignored capture는 `dist/project-evidence/project.png`, SHA-256 `e40425b200d7025f31b4524600aa1a82441c628048d5c7a656881a006792f5f9`다. 이 capture는 새 batch session이며 별도 JSONL process의 live frame이라고 표시하지 않는다.

P01.1a~c의 좁은 기반만 체크했다. P01 전체·D1, 이후 별도 D2/D3는 계속 미통과다. visible project 실행·attached C# module·일반 import·hierarchy/prefab·live reload/save·MCP·game export를 비롯한 제품 항목이 남아 있다.

## 다음 계약과 반복 검증 자동화

새 [에셋 상세 문서](../03-architecture/asset-import-cook-and-residency.md)는 공식 glTF/PNG/WAV 조사, CPU cook와 GPU residency identity, 부분 변경 무효화와 last-good generation을 명세하기 위해 만들었다. 새 importer 구현/측정 결과는 아니다. 기존 제작 개요·출처 등록부·문서 지도와 연결했다.

수동으로 외부 project를 작성하고 JSONL 명령/결과를 조립하는 반복을 줄이기 위해 [DEV06 smoke 계약](../04-plan/development-iteration-pipeline.md)에 reusable CPU 검사와 archive 재사용 경로를 먼저 적고 Luna xhigh에게 구현을 맡겼다. CLI·package가 같은 검사를 사용하고 새 Rust build를 불필요하게 실행하지 않는 것이 목적이다.

조정자는 `python3 scripts/smoke_project.py --cli target/debug/daedalus`로 18 checks를 독립 통과했다. empty init/no-overwrite, source identity, dynamic handle, 수락 전후 실제 state, exact command ID·Applied revision, stale RevisionConflict, snapshot/current tick, unsupported 오류와 정상 종료를 대조한다. 입력은 임시 external project이며 종료 후 정리한다. 같은 코드가 archive 추출 binary 검사에도 사용된다. CPU smoke는 GPU·visible window·managed 검사를 대체하지 않는다.

`dist/project-evidence/cpu-smoke.json`의 executable SHA-256은 `532017431c2b982718c843cda6b489f7cbb9c93ff2c493e11aef53b8302b1d54`, warm 단일 관찰 wall time은 0.093575초다. 이전 수동 작업과 통제된 시간 비교는 없으므로 개선 배율을 쓰지 않는다. Python 전체 regression은 41건 통과했으며 응답 timeout/잘못된 JSON/과대 line 때 자식 process와 file handle 정리도 검사한다. package stub unit test의 project smoke는 mock 경계를 명시하고 실제 binary 시험과 구분한다.
