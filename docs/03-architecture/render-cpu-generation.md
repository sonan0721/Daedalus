# P03.3b.3.2a Renderer CPU generation 계약

상태: CPU 계약 동결·source 작성/통합 검증 중 · 2026-10-17

## 범위

[staged renderer](staged-render-asset-generation.md)의 CPU 준비·검증·실제 backing 회수만 동결한다. GPU stage/submit/window API는 이 slice에 없다. [runtime consumer](project-generation-consumer.md)의 표준 source pin을 host가 전달하며 renderer는 runtime에 의존하지 않는다. 이 slice만 통과해도 b.3/P03/D1은 미완료다.

현재 `ProjectRenderResources::prepare/extract*`와 `RenderPacket` 생성/검증 API는 보존한다. 새 managed 경로와 legacy 경로가 같은 준비·검증 의미를 사용한다. 기존 API 전체가 새 registry로 자동 추적된다고 주장하지 않는다. 새 host는 반드시 managed 경로를 사용한다.

## 공개 경계

| API/type | 동결한 책임 |
| --- | --- |
| `SourcePin::new(Arc<dyn Send + Sync>)` | renderer-local opaque pin. source만 보유하며 renderer owner/registry를 역소유하지 않음. Debug는 payload를 출력하지 않음 |
| `PacketCpuOwner::new(cpu_limits, packet_limits, asset_limits)` | 새 `PacketCpuLimits`와 기존 두 limit type을 검사. 기존 public struct literal에 field를 추가하지 않음. owner는 non-Clone이며 host CPU worker가 독점 사용 |
| `prepare_generation(project, generation, session, authored_handles, source_pin, cancelled)` | 실제 borrowed PreparedProject·기존 BTreeMap binding으로 준비. 성공은 opaque checked `CpuGenerationId`. 새 import/authority/device 없음 |
| `extract_validated(id, snapshot, publication_sequence, view, cancelled)` | 실제 RenderSnapshot과 별도 publication sequence로 생성·같은 validator 실행. opaque non-Clone `CpuFrameHandle`을 Arc로 반환. 기존 snapshot sequence는 authority identity로 따로 보존 |
| `stage_input(id)` | opaque resource-only `PacketStageInput`. session/generation/digest·canonical requests/counts·정확한 limits·pin을 보유. tick/pose/가짜 packet 없음 |
| `retire_generation(id)` | generation을 회수 대상으로 표시. known retiring은 idempotent, 회수/미발급 ID는 typed stale. 이 호출 자체가 final free/반환 byte의 증거가 아님 |
| `try_reclaim()/status()` | 실제 worker uniqueness 확인·회수 진행과 작은 사용량/slot metadata. held alias는 Pending/Busy와 원래 charge를 유지 |

generation/frame/stage-input은 registry와의 순환 참조가 없다. renderer-local generation ID는 runtime candidate ID가 아니다. host가 실제 Initial/Candidate identity와 대응한다. renderer의 추후 PacketStageId도 이 원칙을 따른다. counter는 checked 증가하며 exhaustion은 영구 typed 오류다.

`CpuFrameHandle`은 immutable packet과 private validation을 보유하고 source identity·publication/authority sequence·tick·view를 조회한다. packet의 borrowed 조회는 허용하되 validation을 boolean으로 우회하거나 mutable packet/validation을 노출하지 않는다. 신뢰된 native caller가 borrowed RenderPacket을 clone할 수 있으므로 그 independent leaf도 추적한다. renderer 내부 drawing adapter는 정확히 일치하는 packet/asset limits fingerprint의 검증 결과만 소비한다.

RenderPacket에 additive optional authority-snapshot sequence getter를 둔다. legacy constructor/extract는 기존 sequence 의미와 API를 유지하며 알 수 없는 explicit authority identity는 None이다. managed 경로만 실제 snapshot sequence와 전달받은 publication sequence를 각각 넣는다. 기존 transition/identity 검사를 완화하지 않는다.

## 예산과 admission

| CPU lane | 기본 per-item | aggregate/slot |
| --- | --- | --- |
| prepared metadata | 8MiB/generation | 16MiB, generation 두 슬롯 |
| legacy Mesh Vec 실제 복사 | 64MiB/generation | 128MiB |
| frame backing | 16MiB/unique backing | 48MiB, 최대3개 |
| 한 CPU 작업의 scratch | 16MiB | owner 하나의 임시 peak |

