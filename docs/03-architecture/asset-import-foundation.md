# P03.1 공통 에셋 import·cook 구현 계약

상태: 구현 전 public API handoff · 2026-10-16 · P03/DEV03/DEV05

## 목적과 선행 근거

[전체 에셋 설계](asset-import-cook-and-residency.md)를 실제 Rust API·CLI·검증으로 연결한다. model/image/atlas/audio마다 resolver·hash·cache·진단을 다시 만드는 반복을 줄인다. P02 foundation은 PR9 main `66121d7676727b496d894d1c64a7fc6616a3551d`의 필수 검사·Mac/Linux package까지 통과했다. P02 전체·D1은 여전히 미통과다.

이번 범위는 **외부 project의 실제 CPU import/cook/catalog와 CLI**다. 다음 P03.2에서 model primitive·texture/sampler·sprite·audio를 실제 runtime/render/audio에 연결한다. source watch/live reload·general component binding·MCP는 별도다. 빈 interface나 모든 포맷을 Unsupported로 반환하는 구현을 이번 성과로 대신하지 않는다. 아래 지원 포맷은 실제 payload와 invalidation 검증을 필요로 한다.

## 저작 포맷과 지원 경계

scene asset descriptor는 기존 schema 1과 StableId/uri를 유지한다. 기존 `daedalus.mesh`·public ProjectMesh/ProjectVertex·PreparedProject.meshes()·renderer Vertex의 의미를 바꾸지 않는다. 신규 kind는 다음과 같다. 각 신규 metadata는 schema_version 1을 가진 strict typed settings이며 unknown 필드는 오류다. 생략 metadata에는 문서화한 기본 settings를 적용한다.

| kind / importer | 이번 실제 출력 | 오류로 진단할 요구 |
| --- | --- | --- |
| daedalus.model / gltf2-static-v1 | glTF/GLB static triangle·indexed/nonindexed 정규화·node hierarchy/local transform·primitive ordinal·POSITION/NORMAL/optional UV0·PBR scalar factor·base-color PNG·sampler | skin/animation/morph·다른 primitive topology·없는 필수 normal/texture UV·JPEG/data URI·unsupported used/required extension·미지원 texture role/alpha/double-sided 요구 |
| daedalus.image / png-rgba8-v1 | PNG palette/gray/RGB/RGBA·tRNS 확장→RGBA8, usage/sampler/mip 의미 | 16-bit/HDR/특수 color-profile 변환은 첫 버전에서 명시 unsupported. 알 수 없는 gamma 보정으로 색을 바꾸지 않음 |
| daedalus.sprite_atlas / sprite-atlas-json-v1 | versioned JSON·declared image StableId·region StableId/pixel rect/pivot/pixels-per-unit·nearest/no-mip pixel policy | 경계 초과/중복 ID/잘못된 image kind·지원하지 않는 atlas filter/mip policy |
| daedalus.audio / wav-pcm-f32-v1 | WAV integer PCM 8/16/24/32 또는 float32→f32 mono/stereo·sample rate/frame count | 비지원 codec/channel/rate·잘린 payload·비유한 sample. finite float sample을 임의 clamp하지 않음 |

model은 default scene을 사용하고, default가 없으면 scene 1개인 경우만 자동 선택한다. 여러 scene이면 metadata의 명시 scene index를 요구한다. scene 없는 자료를 임의 identity node로 그리게 만들지 않는다. transform·index·accessor/count·material scalar·실제 attribute payload를 검증하고 노드 cycle/multiple-parent·depth 초과를 오류로 낸다. 미사용 정의의 보존과 실제 선택 scene roots를 구분한다.

base-color는 sRGB 의미, alpha는 linear 값이며 normal/data map을 color로 잘못 해석하지 않는다. UV 없는 geometry는 untextured material에만 허용한다. glTF의 material factor·UV·sampler 의미를 잃지 않는다. 첫 3D opaque color mip은 versioned linear-light box filter 뒤 sRGB encoding으로 생성한다. odd dimension·1×N·known-color를 검사한다. atlas는 nearest/no-mip으로 유지한다. 미지원 alpha-aware mip/재질을 조용히 근사하지 않는다.

## CPU API와 불변 자료

새 `daedalus_content::assets`는 renderer/audio/task runtime을 의존하지 않는다. SourceResolver와 native/byte-map adapter, ImportLimits·Cancellation·located AssetImportError, immutable catalog와 cache를 제공한다. cancellation은 외부 task token을 adapter로 연결할 수 있어야 하며 tick/window callback에서 동기 import를 실행하지 않는다.

