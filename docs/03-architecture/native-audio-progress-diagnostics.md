# Native 오디오 재생 진행·종료 진단 계약

상태: 과거 Kira/CPAL 진행 진단 실패·A-042 뒤 native-only19/clean package combined72 통과·과거 실패 원인 미확정 · 2026-10-17 · P03 회귀/DEV06 · 제품 인수 미통과

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

## 다음 한 요인 비교: Kira 없는 CPAL stream

직접 Kira 진단은 실제1건 실행됐지만10초 동안 position0·CPU표본0·Playing으로 실패했다. 이전 성공 binary도 현재 같은 helper에서 실패했다. pinned CPAL source에서 해당 Xrun은 CoreAudio processor-overload property notification으로 전달되고, Kira는 Xrun을 device-invalidated처럼 자동 restart하지 않는다. 내부 mixer block128은 CPAL device buffer128 지정이 아니다. Kira가 사용하는 device/config의 buffer는Default다. 이 둘을 혼동해 buffer를 바꾸지 않는다.

다음 ignored test는 같은 audio crate test-only 영역에서 Kira가 재노출하는 pinned CPAL을 직접 사용한다. Kira manager/renderer만 제외하고 `default_host → default_output_device → default_output_config().config()`와 f32/default-buffer/실제 play는 유지한다. 명시 device로 바꾸면 CPAL의 DefaultOutput→HAL 모드까지 달라지므로 이 비교에서는 바꾸지 않는다.

출력 callback은 silence를 채우고 atomic callback/frame 계수만 증가시킨다. callback 안에서 출력·format·동적 할당·mutex 대기·sleep을 하지 않는다. error callback은 유한 atomic 계수/분류만 기록하고 출력은 관측 thread에서 한다. startup 설정·wall time과 play 후 최대10초의 callback/frame 증가·오류 분류를 보존한다. 성공은 실제 여러 data callbacks/유효 frame 증가가 관측된 경우뿐이며 audibility/clip 종료/기존 CLI 통과를 뜻하지 않는다. callback0은 명시 실패한다. 장치 없음/생성/play 오류도 실패로 보존하며 skip이나 offline 대체를 하지 않는다.

이 비교가 실패하면 wrapper를 제외한 CPAL/default CoreAudio 경로에서도 재현된 것이다. 성공하면 Kira 경로와의 차이를 더 조사한다. 어느 경우든 자동 backend 교체·dependency 업그레이드·OS 설정 변경을 실행하지 않는다. 최초 범위에서는 explicit HAL 비교를 보류했고, 첫 결과 리뷰 뒤 아래 별도 계약으로 범위를 확정했다.

## 직접 CPAL 실패 뒤 같은 physical device 비교

직접 CPAL 첫 실행은48kHz/stereo/F32/default buffer에서 build/play를 성공했지만 약10초 동안 실제 atomic data callback/frame 계수가 모두0이었다. Kira telemetry 부재와 달리 이 계수는 직접 callback 진입을 센다. 오류 계수도0이었다. 위 결과를 리뷰한 뒤 다음 한 요인 비교를 허용한다.

같은 ignored direct-CPAL silence 계측 함수를 두 entrypoint에서 재사용한다. 기존 default entrypoint 의미/실패는 유지한다. 새 macOS-only entrypoint는 현재 default handle의 opaque device ID와 `host.devices()`의 ID를 비교해 **같은 physical device**의 열거 handle을 고른다. display name/hardcoded device 이름으로 고르지 않는다. ID 없음/불일치/중복/설정 불일치는 실패로 남기며 다른 장치를 조용히 사용하지 않는다. 두 handle의 ID·sample rate·channels·reported format과 default buffer 일치를 확인한다. 같은 f32 silence·atomic callback/error 계수·10초 deadline·최종 진행 assertion을 사용한다.

