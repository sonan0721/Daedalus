# P03.2b 스프라이트·정사영·실제 표시 정보 계약

상태: source/API 리뷰 반영·구현 계약 확정·검증 미실행 · 2026-10-16 · P03.2b/DEV01/DEV03/DEV04/DEV05/DEV08

## 목적과 선행 gate

[P03.2a](model-texture-runtime.md)는 PR11 다섯 CI·clean release/extracted 검사·main `6d1711f` 동일 tree까지 인수했다. 이 계약은 같은 외부 project/catalog와 authority snapshot에서 실제 atlas sprite를 표시하는 다음 엔진 기능이다. 별도 테스트 renderer·sprite importer·texture cache·게임 project를 만들지 않는다. tilemap/유체/2D lighting/애니메이션/UI/일반 고해상도 sprite 품질은 후속 P05/P07/P09에 남아 있으며 이 첫 nearest atlas 소비를 최종 2D 엔진 범위로 고정하지 않는다.

[상위 consumer 계획](asset-runtime-consumers.md)의 b 경로다. sprite 전체 검증 뒤에도 c audio·P03.3 source generation/reload·P03.4 반복 제작 계측·D1 엔진 제품과 두 게임은 미완료다. 이 MD의 존재나 자체 생성 atlas 검사는 제품 인수가 아니다.

## 확인한 source와 공통화 경계

2026-10-16 Luna xhigh의 read-only 감사와 조정자 리뷰 기준이다.

- `daedalus-content/src/assets/types.rs`의 AtlasRegion은 immutable rect/pivot/PPU, SpriteAtlas는 image StableId/regions/cook key를 보유한다. atlas importer는 SpriteAtlasSrgb·nearest·no-mip을 이미 검사한다. pivot 원점과 실제 draw 의미는 여기서 정한다.
- ProjectSession은 camera-only를 포함한 모든 prepared entity의 transform/handle을 실제 ECS에 넣고 snapshot에 발행한다. 새 camera용 ECS store나 별도 session이 필요하지 않다.
- `ProjectRenderResources`의 prepare-once/extract-authority, imported UV vertex, geometry/image/sampler/pair cache와 admission/retirement를 확장한다. 현재 imported draw가 모두 model이라고 가정하는 private 분기만 공통화한다.
- 기존 model shader는 alpha1·PBR·depth write다. sprite는 unlit alpha shader와 정렬 의미가 필요하다. PNG alpha를 보관하는 것과 실제 합성은 다르다.
- Camera/ProjectCamera는 perspective만 가진다. 공개 struct literal을 바꾸지 않고 packet의 projection과 실제 사용한 view 보고를 추가한다. native local camera override는 authority pose·정사영 zoom과 함께 다룬다.

## scene schema 1의 additive 입력

sprite entity에는 기존 `daedalus.transform`과 다음 component를 넣는다.

```json
{
  "id": "component.hero.sprite",
  "type_id": "daedalus.sprite_renderer",
  "asset_refs": ["asset.characters"],
  "data": {
    "atlas": "asset.characters",
    "region": "hero.idle.0",
    "tint": [1.0, 1.0, 1.0, 1.0],
    "sorting_layer": 0,
    "order_in_layer": 0
  }
}
```

atlas/region은 StableId다. asset_refs는 atlas 한 개와 정확히 일치하며 image ID를 다시 나열하지 않는다. catalog의 atlas→image dependency를 따른다. tint 기본은 `[1,1,1,1]`, 네 값은 finite [0,1]의 linear RGB/linear opacity다. layer/order 기본0, 각각 signed32-bit 정수다. unknown/missing field·잘못된 kind/region·중복 sprite·mesh/model/sprite 충돌을 source/JSON path 있는 오류로 거부한다. root entity 제약은 유지한다. 일반 legacy asset-unaware loader는 sprite를 Unsupported로 진단한다.

camera entity에는 기존 transform과 다음 component를 넣는다. asset_refs는 비어 있어야 한다.

```json
{
  "id": "component.camera.2d",
  "type_id": "daedalus.camera_2d",
  "asset_refs": [],
  "data": { "vertical_size": 10.0, "near": 0.1, "far": 100.0 }
}
```

각 값은 필수이며 finite, `0.001 <= vertical_size <= 1_000_000`, `0 < near < far`다. camera2d는 project당 최대1개, sprite가 있으면 정확히1개를 요구한다. camera entity와 renderer component를 혼합하지 않는다. camera2d와 `presentation.camera` 동시 지정은 모호한 우선순위를 만들지 않고 거부한다. sprite가 없는 기존 project와 manifest는 그대로 perspective다. camera2d 자체는 외부 asset이 없으므로 legacy 준비 경로에서도 지원할 수 있지만 지원·재검증을 일관되게 적용해야 한다.

