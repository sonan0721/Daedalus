# E06: 실제 물리 backend·오디오 mixer 검증

날짜: 2026-10-15 · 구현 Luna xhigh · 조정자 독립 실행 확인

## 구현 범위

`daedalus-physics`에 Jolt3D·Rapier2D의 owned world, primitive shape/body, fixed step, ray query, session/generation body handle, revision과 입력 상한을 추가했다. Jolt는 이 Mac에서 native C++ source build와 실제 step을 통과했다. 여러 world의 동시 생성·step·해제를 검사했다. Rapier는 body 삽입·제거 직후 query를 갱신하는 zero-time step 경로를 사용한다.

`daedalus-audio`는 Kira의 실제 manager/mixer를 사용한다. CPAL native backend 생성 API, 장치를 열지 않는 default-output probe, bounded static mono/stereo PCM, voice handle, volume/pan/stop과 offline sample 렌더를 제공한다. offline backend는 장치 출력 대신 Kira가 계산한 PCM을 수집한다.

## 리뷰에서 보완한 문제

- Kira 기본 Tween10ms와 “즉시 정지” API 설명이 달랐다. explicit zero-duration tween을 사용하고 실제 mixer 처리 뒤 정지를 검사했다.
- sound handle의 Stopped 상태와 mixer resource slot 회수 사이에 한 처리 경계가 있었다. 아직 회수되지 않은 slot은 voice 포화로 반환하며 다음 processing block 뒤 재사용되는지 확인했다.
- Jolt의 실패/불완전 step은 native state 일부를 바꿨을 수 있다. revision/tick을 먼저 구분하고 해당 world를 poisoned 처리해 이전 revision의 정상 상태로 조회하지 않게 했다.
- 제거·다른 world·재사용 generation의 handle, 잘못된 body/ray 입력과 world 상한을 검사했다. safe adapter에는 workspace의 unsafe 금지를 적용했다.

## 실제 실행

환경은 Apple M3 Pro/macOS/aarch64, Rust1.94.0이다. 조정자가 다음 명령을 직접 실행했다.

```sh
cargo test -p daedalus-physics -p daedalus-audio --locked -- --nocapture
cargo test -p daedalus-audio inspect_native_default_output_device_without_playback --locked -- --ignored --nocapture
```

첫 명령에서 physics7건·audio3건이 통과했다. audio hardware diagnostic1건은 첫 명령에서 ignored였고 두 번째 명령으로 별도 통과했다. 실제 default output은 MacBook Pro 스피커·48,000Hz·2채널이었다. probe는 출력 stream을 열거나 들리는 음을 재생하지 않았다.

Luna가 package별 fmt·all-target Clippy `-D warnings` 통과를 보고했다. 조정자의 최종 workspace 통합 검사와 이 package 단위 근거는 별도로 기록한다.

## 남은 범위

이 결과는 실제 rigid-body backend와 static PCM mixer의 최소 통합 근거다. ECS 자동 physics phase·entity-body component mapping·C# physics API·character controller·terrain collider streaming·CCD/관절/래그돌 품질·오디오 버스/streaming·HRTF·차폐·voice chat·실제 청취/loopback·device 복구는 완료되지 않았다. Rapier adapter는 현재 단일 스레드이며 Jolt world의 worker는 별도 제한된다.

E06 전체의 액션·캐릭터·장치 수명·품질 게이트는 계속 열린 상태다. 별도 backend world에서 성공한 fixture를 게임 엔진 전체의 조작감·성능 완료로 표시하지 않는다.
