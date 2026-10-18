# Native renderer owner: 부분 초기화와 창 종료의 수명

상태: P03.3b.3.3a source·집중 CPU/Metal 검증 통과 · 2026-10-18 · 최종 CI/package 인수 전

## 목적과 적용 범위

[Native project host](native-project-host.md)가 실행 창에서 GPU owner를 돌려받을 때, 완성된 PacketRenderer만 받으면 초기화 중 실패한 Device/Buffer/Texture를 놓친다. 이 문서는 이미 엔진에 반환된 장기 보유 GPU handle을 부분 초기화부터 같은 owner에 저장하고, 창 종료 뒤 지정 CPU worker에서 회수하는 경계를 고정한다. 기존 [CPU generation](render-cpu-generation.md)과 [device generation](render-device-generation.md)의 예산·검증을 재구현하지 않는다.

첫 구현은 recoverable owner와 기존 생성자의 공통화다. 실제 managed 창과 host의 반환/backstop 연결은 후속 3.3b/c이며 이 문서만으로 통과하지 않는다. 기존 public 생성자·WindowOptions struct literal·packet message/outcome은 호환성을 유지한다. legacy 경로의 drop thread까지 소급 보장하지 않는다. 일반 임시 command encoder/SurfaceTexture의 UI 사용·해제와 장기 자원의 마지막 소유권을 구분한다.

## source에서 확인한 실패 구간

기준은 PR19/main `c507835`다. `Renderer::from_adapter`는 request_device 뒤 layout/pipeline/buffer를 지역 변수로 만들고 마지막에 Renderer를 조립한다. `PacketRenderer::new`도 Renderer를 소비한 뒤 유한 한도를 검사하고 여러 buffer/bind group을 지역 변수로 만든다. window는 Renderer 생성 뒤 dimensions 검사와 surface configure를 거쳐 packet 변환한다. stage geometry의 vertex→index, texture→view 쌍도 두 번째 호출이 unwind하면 첫 지역 handle을 잃는다.

함수 밖 catch만 추가하면 내부 app/지역 변수부터 unwind한다. holder를 먼저 만들고 holder를 borrow한 작업을 catch해야 한다. Device clone 하나는 다른 buffer/view/layout의 마지막 drop 위치를 보존하지 않는다. 2026-10-17 trait probe는 Renderer/PacketRenderer의 Send를 해당 Mac artifact에서 확인했을 뿐 실제 partial owner·drain·thread 순서를 증명하지 않았다.

## 하나의 move-only owner

renderer는 `WindowGpuOwner`와 작은 `WindowGpuOwnerPhase`를 공개한다. 내부 필드는 비공개·non-Clone이며 Window/Surface/authority나 별도 project/CPU registry를 보유하지 않는다. 감싼 PacketRenderer의 미완료 stage/submission은 기존 SourcePin·CpuFrameHandle pin을 drain까지 유지한다. 이를 먼저 버려 자원 수명을 끊지 않는다. host는 내부를 꺼내거나 두 renderer로 복제하지 않고 worker로 이동한다.

| phase | 보유/전이 |
| --- | --- |
| NoDevice | 아직 요청하지 않음 |
| DeviceRequest | 요청 진행 또는 실패. 반환받은 device handle 없음 |
| RendererPartial | 반환된 Device/Queue를 즉시 저장. base 초기화의 각 반환 handle도 즉시 저장 |
| RendererReady | 완성 Renderer를 저장. dimensions 검사/surface configure 중에도 이 상태 유지 |
| PacketRendererPartial | Renderer를 먼저 소유하고 packet 고정 자원 준비 중 |
| PacketRendererReady | 완성 PacketRenderer와 기존 stage/cache/submission 장부 소유 |

public 경계는 `new`/`Default`, `phase`, `has_device`, `try_drain_after_window`다. 마지막 operation은 기존 `PacketRendererDrainStatus/Error`를 재사용한다. PacketRendererReady는 기존 nonwaiting drain을 호출한다. 아직 submit하지 않은 NoDevice/DeviceRequest/부분 초기화/RendererReady는 GPU 제출 대기가 없어 Drained를 반환한다. 이 결과가 owner를 소비하거나 UI에서 drop하게 만들지는 않는다. drain 오류도 owner를 잃지 않는다. owner의 실제 마지막 drop은 host worker의 책임이다.

