# 엔진 구현 체크리스트

상태: D1 엔진 구현 중·제품 인수 미통과 · 2026-10-16 · 모든 코딩 Luna xhigh

**전체 목표는 진행 중이다.** [제품 인수 계약](product-delivery-and-acceptance.md)의 D1/P01~P16을 통과한 뒤 별도 D2 3D 게임·D3 2D 게임으로 검증한다. 아래 E작업과 live checkpoint 통과는 엔진 제품 인수가 아니다. 현재 gap은 [실제 코드 감사](../06-worklog/2026-10-16-product-readiness-audit.md)에 기록한다.

## 착수와 검증 원칙

사용자가 설계 문서를 기반으로 엔진 구축을 시작하도록 요청했다. 기존 전체 설계 게이트를 통과했다고 표시하지 않고, 모듈별 계약·최소 통합·오류 시험·실제 실행 근거를 쌓는다. 엔진 전용 저장소이며 샘플 게임은 추가하지 않는다. 기능 검증 목적이 명시된 작은 fixture는 허용한다.

실제 빌드 가능한 구현, 소스만 있는 미검증 구현, 아직 계획뿐인 기능을 구분한다. 상용 참조 게임과 동등한 품질·144fps·모든 플랫폼 지원을 구조만으로 완료 처리하지 않는다. 전용 서버와 GPU offscreen을 구분한다.

## 구축 단위

| 작업 | 산출물·책임 | 주요 종료 증거 | 현재 |
| --- | --- | --- | --- |
| E01 / I01 | Cargo workspace, core·ECS·fixed clock·명령·유한 task·snapshot·Rust 시스템 등록 | stale ID/revision·포화·취소·job 실패·느린 worker 중 표시 소비·shutdown 시험 | 병렬 ECS·CPU 예산 진단·지속 JSONL 제어 검증. 실제 창/스트리밍 통합 후속 |
| E02 / I02·I04 | wgpu renderer, 실제 offscreen capture, native window/input, 독립 presentation | 실제 GPU PNG·상태 manifest·resize·로딩 중 UI/입력 경로 | Metal capture·실제 Mac 창·로딩 중 ECS/카메라 입력·resize·취소 확인. 고급 렌더·성능 인증 후속 |
| E03 / I06 | voxel·tile·fluid·light·terrain revision과 비동기 파생 데이터 | 경계·greedy mesh·질량 보존·광원 제거·stale 결과 시험 | 초기 CPU 알고리즘18건 통과. async/영역 연결 후속 |
| E04 / I03 | C# SDK·CoreCLR 연결·module 교체·빌드형 Rust 게임플레이 | 실제 C#→Rust 명령·상태 보존·실패 복구·참조 회수 | bridge7건·.NET2 scenario 통과. transform SDK·명시적 module 교체 구현, 일반 API·자동 build 후속 |
| E05 / I05 | 권한 command·영속화·세션 transport·관심 영역 | 두 client·중복/손실·저장 복원·30명 부하 증거 | 대기 |
| E06 / I08 | 2D/3D 물리·캐릭터·오디오 | 실제 backend 통합·접지/충돌·audio callback/stream·복귀 | Jolt/Rapier primitive·Kira PCM 10건+장치 probe 확인. 캐릭터·stream·ECS 연결 후속 |
| E07 / I07·I08 | UI·텍스트/IME·애니메이션·내비·렌더 효과 | 실제 입력·경로 갱신·pose·시각/행동 fixture | 대기 |
| E08 / I09 | AI 친화적 scene/asset/cutscene/terrain 원본·CLI/MCP·native/web export | schema 검증·실행 중 조작·재현 캡처·깨끗한 배포 | scene validate/patch·지속 CLI·로컬 package·두 native PR dry build 확인. tag publish·MCP·web 후속 |
| E09 / I10 | 통합·회귀·장시간·품질/성능·릴리스 | 검증 matrix·실측 artifact·지원 상태·미완료 목록 | 대기 |

작업 이름은 모듈별 추적 단위이며 한 번에 모두 병렬 편집하지 않는다. 선행 API가 필요한 작업은 계약을 먼저 공유하고 파일 소유권을 분리한다. 세부 체크는 실제 구현을 리뷰하면서 이 표 아래에 추가한다.

## E01 계약

