# Native viewer의 입력 소스와 자동 검증

상태: additive API 구현·로컬 전체 Rust/Python·실제 Mac 통합65 통과, CI/package 대기 · 2026-10-18 · DEV06/P02/P03

## 목적과 관측 근거

[실제 통합 기록](../06-worklog/2026-10-18-native-host-integration.md)의 전체 native 실행03은 입력이 없다는 camera 검증 중 실제 key events6/local camera changes5를 기록했다. 입력의 발생 주체를 추정하거나 assertion을 낮추지 않는다. 자동 검증은 protocol이 변경한 authority camera와 실제 표시를 안정적으로 대조해야 하며, 동시에 개발자의 다른 앱 입력을 받으려고 창/app을 활성화할 필요가 없다. 현재 managed 교체 집중 실행04의35통과와 이 quiet camera 실패는 별개로 유지한다.

## 명시적 정책과 API 호환성

renderer에 `WindowInputPolicy::{Native, ProtocolOnly}`를 추가한다. default는 Native다. 기존 public struct `WindowOptions`, `ManagedPacketWindowOptions`, `NativeProjectViewOptions`에 필수 field를 추가하지 않으며 기존 함수 signature와 default를 유지한다.

| API | additive 의미 |
| --- | --- |
| `run_packet_windowed_presented_with_input_policy(initial_packet, messages, options, limits, asset_limits, policy)` | 기존 packet viewer와 같은 private 구현에 policy를 전달한다. 기존 presented 함수는 Native로 위임한다. |
| `run_packet_windowed_managed_with_input_policy(initial, server, options, policy)` | 기존 managed catch/owner 반환과 event loop를 그대로 사용한다. 기존 managed 함수는 Native로 위임한다. |
| `NativeProjectHost::run_view_with_input_policy(options, policy)` | 같은 host/one-shot consumer/window 실행에 policy를 전달한다. 기존 run_view는 Native로 위임한다. |

CLI `project view --input-source native|protocol`은 default native이며 두 viewer branch 모두 같은 enum으로 매핑한다. 새로운 source load·actor·renderer를 만들지 않는다. report에는 선택한 `input_source`를 명시한다. live capture/MCP/일반 gameplay input SDK를 구현했다는 의미는 아니다.

## 이벤트 처리와 창 시작

Native는 기존 key/camera/observer 동작을 유지한다. ProtocolOnly는 viewer의 native keyboard·pointer·wheel에서 local camera 조작과 diagnostic arrow/F10/Escape 동작을 실행하지 않고, 그런 control 이벤트를 runtime input observer로 전달하지 않는다. protocol mutation/Step/Close는 그대로 동작한다. 실제 관측 counter는 없었던 입력으로 바꾸지 않는다.

Focused·Resized·CloseRequested 등 창 생명주기는 두 모드 모두 처리한다. resize는 실제 surface/physical dimensions에 반영하며 OS close button은 정상 종료 요청이다. ProtocolOnly에서 큰 event queue를 만들거나 event loop를 막지 않는다. held-key 이동·drag·wheel 등 개별 camera 경로에 빠진 guard가 없어야 한다.

ProtocolOnly 시작은 window attributes의 active=false를 요청한다. macOS는 event-loop builder의 activate_ignoring_other_apps=false도 설정해 기본 app activation 요청을 막는다. Native의 builder/window 기본값은 유지한다. 이는 애플리케이션이 요청하는 정책이며 OS 전체의 focus 고정을 보장하지 않는다. X11/Wayland 등 with_active 미지원 플랫폼에서도 엔진 내부 입력 routing 정책은 유지한다. window는 계속 실제 GPU로 렌더링한다.

## 자동 검증과 소유권

기존 `smoke_project.py`의 자동 project view 실행은 protocol 입력을 명시한다. 공통 command helper와 모델/스프라이트의 다른 직접 command 구성을 모두 점검한다. 기존 authority-camera assertion과 actual view/행렬·세대·source·종료 검사를 유지하며 입력이 들어왔다는 이유로 성공시키거나 생략하지 않는다. 수동 native 입력 검증은 기본 Native 경로로 별도 유지한다.

Luna D는 renderer window.rs/lib.rs와 host.rs의 policy·wrapper·실제 이벤트 gate 및 의미 있는 CPU 회귀만 소유한다. Luna A는 CLI project_view.rs/activation.rs와 smoke_project.py/test_smoke_project.py를 소유한다. API 이름을 먼저 공유하고 한 Cargo lease로 source 동결 뒤 focused CPU/Python→전체 Rust→실제 전체 native 재실행을 한다. manifests/lockfile 변경이나 새 test framework는 필요하지 않다. 기존 public example과 native API의 compile 호환성도 확인한다.

## 인수 기준

- 기존 Native API/default가 유지되고 protocol mode에서 physical control 입력이 local camera·diagnostic session operation을 만들지 않는다.
- Focus/resize/close는 동작하고 정상 JSONL Step·asset Stage/Commit·actual present/회수·종료가 유지된다.
- 기존 quiet authority-camera 검증을 포함한 전체 native flags가 같은 binary에서 통과한다.
- pipeline 개선 결과를 전체 D1·144fps·OS focus 보장으로 보고하지 않는다. capture pixels와 CI/package 제품 인수는 별도다.

실제 검증의 명령·source 범위·실패와 후속65통과는 [통합 기록](../06-worklog/2026-10-18-native-host-integration.md)을 따른다. 이전 실패를 지우거나 Native 물리 입력 자체의 새 자동 검증·live pixels·전체 엔진 인수로 확대하지 않는다.

## 1차 자료

2026-10-18 pinned winit0.30.12 source를 확인했다. docs.rs의 해당 version 페이지 조회는 실패했고 tag 원본은 확보했다.

- [WindowAttributes 원본](https://raw.githubusercontent.com/rust-windowing/winit/v0.30.12/src/window.rs): with_active와 플랫폼별 미지원 범위.
- [macOS builder 원본](https://raw.githubusercontent.com/rust-windowing/winit/v0.30.12/src/platform/macos.rs): with_activate_ignoring_other_apps와 기본 app activation 정책.

실제 focus/입력 결과는 후속 실행 근거로 기록하며 source만으로 hardware 검증을 대신하지 않는다.