새 binding은 private `PreparedProject` collection으로 보관한다. `PreparedSpriteRendererBinding`은 component_id/atlas_asset_id/region_id/tint/sorting_layer/order_in_layer accessor, `PreparedCamera2dBinding`은 entity_id/component_id/vertical_size/near/far accessor를 제공한다. `sprite_renderers()` map은 entity StableId가 key이고 `camera_2d()`는 Option 참조다. PartialEq·validate의 재구성과 metadata 사전 charge를 갱신한다. PreparedEntity·ProjectManifest·ProjectPresentation·ProjectCamera 공개 field를 추가하지 않는다.

## 좌표·quad·authority

atlas rect `[x,y,width,height]`는 원본 PNG의 좌상단 pixel edge 기준이고 region 회전/trim은 첫 subset에서 지원하지 않는다. pivot `[px,py]`는 region의 **좌하단** 기준 normalized 값이다. 기존 IR처럼 finite면 [0,1] 바깥 pivot도 허용해 외부 anchor를 표현한다. PPU는 source에 있는 positive finite 값이다.

local XY plane의 z=0, +X 오른쪽/+Y 위다. bounds는 `left=-px*w/PPU`, `right=(1-px)*w/PPU`, `bottom=-py*h/PPU`, `top=(1-py)*h/PPU`다. image 크기를 W,H라 할 때 left/right UV는 x/W,(x+w)/W, top/bottom V는 y/H,(y+h)/H다. local top vertex에 top V를 연결하며 import/shader 양쪽에서 중복 flip하지 않는다. 실제 fragment center를 기준으로 nearest border/이웃 region/투명 pixel을 검사한다.

최종 sprite transform은 실제 authority entity matrix × 준비한 local geometry다. 위치/회전/scale과 mirror는 snapshot을 따른다. 첫 공통 instance 경로는 finite/invertible transform을 요구하며 singular/zero-scale은 명시적으로 거부한다. visibility나 축을0으로 만들어 숨기는 기능을 구현했다고 표시하지 않는다. geometry preparation/hash/packing을 매 frame 반복하지 않는다. layer/order가 같으면 authored entity StableId ASCII 순서로 오름차순 정렬하고 뒤의 draw가 앞에 합성된다. runtime handle 재사용 순서나 HashMap iteration이 tie-break가 되지 않는다. instance ID는 실제 entity handle + primitive ordinal0이다.

camera pose는 snapshot translation, normalize한 rotation×-Z forward/×+Y up를 사용한다. source transform은 최초 ECS state만 정한다. camera scale은 unit이어야 하며 각 축 `abs(scale-1) <= 1e-6`를 초기/추출 양쪽에서 검사한다. 최종 finite pose와 nonzero quaternion을 검사한다. packet의 Camera near/far는 camera2d component 값, FOV는 유효한 기존 기본값을 보관하되 orthographic일 때 투영에 사용하지 않는다. 카메라와 sprite의 stale/wrong-session handle을 표시용 별도 자료로 우회하지 않는다.

정사영은 right-handed view와 depth [0,1]이다. physical viewport 높이에 대응하는 world size는 vertical_size, width는 vertical_size×physical aspect다. near/far는 view depth clip이며 painter layer를 z값으로 몰래 변환하지 않는다. resize는 aspect/global/depth target만 바꾸고 geometry/image를 다시 upload하지 않는다. 기본 원점 camera는 -Z를 보므로 보이는 sprite 예제 camera는 양의 Z에 둔다.

## shading·순서·surface

첫 sprite는 unlit이며 texture RGB를 sRGB decode한 linear 값에 linear tint RGB를 곱한다. alpha는 texture alpha×tint alpha다. source는 straight alpha이며 색을 alpha로 미리 곱하지 않는다. blend는 color `SrcAlpha / OneMinusSrcAlpha`, alpha `One / OneMinusSrcAlpha`, Add다. opaque background의 독립 reference는 `Cout=Cs*As+Cd*(1-As)`이고 sRGB encode는 합성 후 한 번이다. alpha0 texel의 숨은 RGB가 화면에 남지 않아야 한다.

