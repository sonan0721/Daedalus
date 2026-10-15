# 물리·오디오 초기 backend 구현 계약

상태: E06 구현 기준안. native 통합 시험 전 · 2026-10-15

## 범위와 선택

3D 물리는 Jolt를 우선 연결하고 2D는 Rapier 2D를 사용한다. Jolt의 Rust 경계는 oxijolt를 빌드·수명·타깃 검증 후 채택하는 후보로 둔다. 정확한 릴리스·feature·Rust/C++ 도구 요구는 실제 cargo metadata와 빌드 결과로 기록한다. C++ 부품을 허용한 Rust 코어 방향을 유지하며 자체 근사 중력 코드를 실제 물리 backend로 대체해 표시하지 않는다.

오디오는 Rust 소유 서비스와 native mixer/출력 경계를 둔다. Kira는 mixer·공간음·clock 후보이고 CPAL/miniaudio는 장치 계층 비교 대상이다. 현재 Kira 문서는 Wasm streaming 제한을 명시하므로 native 통합을 웹 기능 동등성 완료로 쓰지 않는다. 실제 채택 버전과 재사용 범위는 통합 결과에 기록한다.

## 물리 API와 소유권

- 2D/3D world를 simulation 경계가 소유하고 생성·shape·body 변경·step·query·해제를 제공한다. renderer와 C#은 raw body pointer를 보관하지 않는다.
- 엔진 entity handle과 backend body ID의 매핑·세대를 검증한다. 삭제·세션 교체 후 오래된 body/query 결과를 거부한다.
- 초기 shape는 2D box/ball, 3D box/sphere/capsule을 최소로 하고 static/dynamic/kinematic 의미를 구분한다. 지원하지 않은 shape는 명시적 오류다.
- fixed dt는 유한 양수와 상한을 검사한다. 중력·질량·회전·크기·속도의 유효 범위를 검사하고 backend 실패를 결과로 전달한다.
- 최소 query는 ray와 충돌 위치/법선이다. filter·layer·sensor 정책을 공개하며 구현하지 않은 CCD·joint·래그돌·character 기능을 상위 라이브러리의 기능만으로 완료 처리하지 않는다.
- Jolt 내부 worker 수는 명시적으로 제한한다. ECS·Rayon·Jolt가 각자 전 코어 pool을 만들지 않도록 프로세스 예산을 맞춘다. 공유 job adapter가 없으면 초기 별도 worker 예산과 제한을 기록한다.
- 지형 shape는 배경 생성과 world 반영을 나누고 terrain/collider revision을 보존한다. 첫 통합 fixture의 고정 shape가 파괴 지형 스트리밍 충돌을 검증한 것은 아니다.

## 오디오 API와 소유권

- 장치 callback은 준비된 PCM/mixer 상태만 소비한다. file I/O·ECS lock·C# callback·compile·decode를 callback에 넣지 않는다.
- 초기 기능은 buffer 등록·play/stop·volume/pan·버스·동시 voice 상한·frame/sample clock 식별이다. 미구현 spatial HRTF·차폐·잔향·voice chat을 지원 목록에 추가하지 않는다.
- 짧은 음원과 긴 streaming 음원의 소유권·decode 위치·메모리 상한을 구분한다. 출력 device가 없으면 명시적 오류 또는 요청한 offline 모드로만 실행한다.
- offline mixer는 실제 sample 결과를 만들고 WAV/분석 artifact로 검증한다. 단순히 play command가 수락됐다는 이유로 소리가 났다고 하지 않는다.
- 장치 enumerate/open/start와 실제 청취·loopback 측정은 다른 증거다. 테스트에 들리는 음을 자동으로 재생할 필요는 없으며 실제 출력 미검증 범위를 기록한다.
- 이벤트 ID·module generation을 사용해 tick 재실행·reload 시 중복 sound trigger를 제어한다. 음성 채팅은 별도 서비스다.

## 최소 검증

| 영역 | 입력 | 확인할 실제 결과 |
| --- | --- | --- |
| 3D | static floor·dynamic sphere·고정 step | 낙하 후 floor 접촉·관통 허용 범위·ray query·삭제 후 stale ID |
| 2D | static platform·dynamic body·fixed step | 접촉·위치·query·삭제·잘못된 입력 오류 |
| 수명 | 여러 world 생성·step·해제 | backend 전역 초기화·작업 종료·handle 정합성 |
| 오디오 | 고정 PCM·gain/pan/stop·voice 포화 | 실제 sample·peak/길이·무음 종료·명시적 포화 결과 |
| 출력 | native device 상태 조회·초기화 | backend/device·설정·명시적 미지원 진단 |

이 표는 구현 전 계약이다. 라이브러리 이름·구조체·mock 명령만으로 완료되지 않는다. 조작감·계단·경사·CCD·네트워크 예측·장시간 안정성은 추가 fixture와 인수 계약을 유지한다.

## 근거와 한계

[Jolt](https://github.com/jrouwe/JoltPhysics), [oxijolt API](https://docs.rs/oxijolt/latest/oxijolt/), [Rapier](https://rapier.rs/docs/user_guides/rust/getting_started/), [Kira](https://github.com/tesselode/kira), [CPAL](https://github.com/RustAudio/cpal), [miniaudio](https://miniaud.io/docs/manual/index.html). 소개 문서의 기능은 다이달로스의 실제 통합 상태가 아니며 구현한 부분만 검증 상태에 올린다.
