# 외부 프로젝트의 로드·실행과 공통 제작 자료

상태: P01/P03/P02 선행 구현 계약·제품 gate 미통과 · 2026-10-16

## 목적과 첫 구현 범위

`view`의 고정 장면을 더 확장하는 대신, 엔진 밖의 사용자 directory에서 project·scene·asset을 읽어 실제 ECS와 renderer에 연결한다. 엔진 CLI의 project 명령은 이 공통 library를 소비한다. 이 첫 구현은 **P01 하위 기반**이다. C# game module·live project reload·완성 hierarchy/prefab·일반 import/cook·MCP·save/game export까지 구현했다고 표시하지 않는다. 해당 미통과가 남아 있으므로 D1도 계속 열려 있다.

첫 저장 형식은 JSON이다. `daedalus.project.json`은 `schema_version: 1`, `name`, `scene` 상대 경로와 선택적 `presentation` 설정(camera/light/ambient/clear)을 가진다. scene은 기존 `SceneDocument` schema 1을 그대로 쓴다. 다른 renderer component나 asset type을 임의 cube로 바꾸지 않는다.

## 소유권과 자료 구조

| 계층 | 소유/책임 | 금지 |
| --- | --- | --- |
| content/project | manifest·typed authoring component·CPU mesh·공통 semantic validation·prepared project | live ECS·GPU 객체·게임별 fixture |
| native source adapter | project-root 경로 해석·bounded file read·digest 검증 | main/window callback I/O·remote URL 자동 다운로드 |
| runtime/project | PreparedProject→실제 Engine/entity handle map·tick·권한 command·report | renderer/GPU 의존·ECS index를 stable asset/entity ID와 동일시 |
| renderer/project | prepared asset와 동일 세션 snapshot→RenderScene projection | 프로젝트 재로드·다른 엔진 세션 생성·fixture fallback |
| CLI/project | init/validate/inspect/run/serve/capture orchestration | 명령마다 별도 project parser/validation 구현 |

`PreparedProject`는 원본 manifest/scene, typed entity 목록, StableId→CPU mesh, 읽은 file identity/digest 목록을 소유한다. native adapter를 쓰지 않는 byte-based 준비 진입점도 둬서 후속 web loader가 filesystem 계약을 가져가지 않게 한다. 이를 구현했다고 web 지원으로 표시하지 않는다.

준비한 묶음은 private field와 읽기 전용 getter를 사용한다. source hash를 유지한 채 외부에서 mesh/scene을 가변 변경할 수 있게 두지 않는다. 콘텐츠를 편집하려면 새 byte 입력을 검증해 새 prepared generation/identity를 만들고, 런타임 entity transform의 변경은 별도의 authority revision과 snapshot에 기록한다.

공통 public type 이름은 `ProjectManifest`, `ProjectPresentation`, `ProjectTransform`, `ProjectMesh`, `ProjectVertex`, `ProjectMaterial`, `PreparedEntity`, `PreparedProject`, `ProjectError`, `ProjectLimits`다. validation은 원본과 semantic/component 오류의 파일·JSON 위치를 진단한다. 준비 실패 뒤 부분 project를 사용하지 않는다.

## 첫 지원 component/asset

- `daedalus.transform`: translation/rotation/scale. 생략한 entity는 identity. 유한 값·유효 nonzero quaternion·nondegenerate scale을 검증한다.
- `daedalus.mesh_renderer`: `mesh` stable asset ID와 optional scalar `material`(base_color/metallic/roughness/emissive). `asset_refs`와 actual mesh ID 일치를 확인한다.
- `daedalus.mesh` asset: project-root 상대 URI, schema 1 indexed triangle JSON(`vertices: [{position,normal}], indices`). 이것은 엔진의 CPU mesh 원본 형식이며 glTF/texture/sprite importer를 대체한다고 부르지 않는다.
- 이 단계는 root entity의 3D opaque mesh만 실행한다. unknown component/asset·parent/hierarchy 등 지원하지 않는 의미는 위치 있는 unsupported 오류다. 일반 scene 구조 검증 성공과 실행 가능 component 지원을 구분한다.

presentation은 소유한 camera/light 값이며 renderer의 초기 기능만 노출한다. texture·skin·2D sprite 등의 필드를 이름만 받아 무시하지 않는다. 기본 empty project는 유효하고 빈 화면을 만든다. 자동으로 상자/지형/gameplay를 주입하지 않는다.

## source 읽기·identity·상한

manifest/scene/asset 모두 프로젝트 root 기준 상대 경로로 처리한다. absolute·`..`·URI scheme·directory·root 밖 symlink를 거부한다. root 내부 symlink를 허용할 때도 canonical 대상이 root 안인지 확인한다. load 시 원본을 바꾸지 않는다.