예정 entrypoint는 `prepare_project_with_assets(manifest_bytes, scene_bytes, resolver, cache, limits, cancellation)`와 `load_project_with_assets(root, cache, limits, cancellation)`다. PreparedProject에 private catalog와 immutable accessor를 추가한다. 기존 prepare/load signature와 legacy-only 출력/오류는 유지한다. 신규 pipeline의 native/byte 경로는 같은 parser와 digest를 사용한다. 정확한 Rust signature와 format-driver context는 core 담당자가 공유·리뷰한 뒤 dependent 코드를 작성한다.

- Catalog: StableId→typed asset, unique cooked-resource key→Arc payload, source identities/dependency edges, active import settings/version·catalog digest·통계. 큰 payload를 asset ID나 node마다 복제하지 않는다.
- StaticModel: ordered node parent/local transform·selected roots·mesh/primitive reference, primitive ordinal·material slot, immutable position/normal/UV/index resource handle. source IR와 GPU packing을 같은 public Vertex로 고정하지 않는다.
- Material/image: scalar PBR factor·image ref·sampler, width/height·RGBA8/mip levels·role/color encoding/filter version. model-owned image identity는 owner StableId+image index로 구분하고 실제 PNG payload는 hash로 공유한다.
- Atlas: image asset reference와 bounded region metadata. PCM: immutable interleaved samples·rate/channels/frame count.

payload는 생성 시 검증하고 immutable getter/Arc로 공유한다. hash를 만든 뒤 외부가 배열을 바꿔 cache identity를 위조할 수 없게 한다. metadata clone과 resource ownership을 구분한다. `.validate()`는 catalog/descriptor/resource 참조 일치를 재검증하며 renderer가 없는 자료에 runtime adapter가 있는 것처럼 응답하지 않는다.

## 수치 계약

초기 native/byte import의 hard ceiling은 아래와 같다. 기존 ProjectLimits를 몰래 늘리지 않고 신규 ImportLimits에 별도로 표현한다. 낮은 사용자 설정은 허용하되 0/overflow/상호 모순은 시작 전에 거부한다.

| 항목 | ceiling |
| --- | --- |
| 기존 manifest/scene/legacy mesh | 64 KiB / 16 MiB / 8 MiB 유지 |
| 모든 source 합계 / 새 imported file | 64 MiB / 32 MiB |
| declared assets / unique sources | 256 / 1,026 (manifest/scene 포함) |
| source ID / dependency depth | 4,096 UTF-8 bytes / 256 |
| unique geometry vertex/index | 1,000,000 / 3,000,000 |
| model nodes / primitives / materials / decoded images | 전체 catalog 16,384 / 4,096 / 1,024 / 256 |
| image axes / decoded base RGBA 합계 | 4,096 / 64 MiB |
| atlas regions | 전체 16,384 |
| audio | mono/stereo, 8~384 kHz, 10,000,000 frames 이하, aggregate decoded/cooked bytes에도 계수 |
| CPU cooked catalog | unique payload+bounded metadata의 보수적 합계 128 MiB (mips 포함) |
| disk cache | 512 MiB / 1,024 entries, active/in-flight pin 회수 전 삭제 금지 |

source·decoded·cooked·expanded instance·GPU residency는 서로 다른 예산이다. batch catalog ceiling은 동시에 존재하는 임의 개수의 catalog나 process RSS 한도가 아니다. P03.3 runtime admission이 active/candidate/in-flight 세대 전체를 별도 제한한다. 큰 allocation 전에 count/length/stride/row/sample 계산을 checked arithmetic으로 확인한다. decoder 내부 limit만 믿지 않는다. cancellation은 파일/primitive/row/sample chunk 경계에서 확인하고, 단일 foreign parser 호출의 강제 선점 보장은 하지 않는다.

## Resolver·cache·무효화

URI는 참조 파일 directory 기준으로 normalized project-relative ID가 된다. absolute/.. escape/scheme/backslash/NUL·모호한 percent encoding·root 밖 symlink를 거부한다. 동일 source는 한 번 읽고 Arc/hash/길이를 공유한다. resolver가 max_bytes보다 큰 값을 돌려줘도 core가 다시 검사한다. native adapter는 기존 contained path/read 제한을 재사용하고 byte adapter와 같은 IR/key/오류를 만든다.

ImportKey는 container/source/parser와 실제 dependency identity를 사용한다. LeafCookKey는 domain/cooker version·IR/layout·settings/target와 **해당 출력이 소비한 payload/descriptor**를 사용한다. GLB 전체 hash를 geometry leaf에 넣지 않는다. source 재해석과 geometry/image decode/cook·GPU upload를 별도 계측한다. PNG base decode는 같은 encoded payload/decoder settings에서 재사용하고 role/mip variant 때문에 불필요하게 다시 decode하지 않는다.

