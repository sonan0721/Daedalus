# P03.2a 모델·텍스처 실행 계약

상태: 로컬 구현·CPU/Metal/native 검증 통과·clean package/PR CI 대기 · 2026-10-16 · P03.2a/DEV01/DEV03/DEV05/DEV08

## 목적과 범위

[P03.1 CPU gate](../06-worklog/2026-10-16-asset-import.md)는 PR10의 필수 다섯 CI와 main 병합까지 인수했다. 다음 산출물은 같은 외부 project의 imported static model과 base-color PNG를 실제 창 및 batch PNG capture에 표시하는 엔진 기능이다. [상위 consumer 계약](asset-runtime-consumers.md)의 a 경로를 구체화한다. sprite·audio·source reload는 각각 P03.2b/c·P03.3이며 이 계약의 통과만으로 P03.2나 D1을 체크하지 않는다.

개발 반복 비용을 줄이는 대상은 project loader, model 확장, texture 준비, window/capture 렌더링, smoke 조립이다. 모두 기존 경로에 연결하며 별도 CLI 모델 renderer나 테스트 전용 importer를 만들지 않는다. 이번 batch 준비는 실행 중 업로드 분할·144fps·로딩 UI 인수의 증거가 아니다.

## 입력·공개 API·소유권

scene schema 1의 기존 component envelope 안에 아래 component를 추가한다. JSON은 envelope 예시이며 entity의 기존 transform component는 별도로 유지한다.

```json
{
  "id": "entity.renderer.main",
  "type_id": "daedalus.model_renderer",
  "asset_refs": ["asset.hero"],
  "data": { "model": "asset.hero" }
}
```

`asset_refs`는 정확히 하나이며 `data.model`과 같아야 한다. catalog의 해당 asset은 static model이어야 한다. data의 미지 field, 누락·중복·잘못된 kind·같은 entity의 중복 model renderer 및 legacy mesh renderer와의 충돌을 source/JSON path 있는 오류로 거부한다. 첫 버전은 imported primitive material을 사용하며 entity material override는 받지 않는다. 실행 가능한 root entity 제약을 바꾸지 않는다. glTF 내부 node hierarchy 지원이 일반 scene parenting 구현을 뜻하지 않는다.

`PreparedEntity`, `Vertex`, `RenderInstance`, `PacketRendererLimits`, `PacketResourceStats`의 기존 공개 field와 struct literal을 유지한다. private collection과 additive API를 사용한다.

| API | 계약 |
| --- | --- |
| `PreparedModelRendererBinding` | private field, `component_id()`/`model_asset_id()` 불변 accessor |
| `PreparedProject::model_renderers()` | entity StableId를 key로 하는 validated binding map의 불변 참조 |
| `PreparedProject::release_asset_cache_pins(self) -> Self` | 아래 명시적 eager 자료 수명 계약, default loader는 변경하지 않음 |
| `ProjectRenderResources::prepare`/`extract` | model binding까지 준비하고 같은 authority snapshot에서 packet을 추출 |
| `Renderer::into_packet_renderer_with_asset_limits` | 기존 packet limits와 별도의 `PacketAssetLimits`를 받는 additive constructor |
| `PacketRenderer::asset_resource_stats()` | 기존 stats를 깨지 않고 texture·sampler·bind group·합산 resident/staging·재사용 계측 제공 |
| `run_packet_windowed_reported_with_asset_limits` | 기존 window/packet outcome을 유지하고 `PacketAssetWindowOutcome { packet: PacketWindowOutcome, asset_resource_stats: PacketAssetResourceStats }` wrapper 반환 |

기존 `load_project`/`prepare_project_from_bytes`는 legacy 범위와 오류 의미를 보존한다. 새 component는 asset-aware loader/validator가 처리한다. `PreparedProject::validate()`도 불변 catalog와 source scene에서 binding을 재검증하며 private map을 무조건 신뢰하지 않는다. 새 불변 field는 PartialEq에 반영한다. asset-aware와 legacy loader가 같은 legacy project를 읽을 때 source digest는 같아야 한다. 빈 catalog domain을 추가해 기존 source identity를 바꾸지 않는다.

## 공통 CLI loader와 디스크 캐시 잠금