각 file read는 상한+1로 제한하고 전체 file/byte·entity·mesh/vertex/index 예산도 합산한다. 초기 기준은 manifest 64KiB, scene 16MiB, mesh file 8MiB, total 64MiB, entities 16,384, mesh assets 256, 총 vertices 1,000,000·indices 3,000,000 이하다. byte parser에도 같은 상한을 적용한다. public limits는 validated positive 범위다.

읽은 byte의 SHA-256과 상대 source ID를 기록한다. `content_hash`가 있으면 `sha256:<64 lowercase hex>` 의미를 검사하고 불일치를 거부한다. 현재 lock에 있는 `sha2 0.10.9`를 재사용한다. 이 identity 기록은 향후 cook/cache 입력이며 아직 incremental cooking이나 watcher가 아니다.

## runtime과 snapshot

`ProjectSession::new(prepared, EngineConfig)`는 준비 내용을 재검증한 뒤 Engine을 만들고 entity를 실제 spawn한다. stable authored ID→session EntityHandle map을 보존한다. source scene revision과 authority entity revision을 혼용하지 않는다. command는 기존 WorldCommand/expected revision/apply tick 경로를 사용한다.

공개 `engine()/engine_mut()`, `project()`, `entity_handle(stable_id)`, `entities()/report()` 경로를 제공하고 map/report에 현재 transform·revision을 포함한다. renderer는 같은 Engine이 게시한 RenderSnapshot만 사용한다. projection 때 session/handle 누락·다른 session·stale entity를 임의 identity로 바꾸지 않는다.

첫 source ingest와 decode는 session 시작 전에 수행한다. 전체 replace/reload·save는 별도 계약이며, 기존 world를 반쯤 바꾼 뒤 load 실패를 성공 처리하지 않는다. 장기적인 async asset load/cook은 task와 유한 결과 credit에 연결해야 한다.

## CLI와 AI 사용

명령은 `project init <directory>`, `project validate/inspect/run/serve/capture <directory>`를 사용한다. `run`은 명시적 bounded tick 수를 실행하고 실제 stable ID/handle/state를 JSON으로 반환한다. `serve`는 기존 bounded JSONL 동작과 같은 Engine을 유지하고 `project_info`로 authored ID map과 source revision을 조회하게 한다. 기존 bare `serve`는 그대로 사용할 수 있고, 그 세션의 `project_info` 요청에만 project 없음 오류를 명시한다.

`capture`는 capture feature에서만 제공한다. 해당 project의 실제 mesh·material·snapshot을 renderer에 넘기고 tick·session·scene revision·source digest·adapter·출력 size를 report한다. renderer fixture를 호출하지 않는다. batch capture 명령은 새 session을 여는 것이므로 별도 실행 중인 `serve`의 frame이라고 표시하지 않는다. 같은 visible session의 input/wait/live capture·MCP는 후속 P02 인수 항목으로 남긴다.

`project_info`는 authored ID binding과 source identity를 유한한 `offset/limit` page로 반환한다. 전체 entity JSON을 매 요청마다 복사한 뒤 잘라내지 않는다. source metadata를 현재 transform/tick처럼 보고하지 않으며, 현재 tick/state가 필요하면 동일 Engine에서 읽는다. renderer projection도 expanded instance의 vertex/index/byte 합계를 먼저 검사한 뒤 geometry를 복사한다. 원본 asset 한 개가 작아도 많은 instance가 제한을 우회하지 않게 한다.

기존 top-level fixture `capture`는 진단용으로 보존하며 `project capture`와 구분한다. renderer는 core/content의 소유 데이터·session ID·stable binding map·snapshot을 받으며 runtime crate 자체를 의존하지 않는다.

`init`은 비어 있는 data-only project를 만들며 대상 directory를 덮어쓰지 않는다. generated game code·asset pack·sample game을 엔진 repository에 넣지 않는다. init 자체가 C# 게임 작성/실행 기능을 완성한 것은 아니다.

## 검증과 분담

엔진 밖의 임시 directory에서 작성한 실제 mesh/scene을 load→ECS 조회→권한 command→step→projection→실제 GPU capture로 연결한다. source 변환과 validate/run/capture가 같은 parser를 쓰는지 확인한다. unsupported·root escape·hash mismatch·과대 입력·잘못된 geometry·다른 session snapshot은 실패해야 한다. 기존 CLI/bridge 기능도 회귀 검사한다.

Luna xhigh의 content 담당은 `crates/daedalus-content/**`, runtime 담당은 `crates/daedalus-runtime/**`, CLI/projection 담당은 `crates/daedalus-cli/**`, `crates/daedalus-renderer/**`, Cargo.lock을 단독 소유한다. 공개 type/API를 먼저 공유하고 focused Cargo 검사를 순서대로 수행한다. 조정자는 문서·리뷰·독립 실행·Git을 담당한다. P01 전체·D1 완료 여부는 제품 인수표를 따른다.
