# P03.3b.3.2b 같은 device의 bounded generation staging

상태: device 계약 동결·source 작성/통합 검증 중 · 2026-10-17

## 범위와 입력

[CPU generation](render-cpu-generation.md)의 immutable input을 기존 PacketRenderer/device/cache에서 준비한다. [전체 staged 설계](staged-render-asset-generation.md)의 GPU 작업 계약이다. runtime dependency·새 device·가짜 publication을 만들지 않는다. 실제 window/host present ACK와 b.3 제품 경로는 후속 통합 인수다.

private `ResourceRequests<'a>`는 geometries·model_bindings·sprite_bindings의 기존 HashMap을 빌려 본다. PacketValidation과 PacketStageInput이 같은 projection을 제공하고 기존 `plan_resources`가 이를 읽는다. stage 전용 importer/cache planner/authoring 비교기를 복제하지 않는다. CpuFrameHandle은 동일하게 검증한 packet과 private validation을 제공하며 exact packet/asset limits fingerprint를 검사한다.

## 공개 device 경계

| API/type | 동결한 의미 |
| --- | --- |
| `PacketStageLimits` | additive config. 기본 pump upload4MiB·실제 candidate GPU create 호출8회·outstanding stage chunk1개. 양수/checked/최소 atomic group 검증 |
| `try_begin_stage(input, limits)` | owned PacketStageInput을 받아 renderer-local opaque checked PacketStageId 반환. 같은 renderer namespace만 유효. 전체 후보 admission/union pin·예약, GPU write/일괄 upload 없음 |
| `try_stage_status(id)` | 작은 identity/progress/state/counter/failure만 반환. input/source/이미지/keys 배열 없음 |
| `try_cancel_stage(id)` | 새 chunk를 중단. 같은 known terminal 취소는 idempotent. 이미 제출한 부분 자원은 completion까지 Cancelling/Retiring으로 남음 |
| `render_cpu_frame_with_stage(target, width, height, frame, view, stage, loading_elapsed)` | acquired target·optional Arc<CpuFrameHandle>·실제 local view·optional stage ID를 사용. old-ready 또는 초기 clear/loading과 stage chunk 최대1개를 한 submit/serial로 조합 |
| `PacketStageFrameSubmission` | optional ordinary PacketViewSubmission과 small stage progress. bootstrap clear는 game frame/submission identity가 없음 |
| `try_drain_after_window()` | nonwaiting poll, Pending 또는 Drained. 실제 in-flight/partial roots를 보유한 채 완료 관측. UI에서 blocking wait를 하지 않음 |

기존 legacy draw/window/message enum API는 보존한다. 새 input의 identity/limits가 일치하지 않으면 GPU 효과 전에 typed 오류다. stage IDs는 host가 실제 runtime Initial 또는 Candidate identity에 대응하며 candidate epoch를 renderer identity로 추정하지 않는다. renderer가 issued하지 않은/다른 renderer ID는 typed invalid/stale다. counter exhaustion은 효과 전 영구 오류다.

stage 상태는 Admitted/Uploading/AwaitingCompletion/Ready/Cancelling/Retiring/Cancelled/Failed를 구분하는 bounded 표현을 사용한다. 외부 status의 diagnostic은 최대64byte code·2048byte message이며 전체 논리 metadata는32KiB 이내다. 실제 used/remaining bytes·create calls·upload/completion serial을 보고한다. 예상 작업량과 완료 작업량을 섞지 않는다. 알려진 terminal status는 유한 마지막 슬롯만 보존하며 무제한 역사 map을 만들지 않는다.

아직 create/upload가 없는 후보는 status poll만으로 Admitted에서 Uploading으로 바뀌지 않는다. victim 목록이 비어 있다는 사실은 작업 시작의 근거가 아니다. 실제 create/upload progress가 시작 상태를 구분하며, 완전히 준비된 cache-hit 후보는 별도 불필요한 upload 없이 Ready가 될 수 있다.

CPU generation의 private canonical resource-set token을 stage admission 때 보존한다. Ready 뒤 큰 StageInput/plan을 반환해도 작은 token과 source pin은 필요한 실제 수명까지 유지한다. 첫 candidate frame은 기존 CPU 검증·같은 token·session/generation/digest·정확한 limits로 확인한다. UI에서 expected-set을 다시 할당하거나 expected bind group×draw의 중첩 탐색을 실행하지 않는다. token은 GPU completeness/serial 검증을 대신하지 않으며, 다른 CPU owner의 token은 같은 문자열 identity여도 거부한다.