`project validate/inspect/run/serve/view/capture`는 하나의 asset-aware 준비 helper를 사용한다. mode는 명시적으로 나눈다. validate/inspect는 MemoryCookCache를 사용해 read-only 검사에 디스크 cache lock을 요구하지 않는다. run/serve/view/capture는 유한 FileCookCache를 이용해 eager batch 준비 후 project의 cache pins를 명시적으로 해제하고 cache owner를 drop한 뒤 session을 시작한다. cook은 기존 helper/보고와 pinned 작업 수명을 유지한다. init과 별도 절차형 `daedalus view`는 이번 입력 경로의 대상이 아니다.

현재 catalog의 모든 IR·decoded image backing은 독립 owned Rust/Arc 자료이며 mmap·lazy file 읽기를 하지 않는다. cache pin은 disk entry eviction 보호와 exclusive owner lock 수명만 가진다. 따라서 `release_asset_cache_pins`는 private catalog pin collection만 비우며 자료·source identity·digest·report·geometry/pixels를 바꾸지 않는다. 기존 loader가 돌려주는 project는 기본적으로 계속 pinned 상태다. clone 하나가 pin을 보유하면 마지막 해당 clone이 해제/drop될 때까지 잠금은 남는다. cache helper는 가능한 한 pub(crate)로 제한한다.

이렇게 해야 창이 켜진 동안 별도 capture/run/client가 같은 default cache를 사용할 수 있다. 초기 두 batch load가 겹쳐 실제 cache owner가 잠겼으면 typed Busy를 즉시 반환한다. 숨은 무한 retry나 강제 lock 해제는 하지 않는다. 미래 P03.3 host의 generation/lazy 작업이 필요하면 그 host가 별도 owner/pin 수명을 소유한다. 이번 detach를 자동 lazy loading과 결합하지 않는다.

## 준비 시 model 확장과 프레임 추출

selected-scene root 순서에서 depth-first로 node를 확장한다. 현재 IR은 node parent와 document node index를 보존하고 원본 children array 순서는 보존하지 않는다. 따라서 자식은 document node index 오름차순, primitive는 model의 보존된 primitive 순서로 열거한다. root·parent·범위·중복 방문·cycle을 검증한다. 입력이 같은 generation 안에서 draw 순서는 고정된다.

동일 mesh가 여러 node에 나타날 수 있다. 각 `(node index, global primitive index)` 발생에 checked u32 draw ordinal을 부여한다. `RenderInstanceId`의 entity handle과 이 ordinal이 실제 draw를 구분한다. primitive index만 ID로 쓰지 않는다. expanded instance/vertex/index와 bytes를 allocation 전에 checked 계산하고 기존 packet 한도를 적용한다.

prepare에서 immutable geometry·material·texture binding·node global matrix·draw mapping을 한 번 만든다. imported geometry는 가능한 한 `Arc<GeometryResource>` backing을 유지하고 entity/node별 전체 vertex 복제를 피한다. 같은 payload의 geometry는 StableId alias가 달라도 device cache에서 공유할 수 있어야 한다. layout/내용/device generation을 GPU identity에 포함한다. leaf image key에 포함된 role/sampler 차이로 같은 pixel payload를 불필요하게 두 texture로 만들지 않는다.

extract는 현재 권한 session의 EntityHandle/revision/snapshot 검증 후 `authority entity world × glTF node global`을 계산한다. authored transform이나 CLI report에서 현재 위치를 다시 읽지 않는다. full affine matrix를 유지하고 최종 3×3 inverse-transpose로 normal을 계산한다. finite·affine bottom row(기존 scene 규칙의 1e-6 tolerance)·determinant가 0이 아님·계산 결과 finite를 검사한다. 임의의 near-zero determinant epsilon을 새로 추가하지 않는다. 현재 cull_mode None을 유지하고 mirrored transform 때문에 공유 index를 뒤집거나 geometry를 복제하지 않는다.

snapshot-only 변경으로 source 읽기·import/decode·node 확장·geometry hash/pack·texture hash/upload가 반복되면 실패다. transform/material slot의 필요한 dirty write와 frame global 갱신은 별도 counter로 구분한다. source watch와 generation 교체 자체는 P03.3에서 구현한다.

## GPU layout·색·sampler