이 숫자는 동결한 논리 장부의 기본 상한이며 allocator/RSS/실행 시간 보장이 아니다. source imported geometry/image/mip는 기존 source/cooked 장부에서 센다. renderer wrapper·복사본·추가 map만 여기 센다. 동일 backing alias는 byte/slot을 중복 청구하지 않으며 회수는 막는다.

두 generation 슬롯은 active+candidate 또는 active+retiring이다. cancelled/failed partial도 실제 별칭이 남으면 retiring을 점유한다. 외부 alias를 유지한 채 세 번째 generation을 숨겨 준비하지 않는다. frame 최대3개에는 ready·새 current·이전 표시가 보유한 실제 backing을 포함한다. 차지할 여유가 없으면 효과 전 Busy다. per-item/hard 한도 초과·0/overflow·최소 진행 단위도 담지 못하는 config는 영구 typed budget/config 오류다. 한 항목을 영원히 Busy로 반환하지 않는다.

후보 Ready를 보고하기 전 첫 committed frame의 자리가 필요하므로 prepared generation은 첫 frame의 논리 byte/slot reservation을 포함한다. 실제 frame이 생기면 같은 예약을 actual backing charge로 전환해 중복 청구하지 않는다. owner는 이 예약을 다른 generation frame에 소비하지 않는다. retirement/cancel 때 아직 사용하지 않은 예약은 반환한다. GPU Ready 보고와 actor Commit은 후속 host에서 이 예약을 실제로 확인한다.

최대16384 instances와 metadata8MiB는 독립 제한이다. 모든 문자열/모델/스프라이트 조합이 최대 instance 수까지 들어간다는 약속은 없다. 현재 두 sidecar가 전체 expanded count로 각각 예약되는 중복은 실제 model binding 수와 sprite binding 수로 나눠 준비·estimator에 함께 반영한다. 이 작은 변경의 기능/byte 차이를 검사하며 일반적인 속도 향상을 추정하지 않는다.

## estimator와 실제 leaf

checked `size_of`·문자열 정렬·요청 capacity로 할당 전 preflight하고, 실제 Vec/HashMap capacity와 맞춰 admission을 확정한다. 기존 HashMap 논리 estimator의 bucket/load factor/control/slack 의미를 공유한다. capacity reconcile에서 상한을 넘으면 큰 값이 UI로 나가기 전에 CPU owner에서 실패·회수한다. scratch에는 Vec→Arc 겹침, snapshot index, unique ID/geometry·normal map 생성과 준비 traversal의 일시 값도 포함한다.

canonical registry는 handoff 전에 실제 공유될 Arc를 등록한다. ProjectRenderResources root/binding Vec·model draw slices·MeshResource와 legacy Vec·ModelTextureUpload·model/sprite maps·sprite order, frame instances·`Arc<OnceLock<...>>` normal cache·private PacketValidation의 geometry/normal map root를 포함한다. normal cache가 lazy 초기화되기 전에도 최대 필요한 비용과 canonical root를 확보한다. stage input/resource plan의 independent mesh/texture aliases도 같은 장부에 남는다.

parent map/template/frame validation을 먼저 회수한 뒤 leaf를 회수한다. Sized Arc는 worker `try_unwrap`, unsized Arc slice는 worker `get_mut`로 strong alias와 Weak 부재를 확인한 뒤 drop한다. strong_count 관측만으로 마지막 free를 허용하지 않는다. partial prepare/extract 실패·취소에서도 canonical roots를 worker에서 회수한다. source pin은 해당 generation/frame의 실제 leaf 회수 뒤 반환한다. 다른 generation으로 GPU content key를 재사용하는 것과 CPU leaf를 공유하는 것은 별개다.

managed generation은 crate-private의 opaque resource-set token을 한 번 발급한다. 같은 canonical token을 StageInput과 해당 generation에서 CPU 검증한 FrameHandle이 공유하며 caller가 생성하거나 바꿀 수 없다. token 자체는 resource map/source/backing을 소유하지 않는 작은 별도 Arc이고, GPU Ready 뒤에는 이를 보유해 첫 frame의 resource-set 동일성을 O(1)로 대조한다. session/generation/digest·정확한 limits 검증도 유지한다. 문자열 ID만 같거나 같은 source를 다른 CPU owner에서 준비했다는 이유로 token을 같게 취급하지 않는다.