- 권한 ECS는 simulation 실행 영역이 소유한다. Bevy ECS는 부품으로 사용하고 전체 Bevy 엔진은 도입하지 않는다.
- entity handle은 session/world·generation을 검사한다. 명령에는 ID·대상·revision·결과를 둔다.
- 고정 tick은 설정값이다. 초기 개발 설정은 60Hz이며 144fps presentation 목표와 구분한다. 따라잡기 횟수·초과를 드러낸다.
- 배경 CPU 작업은 입력을 소유하고 결과를 반환한다. 유한 admission·결과 credit·try 제출·협조 취소·실패 결과를 제공한다. blocking I/O를 같은 CPU pool에 넣지 않는다.
- snapshot은 필요한 데이터만 불변 게시한다. 느린 소비자 때문에 simulation이 동기 대기하지 않으며 큐 상한과 게시 실패 의미를 정한다.
- Rust gameplay는 공개 builder/system 등록 경로를 사용한다. 코어의 재빌드 경로이며 native hot-reload가 아니다.
- 설정·trace·상태를 직렬화할 공통 계약을 둔다. 가짜 backend나 고정 성공 응답을 실제 엔진 기능으로 표시하지 않는다.

## E02 계약

- renderer는 live ECS를 직접 읽지 않고 렌더 입력을 소유한다. wgpu 버전은 설치 Rust와 실제 빌드 호환을 확인해 lock한다.
- offscreen은 실제 GPU texture를 렌더하고 readback한다. no-adapter를 명시적 오류로 반환하며 검은 placeholder 이미지를 성공으로 반환하지 않는다.
- viewport·camera·mesh·material·light의 작은 검증 장면은 엔진 테스트 fixture로만 제공한다. 게임 프로젝트는 만들지 않는다.
- native OS event·presentation은 simulation/worker의 장기 작업 완료를 동기 대기하지 않는다. 표시와 캡처의 비용·실패는 분리한다.

## E03·E08 초기 데이터 계약

- 엔진 콘텐츠의 작은 직렬화 가능 데이터 모델을 `daedalus-content`에 둔다. scene은 schema version·stable ID·asset 참조·entity/component 목록과 revision을 갖고 중복 ID·누락 참조·잘못된 값·계층 cycle을 검증한다.
- scene patch는 expected revision을 검사하고 전체 변경을 검증한 뒤 교체한다. 실패 시 기존 scene을 유지한다. 런타임 저장과 편집 원본의 역할을 구분한다.
- 모델·텍스처 등 고밀도 데이터를 모두 JSON으로 바꾸지 않는다. AI가 읽는 식별자·참조·파라미터와 binary 자산·쿠킹 산출물을 분리한다.
- voxel chunk는 정수 좌표·seed·generator version·data revision을 갖는다. greedy mesh는 방향·material·경계 이웃의 점유를 고려하고 오래된 작업 결과를 거부할 metadata를 보존한다.
- 초기 타일 유체는 보존되는 정수량·cell capacity와 고정 입력에서 다음 상태를 계산한다. 경계 유출입·이중 적용·음수·용량 초과를 시험하며 무제한 활성 영역을 한 tick에 처리하지 않는다.
- 초기 조명은 광원 제거 뒤에도 이전 빛이 남지 않는 reference 경로를 둔다. 대규모 incremental frontier 최적화는 검증 전 별도 상태로 남긴다.
- 초기 경로 탐색은 점유 grid의 bounded/cancellable query다. 일반 3D navmesh·플랫포머 jump graph·군중 회피를 구현했다고 부르지 않는다. world revision과 결과 stale 여부를 전달한다.
- 콘텐츠 라이브러리는 CPU·직렬화 계층이며 권한 ECS나 GPU 객체를 소유하지 않는다. 실제 async 생성·렌더·저장 연결은 통합 작업과 해당 시험으로 구분한다.

## E04·E06 상세 계약

C# 연결은 [managed host·bridge](../03-architecture/managed-host-and-native-bridge.md), 실제 물리·오디오 연결은 [backend 계약](../03-architecture/physics-and-audio-runtime.md)을 따른다. 각 작업의 코드 시작 전 해당 소유자와 범위·버전·테스트를 공유한다. 물리·오디오·GUI·CoreCLR이 없어도 코어 CPU 검사는 실행 가능하게 모듈을 분리한다.

## E01·E02·E08 실행 통합

[실행·제어·표시 계약](../03-architecture/runtime-control-and-presentation.md)에 따라 process CPU 예산이 있는 병렬 ECS, 같은 세션의 bounded JSONL 제어, 콘텐츠 검증·실제 GPU capture CLI를 연결한다. 창은 macOS main thread에서 실행하고 simulation·worker 완료를 callback에서 기다리지 않는다. 첫 독립 라이브러리 커밋과 이 통합의 검증을 분리한다.

## 기록할 근거

각 작업에 실제 변경 파일·선택 버전·환경·명령·결과·artifact·남은 제한을 기록한다. 현재 실행 가능한 명령은 루트 TESTING에 갱신하고, CI와 테스트 코드는 Luna xhigh가 작성한다. 이 문서의 미완료 체크를 코드 파일 생성만으로 완료 처리하지 않는다.

## E08 CLI 배포 후속