legacy position/normal 24-byte layout을 보존한다. imported textured model은 position/normal/UV0의 명시적인 32-byte layout을 사용한다. 현재 instance attribute 2~9와 충돌하지 않도록 UV0는 shader location 10이다. 이 경로를 사용할 때 최소 11 attribute와 stride/buffer/texture 한도를 검사한다. legacy 10-attribute 장치 경로까지 불필요하게 차단하지 않는다. UV 없는 untextured model도 명시적으로 지원하며 texture가 필요한데 UV가 없으면 importer 오류를 유지한다.

RenderPacket의 texture/material sidecar는 private immutable 자료로 추가한다. 기존 constructor/with_instances의 legacy 의미와 올바른 validation을 보존하며 PartialEq도 binding 차이를 반영한다. 공개 additive constructor가 필요하면 renderer owner가 CLI 작업 전에 정확한 signature를 공유한다. snapshot마다 image 전체를 hash하거나 새 sampler/bind group을 만들지 않는다.

base-color는 `Rgba8UnormSrgb`에서 sample해 linear RGB로 만들고 linear factor를 곱한 뒤 기존 PBR·ACES·output encode 경로를 사용한다. glTF용 PNG 색 메타데이터 해석은 P03.1 정책 그대로다. texture/UV는 upper-left 기준으로 한 번만 적용하며 importer와 shader에서 각각 뒤집지 않는다. alpha는 첫 opaque subset에서 불투명으로 처리한다. 아직 미지원인 alpha mode·normal/data map 등을 흰색 대체로 숨기지 않는다.

sampler는 `ModelTextureRef::sampler()`의 min/mag·mip none/nearest/linear·wrap clamp/repeat/mirrored를 따른다. image cook용 sampler를 primitive sampler로 오인하지 않는다. mip none이면 LOD를 base level로 제한한다. cooked mip의 차원/offset/byte count를 검증하고 전체 chain을 업로드한다. texture payload와 sampler는 독립 cache이며 bind group은 그 pair를 공유한다. mip/format/dimensions/pixel identity를 준비 때 한 번 계산한다.

실제 shader에는 ACES와 ambient Fresnel이 있다. sRGB 시험은 최종 pixel이 입력 128과 같다고 가정하지 않는다. 독립 f64 sRGB decode로 얻은 linear 색을 기존 scalar material reference에 넣어 동일 BRDF/tone mapper 결과와 비교한다. 비대칭 four-color와 mid-gray 입력, factor·wrap·nearest/linear·mip를 각각 검증한다. 경계/보간 오차를 피한 sample과 명시적 tolerance를 사용한다.

## 유한 자원 예산과 transaction

기본값은 아래 구현 계약이며 장치의 더 작은 한도는 추가로 적용한다. 새 `PacketAssetLimits`는 checked 검증과 2배 transition reserve를 가진다. 기존 geometry 한도는 합산 resident의 부분 한도이며 별도 더하는 예산이 아니다.

| 종류 | 기본 한도와 의미 |
| --- | --- |
| 후보 resident working set | 128 MiB; geometry+texture mip bytes+공유 fixed instance/material/global buffer를 한 번 합산 |
| resident hard | 256 MiB; active+candidate+GPU 미완료 retired 자료 포함 |
| geometry 부분 한도 | 기존 preferred64/hard128 MiB, active256/total512 entries 유지 |
| texture entries | active256/total512 |
| sampler entries | active256/total512 |
| texture/sampler bind groups | active1024/total2048 |
| GPU upload staging | 별도 hard128 MiB; 모든 queued buffer/texture write를 제출 완료까지 유지 |
| packet in-flight submissions | 기본/상한3개; write가0인 draw도 포함해 ledger/completion callback 수를 제한 |
| CPU upload scratch | 별도 hard128 MiB; geometry pack/필요한 upload 복사·padding의 동시 peak |
| 자체 resource bookkeeping | 별도8 MiB의 보수적 고정-record/entry accounting; driver 내부 overhead 추정으로 사용하지 않음 |

tracked device resident+staging 최대는384 MiB이고 CPU scratch까지 합친 해당 두 분류의 최대는512 MiB다. 이것은 process RSS나 정확한 physical VRAM 보장이 아니다. framebuffer/depth/readback에는 기존 dimension/byte 한도가 별도로 적용되며 이 표로 전체 GPU 메모리 한도를 주장하지 않는다. Arc·HashMap allocator overhead도 정확히 관측했다고 쓰지 않는다.