고정 CPAL source에서 default handle은 `is_default_output=true`의 DefaultOutput AudioUnit을 사용하고 열거 handle은 false의 HAL-pinned output을 사용한다. OS 기본 장치나 hardware sample rate/buffer를 사용자가 바꾸는 작업이 아니다. test가 요청하는 config는 해당 장치의 현재 default이며 별도 fixed buffer를 지정하지 않는다. `stream.play()`·Drop은 실제 장치 사용의 정상 수명을 따른다. callback에서 로그/할당/대기하지 않는다. 다른 제품 기능/API/엔진의 device 선택 정책은 변경하지 않는다.

HAL만 진행하면 같은 물리 장치의 DefaultOutput/HAL 경로 차이를 근거로 남기고, 곧바로 production을 HAL로 바꾸지 않는다. 둘 다0이면 관측된 실패를 Kira wrapper나 default-routing 한 경로만으로 좁힐 수 없다. 그때 실제 시스템 출력 상태·CoreAudio 오류를 추가 조사하며 OS 재시작/daemon 조작은 자동 수행하지 않는다. 두 결과를 얻기 위해 성공할 때까지 반복하지 않는다. deadline은 대기 정책이며 OS scheduler·출력 overhead까지 정확히10.000초 안에 끝난다는 hard real-time 보장은 아니다. 실제 elapsed를 숨기지 않는다.

## 두 출력 모드 실패 뒤 OS 비교

직접 CPAL의 두 모드에서 actual callback0이 확인됐으므로 다음은 source를 다시 바꾸기 전에 수행하는 로컬 운영 진단이다. OS 버전과 최근 CoreAudio 오류 중 startup/overload 관련 항목만 제한해 읽고 ignored 원본에 보존한다. 권한 부족이나 로그 없음은 그대로 기록하며 정상 출력 근거로 취급하지 않는다. 다른 앱의 실제 재생 상태는 사용자에게 별도 확인했고 답이 없으면 정상/비정상을 추정하지 않는다.

같은 Mac에 기본 제공되는 `afplay`와 기존 system sound 파일을 사용해 **해당 재생만 volume0**으로 finite playback을 비교할 수 있다. 전역 volume/default device/daemon 설정을 바꾸지 않고 파일을 repo/package로 복사하지 않는다. 최초 시도는10초 대기 뒤 중단을 요청했으나 tool 왕복 동안 실제15.30초가 지났고 AudioQueueStart(-66681)/exit1을 얻었다. 중단 신호와 자연 오류의 순서를 확정하지 못하므로 독립적인 무중단 실패 근거로 삼지 않는다.

이 모호함 때문에 OS utility만 한 번 더20초 연속 관측한다. 그 전에 신호를 보내지 않고 자연 exit/error를 확인하며,20초 뒤에도 실행 중이면 자기 process만 종료하고 관측 한도로 표시한다. 이것은 engine/CLI의10초 Ended 인수 조건 증가가 아니다. 정상 exit는 OS utility의 process 완료일 뿐 실제 가청 출력이나 callback 계수 증거가 아니다. 실패/대기 정지는 정확한 종료와 stderr를 남긴다. OS utility가 완료돼도 기존 CPAL/CLI 회귀가 통과한 것으로 보고하지 않는다. 이 비교 뒤 production 수정이나 장치 복구는 별도 원인·회귀 계획에 근거해야 한다.

## 사용자 가청 보고 뒤 재검증

2026-10-17 A-042에서 사용자는 다른 앱의 출력이 정상이고 조정자가 실행한 소리도 일부 들렸다고 답했다. 어느 과거 시도/시점인지 특정하지 않았으므로 실패한 callback0/position0 로그를 삭제하거나 성공으로 바꾸지 않는다. 시스템 전체가 무음이라는 가정도 하지 않는다. 앞선 검사 이후 수 시간이 지나 환경 상태가 같다고 단정할 수 없으므로, 먼저 보존된 수정본 archive의 동일 CLI/hash와 unchanged helper로 native-only 한 번을 실행한다. Ended 기준/10초 deadline·장치·backend·설정은 그대로다. 성공하면 현재 실행의 진행/종료 근거이고 원인 확정/과거 실패 면제가 아니다. 실패하면 실행 process의 user/Aqua session·기본 출력 경로와 일반 앱 차이를 read-only로 조사하며 새 코드/OS 조작 전에 한 요인 계약을 다시 명시한다.
