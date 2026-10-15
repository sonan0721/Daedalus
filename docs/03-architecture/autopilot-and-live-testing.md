# 오토파일럿·실행 중 테스트·로직 주입

상태: 두 기능 모두 사용자 요구, 프로토콜·예산은 설계 초안 · 2026-10-15

연결: REQ-006·015·020·024, [CLI·MCP](cli-mcp-observability.md), [Lua 조사](../02-research/unity-lua-and-autopilot.md), [게임플레이 SDK](gameplay-and-mod-sdk.md).

## 목표와 소유권

AI와 개발자가 실행 중인 세션을 관찰·조작·검증하고, 필요한 경우 새 C# 게임 로직도 투입한다. 두 기능을 제공하되 모든 테스트 동작마다 게임 assembly를 다시 로드하지 않는다. 게임 world·입력 라우터·시뮬레이션 스케줄은 엔진이 소유하고 외부 실행기는 요청·관찰·검증 순서를 소유한다.

## 두 실행 경로

| 경로 | 입력 | 엔진에서 수행할 일 | 상태 유지 |
| --- | --- | --- | --- |
| 플레이 제어 | 입력/action·조회·조건·step·capture 요청 | 정상 입력/게임 명령 처리, 관측 결과 반환 | 같은 world·session 유지 |
| 로직 주입 | C# 소스/빌드·module ID·API/schema version | 비동기 build·검증·staging·migration·안전 경계 전환 | 명세한 권한 상태 유지, 실패 시 이전 세대 |

테스트 순서의 분기·반복·assertion은 외부 실행기에서도 작성할 수 있다. 실행기를 바꾸거나 재시작해도 게임을 재컴파일할 이유가 없도록 공통 제어 프로토콜을 제공한다. C# 실행기를 제공할 수 있지만 CLI/MCP 클라이언트의 구현 언어를 C#으로 제한하지 않는다. 엔진 내부 범용 eval은 기본 제어 API로 가정하지 않는다.

## 플레이 제어 API에 필요한 동작

| 분류 | 명세할 기능 | 결과 근거 |
| --- | --- | --- |
| 관찰 | entity/component snapshot, UI tree·focus·visible state, 세션·로딩 상태 | schema·world/session·simulation tick |
| 입력 | 장치/action·버튼 press/release·axis·pointer·text 입력 | 수락 시각·입력 적용 tick·실제 처리 결과 |
| 대기 | tick/frame 진행, 조건 만족, 로딩 완료 | 기준 clock·조건·deadline·관측 tick |
| 시간 제어 | pause/resume·정해진 tick 수 step | 허용 실행 모드·진행 tick·권한 범위 |
| 게임 명령 | 등록된 interact/spawn/debug command | 권한·revision·실패 이유·완료 tick |
| 캡처 | camera·render frame·품질·이미지/영상 요청 | frame ID·사용 snapshot tick·asset/module generation |
| 검증 | invariant·기대 상태·시각 비교·trace 수집 | 기대/실제·manifest·재현 입력 |

이 표는 API 요구이며 현재 실행 가능한 command 목록이 아니다. 물리 시간·렌더 프레임·wall clock을 혼용하지 않는다. pause 상태에서 tick 기반 조건이 진행하지 않으면 step 또는 명시적 timeout 결과를 사용한다.

## 비동기 실행과 실패

요청은 유한 큐·request ID·session ID·deadline·취소 규칙을 갖는다. 수락과 적용 완료를 분리한다. 시나리오가 기다리는 동안 렌더/UI/input 스레드를 sleep·join으로 막지 않는다. 입력 지속 시간·중복 전송·연결 끊김을 정의하고 취소 시 눌린 버튼·axis를 해제한다.

오래된 session/entity/module generation 요청은 거부한다. timeout이 실제 작업 취소를 뜻하는지 결과에 표시한다. 외부 실행기가 실패하면 게임은 계속 실행할 수 있어야 하며 종료된 실행기의 미완료 요청을 정리한다. 반대로 in-process 게임 코드가 협조하지 않는 경우에는 CoreCLR만으로 안전한 강제 종료를 보장할 수 없다.

캡처는 입력 수락 직후의 임의 화면으로 성공 처리하지 않는다. 요청한 상태 변경이 적용된 tick과 이를 반영한 렌더 snapshot을 연결한다. 비동기 GPU readback·인코딩의 완료는 별도 상태로 반환한다.

## 정상 플레이와 디버그 조작 구분

이동·공격·UI 조작 검증은 실제 게임의 입력/action·UI routing 경로를 통과한다. 위치 직접 수정·무적·임의 spawn 같은 debug 명령은 별도 capability와 결과 표시를 둔다. teleport로 이동한 결과를 이동·충돌 테스트 통과로 기록하지 않는다.

멀티플레이 검증에서는 client 입력, server 권한 적용, client 반영을 구분한다. 로컬 UI가 바뀐 것만으로 서버 world 변경을 통과 처리하지 않는다. pause/step이 로컬 표시만 멈추는지, 테스트 서버의 권한 tick까지 제어하는지는 세션 모드별로 명세한다.

## 로직 주입과 코드 변경

C# 지원 edit 적용과 모듈 교체는 [핫리로드 계약](scripting-and-hot-reload.md)을 따른다. 새 로직 컴파일·로드를 테스트 명령 평가와 분리하고 source/build hash·진단·migration·새 generation·첫 영향 tick을 보고한다. 지원하지 않는 편집은 명확한 재시작 필요 결과를 반환한다.

Rust 게임플레이는 사용자 선택에 따라 재빌드·재시작한다. 오토파일럿이 Rust 게임을 테스트할 수 있다는 요구를 Rust 자체 hot-reload 요구로 확대하지 않는다. Lua VM을 추가하지 않고도 두 경로를 설계할 수 있으나 실제 지연·회수·상태 보존은 후속 통합 시험이 필요하다.

## 실행 범위와 재현

개발 제어 endpoint의 접근 범위와 생산 빌드 포함 여부를 build profile/capability로 명시한다. PC 모드 로딩, 개발 중 로직 주입, 원격 플레이테스트는 다른 권한 기능이다. 공개 서버에 개발자용 임의 코드 주입 권한을 자동으로 노출하지 않는다.

재현 자료에는 엔진·게임 module·SDK·자산 hash, seed, scenario revision, 적용 tick이 있는 입력 trace, network 조건, snapshot/capture 연결, 실패 진단을 담는다. 동일 자료의 플랫폼별 허용 오차와 비결정적 효과는 별도 검증 카드로 정의한다.

## 향후 최소 인수 시나리오

1. 같은 실행 세션에서 서로 다른 테스트 순서를 보내고 world identity가 유지됨을 확인.
2. 입력 → tick/조건 대기 → UI interaction → 상태 assert → 관련 프레임 capture를 하나의 trace로 연결.
3. 컴파일 오류·API 불일치·migration 실패에서는 기존 C# 로직 유지, 정상 변경은 보존 상태로 전환.
4. 로딩·복셀 작업·느린 테스트 요청 중 UI·입력·렌더 지연과 큐 상한 측정.
5. 실행기 종료·timeout·중복·낡은 세대·cancel 때 입력 해제와 결과 정합성 검증.
6. client 요청 → server 적용 → client 표시 확인. 직접 debug 변경과 정상 플레이 증거 분리.

현재 이 시나리오는 계획이며 테스트 실행 결과가 아니다. 기술 실험과 구현 코드는 Luna xhigh가 작성한다.