사용자 추가 요구에 따라 [CLI 빌드·태그 릴리스](../03-architecture/cli-build-and-release.md)를 구현한다. 현재 로컬 versioned binary·manifest·checksum을 만들고 관리하며, future `v<version>` push 자동 배포를 구성한다. 지금 태그·공개 Release는 만들지 않는다. PR packaging·실제 태그 publish를 별도 근거로 기록한다.

## E01·E02·E03·E08 native live checkpoint

사용자 A-038에 따라 [native live session 계약](../03-architecture/native-live-session.md)을 구현한다. 아래 게이트는 코드 생성만으로 완료하지 않는다.

- [x] CLI 한 명령으로 실제 native window 실행·camera·resize·종료.
- [x] 권한 ECS 명령의 적용 결과를 동일 session snapshot으로 표시.
- [x] bounded background generation·meshing·revision/generation 검사·stale 거부.
- [x] 실제 loading 중 input·presentation 지속을 Mac 입력과 실행 report로 확인.
- [x] 포화·취소·종료·frame interval·회귀 검사 및 window 포함 local package 검증.

통과 근거와 실제 수치는 [작업 기록](../06-worklog/2026-10-16-native-live-session.md)에 둔다. 이는 한 청크 native 통합 체크포인트이며 전체 E01~E08 품질 게이트의 완료가 아니다.

## P01 제품 제작 경로의 선행 작업

전체 D1/P01은 미통과다. [project lifecycle 계약](../03-architecture/project-lifecycle.md)에 따라 다음 하위 작업을 진행한다. 이 목록을 모두 통과해도 game module·live edit/reload·hierarchy/prefab·정식 importer·MCP·저장/배포가 남아 있으면 P01/D1을 닫지 않는다.

- [x] P01.1a: 외부 project manifest·typed scene/mesh·bounded source/digest·semantic 검증.
- [x] P01.1b: prepared project를 실제 Engine에 적용하고 stable ID·handle·revision·snapshot으로 조작.
- [x] P01.1c: 공통 library로 CLI init/validate/inspect/run/serve/capture 연결, project 자료의 실제 GPU 출력.
- [ ] P01.2: 일반 asset import/cook·계층·prefab·scene lifecycle·last-good reload.
- [ ] P01.3: C# project module·SDK build/watch·상태 보존·live session 제어/capture·MCP 연결.
- [ ] P01 제품 인수: 외부 사용자의 project 생성→작성→실행→수정/검증→저장/재개→배포 경로를 인수표 전체와 대조.

P01.1의 분담은 content/runtime/CLI-projection으로 나누고 Cargo.lock은 CLI 담당자 한 명이 수정한다. 코드 생성 전 공개 API를 공유하며 root는 실제 외부 directory로 독립 검증한다. DEV01/DEV05의 공통화가 실제 명령 경로에 사용되는지도 확인한다.

## P02 같은 project의 지속 실행과 공통 제어

[session service 계약](../03-architecture/session-control-service.md)을 먼저 고정한다. 아래는 실제로 같은 project/authority를 공유해야 하며 interface나 fixture facade 생성만으로 체크하지 않는다. P02 전체와 D1은 계속 미통과다.

- [x] P02.1a: typed operation/result·attachment별 소유권·admission/outcome credit·clock mode의 public API 고정.
- [x] P02.1b: 실제 ProjectSession의 generic host/client·manual/realtime·frame·stop/shutdown과 복수 attachment 검증.
- [x] P02.1c: 기존 fixture LiveSession의 공통 kernel 재사용·호환 검사, migration 중 임시 중복의 명시/제거.

- [x] P02.2a: 외부 project view와 같은 session의 JSONL 제어, 입력/창 종료/포화 실패 처리.
- [x] P02.2a.1: shared source·실제 tick 0·같은 actor의 native 창/JSONL·실제 Mac 입력과 정상 종료·단위 수준 포화/실패 검증.
- [x] P02.2a.2: 실제 process의 EOF 후 viewer 유지·의도적으로 막힌 stdout·manual F10/일반 입력과 reserved Step 경합 검증. native 22-check smoke와 별도 release physical 입력/report로 확인.
- [x] P02.2b: 불변 loaded generation의 CPU/GPU resource 재사용과 transform-only update 계측. 실제 source watch/reload·asset generation 교체는 P01.2/P03로 남는다.
- [ ] P02.3: live capture/조건/입력·실제 MCP·SDK attach·trace, 필요한 2D/UI/서버 연결까지 제품 인수.
- [ ] P02.3a: [같은 frame의 비동기 capture](../03-architecture/live-capture-and-artifacts.md), 공통 ticket·유한 artifact·실제 PNG/authority 대조. 현재 설계 리뷰 중.

