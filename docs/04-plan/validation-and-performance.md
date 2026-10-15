# 정확성·품질·성능 검증 설계

상태: 미래 엔진 검증 계획. 현재 실행 가능한 것은 루트 TESTING의 문서 검사뿐 · 2026-10-15

## 성능 기준선

사용자는 현재 Mac과 144fps 이상을 주요 목표로 정했다. 기준 기기는 M3 Pro CPU12/GPU18코어·통합 메모리18GB·내장3024×1964다. **내부 렌더 해상도·품질·장면·허용 초과율은 미결**이므로 아직 완성된 성능 인수 계약은 아니다.

144fps의 프레임 간격은 `1000 / 144 ≈ 6.944ms`다. CPU 프레임·GPU 실행·제출 처리량·실제 표시율·입력부터 표시까지의 지연을 나누어 측정한다. 비동기 제출 수만 많다고 GPU가 144frame을 완료했다고 계산하지 않는다.

해당 M3 Pro 14인치 제품의 내장 ProMotion은 최대120Hz로 명시되어 있다. 로컬 사양과 제품 문서를 함께 참고한 장치 해석이다. 따라서 내장 화면에서 눈에 보이는 144Hz 표시를 인증하지 않고, 처리량 시험·GPU 완료·고주사율 외부 화면 시험을 구분한다. 브라우저 requestAnimationFrame도 표시 cadence와 별도로 해석한다. [Apple 공식 사양](https://support.apple.com/en-us/117736)

## 측정 계약 카드

| 필드 | 기록할 내용 |
| --- | --- |
| 환경 | engine commit·OS·toolchain·backend·GPU/driver·전원·온도·화면·worker 수 |
| 렌더 | 내부/출력 해상도·quality·present mode·frame cap·asset hash·camera |
| 작업 | entity 구성·읽기/쓰기·활성 cell/chunk·변경률·시야·이동·network |
| 시간 | cold 시작·warmup·관측 기간·반복 수·clock·계측 overhead |
| 지표 | p50/p95/p99/p99.9·최대·예산 초과·raw trace·GPU 완료 |
| 자원 | 메모리 high-water·큐 bytes/depth/age·in-flight·upload·retirement |
| 완료 | 응답성과 결과 처리량·정합성·시간 내 수렴·취소·진행 |

초기 반복안은 warmup 분리 + 수분 이상 관측 + 여러 번 반복이며, 정확한 길이는 workload와 표본 수에 맞춰 정한다. 장시간 soak·cold load·shader compile·최초 다운로드는 별도 시나리오다. 평균 fps 하나로 합격하지 않는다.

## 정확성 층

| 층 | 시험 대상 | 증거 |
| --- | --- | --- |
| 순수 데이터 | 좌표·ID generation·schema·명령 중복·질량 보존 | 기대값·property/reference 비교 |
| 동시성 | 게시/회수·queue 포화·cancel/완료·shutdown·FFI | 소유권 리뷰·가능한 모델 검사·sanitizer·stress |
| 시스템 통합 | script migration·chunk revision·GPU 수명·저장 | 상태 invariant·error injection·trace |
| 플랫폼 | native/Web 의미·worker·IME·device resume·export | 실제 환경별 실행·manifest |
| 네트워크 | 권한·손실·중복·순서·재접속·30명 부하 | server 상태·client 수렴·대역폭 |
| 시각 | 재질·색·shadow·tile·UI·animation·temporal | 고정 이미지·연속 프레임·허용 오차·수동 검토 |

Rust safe 코드·테스트 한 번만으로 모든 race 부재를 증명했다고 쓰지 않는다. unsafe·FFI·GPU와 논리 revision은 따로 본다. 도구의 지원 타깃·증명 범위를 함께 기록한다.

## 핵심 엔진 워크로드

| ID | 목적 | 통과할 조건 |
| --- | --- | --- |
| V-001 | 느린 mesh·I/O·script·physics 주입 | UI·입력·표시·취소가 승인 지연 계약 충족 |
| V-002 | queue·VRAM·resident 예산 포화 | 유한 자원·명시 Busy·중요 명령 유실 없음·낮은 우선순위도 진행 |
| V-003 | snapshot producer/consumer 속도 역전 | 덮어쓰기·use-after-free·무한 보관·메모리 증가 없음 |
| V-004 | 대량 ECS read/write와 구조 변경 | 정의한 결과·충돌 없는 실행·tick budget. entity 수만으로 평가하지 않음 |
| V-005 | reload 오류·schema 변경·지연 callback | 상태 보존·실패 복구·이전 세대 거부·반복 회수 |
| V-006 | 기본 render·capture·device loss | 실제 이미지·색공간·비동기 map·유한 캡처·복구 |
| V-007 | 청크 이동·teleport·편집·경계 | mesh/light/collider revision·상주 메모리·완료 지연 충족 |
| V-008 | 유체 대량 방출·타일 파괴·광원 제거 | 질량·용량·경계·반응·수렴·tick budget 충족 |
| V-009 | 30명 분산·집중·신규 접속 | authority·수렴·입력/전송 우선순위·서버/대역폭 예산 |
| V-010 | crash·손상 journal·업그레이드 | 합의된 데이터 손실·복구 시간·migration 계약 충족 |
| V-011 | 한글 IME·게임패드·긴 UI·DPI | 실제 입력·focus·읽기·가상화·로딩 중 동작 |
| V-012 | 캐릭터·pose·판정·오디오·경로 | event 중복/누락·음 끊김·경로 버전·조작 지연 계약 |
| V-013 | HTML/Wasm·native export | 깨끗한 배포·최초 로드·version·저장/API 의미 동일 |
| V-014 | 실행 중 오토파일럿 조작·검증 | 같은 session 유지·실제 입력 경로·조건/시간·server 적용·관련 frame 증거·취소 입력 해제 |
| V-015 | C#/Rust 게임플레이·C++ 어댑터 | 공통 schema/명령/등록·ABI layout·allocator·타깃 진단·Rust 재빌드·C# 반복 reload |

각 workload의 입력량·budget 숫자는 W1에서 채운다. 단순히 부하를 줄여 frame만 유지하고 지형·유체 결과가 무기한 밀리면 불합격이다. 렌더 응답성·simulation tick·스트리밍 처리량을 함께 측정한다.

V-005·014·015의 상세 인수는 [SDK](../03-architecture/gameplay-and-mod-sdk.md)와 [오토파일럿](../03-architecture/autopilot-and-live-testing.md)을 따른다. 지원 edit 적용, 전체 모듈 교체, 일반 테스트 요청을 구분한다. native DLL hot-reload는 초기 합격 조건에 넣지 않는다.

## CI의 실제와 미래

현재 CI는 Markdown·문서 검사기만 검증한다. 미래에는 CPU correctness·Wasm conformance·실제 GPU smoke·성능 전용 runner·30명 server 부하·장시간 soak를 비용과 환경에 맞춰 나눈다. GPU 없는 일반 runner의 문서 테스트를 그래픽·144fps 검증으로 표현하지 않는다.

성능 회귀는 기준 장비에서 같은 input·profile·warmup·원시 자료로 비교한다. GPU별 pixel 차이를 blanket snapshot update로 숨기지 않고, 정확성 허용 오차와 품질 변화의 검토 이유를 남긴다.