window 내부 borrowed 초기화/준비·ready renderer 접근은 crate-private이다. 정확한 helper 배치는 Luna가 같은 source에서 구현하되 외부 facade나 source/runtime 의존을 추가하지 않는다. 모든 ready 조건을 먼저 검사한 뒤 단순 move만으로 승격한다. 일부 필드를 take한 뒤 fallible 검사/할당/사용자 callback을 호출하지 않는다.

request_device에 engine handle이 반환되면 adapter info 문자열 할당이나 다음 GPU 호출보다 먼저 Device/Queue를 저장한다. 실패가 handle 반환 전이면 `has_device=false`다. backend 내부에서 device를 전혀 만들지 않았다는 주장은 하지 않는다. request future panic도 `device_request_panicked/no_handle_returned`와 post-handle 실패를 구분한다. abort/OOM-abort·프로세스 종료는 unwind 복구 계약 밖이다.

## base·lazy 초기화 handle 보관

고정 필드 holder를 사용하고 생성 handle을 담는 무한 Vec를 만들지 않는다. durable layout/pipeline/buffer/group뿐 아니라 base shader+mesh pipeline layout, overlay shader+layout+pipeline layout, lazy model/sprite 각각의 shader+pipeline layout도 보관한다. base 5개와 lazy 4개의 고정 임시 handle이다. 반환 즉시 설치한 뒤 다음 생성 호출로 넘어간다.

lazy ensure 함수가 Renderer 안에서 실행되므로 완료된 holder는 Renderer의 private 고정 보관 필드로 이동한다. managed retention policy는 이 bundle을 worker의 최종 drop까지 유지한다. legacy constructor policy는 완료된 임시 handle을 기존 caller thread에서 해제할 수 있다. 같은 shader/pipeline 생성 코드를 두 벌로 복제하지 않는다. 부분 초기화 holder는 어떤 policy에서도 catch 범위 밖의 지역 변수로 먼저 소멸시키지 않는다.

PacketRenderer 준비는 device/CPU budget·stage ID·size arithmetic·slot storage reserve를 GPU 생성 전에 검사한다. 준비 holder가 원 Renderer와 반환된 instance/material/globals buffer 및 두 bind group을 각각 보관한다. packet 고정 자원 준비 실패는 이미 소유한 Renderer를 포함해 같은 WindowGpuOwner 안에 남는다. 초기화 중 queue submit은 하지 않는다.

고정 holder의 논리 bookkeeping을 기존 checked estimator에 포함한다. 기존 8MiB bookkeeping/working-set 한도를 올려 통과시키지 않는다. 이 장부는 allocator/RSS나 driver 내부 상한이 아니다.

## stage의 두 handle 생성 구간

StageRecord를 접수·예약한 뒤 create를 시작한다. geometry의 vertex/index, imported texture/view, default texture/view에 유한 partial slot을 둔다. 첫 handle을 slot에 설치한 뒤 두 번째 create를 호출한다. 두 handle과 사전 계산된 metadata가 모두 있어야 기존 complete cache entry로 이동한다. ordinary complete cache type까지 optional로 넓히는 것보다 작은 pending pair slot을 우선한다. 실제 source상 더 단순한 동등 구현이 필요하면 변경 이유를 먼저 리뷰한다.

map/reserve·checked metadata 변환은 handle 생성 전에 수행한다. 미완성 slot은 upload/binding/Ready/promotion 대상이 아니다. pair 사이 fatal panic은 같은 PacketRenderer owner에 남아 창 실패 반환 뒤 worker drain/drop을 따른다. fatal panic 뒤 stage를 정상 resume하거나 Ready로 보고하지 않는다. counter가 완료 전까지 부분만 갱신된 경우 metadata는 failure/incomplete로 표현하고 성공 create 수를 지어내지 않는다. 유한 pending pair와 retained handle의 비용도 장부에 포함한다.

일반 실행의 취소/세대 회수는 PR19의 기존 nonwaiting serial retirement와 cache 한도를 보존한다. `try_cancel_stage`/`refresh_stage_state`가 완료된 stage/cache handle을 render 호출 thread에서 해제하는 경우까지 이번 초기화/창 종료 계약이 worker로 옮겼다고 하지 않는다. CPU의 큰 공유 backing은 별도 PacketCpuOwner registry가 worker에서 회수한다. 모든 정상 GPU cache 해제의 thread/driver 시간을 바꾸는 새 disposal queue는 이번 slice에 추가하지 않으며 실제 host 반응성/제품 성능 gate에서 별도 평가한다. 생성/해제 호출 수의 상한은 wall-time/144fps 보장이 아니다.