하나의 전역256 MiB에 old128+new128+staging을 모두 넣으면 교체할 공간이 없어질 수 있다. 위 staging 분리는 이 교착을 피한다. 그러나 후보가 단독으로도 staging128 MiB를 넘으면 terminal located LimitExceeded다. 영원히 Pending으로 남기지 않는다. queued `write_buffer`에는 frame instance/material/global 변경도 포함한다. 최대 instance 수의 해당 변경만 약2.75 MiB가 될 수 있으므로 초기 asset upload만 계측해서는 안 된다. queue.write_texture는 unaligned row를 허용하지만 별도 copy-buffer 경로가 필요한 padding도 실제 사용량에 포함한다.

packet validation, slot 계획, 신규 byte/entry/staging/scratch 합산, eviction 계획 등 fallible 작업을 GPU mutation 전에 마친다. 한도가 맞지 않으면 마지막 ready packet과 shared slot 내용을 유지하고 bounded pending 하나만 최신 후보로 coalesce한다. 단독 초과는 Rejected, GPU 미완료 pin에 의한 일시 부족은 Busy/Pending으로 구분한다. terminal 오류를 retry로 숨기지 않는다.

모든 fallible 준비 뒤 slot write와 해당 draw를 같은 queue 순서/submission으로 commit한다. write를 queue에 넣고 후보를 포기한 뒤 old packet을 변경된 slot으로 그리지 않는다. 이후에도 실패 가능한 단계가 남으면 alternate slot 또는 이전 값 복구라는 실제 transaction을 구현해야 한다. staging은 completion callback/watermark가 지난 뒤 회수하고 submit 성공을 완료로 보지 않는다. redraw에서 GPU 완료를 기다리는 Poll::Wait를 새로 넣지 않는다. 기존 batch capture의 명시적 완료 대기는 live path와 구분한다.

staging 계측에는 loading overlay uniform도 포함한다. 후보가 Busy여도 old packet의 필요한 frame write를 감당할 수 있으면 실제 old draw를 submit하고 그 identity를 반환한다. old frame조차 submit할 수 없으면 typed ResidencyBusy를 반환한다. window loop는 그 acquired surface frame을 drop하고 다음 redraw를 기다리며 present·frame count·presented metadata를 갱신하지 않는다. 실제 draw 없이 Ok(old packet)을 반환해 미초기화 frame을 present하는 경로를 금지한다.

byte 한도만으로 submission ledger와 callback 수를 제한할 수 없다. 변경 write가 없는 draw도3개 in-flight 제한에 포함하고 queue mutation 전에 admission한다. 완료 watermark가 지난 submission만 ledger에서 제거한다. 모든 submit 뒤 그 frame이 사용한 geometry·texture·sampler·bind group·untextured model용 default texture의 last_submission을 현재 serial로 갱신한다. unchanged/old-ready frame도 갱신하며 최초0 값을 유지한 채 in-flight resource를 completed eviction 대상으로 삼지 않는다.

## 실제 실행·보고와 반복 검사

project view와 project capture 모두 `PreparedProject → ProjectRenderResources → RenderPacket → PacketRenderer`를 사용한다. capture의 기존 JSON identity/authority/projection field는 의미를 유지하고 packet 통계에서 산출한다. 기존 공개 `project_snapshot` API는 보존하지만 CLI capture의 별도 legacy renderer 분기는 제거한다. batch capture는 새 session이며 live presentation capture라고 보고하지 않는다.

기존 `projection.expanded_geometry_bytes`는 vertex/index에 `RenderObject` record까지 더하던 값이다. 호환성을 위해 각 draw의 정확한 packed mesh bytes(`MeshHandle::gpu_bytes()`)와 `size_of::<RenderObject>()` allowance를 checked 합산한다. legacy 입력의 값은 이전 API와 같아야 한다. imported UV layout은 새 packed byte 수를 사용한다. 실제 RenderObject를 만들거나 geometry를 복제하지 않는다. `expanded_geometry_bytes_basis`로 `per_draw_packed_geometry_plus_legacy_object_record`를 명시하고 `packet_expanded_gpu_geometry_bytes`에 mesh bytes만의 합을 별도 제공한다. 전자는 호환용 논리 추정치이며 실제 CPU allocation·공유 GPU resident bytes가 아니다. GPU instance128 bytes를 전자에 새로 더해 기존 값을 바꾸지 않는다.

