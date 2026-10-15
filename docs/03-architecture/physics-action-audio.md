# 물리·캐릭터·애니메이션·오디오·내비게이션

상태: 고품질 액션을 위한 엔진 계약 초안. 라이브러리 미선정 · 2026-10-15

## 이 문서를 분리한 이유

사용자가 원하는 품질은 지형과 이미지뿐 아니라 UI·액션까지 포함한다. 물리 엔진을 연결하는 것과 캐릭터가 만족스럽게 움직이고 애니메이션·효과음이 맞물리는 것은 다른 작업이다. 이 문서는 게임 규칙을 하드코딩하지 않고 이를 제작할 엔진 기능을 나눈다.

## 물리 소유권과 변경

물리 world는 지정 simulation 경계에서 소유한다. tick 동안 rigid body·shape·query 자료를 누가 읽고 쓰는지 선언하고, 결과 pose·contact·trigger event를 버전 있는 출력으로 제공한다. renderer·script·MCP가 solver를 임의 수정하지 않는다.

Jolt는 C++ rigid body 후보이며 Rust FFI·작업 시스템·Wasm을 따로 검증한다. 2D는 Rapier 2D·타일 전용 충돌·조합을 비교한다. 3D rigid body가 2D 유체를 대신하지 않는다. [Jolt](https://github.com/jrouwe/JoltPhysics), [Rapier 2D](https://docs.rs/rapier2d/latest/rapier2d/)

shape 생성은 배경 작업으로 가능해도 live physics world 반영에는 안전한 경계가 필요하다. 지형 revision과 collider revision을 추적하고, 지연 중 권한 tile/voxel query·안전 proxy·진입 제한 중 어떤 정책을 쓸지 정한다. 서버·클라이언트 차이도 기록한다.

## 캐릭터·액션 기능

| 기능 | 엔진이 제공할 계약 | 게임이 정의할 것 |
| --- | --- | --- |
| 이동 | sweep·slide·접지·계단·경사·플랫폼·중력 방향 후보 | 속도·가속·점프·대시 규칙 |
| 행동 | action state·취소/전이·입력 buffering·타이밍 event | 공격 조합·스킬·쿨다운 |
| 판정 | ray/sweep/overlap·layer·hit 결과·tick | 피해·팀·무적·보상 |
| 카메라 | 추적·충돌 회피·spring·blend·cut | 시점·연출·조준 방식 |
| 네트워크 | 입력 sequence·예측 상태·권한 보정 | 경쟁성·lag compensation 허용 |

hitbox·hurtbox·animation event·physics contact를 같은 사건으로 뭉개지 않는다. 하나의 피해가 이중 발생하지 않도록 event ID·tick·소유자를 정의한다. 입력→행동→pose→충돌→표시의 지연을 각각 계측한다.

## 애니메이션 데이터 흐름

`게임플레이 상태 → animation graph/상태 전이 → clip sampling·blend → 필요 시 IK/root motion → simulation 또는 presentation pose → skinning/draw`

root motion이 이동의 원인인지 시각 결과인지, IK가 물리 쿼리를 어느 tick에서 읽는지 결정해야 한다. simulation의 권한 collider와 표시용 pose를 구분한다. event는 loop·blend·프레임 건너뜀·rollback·reload에서 중복/누락 정책을 가진다.

픽셀 프레임, HD sprite, 2D skeletal, 3D skeletal의 공통 계약은 clip 시간·event·상태 전이·asset generation이다. ozz는 애니메이션 부품 후보이고 전체 action graph나 게임플레이 시스템을 자동 제공하지 않는다. [ozz 소개](https://raw.githubusercontent.com/guillaumeblanc/ozz-animation/master/README.md)

## 오디오 계약

오디오 backend는 아직 선택하지 않았다. callback은 준비된 data를 소비하고 file I/O·world lock·불필요한 allocation을 피하도록 설계한다. decode·stream·mix·bus·volume·spatialization·voice limit·우선순위·device 변경·pause/resume을 조사한다.

시각 frame clock과 audio sample clock이 다름을 드러내고, animation event·hit event의 중복 재생을 막는다. Web autoplay/user activation·모바일 audio route는 platform adapter의 계약이다. 음성 채팅은 별도 네트워크·권한·서비스 조사 영역이다.

## AI·내비게이션 계약

path·공간 쿼리는 입력 world revision과 취소 토큰을 갖는 유한 작업으로 제공한다. 파괴된 지형에서 과거 경로를 확정 결과처럼 사용하지 않는다. tile grid·voxel·일반 3D navmesh의 표현은 나누되 요청·결과·age·실패 계약은 공통화한다. 최종 내비게이션 라이브러리는 미선정이다.

## 인수와 후속 조사

최소 fixture는 계단·경사·움직이는 플랫폼·고속 충돌·지형 파괴·pose 전이·event 중복·오디오 underrun·device resume·경로 취소를 다룬다. 플레이 가능한 게임 프로젝트를 만들 필요는 없다. 캐릭터·액션·오디오·내비게이션은 각각 별도 상세 명세와 기술 실험을 갖기 전 완료로 표시하지 않는다.
