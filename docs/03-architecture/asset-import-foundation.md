# P03.1 공통 에셋 import·cook 구현 계약

상태: public API/settings handoff 완료·구현 중·인수 전 · 2026-10-16 · P03/DEV03/DEV05

## 목적과 선행 근거

[전체 에셋 설계](asset-import-cook-and-residency.md)를 실제 Rust API·CLI·검증으로 연결한다. model/image/atlas/audio마다 resolver·hash·cache·진단을 다시 만드는 반복을 줄인다. P02 foundation은 PR9 main `66121d7676727b496d894d1c64a7fc6616a3551d`의 필수 검사·Mac/Linux package까지 통과했다. P02 전체·D1은 여전히 미통과다.

이번 범위는 **외부 project의 실제 CPU import/cook/catalog와 CLI**다. 다음 P03.2에서 model primitive·texture/sampler·sprite·audio를 실제 runtime/render/audio에 연결한다. source watch/live reload·general component binding·MCP는 별도다. 빈 interface나 모든 포맷을 Unsupported로 반환하는 구현을 이번 성과로 대신하지 않는다. 아래 지원 포맷은 실제 payload와 invalidation 검증을 필요로 한다.

## 저작 포맷과 지원 경계

scene asset descriptor는 기존 schema 1과 StableId/uri를 유지한다. 기존 `daedalus.mesh`·public ProjectMesh/ProjectVertex·PreparedProject.meshes()·renderer Vertex의 의미를 바꾸지 않는다. 신규 kind는 다음과 같다. 각 신규 metadata는 schema_version 1을 가진 strict typed settings이며 unknown 필드는 오류다. 생략 metadata에는 문서화한 기본 settings를 적용한다.

| kind / importer | 이번 실제 출력 | 오류로 진단할 요구 |
| --- | --- | --- |
| daedalus.model / gltf2-static-v1 | glTF/GLB static triangle·indexed/nonindexed 정규화·node hierarchy/local transform·primitive ordinal·POSITION/NORMAL/optional UV0·PBR scalar factor·base-color PNG·sampler | skin/animation/morph·sparse accessor·다른 primitive topology·없는 필수 normal/texture UV·JPEG/data URI·unsupported used/required extension·미지원 texture role/alpha/double-sided 요구 |
| daedalus.image / png-rgba8-v1 | PNG palette/gray/RGB/RGBA·tRNS 확장→RGBA8, 아래 명시 color policy·usage/sampler/mip 의미 | 16-bit/HDR/ICC·비지원 primaries 변환은 명시 unsupported. 알 수 없는 profile을 임의 삭제하지 않음 |
| daedalus.sprite_atlas / sprite-atlas-json-v1 | versioned JSON·declared image StableId·region StableId/pixel rect/pivot/pixels-per-unit·nearest/no-mip pixel policy | 경계 초과/중복 ID/잘못된 image kind·지원하지 않는 atlas filter/mip policy |
| daedalus.audio / wav-pcm-f32-v1 | WAV integer PCM 8/16/24/32 또는 float32→f32 mono/stereo·sample rate/frame count | 비지원 codec/channel/rate·잘린 payload·비유한 sample. finite float sample을 임의 clamp하지 않음 |

model은 default scene을 사용하고, default가 없으면 scene 1개인 경우만 자동 선택한다. 여러 scene이면 metadata의 명시 scene index를 요구한다. scene 없는 자료를 임의 identity node로 그리게 만들지 않는다. transform·index·accessor/count·material scalar·실제 attribute payload를 검증하고 노드 cycle/multiple-parent·depth 초과를 오류로 낸다. 미사용 정의의 보존과 실제 선택 scene roots를 구분한다.

base-color는 sRGB 의미, alpha는 linear 값이며 normal/data map을 color로 잘못 해석하지 않는다. UV 없는 geometry는 untextured material에만 허용한다. glTF의 material factor·UV·sampler 의미를 잃지 않는다. 첫 3D opaque color mip은 versioned linear-light box filter 뒤 sRGB encoding으로 생성한다. odd dimension·1×N·known-color를 검사한다. atlas는 nearest/no-mip으로 유지한다. 미지원 alpha-aware mip/재질을 조용히 근사하지 않는다.

