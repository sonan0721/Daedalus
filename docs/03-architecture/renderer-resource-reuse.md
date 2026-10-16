# renderer의 불변 geometry·instance 갱신과 자원 예산

상태: P02.2/DEV08 다음 구현 계약 초안·미구현 · 2026-10-16

## 만든 이유와 실제 반복

AI edit→관찰/capture 루프와 지속 실행 창이 매번 같은 mesh를 변환/업로드하면 CPU 작업·GPU allocation·frame 비용이 함께 반복된다. Luna xhigh의 로컬 source 감사는 현재 `PreparedScene::from_scene`의 world-space vertex 변환/flatten, `record_scene`의 vertex/index/material/global/depth resource 생성, project projection의 entity별 mesh 복제를 확인했다. window와 offscreen은 이 경로를 쓴다. CPU mesh 변환 cache만으로 이 반복이 모두 해결되지는 않는다.

이 문서는 [같은 session service](session-control-service.md)의 표시 경로와 [asset cook/residency](asset-import-cook-and-residency.md) 사이에서 CPU geometry와 장치 자원 수명을 구체화한다. 코드 채택/계측 전이며 새 renderer나 성능 개선을 이미 구현한 것으로 표시하지 않는다. 고급 재질·texture/skin·2D·post/VFX·P05 품질 gate를 축소하지 않는다.

## 소유와 additive API

renderer-owned `RenderPacket` 후보는 camera/light와 stable instance 목록을 가진다. instance는 ID, immutable `MeshHandle`, transform, material을 참조한다. handle은 stable asset ID·generation·payload hash·vertex-layout version과 `Arc` CPU data를 가진다. hash/generation을 유지한 채 source vertices를 가변 변경할 수 있게 열지 않는다.

project resource registry는 한 prepared generation에서 필요한 renderer mesh를 한 번 준비하고 snapshot마다 instance만 추출한다. 현재 content/renderer vertex type이 다르므로 최초 변환 copy는 남는다. 반복 frame마다 해당 변환·index clone·instance별 mesh duplicate를 다시 수행하지 않는다. 전체 scene를 clone한 뒤 일부를 잘라내는 구현도 피한다.

기존 public `RenderScene`/`RenderObject`/`Vertex`와 `&self` render API는 초기 호환 경로로 보존한다. 효율적인 packet 메서드는 명시적인 mutable renderer/cache 소유권을 사용할 수 있다. 공통 GPU encoding/shading backend를 재사용하고 두 개의 독립 renderer를 장기 유지하지 않는다. compatibility adapter의 ephemeral 준비와 새 packet 경로의 persistent reuse를 구분해 계측한다. old fixture를 새 project 기능이나 새 cache 효과의 근거로 대체하지 않는다.

renderer는 core/content의 owned 자료만 의존하고 runtime crate에 의존하지 않는다. runtime/session snapshot→packet mapping은 presentation bridge가 맡는다. offscreen/window는 같은 packet prepare/encode를 사용하며 다른 device 사이에서는 CPU handle/key만 공유한다. GPU buffer를 다른 Renderer/device에 가져다 쓰지 않는다.

## 첫 GPU layout 후보

geometry는 local-space position/normal indexed buffer로 보존한다. instance record는 model mat4, padded inverse-transpose mat3, material index와 padding으로 128 bytes다. normal은 nonuniform scale에서도 올바르게 변환하며 finite inverse 검증을 유지한다. CPU는 instance가 바뀔 때만 해당 record를 준비한다. vertex마다 world transform을 다시 계산하지 않는다.

첫 instance-step vertex layout은 mesh attribute 2개와 instance attribute 8개를 사용한다. material index는 명시적 attribute이며 `instance_index == material array index`라는 결합을 없앤다. 첫 구현은 mesh별 indexed draw와 안정적인 instance slot을 사용하고, draw batching/indirect/meshlet은 실제 부하를 본 뒤 추가한다.

UV/tangent·joint/weight·추가 UV/color는 versioned layout이나 별도 stream/pipeline으로 확장한다. 현재 public Vertex의 필드를 무작정 추가해 기존 construction site를 깨지 않는다. 미래 skin/morph와 2D sprite/tile은 geometry cache를 공유할 수 있어도 각각 필요한 layout/shader 의미를 유지한다.

