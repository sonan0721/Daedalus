# P03.3b.1 에셋 교체의 authored 호환성

상태: 아래 CPU 비교 계약 동결·구현/검증 전 · 2026-10-16

## 이 문서를 먼저 만드는 이유

[세대 전환 명세](project-asset-activation.md)의 첫 선행 조건은 새 에셋을 적용해도 기존 entity와 gameplay state를 그대로 유지할 수 있는지 판정하는 것이다. actor·renderer·audio가 각각 scene을 비교하면 지원 범위와 오류가 달라진다. 따라서 content crate의 한 비교기를 먼저 구현하고 후속 worker가 재사용한다. 이것만으로 CPU 후보가 활성화되거나 live reload가 구현되지는 않는다.

이번 계약은 P03.3b의 내부 선행 작업이다. b의 source lease·consumer handshake·commit/actual present, c의 audio 다중 bank, P03.4 반복 비용과 D1 제품 gate는 계속 미완료다. 상세 계약이 필요한 경계만 순서대로 동결하며 미결 runtime API의 구현을 병렬로 시작하지 않는다.

## 입력과 결과

입력은 기존 loader가 만든 불변 `PreparedProject` 두 개다. 함수는 파일을 읽지 않고 source를 다시 parse/cook/hash하지 않는다. 현재 ECS transform·revision·entity handle·Engine을 받지 않으며 바꾸지도 않는다. `PreparedProject`가 외부에서 mutable field로 조립되는 타입으로 바뀌면 이 전제도 재검토한다.

공개 Rust 경계는 content crate의 다음 함수·타입이다. 별도 DTO parser·runtime dependency·generic callback은 추가하지 않는다.

```rust
pub fn check_asset_replacement_compatibility(
    active: &PreparedProject,
    candidate: &PreparedProject,
) -> Result<(), AssetReplacementCompatibilityError>;
```

`AssetReplacementCompatibilityError`는 `Clone + Debug + Eq + PartialEq + Display + std::error::Error`를 제공하며 private field와 다음 getter를 둔다.

- `kind() -> AssetReplacementMismatch`
- `entity_id() -> Option<&StableId>`
- `component_id() -> Option<&StableId>`
- `asset_id() -> Option<&StableId>`

`AssetReplacementMismatch`는 `Clone + Copy + Debug + Eq + PartialEq`인 enum이다. variant는 `Manifest`, `SceneSchema`, `AssetSet`, `AssetKind`, `AssetMetadata`, `EntitySet`, `EntityName`, `EntityParent`, `ComponentSet`, `ComponentType`, `ComponentAssetRefs`, `ComponentData`, `ComparisonCapacity`다. component 관련 오류는 owning entity ID도 담는다. 누락/추가 set 오류는 대칭 차집합에서 가장 작은 StableId를 담는다. 큰 JSON value·URI·전체 expected/actual text를 error에 복사하지 않는다. StableId는 기존128 ASCII bytes 상한을 그대로 사용한다.

이 성공 결과는 source 간 호환성 판정이다. actor admission·current desired candidate·expected active generation·consumer 자원 준비를 증명하는 commit token으로 사용하지 않는다. 후속 worker는 비교한 실제 두 source의 identity와 lifecycle을 내부 operation에 묶어야 한다.

## 첫 허용 범위

| 부분 | 비교 규칙 |
| --- | --- |
| project manifest | `schema_version`, name, scene path, presentation 전체가 기존 `PartialEq` 의미로 같아야 함. camera/light/clear 변경은 이번 asset-only 경로에서 거부 |
| scene schema | 같아야 함. loader의 기존 지원 버전 검증도 유지 |
| scene revision | 달라도 허용. authored revision과 실제 ECS revision을 혼동하지 않음 |
| entity 배열 | ID set이 같아야 함. 배열 순서는 무시 |
| entity name/parent | 각각 같아야 함. rename/reparent는 일반 scene patch 범위 |
| component 배열 | entity별 component ID set이 같아야 함. 배열 순서는 무시 |
| component type | 같아야 함 |
| component asset_refs | 현재 vector 순서를 포함해 같아야 함. 순서를 set으로 해석하는 일반화는 하지 않음 |
| component data | 기존 `serde_json::Value` equality로 같아야 함. JSON object key 순서는 비교기의 별도 의미가 아니며 수치 표기를 임의 정규화하지 않음 |
| asset 배열 | asset ID set이 같아야 함. 배열 순서는 무시 |
| asset kind/metadata | 같아야 함. settings/schema/semantic 해석을 바꾸는 metadata 변경은 첫 경로에서 거부 |
| asset URI/content_hash | 변경 허용. candidate loader가 새 URI·hash·포맷·의존성·binding·한도를 정상 검증한 뒤에만 비교기에 도달 |
| 실제 mesh/glTF/GLB/PNG/atlas/WAV bytes | 변경 허용. prepared mesh/catalog·file identity·source digest가 같을 필요는 없음. 새 primitive/region/크기가 consumer에서 실행 가능한지는 별도 staging 검증 |

