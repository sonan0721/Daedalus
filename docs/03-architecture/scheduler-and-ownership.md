# 스케줄러·데이터 소유권·과부하 계약

상태: 독립 엔진 설계 제안. 동시성 검증·구현 전 · 2026-10-15

## 격리할 지연

REQ-004의 핵심은 지형 생성·디스크 I/O·스크립트 갱신·물리 지연 때문에 입력·로딩 UI·표시가 해당 작업의 완료를 기다리지 않게 하는 것이다. CPU·GPU·메모리 대역폭 경쟁은 남으므로, 스레드 분리와 함께 작업량·메모리·동시 실행을 제한한다.

네이티브에서는 OS 이벤트 소유자, simulation 소유자, presentation/render 소유자, 배경 CPU/I/O 실행을 구분하는 안을 검토한다. UI 상태와 시계는 simulation tick이 지연되어도 진행한다. 웹은 같은 논리적 계약을 다른 배치로 구현한다.

## 실행 책임

| 영역 | 소유 데이터 | 기다림에 대한 규칙 |
| --- | --- | --- |
| OS 이벤트 | window·입력·resize | OS idle 대기는 가능. 장기 job·저장·VM 완료를 기다리지 않음 |
| Simulation | 가변 ECS world·tick·권한 상태 | tick 내 필수 의존성은 순서화. render/UI에 기다림을 전파하지 않음 |
| Presentation/render | UI tree·로컬 view·GPU 자원·draw | 필요한 present 제어와 구분하여 simulation·인코더 동기 대기 제거 |
| 배경 CPU | 불변 입력의 생성·mesh·decode | 긴 독점 작업 분할. 같은 pool의 다른 job을 기다리는 순환 금지 |
| I/O | file·network·stream | blocking I/O를 CPU 계산 pool에서 격리 |
| Script | VM·module generation | 지정 소유자 실행. 무제한 VM 실행을 주요 경로에 허용하지 않음 |
| Audio | 출력·mix 소비 | callback의 allocation·I/O·world lock 배제. 상세 명세 후속 |

