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

## 로컬 native 배포본

`e4a172b015a502f3cd2c5d75fb5abb7e0de7e1bc`의 clean worktree에서 기존 `python3 scripts/package_cli.py`를 실행했다. macOS arm64 release/window build, dependency notice/inventory, archive/hash 검증, 추출 binary의 version/inspect와 공통 18-check project smoke가 통과했다. source build는 기존 target cache를 재사용했으며 이 관찰의 Cargo release build는 9.01초다. 과거 다른 feature/환경과의 성능 비교로 해석하지 않는다.

ignored artifact directory는 `dist/local/daedalus-0.1.0-aarch64-apple-darwin-e4a172b015a5-clean-20261016T002407Z-a337222d37f449238d24e36df9585d31/`다. manifest의 `dirty`는 false, features는 `window`, binary SHA-256은 `e203bbaebed0836047b0844cf90e0e8ec6ca9efd307e05b5bb726f8a7a12ed1b`다. 원본 소스·SDK를 다시 찾아야 init이 동작하는 경로는 제거했고, 추출 검사는 외부 임시 directory에서 수행한다.

같은 package staging executable로 외부 project를 513×289 Metal PNG로 출력하는 것도 통과했다(`dist/project-evidence/packaged-project.png`). 새 interactive GUI 입력을 다시 시험한 결과는 아니며, 기존 fixture 창의 실제 입력 근거와 구분한다. tag/public Release는 만들지 않았다. PR의 native Linux/macOS 결과는 확인 후 별도 기록한다.

## PR #6 검증과 다음 미통과