P02.1a/b의 native library 로컬 검증은 수정 후 workspace CPU 158건·check/fmt/strict Clippy와 별도 기존 JSONL·managed·fixture window 회귀로 확인한다. 첫 PR CI의 회수 경쟁 수정과 재검사는 worklog에 별도 기록하며, 실패한 head를 병합하지 않는다. 그 P02.1 시점에는 legacy count-only result guard가 남아 있었다. 후속 P02.1c는 common credit lease와 FIFO 호환·unknown failure를 실제 구현하고 workspace CPU 200건으로 검증했다. 그 158건은 frontend 연결 전 근거이며 P02.2의 실제 창/JSONL 검증은 worklog에 별도로 둔다. MCP/SDK·P02 전체/D1은 계속 열려 있다. [구체 근거](../06-worklog/2026-10-16-product-readiness-audit.md).

P02.1c의 [결과 수명 계약](../03-architecture/legacy-session-result-lifetimes.md)은 공통 CreditPool/lease와 기존 FIFO API를 연결한다. fixture gameplay를 project actor에 합치지 않으며 panic 중 lost ID는 OutcomeUnknown으로 마감한다. 로컬 구현·회귀는 통과했으며 PR 필수 검사를 별도로 확인한다.

**P02 foundation exit**는 실제 외부 project 경로에서 P02.1a~c·P02.2a/b를 통과하고 asset 반복 제작에 필요한 control/capture consumer 계약을 고정한 상태다. 이 선행 gate를 통과해야 P03/W4 asset 통합에 착수한다. 이는 P02.3·P02 제품 인수·D1 통과가 아니다. foundation은 PR9 최종 head의 다섯 CI job과 main `66121d7` 병합까지 확인했다. P03.1은 아래 별도 계약으로 착수하며 P02 제품 인수는 계속 미통과다.

**P02 product acceptance**는 MCP/SDK attach·조건/입력·UI 조회·실제 capture·assert/trace/replay와 필요한 2D/UI/server 연결까지 모두 검증한 뒤에만 통과한다. 이 최종 증거는 P03/P04/P05/P08/P10의 기능도 필요하므로 해당 W4~W7 선행 구현 뒤 통합한다. P02.1c도 필수 조건이다. P01~P16은 제품 인수 영역 ID이고 숫자 순서의 별도 phase가 아니며, 실제 워터폴 단계는 W0~W8과 상위 D1→D2→D3다. 순환 의존성을 없애되 어떤 품질 gate도 삭제하거나 조기 통과시키지 않는다.


## P03 외부 에셋 import/cook와 실제 소비

[전체 설계](../03-architecture/asset-import-cook-and-residency.md)와 [P03.1 구현 계약](../03-architecture/asset-import-foundation.md)을 따른다. CPU import와 실제 화면/사운드 사용은 별도 증거다.

- [x] P03.1a: 불변 CPU IR·native/byte resolver·source/decoded/cooked 예산·versioned 유한 cache/pin/corruption/atomic publish.
- [x] P03.1b: glTF/GLB static model·PNG/mip·atlas·WAV 실제 data와 selective invalidation/cancel/진단.
- [x] P03.1c: 같은 backend의 project cook CLI·기존 smoke --assets·실제 공개 에셋 provenance/CPU 확인·locked BOM/필요 Wasm compile.
- [x] P03.1: 위 CPU 제작 경로 전체. 일부 포맷/메타데이터만으로 체크하지 않음.

P03.1 근거는 [작업 기록](../06-worklog/2026-10-16-asset-import.md)에 있다. source 고정 후 CPU240·Python53·assets31·실제 native22, 공개 원본/negative 진단과 clean local release archive/extracted assets31/native22를 확인했다. [PR10](https://github.com/sonan0721/Daedalus/pull/10)의 최종 head `1c4631c`에서 필수 다섯 CI·두 native packaged assets step을 확인하고 main `5c04db825436d28c39119f74aab6fcf1a55eb51e`에 병합했다. main과 검증한 PR head의 file tree도 동일하다. 이 CPU 하위 gate만 통과한 것이며 실제 consumer·P03 전체·D1/D2/D3는 아래와 같이 계속 미완료다.

- [ ] P03.2: 같은 project/catalog의 실제 model primitive·UV/texture/sampler·sprite/2D·audio consumer 연결과 렌더/출력 검증. [소비 계약 초안](../03-architecture/asset-runtime-consumers.md)의 a/b/c 경로를 각각 확정·검증하며 모두 통과해야 체크한다.
- [ ] P03.3: background import/cook·candidate/active resource generation·실행 중 변경/취소/stale·실패 last-good·CPU/GPU/audio 회수.
- [ ] P03.4: warm/nochange·PNG/buffer/atlas/WAV edit의 read/decode/cook/upload 수와 실제 반복 루프 비용·반응성 계측.

전체 P03/D1은 최종 제작 경로를 인수할 때만 통과한다. P05/P07/P09와 함께 필요한 2D/audio/animation 소비를 검증하며 P번호를 별도 숫자 순서 phase로 해석하지 않는다.