CacheRead는 Miss/검증한 Hit/Corrupt를 구분한다. version/key/kind/declared length/payload SHA를 검사한 뒤에만 hit다. `cache/v1/import/<key>`와 `cache/v1/cook/<key>`처럼 engine-owned namespace 아래 bounded envelope를 쓴다. metadata는 bounded text, 큰 숫자/픽셀/PCM 배열은 명시 endian binary로 저장한다. 임시 file→flush→atomic rename으로 게시하며 취소/실패 partial은 정상 entry가 아니다. 이 재생성 가능한 cache를 world-save 내구성으로 홍보하지 않는다.

동시 같은-key publish·손상/잘린 file·oversize·cache root 안의 unrelated file을 다룬다. 같은 key의 불일치 payload를 조용히 덮어쓰지 않는다. count/bytes eviction과 pin은 유한하고 failed/cancelled catalog는 이전 complete catalog를 교체하지 않는다. mtime은 cache 성공 근거가 아니다. 최초 cache 구현은 cross-process lease를 지원하지 않으면 exclusive cache ownership을 명시적으로 검사해 동시 사용을 거부한다. 이를 무제한 기다리는 file lock으로 해결하지 않는다.

## CLI와 실제 제작 경로

`daedalus project cook <directory> [--cache-dir <directory>]`가 같은 content pipeline을 사용한다. 기본 cache는 project의 `.daedalus/cache` 아래 versioned engine namespace다. source를 수정하지 않으며 cache 경로/지원 포맷·runtime 미연결을 report한다. 기존 validate/run/serve/view/capture의 legacy 의미를 몰래 바꾸지 않는다. P03.2가 새 catalog를 실제 consumer에 연결하기 전에는 glTF 표시·sprite 재생·WAV 출력 지원이라고 홍보하지 않는다.

JSON summary는 source/catalog identity, asset kind/leaf identity·geometry/image/atlas/audio counts, read/parse/hash/decode/cook/hit/corrupt 수·현재 bytes를 담고 raw payload를 stdout으로 출력하지 않는다. 합계·asset별 작은 summary를 1 MiB 이내로 제한하고 더 큰 상세는 명시 paging/한도 오류로 처리한다. 실패는 typed code·source·JSON/accessor path·dependency chain을 가진 진단과 nonzero exit다. CLI는 별도 importer/cache 구현을 만들지 않는다.

기존 `scripts/smoke_project.py`의 opt-in `--assets`를 확장해 같은 subprocess/temporary-project/binary-hash/cleanup을 재사용한다. 기본 CPU18와 native --view의 의미는 유지한다. 신규 smoke는 네 포맷을 실제 import하고 no-change·외부/embedded PNG edit·geometry edit·atlas metadata edit·WAV edit·corruption/실패에서 expected reuse를 비교한다. 외부 네트워크 다운로드는 smoke 안에서 하지 않는다. root의 별도 실제 공개 에셋 검증은 원출처/license/hash를 보존한다.

## 소유권과 인수

| Luna xhigh 담당 | 소유 파일 | 공유·검사 |
| --- | --- | --- |
| content core | assets/mod·types·source·cache와 해당 tests, content project.rs/lib.rs/Cargo.toml·Cargo.lock | 먼저 immutable IR/driver context/entrypoint 공유. 단독 Cargo target/lock owner |
| format drivers | assets/importers/**와 해당 tests | core API 합의 후 PNG/glTF/atlas/WAV 구현. core files/Cargo 편집·실행 금지, 담당자에게 focused 요청 |
| CLI/automation | CLI main/project/assets 모듈과 해당 tests, smoke_project.py/test_smoke_project.py | 실제 entrypoint를 사용. Cargo는 core handback 뒤 실행 |

root는 문서·공식 자료/에셋 provenance·source 리뷰·독립 통합 검증·commit/PR를 맡는다. manifest/lock과 공유 target은 한 번에 한 명만 소유한다. unresolved API 문제는 구현 전에 공유하고 placeholder 성공으로 넘기지 않는다.

P03.1은 native/bytes parity·네 포맷 실제 data·GLB/accessor/URI/aggregate 한도·필요 invalidation·corrupt/atomic/cache pin·취소·CLI smoke를 모두 통과한 뒤 체크한다. 특히 embedded PNG edit의 container parse 증가는 허용하되 geometry cook 0·image 변경을 검증한다. node transform/material 변경도 geometry를 다시 cook하지 않아야 한다. 실제 Mac CPU/필요한 Wasm compile·dependency notice/locked graph를 각각 기록한다. Wasm compile은 browser 실제 실행을 뜻하지 않는다.

P03.1 인수는 P03 전체/D1 또는 두 게임의 품질 인수가 아니다. 다음 실제 textured model·sprite/audio 소비와 reload·2D/3D 품질 검증은 그대로 열린다.
