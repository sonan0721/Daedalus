# 실행 세션·병렬 ECS·표시 통합

상태: E01/E02/E08 다음 구현 계약 · 2026-10-15

## 목적과 소유권

개별 라이브러리 검증 다음에는 실제 같은 엔진 세션을 관찰·조작하고, 콘텐츠를 표시하는 실행 경로가 필요하다. 이 문서는 CLI를 다시 실행할 때마다 다른 월드가 만들어지는 초기 구현을 넘어서는 계약이다. 게임 fixture를 엔진 기능으로 포장하지 않고 실제 엔진 객체를 연결한다.

OS window/event loop는 macOS의 main thread에 둔다. simulation 실행 영역 하나가 권한 ECS·clock·command 적용을 소유한다. renderer는 소유한 scene 또는 불변 추출 데이터만 받는다. worker는 소유한 입력으로 CPU 작업을 수행한다. 제어 I/O·파일 I/O·GPU readback은 presentation callback에서 기다리지 않는다.

## CPU 예산

Bevy ECS의 병렬 executor와 Rayon background pool이 각각 전체 코어를 가져가는 기본값을 금지한다. 프로세스의 ECS worker 예산·background worker 예산을 명시하고 실제 설정을 진단 결과에 공개한다. UI·OS·audio·driver thread까지 해당 숫자로 제한된다는 보장은 하지 않는다.

Bevy의 process-global pool을 사용하면 최초 설정과 다른 후속 engine 설정을 조용히 무시하지 않는다. 일치하지 않는 요청은 명시적으로 거부하거나 이미 설정된 유효 예산을 반환한다. 독립 system의 동시 실행과 충돌하는 mutable access의 직렬 실행을 실제 일정으로 검사한다. 한 system 안의 거대한 작업까지 자동으로 쪼개진다고 가정하지 않는다.

## 지속 제어

초기 로컬 전송은 stdio JSON Lines다. 프로세스 하나·세션 하나에서 inspect/spawn/get/set/step/snapshot/종료를 반복한다. stdout은 기계가 읽는 응답만 출력하고 진단은 stderr로 보낸다. 요청·응답은 protocol version과 request ID를 가지며 엔티티 ID는 현재 session·generation을 검사한다.

줄 길이·한 요청의 tick 수·entity 수·결과 크기·pending command 수를 제한한다. 한도 초과는 구조화된 오류이며 무제한 `read_line` 뒤에 길이만 확인하는 구현은 피한다. 잘못된 JSON 한 건이 세션을 죽이거나 이전 상태를 바꾸지 않아야 한다. EOF와 명시적 종료는 worker 취소·수명 회수로 연결한다. 이 로컬 trusted 제어 경로를 인증된 원격 멀티플레이 프로토콜이라고 부르지 않는다.

`Accepted`는 큐 수락이고 `Applied`는 tick에서 적용된 결과다. 잘못된 revision·중복 command·이미 제거된 handle·단계 적용 실패를 성공 응답으로 바꾸지 않는다. 교체·시간 초과·취소의 결과는 해당 request ID로 추적한다.

## 콘텐츠·캡처·창 연결

scene validate와 patch는 `daedalus-content`의 schema 및 atomic validation을 재사용한다. 지원하는 component type과 transform 의미를 명시한다. 인식하지 못하는 렌더 component를 임의 cube로 대체해 성공으로 보고하지 않는다. 초기 unsupported 진단을 허용한다.

capture CLI는 실제 renderer 경로를 호출하고 출력 파일·크기·adapter/backend·render 입력을 보고한다. PNG가 초기 검증 형식이다. WebP/동영상·비동기 연속 캡처는 실제 구현 전 미지원으로 남긴다. 라이브러리 검증 fixture를 쓰는 명령은 fixture라는 사실을 명시한다.

native window는 bounded scene/control 채널을 try 방식으로 소비한다. 느린 voxel 생성·ECS tick·파일 작업이 완료되지 않아도 마지막 scene으로 event/input/loading 표시를 처리한다. 최신 상태를 건너뛸 수 있는 표시 메시지와 손실되면 안 되는 command 결과를 같은 drop 정책으로 다루지 않는다. 초기 단순 loading overlay는 완성된 UI 엔진이 아니다.

청크 생성·meshing의 첫 통합은 bounded task에서 수행하고 revision·generator·neighbor stamp를 확인한 뒤 결과를 채택한다. GPU upload 양·메시 output bytes·유효 작업 수를 제한한다. 테스트용 작은 청크가 성공했다고 무한 지형 스트리밍·거주성·VRAM 예산까지 완료한 것으로 표시하지 않는다.

## 실패·종료

control 입력 오류는 요청 단위로 격리한다. simulation panic·channel disconnect·renderer 오류는 세션 종료 원인으로 보고하며 불완전한 결과를 성공으로 출력하지 않는다. 창 종료 후 simulation의 협조 취소와 join은 표시 callback 밖에서 한다. 취소를 무시하는 외부 코드를 강제 중단할 수 있다는 보장은 하지 않는다.

## 종료 증거

- 같은 process/session에서 spawn → step → inspect가 실제 state를 보존하고 malformed/oversized 요청 뒤에도 제어가 이어진다.
- 병렬 ECS의 독립 system 두 개가 겹쳐 실행되고 충돌 system은 안전하게 실행된다. 서로 다른 global 예산 요청은 명시적 결과를 낸다.
- CLI가 실제 GPU PNG를 생성하고 잘못된 scene·잘못된 출력 경로·GPU 부재를 오류로 반환한다.
- 창/입력 경로가 느린 worker를 기다리지 않는 구조와 queue 상한을 검사한다. 실제 창의 반응성은 별도 실행 기록이 있을 때만 검증 완료로 표시한다.
- 기존 C# bridge·content·snapshot API 회귀 검사를 유지한다. 네트워크30명·web·전체 UI·저장 내구성은 각 후속 계약에서 검증한다.
