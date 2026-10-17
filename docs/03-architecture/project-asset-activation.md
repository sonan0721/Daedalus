# P03.3b/c 에셋 세대의 staged activation과 회수

상태: P03.3b.1 로컬 선행 검증·b.2 headless 계약 동결/구현 중. render/audio handshake·전체 b/c 인수 전 · 2026-10-16

## 목적과 선행 조건

[P03.3a](project-asset-preparation.md)는 같은 session에서 다음 CPU 후보를 준비한다. 이 문서는 그 후보를 기존 entity·현재 gameplay state를 유지하며 실제 render/audio consumer에 적용하는 다음 계약이다. [PR14](https://github.com/sonan0721/Daedalus/pull/14)의 verified main `275ea360fb53dde3713aca84910781a4c895a8e4`를 출발점으로 삼는다. CPU 준비, actor commit, device 자원 준비, 실제 표시와 backing 회수는 서로 다른 관찰 지점이다.

이번 경로는 **scene entity/component topology를 보존하는 에셋 교체**다. 일반 scene patch·entity 추가/제거·reparent·게임플레이 module reload·watcher는 별도 계약이다. 전체 P03/D1/D2/D3는 열려 있다. 이 초안이나 이름 있는 API만으로 구현을 체크하지 않는다.

현재 구체 구현 범위는 [authored 비교기](asset-replacement-compatibility.md)와 그 다음 [headless CPU 세대 계약](headless-asset-generation.md)이다. headless의 같은 Engine commit/worker 회수를 먼저 검증하고, 실제 renderer/audio consumer의 장치 준비·표시·voice 수명을 이어서 연결한다. 아래 render/audio 상세 미결을 headless factory가 지원한 것으로 우회하지 않는다.

renderer의 source 확인·작업 단위·pin/종료 owner graph와 미결 API는 [b.3 상세 초안](staged-render-asset-generation.md)에 모았다. b.2 인수와 상세 동결 전에는 renderer/consumer 코드를 배정하지 않는다.

## 확인한 현재 경계

| source | 현재 동작 | 필요한 변경 |
| --- | --- | --- |
| runtime `ProjectSession` | immutable project Arc와 하나의 Engine/StableId→handle map | Engine을 새로 만들지 않는 source/binding 교체 seam |
| `session_control` | frame의 project generation1, preparation 요약과 audio sidecar 보유 | monotonic committed generation·공통 replay·consumer 준비 확인 |
| `project_preparation` | cache owner·private ready payload가 worker에 남음 | payload export 대신 bounded handoff/retirement ownership |
| renderer `ProjectRenderResources` | CPU 준비 때 mapping·geometry/texture descriptor 생성, extract는 현재 pose 사용 | 기존 handle map을 쓰는 off-actor staging·세대별 resource 수명 |
| `PacketRenderer`/window | redraw에서 GPU admission; Busy는 old ready draw, `present()` 뒤 actual identity 기록 | 명시적 candidate staging·복구 가능한 candidate 오류·actual generation ACK |
| `ProjectAudioRuntime` | bank1개, voice alias index; Kira가 Frame Arc를 유지 가능 | 같은 backend의 staged bank·voice generation·active/retired Frame 장부 |

위 사실은 read-only 코드 감사다. 상세 source map은 [후속 감사](asset-import-cook-and-residency.md)에 있다. 현재 두 consumer 모두 별도 staged-generation 슬롯을 제공하지 않는다.

## 먼저 고정하는 불변 조건

1. 같은 SessionId·Engine·EntityHandle·현재 transform/revision·camera entity pose를 유지한다. authored initial transform으로 world를 다시 만들지 않는다.
2. prepare epoch와 committed generation은 다른 단조 증가 ID다. 새 commit은 expected-active generation/source와 current candidate를 검증한다. u64 공간 고갈은 효과 전 오류이며 ID를 재사용하지 않는다.
3. 큰 import·topology 비교·mesh packing·PCM Frame 변환은 actor/UI callback에서 하지 않는다. 같은 worker/cache/importer와 기존 render/audio backend를 재사용한다.
4. 한 후보의 준비·staging·표시·실패를 같은 bounded operation 상태로 추적한다. ticket ACK가 자원이나 old voice의 회수를 뜻하지 않는다.
5. CPU-ready·consumer-staged·actor-committed·submitted·actual-presented·retired를 구분한다. surface present나 audio callback을 actor와 함께 전역 원자적으로 교체한다고 보장하지 않는다.
6. 실패/취소가 commit 전이면 정상 active source·packet·audio bank를 유지한다. commit 뒤 표시 지연/장치 실패는 실제 committed/presented 상태를 각각 보고한다. 현재 gameplay state를 과거 snapshot으로 되감아 rollback하지 않는다.
7. 모든 retained owner는 예산을 유지한다. 새 준비를 위해 아직 사용 중인 packet·looping voice·callback backing을 해제된 것으로 계산하지 않는다.

## 첫 asset-only 호환성 규칙

이 경계의 상세 field·revision/order·오류·index 한도·Rust API는 [P03.3b.1 호환성 계약](asset-replacement-compatibility.md)으로 먼저 동결했다. 아래 표의 초기 방향보다 상세 계약을 우선한다. 이 CPU 선행 구현의 완료가 뒤의 lease/commit/presentation/audio 계약을 동결하거나 구현한 뜻은 아니다.

초기 activation은 보수적인 semantic 비교를 사용한다. 비교·정렬/해시는 off-actor에서 수행하고, actor commit은 검증된 identity·현재 generation·작은 handle 정보를 확인한다. fingerprint만으로 소유권·현재 state 검증을 대체하지 않는다.

| 입력 변경 | 첫 경로의 방향 |
| --- | --- |
| 외부 PNG/WAV/glTF/GLB·atlas/legacy mesh payload | 기존 asset kind/ID와 runtime binding을 만족하면 허용. 크기·primitive/region 변화는 기존 importer/consumer 한도를 다시 통과 |
| entity StableId·component ID/kind·binding/asset_refs·parent 관계 | 동일해야 함. 추가/제거/재연결은 명시적 incompatible topology |
| authored entity transform 또는 component data | 첫 경로에서는 동일해야 함. 실제 world transform은 언제나 현재 authority 값을 사용 |
| camera component/projection 설정 | 첫 경로에서는 동일해야 함. camera pose는 현재 ECS에서 추출. 일반 camera patch는 별도 경로 |
| asset descriptor settings/source path | 같은 asset ID/kind와 binding을 유지하는 범위의 허용 정책을 API 동결 전에 확정. 지원하지 않는 변경은 명시적 오류 |
| manifest/scene schema·runtime adapter 종류 | 기존 지원 범위만 사용. 미지원 component를 무시하지 않음 |
| scene revision·entity/component 배열 순서 | source identity와 semantic topology를 구분. revision/순서만 바뀐 입력의 정확한 비교 규칙은 테스트로 동결 |

현재 source는 여러 파일을 차례로 읽은 immutable captured graph다. watcher debounce나 source hash를 원본 디스크의 동일 순간 transaction으로 해석하지 않는다. commit은 그 captured candidate ID/digest를 대상으로 한다.

## ownership와 handoff

| 소유자 | 유지하는 것 | 금지할 비용/책임 |
| --- | --- | --- |
| runtime preparation worker | cache·captured candidate·topology 결과·project lease 회수 | 두 번째 cache owner·동시 importer·무한 candidate queue·renderer dependency 추가 |
| renderer 쪽 CPU staging owner/coordinator | move-only candidate를 빌려 render CPU resources 준비·off-actor 폐기 | runtime worker에 renderer 작업을 숨기거나 범용 callback framework 추가 |
| 같은 session actor | live world·generation/replay·작은 commit 상태·활성 binding identity | 파일 read/parser·큰 자료 복사·worker/device join·마지막 큰 CPU backing의 동기 해제 |
| render/device owner | GPU resource staging·submission completion·actual presentation | 새 authority/새 GPU device를 후보마다 생성, GPU completion blocking wait를 UI에 배치 |
| 같은 audio backend/owner | 현재/retired bank mapping·voice identity·actual backing 보유 관찰 | reload마다 native stream/backend 재생성, voice reap만으로 callback Arc 해제 단정 |
| public control client | 유한 metadata/ticket·operation ID | PreparedProject/PCM/image payload를 status로 export |

P03.3a의 worker-local payload를 그냥 actor로 이동하면 마지막 Vec/Arc 해제가 actor에서 발생할 수 있다. 따라서 handoff에는 보유 budget·CPU cleanup owner·세대 ID를 함께 묶는 lease가 필요하다. 임의 clone 가능한 payload 반환 함수를 먼저 추가하지 않는다. 외부 caller의 명시적 추가 allocation은 engine-owned 장부와 구분하며, 엔진 내부 경로의 backing을 장부 밖으로 빠뜨리지 않는다.

추가 source 리뷰는 초기 source의 강한 참조도 찾았다. 현재 preparation service·port가 각각 initial project Arc를 보유하고 CLI coordinator도 source를 유지한다. actor의 project만 바꾸면 첫 generation이 회수되지 않는다. startup source identity 검증을 유지하면서 post-bind 참조를 해제/약화하거나, 기존 a API를 보존하는 새 소비형 owner/factory를 제공하는 lifecycle을 먼저 동결한다. getter가 가리키는 값을 조용히 없애거나 API를 panic하게 바꾸지 않는다.

worker의 active cooked 장부도 현재 초기 source 값으로 고정돼 있다. commit/retire 시 generation별 charge가 정확히 이동해야 한다. 후보를 queue로 넘기거나 취소 표시하는 시점에 byte quota를 먼저 돌려주지 않는다. retirement slot이 가득 찼을 때 commit 전에 공간을 예약하고 명시적 Busy를 반환할 수 있어야 한다. 종료/Drop 경로도 큰 backing을 caller thread로 떠넘기지 않는지 검증한다.

회수의 구체 후보는 worker가 모든 engine-owned project의 reclamation Arc를 유지하고 `Arc::try_unwrap` 성공 뒤 큰 project를 worker에서 파괴하는 방식이다. 실패 Arc는 장부와 함께 유지하며 `.ok()`로 버리지 않는다. source 단독 소유권과 초기 참조 consume·panic/stop 정리 순서는 아직 다음 native owner 계약에서 동결할 사항이다. [공식 Arc 계약](https://doc.rust-lang.org/std/sync/struct.Arc.html#method.try_unwrap)은 이 선택의 근거이며 refcount를 한 번 읽은 것만으로 회수 완료를 보고하지 않는다. worker의 기존 outer catch 안에만 backstop을 두면 panic unwind 때 먼저 사라질 수 있어, failure guard와 actor 종료·실제 final drop도 함께 검토한다.

read-only 리뷰의 API 후보는 non-Clone opaque `PreparedProjectLease`와 단일 bounded handoff slot이다. lease에는 candidate ID·memory charge·borrowed source view를 묶고 commit/폐기까지 reservation을 유지한다. `try_take_candidate`의 정확한 visibility·receiver/coordinator 경계와 `project()` borrow가 기존 Clone API에 노출하는 범위는 아직 동결하지 않았다. renderer/runtime 사이에 현재 없는 dependency를 가정해 crate-private 메서드를 다른 crate에서 호출하는 signature를 확정하지 않는다.

one-running/one-latest 준비 정책을 유지한다. staged/retired 자원이 다음 후보와 겹칠 때의 합산 예산을 먼저 확보한다. 확보할 수 없으면 retryable Busy/Capacity를 반환하거나 최신 metadata만 보류한다. 무제한 source를 미리 읽거나 old consumer를 파괴해 공간을 만들지 않는다. staged 후보가 취소/교체되어도 제출된 GPU 작업과 audio backing이 실제 해제될 때까지 charge를 유지한다.

## 상태 전이와 commit

계약의 동작 순서는 다음과 같다. public operation/type 이름과 wire shape는 아래 동결 목록을 마친 뒤 별도 표로 확정한다.

```text
CpuPrepared
  → CompatibilityValidated
  → CpuResourcesStaged
  → RequiredConsumersReady
  → ActorCommitted(generation N)
  → RenderSubmitted / AudioBankSelected
  → ActualPresented(N)
  → OldResourcesRetired
```

RequiredConsumers는 실제 연결된 consumer를 뜻한다. headless CPU service에 GPU 준비를 요구하지 않고, 창/audio가 붙은 service에서 그 consumer를 생략해 성공시키지도 않는다. renderer-neutral runtime과 별도 renderer crate의 dependency 방향을 유지하며 CLI 전용 activation engine을 만들지 않는다.

commit은 공통 world/audio/preparation command ledger를 재사용한다. admission·staging 수락·commit 적용·actual presentation의 결과를 구분한다. 효과 전 오류는 caller ID를 소비하지 않고, 수락한 operation은 같은 ID 재시도로 staging/commit을 다시 수행하지 않는다. 효과 후 actor panic은 기존 OutcomeUnknown 의미를 유지한다. 새로운 replay ledger/정상·Step queue를 별도로 만들지 않는다.

commit 직전 stale expected-active·candidate 취소/새 desired·consumer 실패를 다시 확인한다. commit을 수락한 뒤 오래된 worker/device callback이 최신 상태를 덮지 못하도록 callback에도 session/candidate/generation을 붙인다. 물리/SDK 등 미래 consumer가 생기면 관련 asset 교체에 필요한 준비 책임을 같은 경계에 등록해야 한다. 현재 지원하지 않는 component를 silently 통과시키지 않는다.

## 표시와 실패의 의미

actor의 generation N이 commit돼도 실제 surface가 아직 N-1을 보일 수 있다. report는 committed source와 actual presented source를 각각 기록한다. `latest_frame`을 present ACK로 사용하지 않는다. 이전 packet을 재표시하거나 old asset bindings로 최신 pose를 추출할 때의 identity 규칙도 명시해야 한다. 기존 coordinator의 첫 source 고정 검사를 단순 제거하면 이 차이가 사라진다.

candidate의 parse·topology·CPU/GPU 예산·device staging 실패는 기존 active 자원을 유지하며 bounded 진단으로 끝난다. 기존 일반 draw의 fatal error를 모든 candidate 오류에 그대로 적용하지 않는다. 반대로 device loss·surface fatal failure를 성공으로 바꾸지 않는다. commit 후 장치 실패의 retry/보존·명시적 복구 범위를 구현 전 동결하고, committed generation을 조용히 감소시키지 않는다.

GPU staging은 기존 device와 completion tracking을 사용한다. CPU upload descriptor 준비와 device 생성/업로드를 분리하고 한 번의 UI callback에 전체 import를 넣지 않는다. 실제 upload 작업량·staging bytes·completion/backpressure를 계측하며, 모든 driver call이 1ms 미만이라는 보장은 하지 않는다. 로딩 중 입력/Step/실제 표시의 진행은 따로 검증한다.

현재 `PacketRenderer::plan_resources`는 같은 제출에서 ready packet을 바꾸는 전제라 geometry admission에 `no_active`를 전달하고 texture/sampler도 새 packet의 requested set을 기준으로 victim을 고른다. stage-only API에서 이 planner를 그대로 호출하면 GPU 완료된 이전 정상 자원을 actor commit 전에 축출할 수 있다. 후속 장치 계약은 active+staged pin 집합과 실제 retirement를 함께 다뤄야 한다. 기존 `commit_resources`/upload/draw/ready 교체의 한 경로를 중간에서 단순 분리하지 않는다.

또한 현재 submission은 한 candidate의 전체 geometry/texture upload와 draw를 묶는다. 비동기 CPU import만으로 UI의 device 작업이 작아지는 것은 아니다. 후속 설계에서는 pump별 upload bytes·resource 생성 수·단계별 실패/취소 회수와 기존 in-flight 한도 안의 presentation 진행 여유를 동결하고 실제 driver/GPU/표시 시간을 측정해야 한다. 아직 숫자·stage API·성능을 확정한 구현은 아니다.

후속 read-only 교차 리뷰는 정확한 연결 지점도 확인했다. renderer `packet.rs::ProjectRenderResources::prepare`는 `&PreparedProject`와 현재 authored handle map을 받지만, 추출된 sidecar/packet에 generation 회계 lease는 없다. `packet_renderer.rs::note_submission/poll_retirement`의 serial·callback watermark·staging ledger를 stage-only에도 재사용하되 draw/present를 수행한 것으로 기록하지 않아야 한다. `window.rs`는 최종 presented report만 제공하고 실행 중 candidate의 stage/present ACK 채널은 아직 없다. GPU 완료는 현재 화면에 필요한 old generation을 축출해도 된다는 뜻이 아니다.

다음 계약은 host가 소유하는 bounded stage/present ACK 경계와 prepared-resource/packet까지 이어지는 source cleanup pin을 함께 정해야 한다. opaque move-only lease·짧은 borrowed callback·content read-only view는 아직 대안이며 `Any` token/framework를 채택한 결정이 아니다. 기존 PreparedProject는 Clone 가능한 native type이므로 borrowed 참조도 신뢰된 SDK 경계임을 명시해야 한다. control status에 payload를 노출하지 않는 보장과, 임의 native caller의 별도 clone/allocation을 제한한다는 주장을 혼동하지 않는다.

[S86](../02-research/source-register.md)의 pinned wgpu27.0.1 source 확인은 callback의 비용도 분리해야 함을 보여준다. native 완료 callback은 submit/poll 진행 thread에서 실행될 수 있어 작은 serial/watermark 게시만 맡긴다. 큰 후보/packet의 마지막 drop이나 import·정리를 callback에 넣지 않는다. write 호출의 반환·후속 submission 완료·실제 present를 별도로 추적하며 stage-only도 draw 없이 completion reservation을 사용한다. UI의 blocking Wait로 이를 대신하지 않는다. staging 재사용 방식/작업량 수치와 실제 장치 검증은 아직 후속 동결 항목이다.

## audio bank와 old voice

새 Play는 commit 후 current bank에서 clip을 찾고, 이미 재생 중인 voice는 원래 generation/clip identity를 유지한다. voice의 volume/pan/Stop/query는 old bank를 포함해 원래 voice를 대상으로 한다. alias index만 새 bank에 재사용해 다른 clip을 가리키게 하지 않는다. old loop 때문에 여러 generation이 남으면 한도에서 Busy가 가능해야 한다.

Frame 변환은 `(실제 audio leaf identity, Frame layout version)`의 동일한 backing을 재사용하는 방향이다. 동일 key라도 실제로 별도 allocation을 만들었으면 공유한 것처럼 계수하지 않는다. 새 bank 전체를 매번 다시 변환하는 반복과 voice마다 PCM 복사를 피한다. backend sample clock·기존 native stream과 voice ID는 유지한다.

old bank/frame charge를 해제하려면 tracked voice 종료뿐 아니라 backend/callback의 보유도 설명해야 한다. 기존 private Arc·backend ownership을 조사해 안전한 observation/retirement seam을 동결한다. 확인할 수 없으면 보수적으로 charge를 유지하고 explicit backpressure를 반환한다. 사용자 모드의 임의 native allocation까지 제한한다고 주장하지 않는다.

추가 pinned-source 리뷰는 `PcmClip`→`StaticSoundData`→Kira render-owned `StaticSound`가 같은 `Arc<[Frame]>`를 보유하고 handle은 command/playback state만 보유함을 확인했다. 따라서 voice reap과 backing 회수는 다르다. 다음 audio 계약의 후보는 actual allocation별 단일 Frame store·`(LeafCookKey, Frame layout version)` 재사용·generation별 alias metadata와 voice origin·opaque backing observer다. observer만 weak로 두면 실제 소멸은 알 수 있어도 마지막 큰 free를 callback/actor에서 피하지 못한다. retirement cleanup owner가 strong sentinel을 유지하고 old Play 차단·bank/candidate owner 해제 뒤 sentinel-only baseline을 확인한 후 그 owner에서 해제하는 순서까지 필요하다. unsized `[Frame]`에 Sized project의 try_unwrap을 그대로 적용하지 않는다. 아직 API/owner baseline·metadata/종료 계약을 동결하거나 구현하지 않았다.

## 한도와 수명 장부

아래 기존 상한은 source에서 확인한 값이며 generation-wide 적용 방식은 아직 구현 전이다. 개별 세대마다 새 한도를 주는 방식으로 늘리지 않는다.

| scope | 현재 상한/다음 적용 |
| --- | --- |
| imported cooked/catalog | P03.3a active+candidate 최대256MiB. 후속은 그 scope의 retained active/candidate/retired 소유자를 모두 포함해야 함 |
| importer scratch | scoped128MiB+기존64KiB 여유 정책. read/captured source·decoded image와 구분 |
| prepared CPU geometry/metadata | 현재 per-project64MiB geometry/8MiB metadata. aggregate128MiB/16MiB 제안을 source backing·중복·retired 포함 회계와 함께 리뷰. GPU bytes나 catalog bytes로 대신하지 않음 |
| GPU residency | working128MiB/hard256MiB. geometry preferred64MiB/hard128MiB는 이 안의 sub-limit |
| GPU upload/completion | staging128MiB·CPU upload scratch128MiB·in-flight submissions3·bookkeeping8MiB. 세대 전환 때문에 무제한 별도 queue를 추가하지 않음 |
| audio Frame | 기존 bank256MiB·conversion scratch128MiB·metadata4MiB. active+staged+retired가 같은256MiB/4MiB ceiling을 공유하는 제안. 실제 공유 backing·metadata 계획을 source로 대조해 동결 |
| status/ticket | 기존 bounded metadata와1MiB encoded wire 결과. 큰 generation payload는 해당하지 않음 |

이 장부는 전체 process RSS 한도가 아니다. decoded image와 cooked의 overlap·GPU residency와 upload buffer·source PCM과 converted Frame을 구분한다. 같은 backing은 실제 공유가 증명될 때 한 번 charge하고, 별도 복사본은 둘 다 charge한다. lower-limit 시험으로 retained packet/loop/reader가 공간을 막고 실제 release 뒤 회복하는지 확인한다.

## 개발 반복 최적화

기존 candidate importer/cache·typed actor·ticket/replay·packet/resource cache·audio backend를 그대로 확장한다. generation change마다 project/engine/device를 다시 만들거나 format별 전용 reload를 추가하지 않는다. geometry-only/image-only/atlas-only/audio-only 변경과 unchanged 준비의 cook/packing/Frame conversion/GPU upload counter를 나누어 기록한다.

기존 `smoke_project.py` opt-in과 real Metal/PCM helper를 확장한다. source 편집→Prepare→stage/commit 대기→같은 authority 상태→actual image/audio→assert의 한 세션 루프를 목표로 한다. 새 fixture window/별도 batch capture를 live 결과로 대신하지 않는다. source/config/schema/payload 변화별 invalidation과 실패 cleanup을 먼저 확인하고 실제 이전 비용이 있을 때만 개선율을 계산한다.

## 구현 착수 전 동결 목록

- [x] semantic topology 비교의 정확한 허용 field·revision/order 규칙과 불일치 진단: P03.3b.1 상세 계약 동결. 구현/검증은 별도.
- [ ] worker-private candidate handoff·CPU cleanup owner·lease/release·정지/실패 시 소유권.
- [ ] headless/native/audio 조합의 consumer 등록·ready/commit/presentation handshake와 dependency 방향.
- [ ] prepared CPU geometry/metadata·모든 retired backing의 aggregate 수치/회계와 observer 수명.
- [ ] public Rust API/type/getter·factory·control operation/result/wire/error/ID/replay 표.
- [ ] actor commit 뒤 actual presentation 지연·candidate 오류/device fatal 구분·복구 계약.
- [ ] old audio Frame backing의 실제 회수 관찰과 단일 backend·다중 bank/voice identity.
- [ ] Luna xhigh 파일/manifest/target 책임과 dependent API 공유.

리뷰가 끝난 구체 계약을 동결한 뒤 code를 배정한다. 위 미결 항목에 추정 signature를 써서 dependent source부터 병렬 구현하지 않는다.

## 하위 인수 근거

- 같은 entity handle/revision·진행 중인 pose/camera를 유지하며 실제 PNG/model/atlas/WAV 변경이 같은 session에서 활성화됨.
- invalid topology·stale expected generation·취소·post-import/post-stage stale success/failure·예산 거부 뒤 마지막 정상 source/resources 유지.
- busy staging·retained old packet·GPU completion 지연·old looping voice·callback backing 보유 중 한도 유지와 reserved Step/입력 진행.
- 공통 replay/ACK/reattach/eviction·효과 후 panic이 중복 commit·잘못된 unknown 결과를 만들지 않음.
- 실제 GPU 픽셀/actual presentation generation·실제 PCM sample 변화/old voice 유지·source/cook/Frame/upload counter 대조.
- 기존 CPU/managed/Metal/native 회귀·clean extracted CLI·두 platform package/다섯 CI·검증 head/main tree 인수.

테스트 장면과 숫자는 이 하위 계약의 근거다. 전체 엔진·144fps·30명 서버·별도 두 게임 인수를 대체하지 않는다.