감사한 pinned wgpu 27.0.1의 [Limits](https://docs.rs/wgpu/27.0.1/wgpu/struct.Limits.html)는 기본 vertex attribute 상한 16을 정의하지만 실제 requested device limits를 검사해야 한다. 이 첫 경로는 별도 optional GPU feature를 요구하지 않는 안이다. renderer가 `webgpu` feature를 켜지 않은 현재 설정을 이 문서로 웹 지원이라고 바꾸지 않는다. 브라우저 실제 실행과 layout/resource limit 검증은 별도 gate다.

## cache identity와 변경 종류

device-local geometry key는 payload hash+layout version이다. stable asset ID/generation은 handle의 유효성/무효화에 사용한다. 내용이 같은 no-op generation은 GPU blob을 재사용할 수 있다. source path나 CPU pointer만으로 내용 동일성을 단정하지 않는다. hash 계산을 매 frame의 전체 geometry scan으로 옮기지 않는다.

instance slot은 안정적 ID와 data revision으로 유지한다. unchanged frame은 geometry upload·instance/material write가 없고, transform-only 수정은 해당 instance record만 쓴다. material-only 변경은 geometry upload를 만들지 않는다. instance 제거/slot 재사용은 새 identity가 이전 transform/material을 물려받지 않게 한다. camera/light/global 값은 변경분과 frame 필요에 따라 갱신하며 geometry 통계와 구분한다.

texture·skin·sprite resource를 지원하면 해당 typed payload/layout/settings key와 별도 residency 예산을 연결한다. 현재 scalar material/position-normal 경로를 전체 PBR/animation 지원으로 표시하지 않는다.

## residency·in-flight·교체 진행

unique geometry entry/bytes, instance/material capacity, render-target/depth와 capture bytes를 각각 제한한다. logical expanded geometry budget과 실제 GPU resident unique bytes는 다른 수치다. 같은 mesh 두 instance는 draw work가 둘이어도 한 GPU mesh upload만 한다.

eviction은 active packet과 미완료 submission이 쓰는 자원을 보호한다. retired buffer도 hard budget에 포함한다. `Buffer::destroy`로 in-flight 자원을 즉시 지우지 않는다. GPU 참조가 사라진 시점에 실제 retire를 해제한다. cache miss/eviction은 CPU handle로부터 다시 준비할 수 있어야 한다.

초기 byte 정책은 preferred working set 64 MiB와 transition/retirement 여유를 합한 **hard unique geometry 128 MiB** 후보로 둔다. 이 중 여유는 새 generation을 준비하면서 이전 complete frame을 유지하기 위한 것이다. hard total을 넘는 숨은 overcommit을 만들지 않는다. 실제 기본값과 entry/instance 제한은 API 확정 때 source budgets와 맞춰 잠근다.

공간이 모자라면 candidate를 bounded pending/coalesced 상태로 두거나 명시적으로 거부하고 마지막 ready packet/UI를 계속 사용한다. candidate가 자체 상한을 초과한 경우와 in-flight retirement를 기다리면 되는 경우를 구분한다. max-size mesh의 old→new 교체가 영원히 진행하지 못하는 reserve/eviction deadlock도 시험한다.

[Queue::on_submitted_work_done](https://docs.rs/wgpu/27.0.1/wgpu/struct.Queue.html) callback에는 실제 poll/event-loop 진행이 필요하다. 첫 native 설계는 submission serial/completion watermark를 추적하고 nonwaiting poll로 retirement를 진행하는 안이다. window callback에서 매 frame GPU 완료를 동기 wait하지 않는다. offscreen blocking capture와 live async readback은 다른 실행 계약이다. [Buffer 수명](https://docs.rs/wgpu/27.0.1/wgpu/struct.Buffer.html)과 장치 손실/회수도 함께 검증한다. 위 API는 registry source 감사와 대조했으나 다이달로스 새 cache의 실행 근거는 아니다.

## 실패와 검증

packet 준비는 mesh/layout/indices·finite transform/material·instance ID 중복·예산을 검사하고 required geometry를 임의 cube로 바꾸지 않는다. 실패 후보를 complete packet으로 게시하지 않는다. surface/device/capture 실패는 원인을 구분하고 정상 cache hit로 계수하지 않는다.

검증은 실제 외부 project의 동일 device에서 수행한다.

- 같은 mesh의 두 instance 첫 frame: CPU 변환 1회, GPU geometry upload 1회.
- unchanged frame 반복: geometry flatten/upload와 instance/material write 증가 없음.
- 한 transform 수정: geometry upload 없음, 해당 instance write만 증가. nonuniform scale normal 결과가 기준 경로와 허용 오차 안에서 일치.
- 한 mesh payload 변경: 그 mesh upload 1회, old generation의 in-flight 안전 회수. no-op generation은 같은 내용 자원 재사용.
- material 수정/instance 제거·재사용/resize: geometry와 별도 비용, depth target은 크기/설정 변경 시 재생성.
- 작은 budget·모든 entry pinned·연속 generation·retire 지연·잘못된 geometry·device 오류: bounded pending/failure·last-good·종료 회수.

통계는 conversion/geometry upload count·bytes, instance/material/global write count·bytes, cache hit/miss·active/retired/pending bytes·eviction, frame/source identity를 남긴다. 계측 overhead도 제한한다. cold/warm·변경 종류·해상도·adapter·명령과 실제 CPU 준비 시간을 기록하고 한 번의 결과를 일반적인 속도 배율이나 144fps 인증으로 쓰지 않는다. 실제 GPU 이미지와 CPU invariant를 함께 확인한다.