이 token은 UI에서 expected resource 집합과 draw를 다시 중첩 순회하는 일회성 비용을 없애기 위한 작은 연결이다. CPU owner가 한 번 준비한 immutable generation에서만 frame을 검증/발급한다는 private constructor 불변식으로 검증을 재사용한다. 실제 token Arc·registry slot·frame field 비용은 prepared/frame estimator에 반영하고 canonical root로 등록해 alias/Weak가 남으면 charge도 유지한다. 다른 token/owner의 frame 거부·같은 generation의 새 pose 통과·token retention/reclaim을 검사한다. compile/실제 GPU 검증 전 구현됐다고 보고하지 않는다.

managed generation은 실제 binding과 model draw ordinal을 extraction 순서대로 순회해 dense `Arc<HashMap<RenderInstanceId, usize>>` slot assignment도 CPU에서 한 번 만든다. StageInput과 해당 generation의 frame/validation은 같은 canonical Arc를 공유한다. 초기 GPU에는 active slot이 없고 asset 변경으로 expanded draw 수/ID도 바뀔 수 있으므로 기존 active map을 후보의 첫 frame에 강제하지 않는다. map의 실제 capacity·Arc·registry 항목은 prepared metadata에 한 번 청구하고 frame에는 공유 필드 비용만 센다. generation leaf로 등록해 별칭/Weak가 남으면 준비 장부도 유지한다. public mutable map이나 caller 생성 경로를 열지 않는다. initial·expanded topology 변경·같은 generation pose·별칭 회수를 검증한다.

## 진행·취소와 slice 한계

모든 무거운 준비/추출/validation은 host CPU owner에서 실행한다. coordinator/UI/authority actor에 옮기지 않는다. source/entity/geometry/texture 범위를 순회하는 취소 checkpoint를 두며 invalidated 작업은 새 frame/stage input을 handoff하지 않는다. owner의 partial root는 취소 뒤에도 회수 경계까지 남는다. stage 요청마다 thread를 만들지 않는다.

이 첫 CPU API는 동기 owner 작업이다. checkpoint가 있다는 이유로 진행 중인 prepare를 frame 작업이 선점한다고 주장하지 않는다. host 통합 전에 candidate 준비 중 최신 frame 우선 처리·실제 pose 갱신 지연을 확인하고, 큰 작업이 frame을 지연시키면 같은 준비 코드를 bounded continuation으로 나눈다. 이 진행 검증은 b.3 인수의 미통과 조건으로 유지하며 UI가 old-ready를 그린다는 결과로 대신하지 않는다. 전체 초기 CPU load 이전의 loading UI도 별도 D1 필수다.

## 소유권과 필수 검증

Luna xhigh 한 명이 renderer `packet.rs`, 필요 시 private `packet_cpu.rs`, lib exports와 해당 CPU tests를 소유한다. manifest/lock·runtime·packet_renderer/window·CLI·CI는 수정하지 않는다. GPU 쪽이 공통 planner를 연결할 때 필요한 private ResourceRequests projection만 현재 planner가 읽는 canonical 집합으로 동결해 공유하며, GPU 구현은 별도 source task다.

실제 PreparedProject와 snapshot으로 legacy/model/sprite/empty의 같은 출력·독립 publication identity·resource-only input을 검사한다. 잘못된 binding/session/view/limit는 기존과 같이 실패한다. metadata 실제 capacity와 awkward bucket 경계·두 map의 실제 수·frame의 두 normal map·16MiB scratch·별도 legacy copy의 boundary/overflow를 검사한다. maximum count 자체가 모든 예산 통과라는 테스트는 쓰지 않는다.

실제 RenderPacket clone/leaf alias와 stage input을 잡아 retire/reclaim이 charge를 유지하는지, 외부 alias를 놓은 뒤 worker에서 final drop/회수가 되는지 확인한다. 두 generation·3 frame/reservation 포화·empty source pin·취소/partial failure·연속 replacement와 stale ID를 포함한다. 소멸 관찰용 테스트 pin/실제 별칭으로 drop thread를 확인하되 GPU completion을 모의 성공으로 인수하지 않는다. focused 실제 실행 수·default/no-default renderer CPU·strict Clippy/scoped fmt 뒤 통합한다. GPU·window·native host와 전체 워터폴은 후속 인수다.
