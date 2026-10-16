# Native 오디오 재생 진행·종료 진단 계약

상태: 진단 구현 전 동결 · 2026-10-17 · P03 회귀/DEV06 · 제품 인수 미통과

## 이 문서의 목적

CPU 세대 교체 수정본의 배포 CLI에서 native finite voice가 10초 안에 Ended에 도달하지 않았다. 같은 binary의 별도 native-only 실행에서도 재현했다. CI의 장치 없는 성공이나 default device의 구성 조회로 이 실패를 덮지 않는다. 매번 전체 window/asset/package 검증을 반복하기 전에, 재생 진행과 종료 처리를 구분하는 작은 실제 장치 진단을 먼저 만든다.

관련 계약은 [imported audio](imported-audio-runtime.md), 원본 시도와 binary identity는 [작업 기록](../06-worklog/2026-10-16-project-asset-activation.md)에 둔다. 이 문서는 공개 audio API 확장이나 backend 교체의 결정이 아니다.

## 확인한 사실과 미확정 원인

- 실패한 helper는 같은 voice에 서로 다른 조회 request ID를 사용한다. poll은 실제 runtime reconcile→Kira state/reap을 거친다. 종료 기준은 backend-observed Ended이며 현재 deadline은 10초다.
- failure context의 `status.active_voice_count=0`은 Play **이전** startup 응답이다. 재생 이후 상태와의 모순이나 종료 근거가 아니다.
- 실제 default device 조회는 MacBook Pro 스피커/48 kHz/stereo였다. 이 조회는 stream을 열거나 callback을 검증하지 않는다.
- pinned Kira 0.12.5의 `StaticSoundHandle::position()/state()`는 shared playback 관측값이다. desktop CPAL backend는 `pop_error()`, `num_stream_errors_discarded()`, `pop_cpu_usage()`를 제공한다. 로컬 registry의 해당 pinned source를 대조했다. stream 생성 성공만으로 callback 진행을 보장하지 않는다.
- callback 지연/중단, stream lifecycle, Kira 상태 갱신, runtime 연결 중 어느 것이 원인인지는 아직 미확정이다. 오류 queue가 비어 있어도 정상 callback을 증명하지 않는다.
- [S88](../02-research/source-register.md)의 pinned CPAL CoreAudio source는 device processor-overload listener에서도 Xrun을 전달한다. underrun 오류 하나만으로 data callback 실행 여부를 판정하지 않는다. Kira CPU usage 표본도 callback 계수 전체가 아니라 그 wrapper가 게시한 관측값이다.

## 최소 진단의 소유권과 범위

Luna xhigh가 `daedalus-audio/src/service.rs`의 test-only 영역에 명시적으로 opt-in하는 ignored native 진단을 작성한다. 기존 private service/manager/voice를 사용해 생산 API·protocol·dependency·lockfile을 변경하지 않는다. native가 없는 build는 영향을 받지 않는다. Cargo/장치 실행은 한 lease에서 직렬 수행한다.

기존 helper와 같은 8 kHz mono, 2,000-frame/0.25초, 440 Hz PCM16 상당 clip을 service의 실제 CPAL backend에서 한 번 재생한다. source amplitude는 기존 fixture와 같거나 더 작게 유지하며 looping은 끈다. sample 생성/정규화 방법과 실제 service 설정을 기록한다. offline rendering이나 수동 renderer 진행으로 native callback을 대신하지 않는다.

10초를 넘기지 않는 단조 시계 deadline과 약 50ms 관측 간격을 사용한다. 각 표본은 elapsed·voice state·position을 출력한다. 관측 중 backend error와 discarded count를 유한하게 수집하고 CPU usage 표본 수를 기록한다. 오류가 많아도 표본/문자열 출력에 상한을 두며 무한 drain을 하지 않는다. 마지막 state를 읽기 전에 voice를 reap하지 않는다. 마지막으로 실제 Stopped를 관찰한 경우에만 reap/count를 확인한다. 관측 시간이 늘어난 것을 성공으로 만들지 않는다.

startup 실패·진행 없음·종료 실패는 로그를 남기고 실패한다. test panic 때도 service를 정상 Drop할 수 있어야 한다. subprocess마다 source/hash와 실행 명령을 보존한다. OS 설정 변경, audio daemon 재시작, 사용자 device 변경, timeout 증가, 자동 retry 성공만 채택은 범위 밖이다.

## 판독과 인수

| 실제 관측 | 다음 판단 |
| --- | --- |
| position 증가 + Stopped + 실제 reap | 이 실행에서 low-level native 진행/종료 확인. 원래 extracted CLI 회귀는 별도 재검증 |
| position이 0 부근, state Playing | renderer/callback 진행이 관측되지 않음. device/stream/source 경로를 더 좁힘; OS 원인으로 단정하지 않음 |
| position 증가, finite clip 종료 뒤에도 Playing | Kira 상태 전이/관측과 clip 설정을 조사 |
| stream 오류 또는 discarded 증가 | 정확한 메시지/시점을 보존. transient와 영구 오류 정책은 별도 설계 |
| low-level 성공, 같은 조건 CLI 실패 | actor/backend 소유권·스레드·lifecycle 차이를 조사 |

해당 진단은 실장치가 필요한 opt-in이며 CPU CI의 성공 건수에 섞지 않는다. focused filter가 실제 test 1개를 실행했는지 확인한다. 원인에 맞춘 수정은 별도 source review와 regression을 요구한다. 기존 10초 Ended 인수 조건, PR draft 보류, b.2/b/c 및 D1/D2/D3 미완료 상태는 그대로 유지한다.

직접 service에서도 진행 없음이 재현되면, 과거 성공 기록이 있는 **보존된 이전 archive의 binary**에 같은 unchanged helper의 native-only 검사를 한 번 실행해 비교할 수 있다. 두 executable의 hash/원본 checkpoint를 먼저 확인한다. 이전 버전도 현재 실패하면 이번 CPU Stage 수정만의 회귀라는 가설은 약해지지만 환경/CPAL/Kira 중 원인을 단정하지 않는다. 이 비교는 과거 성공 로그의 재작성이나 성공할 때까지 재실행이 아니다.

## 개발 반복 최적화

기존 smoke entrypoint를 유지하고 실패한 native 부분만 먼저 분리 실행했다. 새 진단은 전체 72-check를 반복하기 전 원인 범위를 줄이기 위한 opt-in test다. production 수정이 생기면 영향받는 CPU/workspace/CLI/package를 다시 검증한다. 단순 진단 추가만으로 unchanged renderer/managed/asset suite를 매번 재실행하지 않는다. 절약 시간/성공률은 아직 측정하지 않았으며 개선 배율을 주장하지 않는다.
