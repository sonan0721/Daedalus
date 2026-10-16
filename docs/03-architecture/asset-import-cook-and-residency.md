# 외부 에셋 import·cook·resource generation

상태: P03 전체 설계·P03.1/P03.2a·b 인수, imported audio c의 배포 인수 진행 중 · 2026-10-16 · D1 제품 인수 미통과

## 만든 이유와 현재 경계

게임마다 texture/model/sprite/audio loader와 reload 처리를 다시 만드는 반복을 없앤다. AI가 편집하는 scene/설정/참조는 텍스트로 유지하고, 고밀도 geometry·image·audio는 binary 원본과 cooked payload로 다룬다. [project lifecycle](project-lifecycle.md)의 schema-1 JSON mesh는 외부 자료 실행의 첫 기반이며 일반 에셋 파이프라인을 대체하지 않는다.

이 문서는 [기존 제작 개요](assets-ui-animation.md)를 import·dependency·cache·게시 계약으로 상세화한다. 아래 포맷/의존성은 조사에 근거한 첫 구현안이다. 실제 Cargo 채택·Mac/웹 빌드·GPU 표시·반복 reload를 통과하기 전에는 지원 기능으로 표시하지 않는다. 고급 재질·skin/animation·streaming과 두 게임의 품질 요구는 그대로 제품 gate에 남는다.

## 실제 기준선과 조사

착수 전 감사의 lock은 wgpu 27.0.1, png 0.18.1, sha2 0.10.9, Kira 0.12.5를 포함했고 `gltf`, `image`, `hound`, Symphonia는 없었다. 당시 content의 source digest에는 입력 file identity만 있었고 importer/settings/target/dependency graph는 없었다. P03.1 구현 branch에는 gltf1.4.1·hound3.5.1을 추가했고 첫 content library compile을 확인했다. 최종 채택/인수 상태는 [의존성 기준선](../02-research/implementation-dependency-baseline.md)과 [작업 기록](../06-worklog/2026-10-16-asset-import.md)을 따른다. 아래 후보 표는 착수 전 조사 이력이다.

renderer의 PNG capture와 single-mip render target은 texture asset import/mipmap 지원의 근거가 아니다. Kira는 defaults 없이 `cpal`만 켰고, hound CPU decode와 실제 AudioService 소비는 P03.1/P03.2의 다른 경계다.

공식 자료 확인일은 2026-10-16이다. Luna xhigh가 read-only로 조사했고, 아래는 source 문서의 확인 내용과 다이달로스 제안을 구분한다.