## managed 창에서의 사용 계약

후속 additive `run_packet_windowed_managed`는 app을 만든 뒤 app이 살아 있는 내부 범위에서 event loop 오류/unwind를 수집한다. 반환형은 owner를 오류에서도 항상 돌려주는 `ManagedWindowRun { outcome: Result<ManagedWindowOutcome, ManagedWindowError>, status: ManagedWindowStatus, gpu_owner: WindowGpuOwner }`다. renderer 안에 별도 owner-return channel을 추가하지 않는다. 초기 source/frame Arc를 public outcome에 보관하지 않는다.

UI 정리는 작은 결과 복사 → CPU frame/packet aliases 해제 → SurfaceState 해제 → Window 해제 → owner 이동 순서다. Surface와 Window를 worker로 이동하지 않는다. surface configuration 실패는 RendererReady owner를, packet 준비 실패는 PacketRendererPartial owner를 반환한다. panic payload는 UI에서 큰 String으로 복사하지 않고 유한 failure code로 보고한다.

host는 run 반환 직후 전용 CPU worker의 private one-shot 반환 경계로 owner를 옮긴다. 소비자가 아직 받지 않았다면 host가 그대로 보유하며 재시도한다. Full/disconnected를 owner drop으로 처리하지 않는다. CPU job panic은 worker 바깥을 unwind시키지 않고 borrowed owner가 살아 있는 범위에서 포착한다. worker는 게임 작업을 실패로 닫아도 GPU 반환 수신·drain/backstop 수명은 유지한다. 구체 host startup/error 경계는 [host 계약](native-project-host.md)의 후속 동결 항목이다.

Drop은 stop 요청만 하고 unfinished join을 하지 않는다. CPU worker와 private cleanup 상태가 host facade보다 오래 살아도 public control은 packet/source/device의 strong/weak roots를 받지 않는다. 실제 host 구현 전에 이 순서와 channel failure 경로를 시험해야 한다.

## 검증·구현 순서

| 하위 gate | 실제로 확인할 것 | 현재 상태 |
| --- | --- | --- |
| 3.3a owner | pre-handle request 오류/unwind, Device 보유·base partial layout·lazy layout/shader·packet buffer/group의 서로 다른 소유권 경계에 injected panic; ready 시 고정 handle 전체 보유·치수 오류의 RendererReady 유지; Send compile; 지정 worker final drop | 검증 중 |
| 3.3a stage | vertex→index, texture→view/default pair 사이 panic에 첫 handle 잔존; incomplete가 Ready/업로드 대상 아님; 정상 stage/legacy 회귀 | 계획 |
| 3.3b window | bootstrap game ACK=0, Ready 뒤 실제 frame만 ACK, surface configure 실패/내부 panic에서도 항상 owner 반환·Surface/Window 먼저 해제 | 계획 |
| 3.3c host | 단일 session/import/CPU worker, view-close 뒤 Step/read, Stage/Commit 거부, whole stop·실제 drain·nonwaiting 종료 | 계획 |
| 3.3d 통합 | public Rust SDK·CLI가 같은 경로, 실제 source edit→Stage→Commit→pixels/pose/present ACK·구 CPU/GPU 회수 | 계획 |

코드 작성은 Luna xhigh만 맡는다. 첫 task는 renderer/packet renderer/staging의 owner와 테스트이며 window/CLI/runtime/manifest는 편집하지 않는다. 공통 export 파일은 이 task가 단독 소유한다. focused CPU → 관련 real GPU → 통합 후 workspace/managed/5 CI/두 package 순서로 검사한다. 이미 통과한 unrelated hardware 검사를 문서만 바뀌었다는 이유로 반복하지 않는다. 이 순서가 모든 future 플랫폼이나 frame-time 성능을 인증하지 않는다.

검증 표의 2026-10-17 리뷰 수정: 최초의 모든 handle 직후 hook을 각각 추가하는 표현을 서로 다른 소유권/전이 경계의 주입과 ready 전체 보유 검사로 구체화했다. 동일한 Option 필드 대입을 반복하는 테스트 대신 실제 holder가 생기는 지점·다음 호출 전 보관·phase 승격·오류 반환을 검사한다. 모든 반환 handle을 즉시 보관하는 구현 요구는 그대로다. 아직 실행하지 않은 request-future unwind와 RendererReady 오류 검사는 통과로 계산하지 않으며 실제 Surface 오류/창 finalizer는 3.3b/c에 남긴다.