같은 ID의 서로 다른 kind나 component binding 변경을 단순 content update로 통과시키지 않는다. 외부 glTF node/material, atlas region 등의 payload 변화는 허용하되 authored component가 새 catalog에서 여전히 유효한지는 기존 asset-aware prepare가 확인한다. 현재 loader가 거부하는 hierarchy/미지원 component를 이 함수가 새로 지원하지 않는다.

## 비교 순서와 비용

첫 오류는 deterministic하게 반환한다. 순서는 manifest→scene schema→전체 asset ID set→ID 오름차순 각 kind/metadata→전체 entity ID set→ID 오름차순 각 name/parent/component set→component ID 오름차순 type/asset_refs/data다. 입력 배열 순서를 바꿔도 같은 mismatch 위치를 돌려준다.

전체 project/catalog/JSON을 clone하거나 canonical JSON을 새로 만들지 않는다. 정렬은 borrowed reference index를 사용하고 component index는 entity별로 재사용/회수한다. 동시에 살아 있는 index element backing의 논리적 합산 상한은4MiB다. Vec header·allocator metadata·입력 project 자체는 이 index 상한 밖이며 전체 RSS 보장이 아니다. 필요한 index 길이·bytes는 checked arithmetic으로 allocation 전에 판정하고 초과/예약 실패는 `ComparisonCapacity`로 반환한다. sort를 위해 숨은 payload 복사를 만들지 않는다.

일반 경로는 active/candidate asset·entity index와 현재 entity의 component index만 보유한다. private lower-limit seam으로 capacity 거부를 검증할 수 있지만 public 옵션/새 전역 allocator/framework는 추가하지 않는다. 이 작업은 actor 밖 CPU 준비용이며 비교 시간이 항상1ms 미만이라는 계약은 없다.

## 구현 책임과 검증

Luna xhigh가 `daedalus-content`의 새 `asset_replacement` module·root re-export와 관련 test만 소유한다. 기존 project test helper를 안전하게 재사용할 수 있으면 그 test module의 최소 변경도 허용한다. manifests·Cargo.lock·runtime·renderer·audio·CLI·scripts·CI는 이 작업에서 수정하지 않는다. 조정자는 명세·리뷰·통합/Git을 맡는다.

실제 loader로 만든 입력을 사용해 다음을 확인한다.

- unchanged와 scene revision/배열 순서만 다른 입력은 통과.
- 실제 source payload/URI/content hash가 달라 source digest가 바뀌어도 동일 authored binding이면 통과. legacy geometry와 asset-aware image/audio 중 가능한 fixture 경로를 재사용하며 결과의 source identity 차이도 assert.
- manifest/presentation·asset set/kind/metadata·entity set/name·component set/type/asset_refs/data가 달라지면 대응 kind와 bounded ID로 거부. loader가 먼저 거부하는 입력은 비교 성공 fixture로 가장하지 않음.
- 여러 mismatch와 배열 reorder에서 deterministic 첫 위치 유지. 실패 후 양쪽 입력 source identity/자료 불변.
- lower index limit·checked accounting이 payload clone 없이 `ComparisonCapacity`를 반환하고 정상 limit에서는 같은 입력이 통과.
- focused tests와 strict content Clippy·fmt, 기존 content 전체 회귀, content Wasm compile. runtime/frame/GPU/PCM/CLI 교체 성공으로 해석하지 않음.

검증 raw는 ignored `dist/project-evidence/p03-compatibility-*`의 고유 attempt 파일에 저장한다. 한 Cargo owner가 shared target을 사용한다. focused 결과를 받은 뒤 변경 범위에 필요한 통합 검사를 선택하며 MD만 바뀐 단계의 hardware/package를 반복하지 않는다.