| 근거 | 확인 사실 | 적용안·검증 한계 |
| --- | --- | --- |
| [glTF 2.0 규격](https://registry.khronos.org/glTF/specs/2.0/glTF-2.0.html) | 외부 buffer/image, data URI, GLB binary/image, PNG/JPEG 표현을 정의 | 첫 구현은 지원 subset을 명시하고 다른 기능은 진단. 규격 전체 지원으로 홍보하지 않음 |
| [gltf 1.4.1 manifest](https://docs.rs/crate/gltf/1.4.1/source/Cargo.toml), [reader](https://docs.rs/gltf/1.4.1/gltf/mesh/struct.Reader.html) | MIT OR Apache-2.0, Rust 1.61 선언, `utils` reader. default `import`는 base64/urlencoding/image를 활성화 | 후보 `=1.4.1`, defaults off, `utils` 중심. names 필요 시 명시. 실제 locked 빌드·새 의존성 notice는 채택 때 검증 |
| [gltf import API](https://docs.rs/gltf/1.4.1/gltf/fn.import.html) | 편의 importer가 연결된 파일도 읽음 | project의 경로/byte 정책을 소유한 resolver와 `Gltf::from_slice` 사용안. 외부 importer에 무제한 filesystem 접근 위임하지 않음 |
| [png 0.18.1 Decoder](https://docs.rs/png/0.18.1/png/struct.Decoder.html) | decoder limits와 output buffer API. caller allocation은 decoder 제한 밖 | 기존 png 재사용, dimension/decoded byte를 엔진이 먼저 계산·제한. best-effort decoder limit만 믿지 않음 |
| [hound 3.5.1 manifest](https://docs.rs/crate/hound/3.5.1/source/Cargo.toml), [API](https://docs.rs/hound/3.5.1/hound/) | Apache-2.0, runtime dependency 없음, PCM/32-bit float WAV 경로 | 후보 `=3.5.1`; bounded decode와 sample 변환은 엔진 책임. 현재 미채택 |
| [Kira 0.12.5 sound](https://docs.rs/kira/0.12.5/kira/sound/index.html), [features](https://docs.rs/crate/kira/0.12.5/features) | static/streaming 경로 구분, decoder feature 선택 | PCM을 기존 service에 전달하는 첫 경로. 장시간 음악 스트리밍·web audio는 별도 검증 |
| [wgpu 27.0.1 TextureDescriptor](https://docs.rs/wgpu/27.0.1/wgpu/type.TextureDescriptor.html), [TextureFormat](https://docs.rs/wgpu/27.0.1/wgpu/enum.TextureFormat.html) | 명시적 mip level 수와 linear/sRGB format | color/data map 의미와 mip 정책을 cook key에 포함. descriptor만으로 mip가 생성되지 않음 |

현재 renderer는 wgpu defaults를 끄고 `webgpu`를 활성화하지 않았다. byte 기반 CPU import를 마련하는 것과 웹 renderer/runtime 완성은 구분한다. 브라우저 import/load의 제한·Worker·fetch·공유 메모리와 native filesystem adapter를 분리하고, 채택 시 필요한 target compile/conformance를 기록한다.

## 첫 포맷 계약과 전체 확장 경로

| 자료 | 첫 구현 후보 | 이후 필수 확장·미지원 진단 |
| --- | --- | --- |
| 3D model | `.gltf/.glb` static triangle primitive, indexed/non-indexed를 정규화, POSITION/NORMAL/TEXCOORD_0, node transform, base-color factor/PNG texture | skin/clip·morph·LOD·tangent/normal·metallic/roughness·occlusion/emissive·alpha·extensions는 개별 capability. 조용히 삭제/대체 금지 |
| image | PNG→RGBA8, bounded variants·dimensions, color-space/sampler/mip settings | JPEG·compressed GPU textures·HDR·KTX2/Basis 등은 별도 adapter/target gate |
| sprite | PNG와 versioned atlas JSON: stable region ID, pixel rect, pivot, pixels-per-unit, sampler/mip policy | animation clip·9-slice·tile atlas·UI integration은 P08/P09/P13와 연결. PNG decode만으로 2D renderer 완료 처리 금지 |
| audio | WAV integer PCM/32-bit float→bounded engine CPU samples | compressed/streaming·spatial·bus/DSP·device recovery는 P07. file parse만으로 사운드 재생 완료 처리 금지 |

원본 exporter 지원 범위는 실제 외부 에셋으로 확인한다. 첫 subset은 기능을 영구 제외하는 결정이 아니다. custom material/shader·animation importer도 같은 registry와 dependency 계약을 사용하게 한다. API 설계 시 skin/tangent/UV·material slot을 버려 확장이 불가능한 단일 packed vertex를 공통 저작 포맷으로 고정하지 않는다.

glTF의 기본 첫 resolver는 root 안 외부 `.bin`/PNG와 GLB buffer-view PNG를 처리하는 안이다. data URI·JPEG·compressed/required extension은 명시적 unsupported 오류를 낸다. 사용 에셋이 요구하면 별도 bounded decoder를 추가한다. 단순 검증에서 extension을 허용한 뒤 실행 때 누락하는 불일치를 만들지 않는다.

## 소유자와 자료 흐름

| 계층 | 소유 자료/역할 | 출력 |
| --- | --- | --- |
| source adapter | native canonical 경로 또는 web fetch 응답의 bounded bytes, normalized source ID | immutable source blob·SHA-256·진단 |
| content importer | format parser·dependency resolver·schema/semantic 검증 | versioned CPU intermediate representation와 dependency edges |
| cooker/cache | target/settings별 변환·hash·atomic cache publication | CPU cooked blob·metadata·dependency manifest |
| project generation | scene/material/asset의 정합한 후보 세대·요청 token | validated immutable generation, old→new 교체 계획 |
| simulation/control | stable asset handle·generation commit·사용자 결과 | authority revision/tick과 적용/실패 결과 |
| render/audio adapter | cooked payload의 장치 자원 준비·수명·예산 | device-local resource handle·ready/failure |

import/cook/파일 I/O를 window callback이나 고정 simulation tick 안에서 동기 실행하지 않는다. CPU decode와 blocking I/O는 해당 실행 영역/예산으로 분리한다. 결과는 유한 credit·cancellation·project generation을 가져야 한다. 처음부터 모든 확장형 plugin ABI를 만들지는 않고, 소유권과 registry 의미를 먼저 고정한다.

## source resolver와 입력 예산

URI는 참조한 asset의 directory 기준으로 해석한 뒤 project-relative ID로 정규화한다. project root 바깥 경로·absolute·URI scheme·외부 symlink를 거부하고 remote 자산은 자동 다운로드하지 않는다. 첫 버전은 모호한 percent-encoded 경로를 거부한다. 후속 percent decoding을 지원하면 decode 후 같은 검증을 적용한다.

glTF/GLB 자체, 외부 buffer/image, metadata, decoded output 각각의 상한과 전체 source/file/vertex/index/primitive/node/image/sample 예산을 정한다. GLB header/version/declared length/chunk 범위와 accessor offset/stride/count를 checked arithmetic으로 확인하고 output allocation보다 먼저 검사한다. decoder 내부 제한과 엔진 전체 제한을 합쳐 사용한다. 중복 source는 한 번 읽고 source hash를 공유한다.

CPU budget과 GPU expanded-instance/upload/residency budget은 다르다. 작은 mesh를 많은 node가 참조하는 경우도 별도로 제한하며, instance마다 geometry를 복제하는 초기 경로를 제품 상한으로 고정하지 않는다. loader 진단에는 source ID, dependency chain, node/primitive/accessor 또는 JSON path와 실제 제한 원인을 남긴다.

## identity·증분 무효화

`project source digest`, `CPU cook key`, `GPU resource key`를 구분한다. 기존 input digest를 import cache key로 이름만 바꾸지 않는다.

- import/parse key: container source bytes·parser/importer version·실제 읽은 dependency identity. `.gltf/.glb` 변경을 다시 해석해야 하는지 판정한다.
- leaf CPU cook key: importer/cooker ID/version·IR/layout schema·settings·해당 출력이 실제 소비한 descriptor/payload 또는 canonical IR hash·target 변환 정책. payload와 manifest를 검증한 뒤 atomic publish한다.
- GPU key: cooked payload hash·resource layout version·format/color-space/mip 정책. device resident handle은 장치 세대와 수명을 별도로 가진다. sampler는 독립 key로 공유할 수 있다.
- Project generation: scene/source digest와 실제 채택된 asset cook identity 집합. 개별 asset reload와 scene authority command의 revision을 혼동하지 않는다.

image/audio/geometry의 변환 경계를 분리한다. PNG-only 수정은 필요한 image와 material generation을 갱신하되 mesh position/index를 다시 cook하지 않는다. atlas rect 변경은 image decode를 반복하지 않는다. WAV 변경은 geometry key를 바꾸지 않는다. `.bin`/GLB geometry·importer version·설정·의존성 제거/경로 변경은 영향받는 결과를 반드시 무효화한다. mtime만으로 cache hit를 판정하지 않는다.

whole GLB source hash를 모든 geometry leaf key에 넣으면 embedded PNG만 바꿔도 geometry를 다시 cook하는 모순이 생긴다. container 재해석과 leaf 재사용을 분리한다. geometry는 실제 소비한 accessor descriptor/attribute/index payload를 key로 삼고 무관한 image bytes·node instance transform·material binding을 제외한다. image는 외부/embedded 위치와 관계없이 실제 encoded payload와 image settings로 판정한다. atlas rect 검증은 해당 image의 dimensions를 확인하되 rect-only 변경으로 PNG를 다시 decode하지 않는다. source 재해석·range 해시·leaf cook·GPU upload count를 각각 기록하며 전체가 O(1)이라고 주장하지 않는다.

cache corruption·누락 dependency·취소·실패 산출물은 성공 hit가 아니다. cache 파일과 resource는 포맷/version/hash/길이를 검증하며 budget에 따른 정리 정책을 둔다. warm no-change의 재사용 효과와 잘못된 재사용을 함께 측정한다.

## 색공간·mip·재질 정확성

base color/emissive 같은 color map과 normal/roughness 같은 data map의 의미를 importer/IR에 보존한다. color는 sRGB decode 후 linear shading을 하고 data는 linear format으로 읽는다. material factor와 texture를 곱하는 의미, UV convention·sampler wrap/filter·alpha 정책도 명시한다.

3D color texture는 cooker가 명시적인 mip chain을 만들고 사용한 filter·색공간·크기 정책을 versioned settings로 기록하는 안이다. pixel sprite는 우선 nearest/no-mip을 명시하며 atlas padding/bleed 계약 없이 일반 downsample을 적용하지 않는다. normal map 지원에는 supplied tangent 또는 version 있는 tangent generation이 필요하다. UV가 없는 mesh에 texture를 지정했을 때 임의 UV를 만들지 않는다.

## 세대 교체·last-good·residency

원본 변경은 새로운 candidate generation에서 읽기→decode→cook→검증→필요 장치 자원 준비를 진행한다. 실패/취소 시 기존 complete generation과 진단을 유지한다. 게시가 완료되기 전에 old CPU/GPU/audio 자원을 파괴하지 않는다. 느린 구세대 작업의 completion은 token으로 거부한다. 부분 material과 다른 세대 image가 조합된 frame은 허용하지 않는다.

표시는 ready resource로만 새 generation을 채택하고 적용 tick/frame을 관찰 가능하게 한다. 이전 generation을 사용하는 in-flight GPU command나 audio voice가 남으면 그 수명이 끝난 뒤 회수한다. memory/upload 예산 초과는 명시적 pending/rejected/evicted 의미를 가지며 무제한 upload로 해결하지 않는다. placeholder 정책은 저작자가 지정한 optional resource에만 명시적으로 적용하며 missing required asset을 성공 처리하지 않는다.

## AI 제작과 추적성

CLI load/validate/inspect/capture와 후속 MCP는 같은 importer/IR/cache/service를 사용한다. source/dependency identity·settings/importer version·candidate/active generation·재사용/재변환 수·실패 chain을 구조화해 조회한다. 같은 visible session의 edit/apply→wait→inspect/capture는 P02에 연결하며 별도 batch capture를 live frame으로 표시하지 않는다.

실제 검증 에셋은 엔진 밖의 임시 project에 둔다. 원출처·작성자·라이선스·version/hash·수정·재배포 조건을 project asset manifest로 기록한다. public 다운로드 가능 여부와 재배포 허용을 구분한다. 엔진 repository에는 source attribution·작은 필요 test fixture만 명시적으로 관리하고, 두 게임 콘텐츠와 대용량 pack은 각각의 별도 repository로 보낸다.

## 인수와 반복 비용 검증

단계별 하위 결과만 체크한다. P03 전체와 D1은 모든 제품 항목을 통과하기 전까지 열어둔다.

- 외부 textured model·sprite atlas·WAV를 같은 project의 공통 pipeline으로 준비하고 실제 render/audio/2D 사용 경로까지 연결한다. parse-only 테스트를 해당 사용 기능의 인수로 쓰지 않는다.
- native/byte adapter가 같은 IR/key/오류를 만들고, Mac 및 필요한 Wasm target 컴파일을 확인한다. 컴파일 통과와 browser 실제 실행을 따로 기록한다.
- source/settings/dependency별 expected invalidation·unchanged reuse·cache corruption·path escape·oversized input·unsupported 요구를 시험한다.
- 실패/취소·연속 reload·stale 완료·장치 손실/회수에서 last-good와 유한 queue/resource를 검증한다.
- cold/warm no-change·PNG edit·buffer edit·atlas metadata edit·WAV edit·실패 reload에 대해 wall time, read/decode/cook/upload 수, 수동 단계, active identity와 capture를 남긴다. 실제 측정 전 속도 배율을 약속하지 않는다.

첫 구현을 시작할 때 public IR/API·수치 예산·소유 파일·lock owner와 위 항목의 실행 명령을 더 고정한다. 이 설계 문서가 생성되었다는 이유로 현재 P01/P03/DEV03을 완료 처리하지 않는다.

## P03.3 착수 전 현재 API 감사

다음은 P03.2c source `6459fcc`에 대한 read-only 조사다. a/b는 인수됐지만 c는 clean archive/CI/main 대기이며 아래 reload 구현은 시작하지 않았다. 과거 P03.1 착수 전 내용과 구분한다.

| 현재 seam | 확인한 동작 | 다음 계약에서 해결할 일 |
| --- | --- | --- |
| `PreparedProject`·`load_project_with_assets` | 불변 catalog와 validated model/sprite/camera binding. native load는 동기 resolver/import이고 취소는 협력적. watcher 없음 | actor/window 밖 유한 candidate 준비. 기존 importer/cache/cancellation을 재사용하고 파일별 loader를 새로 만들지 않음 |
| `ProjectSession` | startup에서 Engine과 StableId→EntityHandle을 만들고 source Arc를 고정 | 같은 Engine/handle·현재 authority transform을 유지하는 source/binding 교체. 새 session으로 재시작하는 것을 reload로 부르지 않음 |
| service frame·native coordinator | generation1과 첫 source digest를 고정하며 변경 source를 거부 | 하나의 generation owner와 monotonic candidate ID, expected-active 검증·stale completion 거부, 동일 세대 snapshot/source identity |
| `ProjectRenderResources`·`PacketRenderer` | immutable CPU 준비·snapshot extract·payload/layout reuse. ResidencyBusy 때 이전 ready packet 표시 가능 | 후보 CPU 준비와 GPU residency/submit·실제 present를 분리. renderer가 authority를 소유하지 않으며 실제 present identity를 coordinator에 보고 |
| `ProjectAudioRuntime` | bank/backend1개. voice는 이 bank의 alias index를 참조 | voice의 원본 generation/clip ID 유지, 같은 backend에 새 bank staging. old loop/callback backing 포함한 유한 retired bank·payload 회계 |

첫 하위 계약은 **에셋 변경만 허용하고 scene topology를 고정하는 경로**를 우선 검토한다. 같은 entity StableId·component/binding topology를 확인하고 authored transform 변경·entity 추가/제거·reparent는 명시적으로 거부한다. 현재 ECS pose/revision/handle과 authority camera binding을 보존한다. 일반 scene patch·구조적 reload는 후속 계약이며 이 제한된 경로로 P03.3 전체를 체크하지 않는다.

제안한 순서는 bounded async 후보 준비 → topology-preserving stage/commit → consumer staging과 회수 → 반복 계측이다. 첫 trigger는 같은 session의 명시적 reload 요청이며 watcher/debounce는 그 요청의 adapter로 후속 추가한다. one-running/one-latest 대기, supersede/cancel·stale-drop·종료·정확한 수치 예산·공개 API와 파일 소유권은 상세 MD에서 구현 전에 고정한다. 기존 task pool의 blocking I/O·cache lease·join 한계를 조사하지 않고 새 작업 thread를 무조건 추가하지 않는다.

CPU128MiB·audio Frame256MiB·GPU 단일 consumer 예산만 각각 통과했다고 여러 generation이 안전한 것은 아니다. active+candidate+in-flight+retired의 동시 상주와 변환 scratch를 합산한다. old looping voice가 여러 generation을 붙잡을 수 있고 Stopped/reaped만으로 CPAL의 마지막 Frame Arc 해제를 단정할 수 없다. narrow backing-owner 관찰이나 보수적 보유가 필요하며, 회수 확인이 안 되면 Busy/명시적 거부를 반환한다. callback join을 새로 보장했다고 쓰지 않는다.

새 status는 CPU-prepared·actor-committed·render-ready/submitted·실제-presented를 구분한다. `latest_frame`은 actor snapshot이며 GPU present ACK가 아니다. 화면이 이전 ready generation을 쓰는 동안 새 generation을 이미 보였다고 보고하지 않는다. 실패·취소·예산 초과 때 기존 정상 세대의 source/packet/voice가 유지되는지 검사한다.

native filesystem의 여러 파일을 차례로 읽은 결과는 하나의 immutable **captured byte graph**가 될 수 있으나 원본 디스크의 동일 순간 atomic snapshot은 아니다. content hash와 dependency validation은 입력의 identity를 설명하며 동시 파일 편집을 자동 transaction으로 만들지 않는다. byte-map 경로·명시적 source transaction과 native capture/recheck 정책을 후속 상세 계약에서 구분하고, watcher debounce/mtime을 원자성 증명으로 사용하지 않는다.

worker/cache 후속 감사는 기존 `Engine.tasks()`에 독립 asset consumer를 추가하는 위험도 확인했다. TaskPool은 단일 completion receiver를 공유하고 LiveSession은 자신이 제출하지 않은 completion을 unexpected로 다루므로 서로의 결과를 소비할 수 있다. blocking source I/O도 simulation CPU slot과 혼합하지 않는다. project-scoped native asset worker 하나를 common service 옆에 두되 별도 Engine/권한 world를 만들지 않는 안을 상세 계약 후보로 선택한다. 기존 cancellation/latest-generation 의미와 importer API를 재사용한다.

cache는 session worker 하나만 연다. `FileCookCache::open`의 `.owner.lock`은 catalog의 CachePin으로도 유지될 수 있어 candidate마다 다시 열면 Busy가 될 수 있다. 초기 load부터 같은 worker의 concrete FileCookCache를 사용하는 안을 우선하며, 기존 loader에서 시작할 경우 cache pins 해제·이전 cache drop 뒤 새 worker가 한 번 열어야 한다. 중간에 다른 process가 lock을 잡으면 explicit Busy/startup failure다. 조용히 memory cache로 바꾸지 않는다. host-neutral CpuCookCache trait에 불필요한 Send 요구를 추가하거나 임의 unsafe Send를 붙이지 않는다.

worker 종료도 협력적이다. 취소를 무시하는 parser 또는 blocking read를 강제 중단하거나 hard deadline 안에 join한다고 보장하지 않는다. UI/tick에서 join하지 않고 owner/control 경로에서 stop 요청과 worker/cache 소유권을 유지하며 실제 종료를 관찰한다. 시간이 지나도 작업이 남으면 Stopping과 lock 보유를 사실대로 보고한다. 구체적인 deadline·retention·process 종료 정책은 구현 전 상세 계약에 남긴다.

## P03.1 착수 전 source 감사와 예산 후보

2026-10-16 read-only 감사는 `PreparedProject`의 mesh map·byte API가 direct JSON mesh source만 알고, renderer의 24-byte vertex와 shader에는 UV/texture/sampler가 없음을 확인했다. audio adapter는 decoded PCM을 받을 수 있지만 WAV parser는 없다. import와 실제 GPU/2D/audio 사용은 별도 연결 작업이며 이 차이를 이름만 있는 asset catalog로 가리지 않는다.

첫 CPU 경계는 host-neutral `daedalus_content::assets`와 byte-map/native-contained resolver·versioned model/image/atlas/PCM catalog다. 기존 public `ProjectMesh`/`ProjectVertex`·`.meshes()`·renderer `Vertex`·schema 1 의미를 불필요하게 바꾸지 않는다. 신규 모델의 UV/material/primitive/node 정보는 별도 IR/cooked payload에 담고, 기존 내부 GPU layout에는 명시 layout version으로 연결한다. skin/animation 등 미지원 요구는 위치 있는 오류로 거부한다. 미래 기능을 위해 지금 모든 generic accessor/animation runtime을 구현하는 것으로 범위를 늘리지 않는다.

아래는 착수 전의 **수치 후보 이력**이다. 현재 P03.1의 확정 계약은 [구현 계약](asset-import-foundation.md)의 한도·settings·cache·검증 표를 따른다. 후보 표를 구현 한도의 또 다른 원본으로 사용하지 않는다.

| 항목 | 후보와 구분 |
| --- | --- |
| 기존 project | source 합계 64 MiB·asset 256·entity 16,384·vertex 1,000,000·index 3,000,000 및 legacy JSON mesh 8 MiB 유지 |
| dependency source | 새 imported file당 32 MiB 이하, 전체 source 64 MiB가 우선. 최대 1,026 unique file 후보는 기존 direct-file 258과 별도 import 설정에서 명시 |
| decoded image | 각 축 4096·RGBA aggregate 64 MiB. source 크기와 decoded 크기를 각각 계수 |
| audio | 기존 PCM 최대 10,000,000 frames·mono/stereo·8~384 kHz 유지. 최대 stereo float 약 80 MB를 decoded aggregate에도 계수 |
| CPU cooked resident | 합계 128 MiB. geometry/image/audio 각각 최대치가 동시에 들어간다고 허용하지 않음 |
| disk cache | project/engine-owned 512 MiB·1,024 entries 후보. active/in-flight entry를 무조건 지워 hit인 척하지 않음 |
| GPU | 기존 preferred/hard 64/128 MiB와 texture budget을 합쳐 다시 계약. 무제한 별도 image cache 금지 |

P03.1 CPU import/cook 뒤 P03.2가 실제 model primitive·texture·sprite/audio adapter를 연결한다. 전체 P03는 invalidation·연속 reload·last-good·실제 소비 증거를 모두 필요로 한다. 최종 2D/audio/animation 인수는 관련 P05/P07/P09 기능과 함께 검증하며 P번호를 엄격한 숫자 순서 phase로 오해하지 않는다. 실제 외부 asset provenance는 [별도 기록](../02-research/asset-validation-provenance.md)에 있고 import 성공과 구분한다. 나머지 실행 검증 상태는 위 작업 기록으로 갱신한다.