성공한 managed submission은 device-active token도 같은 transaction에서 갱신한다. 이후 같은 generation의 새 pose는 active token·identity·정확한 limits로 O(1) 확인하며 기존 CPU validation으로 slot writes를 만든다. Busy/실패 전에는 active token을 바꾸지 않는다. legacy activation이 실제 ready packet을 교체하면 managed token을 지워 stale 권한을 남기지 않는다. old in-flight의 정확한 CpuFrameHandle pin이 이전 token의 실제 수명을 보존하며 작은 token 비용도 bookkeeping에 포함한다.

CPU에서 준비한 canonical dense draw-ID→slot Arc도 candidate와 active를 구분한다. 첫 candidate draw는 candidate map으로 normal/material/UV row를 preflight하고 기존 O(draw) write/encode를 실행하며, 성공한 submit 뒤에만 active map을 Arc-swap한다. 초기 active map이 비어 있거나 expanded model primitive 수/ID가 바뀌어도 이 경계를 사용한다. 같은 active generation의 pose 갱신은 기존 map을 재사용한다. UI에서 후보 HashMap을 다시 만들지 않는다. map의 실제 backing은 CPU registry가 보유하고 device bookkeeping에는 필요한 공유 필드만 센다. legacy activation은 기존 slot planner 결과를 Arc로 감싸 같은 내부 저장형을 사용하며 public API/검증은 보존한다. Busy/실패는 old map/ready/token을 유지한다.

## admission·전환·회수

기존 geometry64/128MiB preferred/hard·256/512 mesh, asset128/256MiB working/hard·texture/sampler256/512·bind group1024/2048를 유지한다. 한 후보의 working set과 old-ready+candidate+incomplete-retired union의 hard ceiling을 각각 적용한다. 기존 LRU/victim planner의 protected set이 이 union을 포함해야 한다. 완료된 unpinned cache만 victim이며 pinned shortage는 Busy, 후보 자체의 oversize/unsupported는 영구 오류다.

renderer의 두 generation 슬롯은 active+candidate 또는 active+retiring이다. 취소된 partial도 실제 serial/alias가 남으면 retiring이다. CPU source의 세 슬롯과 같은 장부가 아니다. candidate Ready 뒤 실제 committed CpuFrameHandle의 session/generation/digest와 canonical resource key set이 stage input과 같을 때만 정상 ready 교체를 허용한다. live tick/publication/pose는 달라질 수 있다. 예상하지 않은 key를 즉석 전체 upload해 Commit을 숨기지 않는다.

GPU cache key 재사용은 기존 payload/layout·texture/sampler/pair 의미를 따른다. **range/mip 초기화 완료와 submission 완료는 별개**다. partial entry는 ordinary ready cache hit가 될 수 없다. cancel 후 fence가 완료됐다는 이유로 incomplete를 complete로 승격하지 않는다. 완전히 초기화되고 해당 upload serial이 완료된 뒤에만 Ready다. cache hit에도 이 조건이 필요하다.

Ready status의 `initialization_serial`은 필요한 geometry/texture/default texture의 실제 초기화 serial 최댓값을 나타낸다. 이번 stage에서 새 upload가 없는 cache-hit도 기존 초기화 serial을 보고하며, upload 초기화 의존성이 없는 빈 집합만 None일 수 있다. 같은 자원을 다시 그려 증가한 last-use serial로 이 값을 덮어쓰지 않는다. 각 자원의 completion/completeness 검사와 별개로 status 요약의 의미도 보존한다.

initialization에 필요한 serial과 마지막 draw/use serial은 구분한다. 계속 그리는 old-ready가 last-use serial을 갱신해도 이미 완료된 upload가 다시 미완료가 되지 않는다. candidate Ready를 unrelated 최신 draw의 완료까지 무한히 미루지 않는다. 반대로 eviction/drain은 실제 last-use serial을 기다린다. zero-new-upload/cache-hit·empty generation도 명시적 source identity/pin을 유지한다.

generation pin 해제·cache residency·실제 eviction bytes는 별도 counter다. 사용이 끝난 complete entry는 bounded LRU에 남거나 다음 generation에서 재사용될 수 있다. pin 해제를 실제 VRAM eviction으로 보고하지 않는다. source CPU leaf 회수는 CPU owner registry의 실제 별칭 검사와 별도다.

## combined submit transaction