공통 CLI 준비는 runtime actor 전에 eager I/O/CPU cook를 끝낸다. fixed tick/redraw에서 file read/decode를 하지 않는다. `project cook.runtimeConsumers`의 false는 해당 CPU-only invocation에서 GPU/audio 소비를 수행하지 않았다는 의미로 유지한다. capability probe나 엔진 전체의 영구 미지원 상태와 혼동하지 않도록 문서를 고친다. view/capture는 실제 model draw·texture upload/reuse 수를 보고하며 image summary를 실제 draw 증거로 쓰지 않는다.

| 검증 | 통과 조건 |
| --- | --- |
| content CPU | 정확한 envelope·잘못된 참조/중복/unknown field·revalidation·legacy digest 동등성 |
| cache 수명 | detach 뒤 project pixels/geometry 유효+같은 root reopen, pinned clone은 reopen 방지, 기존 pin/eviction regression 유지 |
| packet CPU | 선택 scene·동일 mesh의 여러 node/primitive·고정 draw ID·부모 회전/비균일 scale/mirror·singular/non-affine·expanded limit |
| 자원 계획 | 128→128 전환 산술·entry/bytes/staging 포화·completion 회수·후보 거부 뒤 last-ready/slots 불변 |
| 실제 GPU | four-color/gray UV·sRGB/factor·sampler/mip 픽셀과 반복 frame/transform-only의 geometry/texture upload0 |
| 실제 project | 공개 원본 Khronos Cube와 PNG를 엔진 loader로 view/capture; authority identity 및 실제 pixel/텍스처 확인 |
| 개발 회귀 | 기존 helper의 CPU18/assets31/native22 의미 유지, 같은 timeout/hash/cleanup을 재사용한 opt-in model GPU 검사 |

새 smoke framework·중복 packaging backend·CI 다운로드를 추가하지 않는다. CPU fixture는 offline의 작은 생성 입력이며 실제 공개 입력은 [출처 기록](../02-research/asset-validation-provenance.md)을 재사용한다. 실행 시간·counter·binary/source identity·cold/warm 차이를 기록하고 수동 기준이 없는 속도 개선 배율을 발명하지 않는다.

## 파일 소유권과 인수 순서

1. Luna xhigh content owner: content project binding/validator·pin detach·해당 regression. 공개 API를 먼저 공유한다. 첫 focused Cargo/lock 소유자다.
2. Luna xhigh renderer owner: packet·GPU·shader·한도·계측·CPU/GPU regression. content API 고정 후 공통 target/Cargo 소유권을 넘겨받는다. 별도 cold target은 만들지 않는다.
3. Luna xhigh CLI owner: 공통 loader·view/capture adapter·같은 smoke 확장. renderer API가 먼저 공개되고 source가 준비되어야 의존 작업을 연결한다. Cargo 실행은 별도 인계한다.
4. 조정자: 문서·source review·실제 CPU/GPU/공개 에셋 통합·패키지·PR/다섯 CI 증거. 코드 수정이 필요하면 해당 Luna owner에게 돌려준다.

focused 검사 → source 통합 → 필요한 workspace/managed 회귀 → 실제 Mac/Metal pixel/창·로컬 package → PR CI 순서다. 새 변경이나 발견 없이 같은 broad test를 반복하지 않는다. 하위 인수 결과는 작업 기록과 체크리스트에 별도로 남긴다. 현재 이 파일은 실행 성공 기록이 아니다.

## 현재 구현과 근거

Luna xhigh source를 조정자가 통합 리뷰한 뒤 workspace CPU254·Python54·strict Clippy/fmt·assets31·실제 Metal4·native/model24를 통과했다. public Cube 두 entity에서 geometry1/texture1 공유와 실제 대리석 texture를 확인했다. 창이 살아 있는 동안 별도 capture와 이후 authority 제어도 통과했다. 상세 command·binary/source identity와 한계는 [작업 기록](../06-worklog/2026-10-16-model-texture-runtime.md)을 따른다. clean release package와 PR 최종 다섯 CI/main 병합은 아직 대기하므로 P03.2a 체크박스는 열어 둔다.

`active_texture_entries`는 asset texture를 센다. untextured model용 내부 white texture는 별도 고정 자원이며 resident bytes에는 포함한다. resident/staging/scratch/bookkeeping 수치는 명시적으로 charge한 logical resource 비용이다. allocator·driver·attachment를 포함한 실제 전체 VRAM/RSS 측정값으로 해석하지 않는다.