opaque legacy/model stage 뒤에 sorted sprite stage를 그린다. sprite stage는 같은 color attachment를 Load하는 별도 pass로 그리며 depth attachment/test/write 없이 painter order를 따른다. 새 color render target은 만들지 않는다. 따라서 이 subset은 일반 3D object 뒤로 가려지는 billboard와는 다르며, 그 기능은 별도 depth policy를 계약할 때 추가한다. sprite RGB에 model PBR/ACES를 적용하지 않는다. clear color는 기존 renderer의 linear clear 의미를 따른다.

offscreen은 기존 Rgba8UnormSrgb를 사용한다. native는 기존 sRGB surface를 우선한다. sprite가 있는 packet에 non-sRGB target만 제공되면 첫 단계는 create/write 이전 typed Unsupported/InvalidScene으로 거부한다. 기존 manual sRGB encoding 뒤에 alpha blend를 붙이지 않는다. 검증하지 않은 compatible view/compositor fallback을 이번 gate에 포함하지 않는다. legacy/model의 non-sRGB fallback 의미는 유지한다.

## packet·GPU 자원과 호환성

`RenderProjection::{Perspective, Orthographic { vertical_size: f32 }}`를 additive 공개 enum으로 제공하고 opaque `RenderView`(private Camera+RenderProjection)에 보관한다. RenderView는 `new(camera, projection)`/`perspective(camera)` 생성, `camera()->&Camera`/`projection()->RenderProjection` 불변 accessor와 `with_camera`/`with_projection` immutable replacement, `validate()->Result<(), PacketRenderError>`를 가진다. projection enum은 Copy이며 기존 Camera 검사와 최종 finite view-projection을 사용 전에 검증한다. RenderPacket은 private RenderView를 보관한다. 기존 new는 Perspective, `view()`/`with_view()`/`projection()`/`with_projection()`을 추가하고 camera()/with_camera는 기존 signature와 projection을 보존한다. projection finite/positive 검사와 실제 globals 계산을 연결한다. with_instances로 binding을 지울 때 model/sprite 모두 같은 규칙을 적용한다.

공개 Camera/RenderInstance/Material/PacketRendererLimits/PacketAssetLimits/PacketResourceStats/PacketAssetResourceStats/PresentedPacket/WindowReport의 struct literal은 보존한다. 모델의 packet validation을 sprite까지 포함하는 private textured-draw abstraction으로 바꾸되 model/sprite shader 의미를 enum으로 구분한다. geometry/texture/sampler/pair의 cache·counter·admission·submission 수명은 하나다.

공통 unit XY quad는 [0,1]²의4vertex(position XY+local UV, vertex당16bytes)와6개의u32 index로88 packed bytes다. normal은 sprite shader에 필요하지 않다. layout version은 legacy/model과 구분하되 기존 geometry cache/예산/retirement를 사용한다. region별 geometry를 복제하거나 PNG를 잘라 개별 texture로 만들지 않는다. PPU/pivot의 offset/scale을 authority matrix 뒤에 곱하고 region UV rect `[x/W,y/H,w/W,h/H]`를 slot별16-byte vertex row로 전달한다. local bottom V=1/top V=0인 unit UV를 이 rect에 적용한다.

UV buffer는 최초 sprite 사용 때 lazy 생성하고 최대 instance capacity×16bytes로 유한하다. 기본16,384instance에서256KiB이며 resident·queue staging·bookkeeping admission에 포함한다. sprite가 없어져도 renderer가 이 고정 buffer를 계속 소유하면 계속 charge한다. slot 재사용 때 이전 sprite UV와 ID를 비교하며 UV row가 필요한데 write하지 않는 오류를 막는다. 실제 UV queue write는 기존 `instance_write_count/bytes`에 추가로 계산한다. transform-only는128-byte instance row만, region-only는 필요한 transform/UV row만 갱신한다.

GPU tint는 기존48-byte private MaterialUniform의 base_color RGBA lane에 담는 sprite-specific conversion으로 전달한다. public Material에는 alpha field를 추가하지 않고 model/legacy alpha1 의미를 유지한다. SlotState가 비교하는 최종 MaterialUniform에 tint alpha도 포함해 opacity-only 변경이 material write로 계측된다. 별도 sprite material cache/storage를 만들지 않는다. geometry/upload packing·UV row·material 차이 계산은 write 이전 계획 단계에서 모두 검증한다.

기존 model texture upload는 full mip chain을 요구한다. common texture upload 설명을 base-only atlas(1 mip level)와 model cooked chain 모두 받도록 일반화한다. key는 실제 dimensions/RGBA/mip/layout이고 같은 payload의 alias는 하나다. source image/atlas를 매 frame hash하거나 새로 decode하지 않는다.