window는 surface acquire를 먼저 한다. acquire 실패·0 dimensions·미지원 format에서는 write/cursor를 진행하지 않는다. renderer는 completion poll 뒤 frame writes·작은 chunk·actual create quota·scratch/staging·victim union·in-flight/ledger capacity·checked next serial을 전부 preflight한다. queue.write_* 이후 할당/ledger capacity 부족이 처음 드러나지 않도록 한다.

frame write의 현재 layout 상한은 checked `N × (128+48+16)+128+16`이며 N16384이면3,145,872byte다. 실제 size_of/layout로 검증하고 candidate chunk보다 frame reservation을 먼저 확보한다. staging와 upload scratch는 각각 기존128MiB, bookkeeping8MiB다. 전체를 무조건 pack하지 않고 whole vertex/index record 또는 texture mip/row 단위로 cursor를 진행한다. largest indivisible upload unit조차 한도에 안 맞는 후보/config는 영구 오류이며 영원히 Busy가 아니다.

raw quota4MiB는 작업량의 상한이며 매번 그 크기를 할당해야 한다는 뜻이 아니다. 낮춘 scratch/staging 한도에서도 실제 whole-record 단위가 들어가면 진행하도록 effective chunk를 줄인다. geometry 변환의 실제 scratch·global staging·raw quota를 각각 적용하고 index/borrowed texture처럼 추가 복사가 없는 경로에 가상의 큰 scratch를 부과하지 않는다. texture는 raw row bytes와256byte 정렬 staging pitch를 별도로 계산한다. padded 최소 row/default texture조차 global cap에 안 들어가면 stage admission 전에 typed 영구 오류로 거부한다.

combined chunk planner에는 frame/overlay의 실제 또는 검증된 보수적 reservation을 제외한 headroom을 전달한다. geometry는 vertex/index 단위로, texture는 raw quota와 padded staging에 들어가는 whole row 수로 내림한다. 현재 in-flight bytes/count 때문에 **개별로는 들어가는 요청**만 ResidencyBusy다. `next_staging > cap` 자체를 Busy로 표현하지 않는다. transient headroom 때문에 한 단위도 못 올리는 경우와 원래 cap에서 영원히 못 올리는 경우를 구분하고, preflight 실패에서 create/evict/write/cursor를 바꾸지 않는다. lowered cap을 지원하려고 기본 memory/queue/deadline 한도를 늘리지 않는다.

현재 dirty globals/instance/material write 때문에 한 stage 단위를 넣을 여유가 없으면 frame-only submit으로 최신 old-ready/clear를 유지하고 stage cursor를 진행하지 않는다. 다음 unchanged frame의 여유로 진행할 수 있는 후보를 영구 초과로 판정하지 않는다. 이 경우에도 frame 자체의 in-flight admission은 필요하다. 반면 요청한 loading overlay의16byte는 해당 호출 방식에서 매번 필요한 최소 비용이다. 단위+overlay 최소 비용조차 cap을 넘으면 그 호출의 typed 영구 budget/unit 오류를 반환해 zero-progress 성공을 무한 반복하지 않는다. overlay 없는 호출과 가변 dirty write의 일시 지연은 구분하며, 모든 프레임에서 계속 여유가 없는 입력에 무조건적인 stage 완료 시간을 약속하지 않는다.

GPU allocation은 buffer/texture 전체 크기이고 byte upload chunk와 다르다. create buffer/texture/view/sampler/bind group/shader/pipeline layout/render pipeline의 실제 호출을 quota로 센다. 현재 cold model/sprite는 공유 layout1회와 각 pipeline group3회로 최대7회다. atomic pipeline group은 남은 quota에 들어갈 때 시작하고 minimum group보다 작은 config를 효과 전 거부한다. driver 호출의 latency 상한을 약속하지 않는다.

wgpu write는 다음 submit과 연결되므로 stage write만 enqueue한 채 다른 draw를 기다리지 않는다. 한 combined submit의 serial을 touched stage와 draw 자원에 모두 기록하고 그 뒤 cursor/progress를 확정한다. 성공 submit 뒤 metadata를 잃는 경로를 만들지 않는다. write 이후 예상 밖 fatal은 실제 pending charge를 보존하고 failure/drain으로 수렴한다. callback은 작은 serial/watermark만 게시하며 큰 CPU final free를 맡지 않는다.