PNG base decode의 color policy는 `rgba8-srgb-normalize-v2`다. sRGB chunk가 있으면 그 transfer를 우선하고 이미 encoded인 RGB를 유지한다. sRGB가 없고 유효한 양의 gAMA가 있으면 `linear = (sample/255)^(100000/gAMA_integer)`를 계산한 뒤 IEC sRGB의 piecewise transfer로 encode·round-to-nearest-u8한다. alpha에는 적용하지 않는다. cHRM은 없거나 sRGB 표준 좌표 `(31270,32900;64000,33000;30000,60000;15000,6000)`일 때만 지원하며, 없는 primaries는 기존 color-role 정책과 같이 sRGB primaries를 가정한다. gamma/색좌표가 모두 없을 때는 sRGB encoded color라는 저작 기본값을 사용한다. cHRM만 표준인 경우에도 같은 기본 transfer를 사용한다. 비표준 cHRM·ICC/cICP/HDR profile은 이번 경로에서 Unsupported다. 명시 sRGB가 있으면 gAMA/cHRM보다 우선하되 ICC/HDR 충돌을 허용하는 일반 CMS가 되지는 않는다. 이 의미는 [PNG 규격의 gAMA](https://www.w3.org/TR/png-3/#11gAMA)·[sRGB 우선순위](https://www.w3.org/TR/png-3/#11sRGB)를 근거로 한 엔진 정책이며, 전체 색관리 지원은 아니다.

8-bit RGB의256-entry LUT를 한 번 계산해 기존 RGBA buffer에 적용하며 전체 image를 추가 복사하지 않는다. row 취소와 예산을 유지한다. PNG memo/base-cache domain에 정책 v2를 반영해 이전 decoder-policy 산출물을 성공 hit로 쓰지 않는다. known gamma1.0의128→188, sRGB 태그의128 유지, alpha 유지, palette/tRNS·표준/비표준 cHRM·cold/warm parity를 검증한다. 실제 Kenney 원본의 gAMA45455를 삭제하거나 파일을 재저장해 성공 처리하지 않는다.

image leaf key는 width/height·layout·color/settings도 포함한다. 같은 RGBA byte sequence를 1×4와 2×2로 해석한 이미지는 다른 resource다. mip extent는 [wgpu-types 27.0.1 Extent3d](https://docs.rs/wgpu-types/27.0.1/wgpu_types/struct.Extent3d.html#method.mip_level_size)의 `max(1, base >> level)`과 일치해야 한다. 홀수 축을 ceil 절반으로 만들거나 edge texel을 버리지 않고 source footprint의 면적을 반영해 filtering한다. base 전체와 각 mip을 계산용으로 다시 clone하지 않으며 row/chunk 취소 지점을 유지한다.

sampler는 각 축 ClampToEdge/MirroredRepeat/Repeat, magnification nearest/linear, minification nearest/linear와 mip 없음/nearest/linear를 구분한다. 반복 여부 bool이나 두 filter 값만으로 합치지 않는다. 생략된 filtering의 엔진 기본값은 문서화하고, 명시된 값을 IR/key와 후속 GPU sampler에 보존한다. 이는 [glTF filtering/wrapping 규격](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html#samplers)의 구분이며 2026-10-16 API 리뷰에서 첫 표현의 손실을 찾아 보완했다.

### v1 저작 settings와 atlas JSON

빈 descriptor metadata는 schema 1의 아래 기본값을 사용한다. 비어 있지 않으면 `schema_version: 1`이 필요하며 모든 settings/source JSON은 unknown field를 거부한다. model의 `scene_index`는 선택 사항이고 audio/atlas descriptor에는 version 이외의 settings가 없다. image settings는 다음 형태다.

```json
{
  "schema_version": 1,
  "role": "sprite_atlas_srgb",
  "mip_policy": "none",
  "sampler": {
    "min_filter": "nearest",
    "mag_filter": "nearest",
    "mipmap_filter": "none",
    "wrap_u": "clamp_to_edge",
    "wrap_v": "clamp_to_edge"
  }
}
```

role은 `base_color_srgb`/`sprite_atlas_srgb`, mip policy는 `none`/`linear_light_box_v1`, min/mag는 `nearest`/`linear`, mipmap filter는 `none`/`nearest`/`linear`, wrap은 `clamp_to_edge`/`mirrored_repeat`/`repeat`다. 기본값은 base color, no mip, linear min/mag, no mip filter, repeat 양 축이다. sprite role에는 nearest min/mag와 no-mip을 명시해야 한다. mip policy none과 mip filter none은 함께 사용하고, mip filter를 켜면 지원하는 mip policy를 요구한다. role만 바꾸었다고 sampler를 조용히 바꾸지 않는다. glTF model-owned image의 sampler는 model reference의 명시된 값을 보존한다.

atlas **source file**은 아래 형태이며 rect는 pixel 단위 `[x, y, width, height]`다. descriptor 순서와 관계없이 image를 먼저 준비하고 atlas가 image kind/role/sampler/dimensions를 검사한다. 참조 image의 source/dependency identity도 atlas의 dependency 기록에 포함한다.

```json
{
  "schema_version": 1,
  "image_asset": "tile_image",
  "filter_policy": "nearest_no_mip_v1",
  "regions": [
    { "id": "stone", "rect": [0, 0, 16, 16], "pivot": [0.5, 0.5], "pixels_per_unit": 16.0 }
  ]
}
```

이 예시는 새 source schema의 계약이다. project descriptor의 StableId/uri wrapper를 대신하지 않으며, 코드/CLI 검증 전부터 사용 가능하다고 표시하지 않는다.

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
| engine-owned transient buffers | 동시에 살아 있는 parser copy·decoder/output 변환·cache codec buffer 합계 128 MiB + 64 KiB |
| disk cache | 512 MiB / 1,024 entries, active/in-flight pin 회수 전 삭제 금지 |

source·decoded·cooked·expanded instance·GPU residency는 서로 다른 예산이다. batch catalog ceiling은 동시에 존재하는 임의 개수의 catalog나 process RSS 한도가 아니다. P03.3 runtime admission이 active/candidate/in-flight 세대 전체를 별도 제한한다. 큰 allocation 전에 count/length/stride/row/sample 계산을 checked arithmetic으로 확인한다. decoder 내부 limit만 믿지 않는다. cancellation은 파일/primitive/row/sample chunk 경계에서 확인하고, 단일 foreign parser 호출의 강제 선점 보장은 하지 않는다.

transient 한도는 `ImportLimits.max_parser_scratch_bytes`로 표현한다. 초기 API의 GLB-copy 중심 32 MiB 안은 PNG/캐시 buffer까지 적용하면 허용한 64 MiB RGBA를 직렬화할 수 없어 payload buffer 128 MiB와 framing 여유 64 KiB로 보완했다. Vec→Arc 변환에서 두 buffer가 동시에 존재하는 peak도 계수하며, header 88 bytes만으로 단일 4096² PNG가 실패하도록 두지 않는다. 실제 GLB BIN copy는 여전히 source32 MiB 이하이며 실제 크기만 계수한다. decoder 변환과 cache envelope를 동시에 불필요하게 보관하지 않는다. foreign parser 내부 allocator나 process RSS 전체가 이 값 이하라는 보장은 아니다. 각 축/개별 resource ceiling을 만족해도 동시 aggregate/transient 한도를 넘으면 위치 있는 LimitExceeded로 실패한다.

## Resolver·cache·무효화

URI는 참조 파일 directory 기준으로 normalized project-relative ID가 된다. absolute/.. escape/scheme/backslash/NUL·모호한 percent encoding·root 밖 symlink를 거부한다. 동일 source는 한 번 읽고 Arc/hash/길이를 공유한다. resolver가 max_bytes보다 큰 값을 돌려줘도 core가 다시 검사한다. native adapter는 기존 contained path/read 제한을 재사용하고 byte adapter와 같은 IR/key/오류를 만든다.

ImportKey는 container/source/parser와 실제 dependency identity를 사용한다. LeafCookKey는 domain/cooker version·IR/layout·settings/target와 **해당 출력이 소비한 payload/descriptor**를 사용한다. GLB 전체 hash를 geometry leaf에 넣지 않는다. source 재해석과 geometry/image decode/cook·GPU upload를 별도 계측한다. PNG base decode는 같은 encoded payload/decoder settings에서 재사용하고 role/mip variant 때문에 불필요하게 다시 decode하지 않는다.

CacheRead는 Miss/검증한 Hit/Corrupt를 구분한다. version/key/kind/declared length/payload SHA를 검사한 뒤에만 hit다. `cache/v1/import/<key>`와 `cache/v1/cook/<key>`처럼 engine-owned namespace 아래 bounded envelope를 쓴다. metadata는 bounded text, 큰 숫자/픽셀/PCM 배열은 명시 endian binary로 저장한다. 임시 file→flush→atomic rename으로 게시하며 취소/실패 partial은 정상 entry가 아니다. 이 재생성 가능한 cache를 world-save 내구성으로 홍보하지 않는다.

동시 같은-key publish·손상/잘린 file·oversize·cache root 안의 unrelated file을 다룬다. 같은 key의 불일치 payload를 조용히 덮어쓰지 않는다. count/bytes eviction과 pin은 유한하고 failed/cancelled catalog는 이전 complete catalog를 교체하지 않는다. mtime은 cache 성공 근거가 아니다. 최초 cache 구현은 cross-process lease를 지원하지 않으면 exclusive cache ownership을 명시적으로 검사해 동시 사용을 거부한다. 이를 무제한 기다리는 file lock으로 해결하지 않는다.

## CLI와 실제 제작 경로

`daedalus project cook <directory> [--cache-dir <directory>]`가 같은 content pipeline을 사용한다. 기본 cache는 project의 `.daedalus/cache` 아래 versioned engine namespace다. source를 수정하지 않으며 cache 경로/지원 포맷·runtime 미연결을 report한다. 기존 validate/run/serve/view/capture의 legacy 의미를 몰래 바꾸지 않는다. P03.2가 새 catalog를 실제 consumer에 연결하기 전에는 glTF 표시·sprite 재생·WAV 출력 지원이라고 홍보하지 않는다.

JSON summary는 source/catalog identity, asset kind/leaf identity·geometry/image/atlas/audio counts, read/parse/hash/decode/cook/hit/corrupt 수·현재 bytes를 담고 raw payload를 stdout으로 출력하지 않는다. 합계·asset별 작은 summary를 1 MiB 이내로 제한하고 더 큰 상세는 명시 paging/한도 오류로 처리한다. 실패는 typed code·source·JSON/accessor path·dependency chain을 가진 진단과 nonzero exit다. CLI는 별도 importer/cache 구현을 만들지 않는다.

기존 `scripts/smoke_project.py`의 opt-in `--assets`를 확장해 같은 subprocess/temporary-project/binary-hash/cleanup을 재사용한다. 기본 CPU18와 native --view의 의미는 유지한다. 신규 smoke는 네 포맷을 실제 import하고 no-change·외부/embedded PNG edit·geometry edit·atlas metadata edit·WAV edit·corruption/실패에서 expected reuse를 비교한다. 외부 네트워크 다운로드는 smoke 안에서 하지 않는다. root의 별도 실제 공개 에셋 검증은 원출처/license/hash를 보존한다.

기존 Mac/Linux release-package CI job은 package build 뒤 동일 helper의 `--assets`를 staged binary에 명시 실행한다. 추가 Cargo build나 별도 workflow를 만들지 않는다. package backend의 기본 CPU18/archive 검증과 tag/publish 정책은 유지한다. 로컬 인수에서는 추출한 archive binary에도 같은 assets smoke를 별도 실행해 개발 binary와 구분한다. 이는 구현 계약이며 실제 실행 결과는 worklog에 추가한다.

## 소유권과 인수

| Luna xhigh 담당 | 소유 파일 | 공유·검사 |
| --- | --- | --- |
| content core | assets/mod·types·source·cache와 해당 tests, content project.rs/lib.rs/Cargo.toml·Cargo.lock | 먼저 immutable IR/driver context/entrypoint 공유. 단독 Cargo target/lock owner |
| image/atlas/audio driver·공통 codec | assets/importers/mod·image·atlas·audio·binary와 해당 tests | 세 포맷/shared image cook·checked binary codec 소유. glTF 파일/core/Cargo 편집·실행 금지 |
| glTF driver | assets/importers/gltf_static.rs·gltf_static_tests.rs | 다른 담당자의 미착수/소유권 이전 확인 후 병렬 착수. 같은 context·extensionless image cook·codec 재사용, Cargo 금지 |
| CLI/automation | CLI main/project/assets 모듈과 해당 tests, smoke_project.py/test_smoke_project.py, release.yml의 명시 asset smoke step | 실제 entrypoint를 사용. Cargo는 core handback 뒤 실행 |

root는 문서·공식 자료/에셋 provenance·source 리뷰·독립 통합 검증·commit/PR를 맡는다. manifest/lock과 공유 target은 한 번에 한 명만 소유한다. unresolved API 문제는 구현 전에 공유하고 placeholder 성공으로 넘기지 않는다.

P03.1은 native/bytes parity·네 포맷 실제 data·GLB/accessor/URI/aggregate 한도·필요 invalidation·corrupt/atomic/cache pin·취소·CLI smoke를 모두 통과한 뒤 체크한다. 특히 embedded PNG edit의 container parse 증가는 허용하되 geometry cook 0·image 변경을 검증한다. node transform/material 변경도 geometry를 다시 cook하지 않아야 한다. 실제 Mac CPU/필요한 Wasm compile·dependency notice/locked graph를 각각 기록한다. Wasm compile은 browser 실제 실행을 뜻하지 않는다.

P03.1 인수는 P03 전체/D1 또는 두 게임의 품질 인수가 아니다. 다음 실제 textured model·sprite/audio 소비와 reload·2D/3D 품질 검증은 그대로 열린다.