모든 sprite draw·quad/metadata를 기존 packet/project entity와 retained CPU/geometry 예산에 checked arithmetic으로 먼저 charge한다. 생성된 quad의 owned backing/hash 비용도 포함한다. image/pair를 model과 별도 중복 보관하지 않는다. P03.2a resident working128/hard256 MiB, geometry64/128 MiB, staging128 MiB, scratch128 MiB, bookkeeping8 MiB, in-flight3과 entry 상한을 유지한다. 어떤 private GPU metadata 증가도 기존 resident/staging/scratch 계획에 포함한다. 예산이 부족하면 queue write/state mutation 이전 Reject/Busy이고 마지막 정상 frame을 손상시키지 않는다. frame 수로 회수하지 않고 실제 completion watermark를 따른다.

## 실제 view·입력·보고

window의 pose와 projection을 하나의 private local view state로 취급한다. 입력이 없을 때 local override를 생성하지 않아 이후 authority camera 이동이 표시돼야 한다. 2D WASD는 camera right/up plane pan, drag는 pan, wheel은 vertical_size zoom이다. orbit은 2D에 적용하지 않는다. zoom은 finite `[0.001, 1_000_000]` world-unit 범위로 clamp하고 초기 authored vertical_size도 이 범위를 검증한다. 기본 near/far를 wheel로 변경하지 않는다. local override는 viewer 진단 상태이며 authority ECS를 직접 수정하지 않는다.

실제 used view는 다음 하나의 additive presentation 경로로 전달한다. 새 보고 type은 private field/accessor로 만들어 후속 관측 metadata 추가마다 공개 literal을 깨거나 wrapper를 새로 늘리지 않게 한다.

| API | 계약 |
| --- | --- |
| `RenderView` | camera+projection; `camera()`/`projection()`, 생성과 immutable replacement, validation |
| `PacketRenderer::render_packet_view_with_view` | 기존 render_packet_view와 동일 target/size/packet/loading 입력, Option<&RenderView> override를 받아 `PacketViewSubmission` 반환 |
| `PacketViewSubmission` | private actual ready packet Arc+actual used RenderView, `packet()->&Arc<RenderPacket>`/`view()->&RenderView`/`into_packet()->Arc<RenderPacket>`; submit 성공이며 GPU completion/scanout 보장은 아님 |
| 기존 `render_packet_view` | Camera-only override와 기존 return을 보존하는 adapter. projection은 실제 ready packet의 값을 사용함 |
| `PresentedView` | 기존 PresentedPacket metadata+실제로 present한 RenderView; private field, `packet()->&PresentedPacket`/`view()->&RenderView` |
| `PacketPresentationWindowOutcome` | 기존 asset-window 결과+Option<PresentedView>+final RenderView를 private 보관; `packet_outcome()`/`asset_resource_stats()`/`last_presented()`/`final_view()` 불변 accessor와 `into_asset_outcome()` 제공 |
| `run_packet_windowed_presented` | 현재 window options/packet limits/asset limits를 받는 통합 additive entrypoint. 위 outcome 반환; 기존 window entrypoint는 adapter로 유지 |

Busy가 old-ready packet을 선택하면 실제 그 packet/projection을 submission record에서 가져온다. Camera-only override에 incoming rejected/pending packet의 projection을 섞지 않는다. full local RenderView override는 명시적 viewer 상태다. window는 queue submit와 surface present가 실제 실행된 뒤에만 PresentedView를 갱신한다. no-ready Busy는 surface drop/no-present이며 last-presented metadata를 갱신하지 않는다. batch capture report의 view도 실제 capture에 쓴 packet view다. source initial 값으로 zoom 결과를 덮어쓰지 않는다.

CLI view/capture는 기존 공통 loader/packet path와 stats JSON 변환을 유지한다. same-session source watch/live capture/MCP라고 보고하지 않는다. 공통 `render_view_json` 변환을 사용한다. capture root의 `render_view`, native `last_presented.render_view`와 `window.final_render_view`는 `{camera: 기존 Camera JSON, projection:{kind:"perspective"}}` 또는 `{camera:...,projection:{kind:"orthographic",vertical_size:...}}`다. 기존 camera/FOV key는 보존하고 새 projection이 실제 투영 의미를 밝힌다. 마지막 present가 없으면 last_presented는 기존처럼 null이다.

## 검증과 분담·exit