[PR #6](https://github.com/sonan0721/Daedalus/pull/6)은 head `9035efab9ed397e459ee23612f8fd83ebdd91636`의 필수 docs/rust/managed native integration 및 두 native package job이 모두 통과한 뒤 `cd2c2ec9ce20b0e3ee7a624f2f74dc529e8263d6`로 squash 병합됐다. docs 7초·managed 54초·Rust 1분34초, package macOS 2분36초·Linux 2분39초다. 서로 다른 역할/환경의 job 시간을 성능 향상률로 비교하지 않는다. 두 package job은 실제 추출 binary의 공통 project smoke까지 실행했고, publish는 PR이므로 skipped다.

이 병합은 P01.1과 DEV06 하위 결과이며 P01/P02/P03 및 D1 전체를 통과시킨 것이 아니다. 다음 작업은 fixture window와 동기 project control 사이의 간극이다. 새 [session service 문서](../03-architecture/session-control-service.md)는 하나의 project actor·client별 결과·manual/realtime·visible/CLI/MCP·resource reuse·live capture·종료 계약을 먼저 정리하기 위해 만들었다. typed API와 legacy migration 방법을 Luna xhigh가 read-only로 구체화한 뒤 관련 구현을 시작한다.

## P02 설계와 코드 분담

`feat/shared-session-service`에서 service/owned ticket API와 limits를 먼저 커밋했다. [ADR-0006](../05-decisions/0006-owned-session-control-results.md)은 결과 소유권·Manual Step reserve·유한 replay/retention의 선택 이유를 보존하기 위해 만들었다. read-only red-team에서 frame Arc retention과 bounded replay 의미를 보완했다. initial backend는 native이며 새 MCP/웹 지원을 구현했다고 표시하지 않는다.

Luna xhigh 한 명이 runtime의 typed service·공통 primitive·기존 LiveSession 재사용 및 필요한 world의 bounded page helper를 단독 소유한다. root는 코드·테스트를 작성하지 않고 계약·리뷰·독립 검증·Git을 맡는다. 다른 Luna의 renderer 감사는 read-only로 유지하고 core API와 file ownership을 먼저 고정해 충돌/재작업을 줄인다. source가 안정화되기 전 중복 전체 Cargo 검사를 병렬로 실행하지 않는다.

새 [renderer resource 재사용 문서](../03-architecture/renderer-resource-reuse.md)는 현재 frame마다 world-space geometry 변환·buffer 생성이 반복되는 실제 source 감사에 근거한다. 불변 CPU handle, transform/material instance 갱신, 공통 packet backend, 장치별 cache와 in-flight byte budget·변경 종류별 측정을 명세한다. 이 문서는 다음 구현안이며 실제 성능 결과는 아직 없다.

## P02.1 통합 검사와 추가 회수 감사

Luna xhigh의 source freeze 후 조정자는 pinned Rust의 workspace check/fmt·all-feature CPU 151건(기본 hardware ignored 2건)·strict Clippy를 독립 통과했다. 새 bridge/CLI window build, .NET 10.0.402 Release의 경고/오류 0·Rust-backed self-test·2 integration scenario도 통과했다. source-level 리뷰에서 try/drop의 blocking mutex 회귀, in-flight query/Step의 panic terminal 누락, duplicate ticket의 unique-command 종료 통계 오류를 수정하고 회귀 검사했다.

같은 debug CLI로 기존 protocol-1 external project smoke 18개를 통과했다. 해당 binary SHA-256은 `8551bf54d0a2ef704a9d22e0289f7cf15a6f3dce78d1d93fe7b00ee8137f4340`다. 이번 smoke는 native window·managed 검사와 함께 실행됐으므로 이전 단일 warm smoke와 속도 배율을 비교하지 않는다.

`view --seconds 3 --work-iterations 10000 --report dist/project-evidence/p02-legacy-window.json`은 실제 M3 Pro/Metal에서 time-limit 종료했다. 로딩 중 312 frame·authority tick 185, 실제 terrain job 1개 시작/취소·active job 0·termination stop_requested를 기록했다. 키/drag를 주입한 새 물리 입력 검증이나 external project window 검증이 아니다. frame interval은 CPU present 간격이며 이번 결과를 144fps/GPU latency 통과로 표시하지 않는다. 이 fixture 회귀는 공통 mailbox/stop 추출에 대한 좁은 근거다.

후속 read-only 감사에서 기존 core snapshot의 내부 `entities` Arc만 보유하면 wrapper weak tracking을 우회하는 retention 결함과 service의 world dedup capacity 상한 누락을 확인했다. 다음 수정은 core의 wrapper/backing generation 이중 추적·service backing probe·producer 최소 2/frame 최소 1의 별도 예산·dedup 16,384 hard cap이다. 이 문단의 151건은 그 후속 수정 **이전** 통합 결과이며, 수정 후 재검사를 별도로 기록한다. audit issue를 열어둔 채 PR을 병합하지 않는다.

DEV01의 실제 공통화는 native owned-ticket actor와 legacy mailbox/stop까지다. legacy count-only result guard/receiver와 CLI/window/MCP/managed attach는 남았으며 P02.1c·P02 전체·D1을 통과시키지 않는다. 다음 renderer API 명세는 동일 mesh hash/Arc와 stable instance 갱신을 고정했지만 아직 코드나 개선 수치가 아니다.

## P02.1 회수 수정 후 최종 독립 검사

core는 snapshot wrapper와 entity backing의 weak record를 함께 유지한다. service mailbox는 별도의 backing probe를 사용하며 성공한 weak upgrade 참조를 계수 중 유지해 baseline 경쟁을 피한다. wrapper와 backing을 함께 잡아도 generation은 한 번만 세고 둘 다 해제되면 회수한다. native service의 producer capacity는 2~64, frame capacity는 1~64로 독립 검증하며 config를 몰래 확대/축소하지 않는다. command dedup capacity는 1~16,384다. Luna xhigh의 수정 후 read-only source 재감사에서 해당 두 결함과 cap-one 진행 경로의 추가 finding은 없었다. 이 감사는 테스트 실행과 별도다.

조정자는 수정 후 check/fmt·all-feature workspace **CPU 157건 통과, hardware 2건 기본 ignored**·all-target/all-feature strict Clippy·fresh bridge/window CLI build를 다시 통과했다. core 10건과 runtime 48건에는 backing-only/복제 snapshot 보유·lazy extraction 포화·해제 후 회복, frame cap 1과 producer cap 2의 반복 진행, dedup 설정 경계가 포함된다. pinned .NET의 실제 Rust self-test·integration 2 scenario도 새 bridge로 다시 통과했다. Python regression은 이번 branch의 변경 없는 script 기준 41건 통과이며 문서 link checker를 새 Markdown 편집 후 다시 통과했다.

`dist/project-evidence/p02-final-cpu-smoke.json`은 동일 18개 검사를 통과했다. 최종 debug CLI SHA-256은 `b6100d7d4b4db7e435e53ee9eff4090c7806177960ec2dd295ea2a5654d31fa0`다. `p02-final-legacy-window.json`의 실제 M3 Pro/Metal 창은 같은 3초/10,000 work iterations 조건에서 loading frame 320·tick 186, terrain job 1개 시작/취소·active 0·time_limit/stop_requested 종료를 기록했다. 새 physical key/pointer 입력이나 external project window 검증은 아니다. concurrent native/managed smoke의 wall time을 과거 단독 실행과 비교하지 않는다.

이 근거로 P02.1a/b의 좁은 native library 항목만 체크한다. P02.1c와 모든 frontend attach·live capture·실제 MCP·UI/asset/gameplay 통합·전체 D1/D2/D3는 계속 진행 중이다. 다음 package/PR은 clean source와 실제 required/native jobs를 확인한 뒤 별도 기록한다.

## P02.1 로컬 CLI package

구현 commit `e07dd8ebfd771d70ada023ed44a32a8d649e6e76`의 clean worktree에서 공유 release cache로 `python3 scripts/package_cli.py`를 실행했다. release/window build·notice/inventory·archive/hash·추출 binary의 version/inspect와 동일 external-project smoke 18개가 통과했다. manifest는 dirty=false, target aarch64-apple-darwin, binary SHA-256 `e0e124f70b556ac1a489b6fd950bfe0926fb5534a2c04c4d487727d1ae7ed6d4`다. 이 단일 cached build의 Cargo 관찰은 8.43초이며 개선 배율로 해석하지 않는다.

ignored directory는 `dist/local/daedalus-0.1.0-aarch64-apple-darwin-e07dd8ebfd77-clean-20261016T011157Z-428b00c662974964af1ed0fa88ff29c7/`다. 같은 staging binary의 실제 Metal `view --seconds 2 --work-iterations 10000`은 loading frame 224·tick 124, terrain job 1개 완료·active 0·time_limit/stop_requested 종료를 기록했다(`dist/project-evidence/p02-packaged-window.json`). debug 종료 때의 취소와 달리 이 release 실행에서는 job이 완료됐다. physical 입력이나 새 service frontend 지원을 의미하지 않는다. tag/public Release는 발행하지 않았다.

package 뒤에는 test-only 정상 제출/frame polling을 bounded Busy retry로 통일했다. producer와 client의 합법적인 짧은 mutex 경합을 엔진 오류처럼 실패시키지 않기 위해서다. 의도한 오류 검사는 직접 응답을 대조하며 유지한다. 제출 helper 반영 후 root의 전체 157건·fmt·strict Clippy가 통과했고, 마지막 frame helper 편집 후 focused session-control 23건·fmt를 root가 다시 확인했다. production 동작은 바뀌지 않아 GPU/.NET/package를 다시 만들지 않았다. 위 manifest의 source는 계속 정확히 e07dd8e다. PR native jobs는 최종 head를 별도로 빌드한다.

## PR #7 첫 Linux 검사에서 확인한 회수 순서

[PR #7](https://github.com/sonan0721/Daedalus/pull/7)의 최초 head `1bd724109f7555140de3e5421270dd99f91b2073`에서 docs와 managed native integration은 통과했지만 [Rust run 38834232275](https://github.com/sonan0721/Daedalus/actions/runs/38834232275)은 `frame_capacity_one_counts_backing_only_retention_and_recovers`의 다음 Step admission이 ResultCapacity로 실패했다. 로컬 157건 통과가 다른 scheduling 순서의 안전성을 증명하지 못한 사례다.

terminal을 통지한 뒤 actor의 in-flight bookkeeping/완료 함수가 별도 credit 참조를 잠시 보유하는 순서를 조사·수정한다. terminal payload reader의 정당한 lease는 유지하되 bookkeeping만으로 회수를 늦추지 않는다. actor를 terminal publication 직후 멈춘 controlled regression에서 ack/reader 해제 후 Step reserve 재사용을 확인한다. ResultCapacity를 무조건 retry하거나 실패한 CI를 단순 재실행해 통과로 대신하지 않는다. 수정 후 실제 검사 결과와 source는 이어서 기록하며 이 PR은 아직 병합하지 않았다.

source 조사에서 ActiveRequest의 중복 credit Arc와 finish_cell의 terminal 통지 뒤 credit drop을 확인했다. Luna xhigh는 불필요한 actor lease를 제거하고 terminal 전의 bookkeeping 참조 반환으로 바꿨다. 실제 progress/reader lease는 유지한다. 2 MiB 예산과 유한 test-only actor pause로 terminal을 관찰한 직후 actor cleanup을 멈춘 상태에서 ack/마지막 reader 해제→다음 Step 수락을 검증했다. root의 수정 후 workspace **158건·check/fmt·strict Clippy**는 통과했다. 기존 ResultCapacity 오류 검사를 retry로 바꾸지 않았다. 이 시점은 로컬 검증이며 수정된 PR CI 통과나 병합 결과는 아니다.

read-only credit 재감사에서는 실제 reader lease·pending/종료·panic correlation의 추가 finding이 없었다. abandoned payload가 폐기되는 동안 새 admission은 placeholder로만 존재하고 actor가 다음 결과를 병렬 생성하지 않는다는 aggregate-bound 근거를 source 계약에도 남겼다. 원래 failed head의 macOS/Linux package는 각각 3분4초/2분40초 통과했으나 이 결과를 수정된 head의 검증으로 대체하지 않는다.

## PR #7 최종 통과와 다음 P02 연결

수정 head `6ab114f0228ec7702be780338db8797c999a9567`은 docs 5초·Rust 1분4초·managed native integration 1분10초·macOS package 1분3초·Linux package 42초를 모두 통과했다. tag publish는 skipped다. 두 package는 실제 추출 CLI smoke를 실행했다. [PR #7](https://github.com/sonan0721/Daedalus/pull/7)은 그 head로 고정해 병합했고 main merge commit은 `5c2cefc05dc5f5b0123a1394cb2b7d53c125bbd2`다. 서로 다른 cache/환경의 소요 시간을 일반적인 성능 향상률로 사용하지 않는다.

수정 head clean source의 로컬 package도 다시 만들었다. `dist/local/daedalus-0.1.0-aarch64-apple-darwin-6ab114f0228e-clean-20261016T012616Z-2e6be7fc9af7477f8b6357a1d9587837/`이며 dirty=false·release/window·동일 18-check extracted smoke가 통과했다. Cargo cached build는 5.76초다. binary SHA-256은 이전에 실제 Metal 창을 검증한 파일과 같은 `e0e124f70b556ac1a489b6fd950bfe0926fb5534a2c04c4d487727d1ae7ed6d4`였다. 새 service가 아직 CLI adapter에서 사용되지 않으므로 source 수정과 CLI executable identity를 같은 개념으로 취급하지 않는다. 같은 binary의 GPU 창을 불필요하게 다시 시험하지 않았고, library 수정은 별도 158건·Linux CI로 검증했다.

다음 branch는 `feat/renderer-resource-reuse`다. renderer-owned immutable mesh handle·project prepare/extract·persistent GPU cache와 packet 창 API를 먼저 구현하고, 같은 service의 외부 project/JSONL control을 연결한다. window source 감사는 accepted metadata와 실제 surface submission identity의 차이, shared event handler와 camera override·in-flight 한도를 명세에 보완했다. 아직 이 새 renderer나 project window를 구현/검증한 결과는 아니다. 전체 D1/D2/D3는 계속 진행 중이다.

## P02.2a 구현 전 연결 계약

새 [project-view-control.md](../03-architecture/project-view-control.md)는 여러 frontend마다 source/state/command를 복제하는 개발 반복을 줄이기 위해 만들었다. 하나의 불변 source Arc, simulation을 실행하지 않는 실제 tick-0 게시, 같은 service의 protocol-2 ticket adapter, 유한 process I/O와 정상 Close를 명세한다. realtime viewer를 기본으로 두고 deterministic manual은 명시 선택이다. live capture/MCP나 정식 action system은 이 연결만으로 지원한다고 표시하지 않는다.

Luna xhigh renderer 담당자는 renderer 전체·Cargo.lock·공유 Cargo target을 단독 소유한다. runtime 담당자는 disjoint project/session/lib의 shared source/tick-0와 테스트 source부터 작성하며 Cargo는 기다린다. 별도 Luna는 새 계약을 read-only 검토한다. root는 문서·리뷰·통합 검증을 맡는다. 새 범용 개발 도구나 cold target을 만들어 작업을 중복하지 않는다. 아직 새 코드의 최종 검증·개선 수치는 없다.