기본 max-in-flight3과 유효한1/2를 유지한다. stage 전용 slot/minimum3을 추가하지 않는다. stage chunk는 동시에1개만 outstanding이고 그 serial이 미완료일 때 old-ready draw는 별도 여유가 있으면 계속 가능하다. max1에서는 기존 submit 완료 뒤 다음 combined draw가 진행한다. hidden/minimized/surface-unavailable은 device stage를 미루지만 CPU cancel/stop/authority는 진행한다. offscreen host 전체는 별도 D1 목표로 남긴다.

## post-window와 작업 소유권

새 window 경로는 초기 packet 없이 clear/loading으로 시작한다. 실제 frame.present 호출 뒤 game packet/view만 host ACK로 전달한다. bootstrap·queue submit·completion을 game present로 부르지 않는다. 기존 packet-window constructor의 legacy fallback을 새 bootstrap 구현으로 보고하지 않는다.

UI의 Window/Surface를 먼저 같은 thread에서 해제하고 device owner를 capacity1 disposition으로 기존 CPU owner에 반환한다. 완성된 PacketRenderer·변환 전 Renderer·부분 device 초기화·engine handle 생성 전/결과 불명 실패는 [host 계약](native-project-host.md)의 후속 additive 경계에서 구분한다. worker는 window 시작 뒤 stop만 보고 먼저 종료하지 않으며 disposition→nonwaiting drain→실제 owner/CPU backing 회수 뒤 종료한다. per-close janitor·UI unfinished join은 없다. 이 window/host 구현은 별도 작업이고 device slice의 drain API만으로 인수하지 않는다.

Luna xhigh device 작업자는 `packet_renderer.rs`, 필요 시 그 private submodule, `renderer.rs`의 GPU 생성 helper와 해당 tests만 소유한다. packet/CPU/lib exports 소유자와 projection/export 목록을 먼저 공유한다. runtime/window/CLI/manifest/lock/CI는 수정하지 않는다. 한 Cargo/장치 lease를 유지한다.

CPU planning/admission·union victim·partial coverage·checked serial/ledger·quota·cancel·foreign IDs·Ready/last-use 구분·max slots1/2의 회귀를 검사한다. 실제 GPU ignored tests는 old-ready pixels를 유지한 여러 pump→Ready→새 actual frame pixels·resource reuse·취소 partial의 cache 미승격·drain을 확인한다. ledger/completion 주입은 deterministic saturation 근거로만 기록하며 실제 stall 시간을 측정한 것으로 부르지 않는다. existing public model/sprite pixel tests도 보존한다. native host/SDK·package/CI/main과 전체 b.3 인수는 후속이다.

2026-10-17 source 감사는 낮춘 scratch1MiB에서도 raw quota4MiB chunk를 그대로 예약해 streamable geometry를 거부하고, padded256byte texture를 staging cap128에 접수한 뒤 clear 경로에서 영구 Busy가 되는 결함을 확인했다. 위 effective/headroom 규칙은 이 결함의 수정 계약이며 아직 통과 증거가 아니다. 기본 quota 그대로의 lowered scratch/staging 진행, dirty active frame과 한 submit의 headroom 공유, padded 최소 단위의 효과 전 영구 거부·정직한 transient Busy 회귀를 추가한 뒤 인수한다.


## 2026-10-18 혼합 pipeline 초기화의 create-call 계산

실제 model+sprite CLI bootstrap에서 기존 per-task preflight가 공유 texture layout을 두 번 예약하는 source 결함을 확인했다. 빈 renderer에서 model task와 sprite task를 각각 계산하면4+4이지만, 실제 실행은 첫 task가 layout을 만든 뒤4+3의7개 호출이다. 기존 실행의 actual/planned 동등성 검사가 이를 CounterExhausted로 거부했다. 이는 실제 정수 고갈·메모리 한도 부족의 근거가 아니다. 실패 raw와 후속 검증은 [통합 기록](../06-worklog/2026-10-18-native-host-integration.md)에 남긴다.

batch 계획과 effect 전 preflight는 선택한 앞선 task가 생성할 공유 layout을 같은 작은 가상 상태로 반영한다. 같은 batch에서 공유 layout create를 한 번만 세며 다음 batch는 renderer의 실제 현재 상태에서 다시 시작한다. execution의 실제 create-call 계수와 일치 검사는 유지한다. 개별 pipeline task의 atomic 최소 비용, pump 한도8, 부분 handle 보존과 실패 소유권 계약을 약화하지 않는다. 예산을 늘리거나 비교를 지우지 않는다. 기존 model-only/sprite-only와 빈 renderer의 혼합 initial stage/실제 frame 회귀를 검사한 뒤 실제 CLI를 다시 실행한다.
