# P02.3a.2: actual surface capture renderer 작업 기록

상태: source 구현 착수, 실제 GPU 검증 전 · 2026-10-18

## 만든 이유

[runtime a.1 기록](2026-10-18-live-capture-foundation.md)의 CPU metadata fixture와 실제 surface 픽셀 복사를 구분한다. `f3880d7`은 요청·credit·수명 기반만 인수했으며 main의 기존 CLI는 여전히 `live_capture=false`다. 이번 기록은 [renderer 반환 계약](../03-architecture/live-capture-renderer-return.md)의 source·실패·CPU 검사·실제 native proof를 별도로 추적하기 위한 것이다. 아직 테스트 성공 결과는 없다.

## Source 소유권과 순서

Luna D가 renderer의 새 `window/capture.rs`·기존 managed/window·packet renderer/fence/drain·root exports와 집중 시험을 소유한다. 필요한 main-thread native harness와 최소 example manifest는 먼저 공유한다. 새 runtime dependency·Engine·배포 tag·게임 프로젝트를 추가하지 않는다. root는 문서·리뷰·통합/GPU 실행·Git을 맡는다. host a.3 담당자는 현재 읽기 전용 설계 감사만 한다.

runtime/public test source는 freeze됐다. Cargo lease는 renderer 담당자에게 넘겼으며 source를 멈춘 snapshot에서 focused CPU 검사를 실행한다. 다른 담당자는 Cargo를 동시에 실행하지 않는다. 첫 checkpoint는 channel/readback/fence CPU, 다음은 actual window copy/pump와 native harness다. full workspace/실제 GPU 검사는 root source 리뷰 후 별도로 실행한다.

## 동결한 핵심 경계

- 요청/반환 lane과 단일 buffercell을 수락 전에 함께 예약한다. 성공한 renderer key는 재사용하지 않는다. 전송 거부는 원래 boxed request를 보존한다.
- source는 실제 host stage routing의 key/session/generation/digest다. 선택은 실제 drawn `ManagedPresentedView`·tick floor·크기·view와 일치해야 한다.
- 실제 COPY_SRC/format 확인 뒤 one-shot Ready를 게시한다. 지원하지 않으면 normal rendering은 유지한다.
- cell guard를 얻은 뒤 create/install까지 유지해 UI panic 때 local Buffer를 남기지 않는다. scene/overlay→같은 acquired texture copy→present 호출 뒤만 Submitted다.
- map/unmap·Buffer handle Drop과 실제 copy queue fence를 구분한다. normal window Poll은 UI, 반환 owner 뒤 Poll은 CPU owner가 맡는다. view·rowcopy·swizzle·unmap·final Buffer drop은 worker 경계다.
- Drop은 abandon 신호이며, outstanding handle이 남으면 drain Pending이다. 실패/닫힘도 같은 cell을 cleanup으로 반환한다.

## 아직 열려 있는 인수

| Gate | 상태 |
| --- | --- |
| CPU lane/소유권/거부/limit/alignment·map/fence/cancel/panic | 구현·실행 전 |
| 실제 surface capability·같은 acquired frame의 pixels/metadata | native harness 전 |
| known color·RGBA/BGRA·manual-sRGB·overlay·resize·old-ready view | 실제 GPU 검증 전 |
| close/worker drop·mapped view/queue fence·owner drain/final buffer drop | 구현·실제 검증 전 |
| host artifact·PNG/file/reader·CLI protocol capture | 후속 a.3/a.4, 미구현 |

실제 실행 raw·source/binary hash·실패·장치 범위는 실행 뒤에만 추가한다. 별도 offscreen PNG를 이 gate의 증거로 쓰지 않는다. a.2·상위 P02.3a·D1/D2/D3는 미통과다.