OS 이벤트 루프의 스레드 조건은 platform adapter가 지킨다. Bevy의 렌더 분리에도 동기화 경계가 있음을 참고하고, 독립 게시 구조를 동일 구현이라고 부르지 않는다. [winit API](https://docs.rs/winit/latest/winit/event_loop/struct.EventLoopBuilder.html), [Bevy pipeline](https://docs.rs/bevy/latest/bevy/render/pipelined_rendering/struct.PipelinedRenderingPlugin.html)

## 고정 tick과 표시 시계

60Hz 고정 tick은 검토값이며 사용자 확정 수치가 아니다. 단조 시계와 accumulator를 사용하고 한 번에 따라잡는 tick 수를 제한한다. 초과를 계측하며 local·server 정책을 구분한다. 서버가 권한 상태의 tick·중요 명령을 말없이 버리지는 않는다.

physics·script·gameplay를 임의로 동시에 실행하지 않는다. 입력→script 의사결정→물리→충돌 event→명령 적용에는 의존성이 있다. read/write 집합과 허용 지연을 선언하고 충돌 없는 system만 병렬화한다. tick을 넘는 결과는 대상 tick·revision을 검사한다.

표시는 전후 tick의 시간·transform을 보간하는 안을 검토하고 표시 지연도 기록한다. 새 tick이 없을 때 무제한 외삽하지 않는다. 로컬 UI·카메라 반응과 서버가 확정한 위치, 네트워크 예측 상태를 구분한다.

## 렌더 데이터 게시와 회수

전체 ECS 대신 transform·pose·visible object·light 등 필요한 데이터만 불변 상태로 게시한다. 후보는 소유권을 넘기는 유한 mailbox/ring 또는 불변 snapshot 참조다. 단순히 2개 슬롯을 번갈아 덮어쓰면 독자가 늦을 때 안전하지 않다.

1. producer는 비어 있는 credit을 확보한 슬롯에만 구성한다.
2. 완성 snapshot을 sequence와 함께 게시하고 consumer에 소유권 또는 수명이 보장된 참조를 넘긴다.
3. consumer가 읽는 슬롯에는 쓰지 않는다. 독자가 끝나야 회수한다.
4. 슬롯이 모두 사용 중이면 simulation을 기다리게 하지 않고 미게시 표시 갱신을 합치거나 게시를 건너뛴다. 월드 변경 명령은 건너뛸 대상이 아니다.
5. 보관 개수·bytes·최대 age를 계측한다. 캡처·플러그인이 무제한 참조를 유지할 수 있는 API를 만들지 않는다.
6. 큰 CPU 메모리 해제와 GPU retirement 비용도 프레임 예산으로 다룬다.

CPU snapshot 회수와 GPU 사용 중인 buffer/texture 회수는 다른 수명이다. 커맨드 제출만으로 자원을 즉시 재사용하지 않는다. 실제 memory ordering·게시 알고리즘은 모델 리뷰 뒤 선택한다.

## 큐와 포화 정책

| 데이터 | 전달 의미 | 포화 시 동작 |
| --- | --- | --- |
| 표시 snapshot·로딩 진행 | 최신 상태 우선 | 미사용 과거 상태 통합·교체 |
| 마우스 이동·연속 입력값 | 최신값·sequence | 합성 가능. button edge는 별도 보존 |
| button edge·UI 동작 | 순서·처리 결과 | 유한 수락. 미수락은 Busy로 반환 |
| 월드 편집·inventory·저장 | 유실하면 안 되는 권한 명령 | 수락 credit·재시도·dedup·명시적 ack, 필요 시 수락 제한 |
| 청크 작업 완료 | revision이 맞는 필요한 결과 | 오래된 결과 폐기. 투입 시 결과 credit 예약 |
| 캡처 요청 | request ID별 결과 | Busy·timeout·cancel 반환, 무한 축적 금지 |

Crossbeam의 blocking과 try API는 의미가 다르다. 큐 선정과 포화 정책을 함께 결정한다. try API를 썼다는 이유로 전체 알고리즘의 wait-free·고정 시간 실행을 주장하지 않는다. [Crossbeam API](https://docs.rs/crossbeam-channel/latest/crossbeam_channel/)

## 예산과 우선순위

프로세스 전체의 CPU 동시 실행을 관리한다. ECS·Rayon·물리가 각각 모든 코어만큼 pool을 만들지 않게 한다. M3 Pro의 성능·효율 코어 차이는 실제 부하로 평가한다. Rayon은 계산 실행 후보이며 deadline·공정성·취소 정책은 엔진 책임이다. [Rayon pool 설정](https://docs.rs/rayon/latest/rayon/struct.ThreadPoolBuilder.html)

job에 class·투입 시간·우선순위·입력 revision·취소 토큰·출력 상한을 둔다. 근거리 충돌·가시 메시·선행 로딩·장식 등의 배분을 비교하고, 낮은 우선순위의 기아를 age 또는 몫 배분으로 막는다. 큰 작업은 협조적으로 중단할 수 있는 단위로 나눈다.

I/O 완료 처리·allocation·pipeline 생성·snapshot 추출·VRAM upload·대량 despawn도 응답성을 해칠 수 있다. CPU 시간과 bytes를 각각 제한하고 계측한다. 금지 API 목록만으로 검증을 끝내지 않는다.

## 실패·종료·검증

작업 실패를 job 진단으로 전달하고 credit을 회수한다. 종료는 수락 중지→취소→I/O 정리→필요 저장→결과 회수→pool 종료→GPU·window 순서를 지킨다. panic·device loss를 성공으로 숨기지 않는다.

생산자/소비자 속도 역전, 큐 포화, 1 worker, 느린 script·physics, 이미 파괴된 entity, cancel과 완료 경쟁, snapshot 장기 보관, 종료 중 I/O를 시험한다. Rust 타입뿐 아니라 FFI·unsafe·논리 revision·GPU 수명도 검토한다. 합격값은 [검증 계획](../04-plan/validation-and-performance.md)에서 정한다.