Content owner는 binding/unique camera/IR reference/metadata charge/legacy compatibility를, renderer owner는 quad·packet·projection·GPU·window/actual view를, CLI owner는 공통 report/기존 offline smoke 확장을 맡는다. 모든 code/test/script는 Luna xhigh가 작성하고 Cargo.lock/shared target 소유자는 한 명씩 순서대로 사용한다. 새 dependency는 현재 필요하지 않다. 조정자는 MD·source 리뷰·독립 통합/실제 Mac 검사·package/PR을 수행한다.

CPU는 native/byte 동일성, unknown/kind/region/duplicate/conflict 오류, camera uniqueness/near/far/scale, source digest legacy parity, validate 재구성, 비대칭 pivot/PPU/UV/unit quad4vertex6index, stable tie order, snapshot authority·wrong session/stale와 사전 한도 reject를 검사한다. API literal 보존·with_camera/instances 의미도 확인한다.

GPU는 비대칭 colored/transparent atlas의 top/bottom·인접 region 경계·pivot/PPU·mirror, 독립 linear alpha reference(alpha0/부분/1), 겹친 sprite layer/order/tie, opacity tint, resize/aspect를 검사한다. 같은 atlas의 여러 entity/alias가 image1회·공통 unit quad geometry1회·sampler/pair1회를 공유하고 repeat는 추가 upload0, transform-only는 instance만, camera-only는 globals만 증가해야 한다. old model/legacy GPU 회귀도 유지한다. 장치 saturation은 기존 deterministic ledger 회귀를 확장하며 실제 장시간 GPU stall 성능으로 해석하지 않는다.

기존 smoke의 opt-in sprite mode를 사용하고 CPU/assets/view/models 기본 의미를 유지한다. 같은 창의 실제 authority 변경 후 표시/view JSON을 대조하고 가능하면 resize/2D pan/zoom도 실제 OS 입력과 report로 확인한다. 실제 공개 Kenney Tiny Dungeon 원본 PNG/CC0와 [provenance](../02-research/asset-validation-provenance.md)를 재사용해 화면을 확인하며 네트워크 다운로드는 CI에 추가하지 않는다. 작은 sprite 장면은 엔진 fixture이고 Terraria 구현이 아니다.

하위 gate는 source 고정→focused→workspace/Python/managed CI→실제 Metal/native/public asset→clean archive/extracted smoke→최종 다섯 PR CI·main 동일 tree까지 확인한 뒤에만 P03.2b를 체크한다. source/API 리뷰를 반영해 계약을 확정했다. 실제 구현/검사는 아직 미실행이며 이 문서로 gate를 체크하지 않는다.

## 조사 근거와 적용 범위

2026-10-16 확인: [W3C PNG 3rd edition 6.2](https://www.w3.org/TR/2025/REC-png-3-20250624/#6AlphaRepresentation)는 PNG의 unassociated alpha를 명시한다. [wgpu v27.0.1 원본](https://github.com/gfx-rs/wgpu/blob/v27.0.1/wgpu-types/src/lib.rs)의 ALPHA_BLENDING은 straight-alpha blend factors를, Rgba8UnormSrgb는 shader의 sRGB/linear 변환을 정의한다. 로컬 pinned registry source의 같은 내용을 대조했다. 이 사실을 sprite linear-alpha 계약에 적용하는 것은 다이달로스 설계 선택이며 기존 엔진에 이미 sprite 기능이 있었다는 근거는 아니다.

## 확정된 작업 순서와 반복 비용

먼저 content binding을 고정하고 focused 검사 뒤 renderer에 Cargo 소유권을 인계한다. renderer/CLI는 그동안 공개 signature에 맞춰 서로 다른 파일만 편집한다. renderer source 고정 후 CLI build/focused를 수행하고 마지막에 조정자가 broad/hardware/package를 한 번씩 검증한다. 변경/실패가 있으면 해당 범위부터 다시 확인한다.

공통 unit quad 선택은 bytes뿐 아니라 기존256 active/512 total geometry-entry 한도도 고려했다. 최대16,384 region마다 quad를 만들면 작은 sprite만으로 entry 한도를 소진할 수 있다. unit quad1개와 유한 UV row가 이 반복 자원 준비를 없앤다. 실제 upload/dirty counter와 실패를 검사하고 미리 개발 속도 개선 배율을 주장하지 않는다. no-input local override 생성 문제도 이 작업에서 고쳐 authority camera 이동을 기본 상태에서 따라가게 한다. source/API 감사와 실제 결과는 [작업 기록](../06-worklog/2026-10-16-sprite-runtime.md)에 구분한다.
