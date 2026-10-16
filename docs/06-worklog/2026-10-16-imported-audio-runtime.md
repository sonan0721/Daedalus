# P03.2c imported audio 작업 기록

상태: source/API 리뷰 반영·구현 계약 확정·인수 미통과 · 2026-10-16

## 이 문서를 만든 이유

[상세 계약](../03-architecture/imported-audio-runtime.md)은 CPU WAV import 이후 실제 mixer/장치·공통 세션·재시도·유한 PCM 수명을 구현하기 전에 책임과 검증 경계를 고정한다. 이 작업 기록은 설계 제안·source 확인과 실제 소리/PCM/배포 증거를 구분한다. 기존 AudioService의 synthetic PCM이나 장치 probe만으로 imported consumer를 완료 처리하지 않기 위해 만들었다.

선행 P03.2b는 [PR12](https://github.com/sonan0721/Daedalus/pull/12)의 다섯 CI/두 packaged-assets step·main `dd2de2ed37a244aa77be342c62b8afafe958d697` 동일 tree를 확인했다. 현재 branch는 `feat/asset-audio-consumer`이며 P03.2/D1/D2/D3는 계속 열려 있다.

## source 감사와 선택

- imported AudioAsset의 borrowed Arc PCM→Kira Frame bank를 unique LeafCookKey당 한 번 변환한다. 별도 sample Vec 복사를 줄이되 Frame Vec→Arc finalization peak는 예산에 포함한다.
- project actor의 기존 capability/ticket/normal credit·caller high-water/FIFO replay를 재사용한다. protocol request ID는 ACK/detach 이후 side-effect replay key가 아니다. Play와 offline clock 진행의 중복 실행·eviction을 실제 시험해야 한다.
- 장치 없는 기본/오프라인과 명시적 native feature/startup을 나눈다. native 실패는 가짜/mock/offline 성공으로 바꾸지 않는다. same actor의 headless protocol2와 native protocol2가 같은 codec/운영 의미를 사용해야 한다.
- voice는 현재 engine session에서만 유효한 non-reused sequence다. stop 요청·실제 ended 관찰·reaping을 구분한다. 실행 중 bank를 유지하고 종료 때 backend owner를 먼저 drop하되 CPAL의 detached stream-manager 최종 종료/PCM 해제를 actor join으로 인증하지 않는다.
- Luna 세 담당의 read-only source/API 리뷰를 반영했다. native alias는 실제 CpalBackend에만 연결한다. 최종 clamp가 NaN을 제거하지 않아 playback sample ±64 admission을 결정했으며 importer의 원본 finite PCM은 보존한다. 기본 API shape/기존 public struct literal을 유지하고 audio enum 확장은 opt-in 실험적0.1 변경으로 기록한다.
- world 전용 authority 구조는 그대로 둔다. audio는 typed terminal로 완료하고 기존 AuthorityAccepted 대기가 terminal도 허용하는 동작을 재사용한다. 음향 actor·별도 replay cache·별도 JSONL protocol을 늘리지 않는다.
- 계약 commit `b1b50e8` 이후 Luna xhigh 세 담당이 파일 소유권을 나눠 구현 중이다. runtime/CLI manifest를 먼저 고정하고 audio 담당만 Cargo.lock/shared target을 사용한다. 실제 테스트/시간/장치/패키지 결과는 실행 뒤 추가한다.

## 공개 입력 준비 — 아직 mixer 검증 아님

기존 [Kenney Interface Sounds의 CC0·변환 기록](../02-research/asset-validation-provenance.md)을 재사용했다. ignored `dist/qa-assets/audio-runtime-project`에 같은 `audio/back.wav`를 참조하는 두 StableId를 준비했다. 엔진·게임 repository에 오디오 원본이나 새 게임을 추가하지 않았다.

파생 WAV SHA-256은 `090a56ddb09b9b94dbebd26790881d39ff7438eb82d8591bc56691f51c97ea04`다. Python wave/PCM16 산술로 독립 확인한 입력은 mono44,100Hz·2,689frames·16bit이고 peak0.89581298828125/RMS0.08662039814080079/nonzero samples2,670이다. 이 값은 원본 PCM의 통계이며 Kira의 pan/resampling/startup/clamp 이후 출력과 같다고 단정하지 않는다. 기록은 ignored `dist/project-evidence/p03-public-audio-source.json`에 둔다. 이후 실제 CLI의 alias conversion·offline clock·native voice 종료를 별도로 검증한다.

## 저수준 집중 검사 — 전체 P03.2c 인수 아님

Luna의 audio 변경을 source 리뷰하고 조정자가 같은 shared target에서 다음을 독립 실행했다.

- `cargo test -p daedalus-audio --no-default-features --locked`: 5pass/0fail, mixer tests0.82초. no-native probe의 explicit unavailable 포함.
- `cargo test -p daedalus-audio --features native --locked`: 5pass/0fail/1hardware ignored, mixer tests1.03초. 별도 native playback 시험은 아직 아니다.
- 두 로그는 ignored `dist/project-evidence/p03-audio-lowlevel-off.log`와 `p03-audio-lowlevel-native.log`다. scoped diff check도 통과했다. compile 시간과 전체 wall time을 위 mixer test 시간과 섞지 않는다.

경계 검사는 ±64 headroom 보존, just-over64/±f32::MAX/NaN/Infinity 사전 거부, full-clip loop와 자연 종료·stop/reap, stale-service handle을 포함한다. 최초 test 초안의4096 voice/1frame은 startup silence만 볼 위험이 있어 리뷰에서 거부하고128frame의 실제 same-sign saturation 및 반대 부호 hard-pan 출력을 검사하도록 보강했다. 이후 test가 통과했으며 초안은 실패한 엔진 실행으로 기록하지 않는다.

Luna의 strict all-target Clippy는 feature-off/native 각각 통과했다. 첫 feature-off Clippy의 needless return 지적을 수정·재실행했다. 조정자의 broad workspace/CLI·public WAV·실제 장치·clean package 검증은 아직 대기다. 이 저수준 시험은 준비된 clip API만 확인하며 imported project consumer나 P07 전체를 통과시키지 않는다.

후속 source 리뷰는 reap마다 stopped-index Vec를 만들고 offline process 뒤에 fallible allocation이 남는 문제를 확인했다. 계약 `a77d9ee` 후 Luna가 공통 arena의 사전 capacity 확보·in-place reap으로 수정했다. 출력과 cleanup 저장 공간을 mixer 진행 전에 확보하고 이후 정리는 infallible 경로로 실행한다. 128회 repeated reap의 free-slot capacity 유지·stale/recycled generation 회귀를 추가했다. Luna feature-off6pass/native6pass+1ignored 및 두 strict Clippy가 통과했고, 조정자도 `cargo test -p daedalus-audio --all-features --locked` 6pass/0fail/1ignored를 확인했다. ignored 로그는 `dist/project-evidence/p03-audio-lowlevel-reaper.log`다. allocator 전체 profile이나 개발 시간 개선 배율을 측정한 것은 아니다.

## 통합 첫 검사와 실패 — 진행 중

Luna의 feature-off runtime 검사는65/65, CLI 검사는29/29로 통과했다. 첫 audio-feature runtime은74pass/2fail이었다. 보존한 `dist/project-evidence/p03-audio-focused-initial-runtime.log`는 명령·실패 assertion·원인 조사 내용을 담은 **실패 요약**이며 raw Cargo 전체 stdout 로그가 아니다.

- imported WAV volume 변경 시험은 첫 변경 block의 peak를 곧바로 steady-state gain으로 비교해 실패했다. pinned Kira `src/parameter.rs`는 이전/현재 값을 보유하고 `src/sound/static_sound/sound.rs`는 chunk 안에서 gain을 보간한다. zero-duration Tween도 이 chunk 보간을 없애지 않는다. 별도 settling render와 그 실제 clock 진행을 기록하고 정상 구간의 gain을 비교하도록 조사 중이며, 허용 오차를 넓혀 성공 처리하지 않는다.
- 새 normal-lane credit 회귀는 ticket ACK 후에도 terminal progress Arc를 보유하면서 credit 회수를 기대해 ResultCapacity를 만났다. 기존 owned-result 계약대로 마지막 reader가 credit을 보유하는 동작이다. 시험에서 progress owner를 해제한 뒤 회복 요청을 하도록 수정했다. actor의 한도나 실패 응답을 완화하지 않았다.

volume 시험은 실제256frame settling render를 Play/SetVolume 뒤 각각 진행하고 이후 구간을 비교하도록 수정했다. 총256/512/768/1024frame의 clock을 대조하고 원래 gain-ratio 허용 오차0.02는 유지했다. engine의 immediate Tween은 duration0이다. Tween::default의10ms를 원인으로 추정한 발언은 low-level override 확인 후 철회했다. 이 수정은 mixer가 하지 않은 render를 가짜로 진행시키거나 assertion을 완화한 것이 아니다.

작업 인계 중 종료된 담당자에게 단순 메시지만 보내 후속 수정이 실행되지 않은 대기도 확인했다. coordinator가 상태 조회 후 명시적인 follow-up task로 재활성화했다. 별도 작업 배정과 단순 메시지 전송을 구분하는 규칙을 [개발 반복 파이프라인](../04-plan/development-iteration-pipeline.md)에 반영했다.

## 엔진·공통 프로토콜 Rust checkpoint

수정·source freeze 후 조정자가 같은 shared target에서 독립 실행했다.

| 명령 | 실제 결과 | ignored raw log |
| --- | --- | --- |
| `cargo test -p daedalus-runtime --lib --features audio --locked` | 77pass/0fail, tests0.79초 | `dist/project-evidence/p03-root-runtime-audio.log` |
| `cargo test -p daedalus-runtime --lib --all-features --locked` | 76pass/0fail, tests0.73초 | `dist/project-evidence/p03-root-runtime-native-audio.log` |
| `cargo test -p daedalus-cli --all-features --locked` | 39pass/0fail, tests0.05초 | `dist/project-evidence/p03-root-cli-allfeatures.log` |

native-audio feature가 켜진 CPU 검사도 실제 장치 출력 검사는 아니다. audio-only의 no-native startup 실패 시험1건은 all-features에서 조건 제외되므로77/76 차이가 난다. Luna의 feature-off runtime65/CLI29와 strict runtime default/audio·CLI window/window+audio Clippy도 통과했다. scoped diff/docs check가 통과했다.

검사에는 imported Play의 ACK/클라이언트 detach 후 재접속 replay가 voice를 하나만 유지하는 것, 원래 world/audio typed outcome을 양방향 반환하는 것, pending world의 실제 authority 유지, 실패 결과 replay/eviction, normal-result 보유와 별도 Step reserve, audio 효과 뒤 주입한 panic의 OutcomeUnknown이 포함된다. 음향 구현과 별도 권한 actor/replay를 만들지 않았다. 공통 replay lookup·pending 연결·결과 게시 helper와 공통 renderer-neutral protocol2가 두 실행 경로를 처리한다. audio 결과 직렬화는 exhaustive match로 새 variant 누락을 compile 때 검출한다.

CLI window+audio 첫 compile은 args.audio를 borrowed args와 함께 `'static` coordinator로 capture해 실패했다. spawn 전에 Copy flag를 만들도록 고친 뒤39tests/strict Clippy를 재실행했다. native 창을 실행하다 실패한 것으로 기록하지 않는다. 실제 project smoke·public WAV mixer/native·workspace/managed/GPU·clean package·CI/main 인수는 아직 대기다. **P03.2c와 D1/D2/D3는 계속 열려 있다.**

## 첫 workspace 통합 — native 조합의 Clippy 실패

Rust checkpoint `f918c2c` 이후 조정자가 `cargo test --workspace --all-features --locked`를 실행해281pass/0fail/10hardware ignored를 확인했다. `cargo fmt --all --check`도 통과했다. raw 로그는 ignored `dist/project-evidence/p03-audio-workspace-tests.log`다. 이는 장치 출력·실제 GPU 검사와 별개다.

`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`는 RuntimeAudioBackend의 large_enum_variant로 실패했다. 이 Mac compile에서 offline variant2388bytes/native760bytes·enum2400bytes로 보고됐다. 이 수치는 Rust layout의 해당 빌드 관찰이지 플랫폼 ABI 보장이 아니다. audio-only strict 검사에는 native variant가 없어 이 경고가 나오지 않았다. raw 실패 로그는 `dist/project-evidence/p03-audio-workspace-clippy.log`로 보존한다.

계약 `7c359f6`에 따라 두 private backend owner를 startup 한 번만 Box로 두고 선택한 service pointee 크기도 metadata admission에 포함했다. pointer만 세어 owned memory를 숨기지 않으며 Play/query마다 새 Box를 만들지 않는다. Luna의 audio77/native76·strict all-feature runtime 검사가 통과했다.

수정 뒤 조정자가 workspace all-features를 다시 실행해281pass/0fail/10ignored와 strict workspace all-target/all-feature Clippy, fmt를 확인했다. 최종 raw 로그는 `dist/project-evidence/p03-audio-workspace-tests-final.log`와 `p03-audio-workspace-clippy-final.log`다. 첫 Clippy 실패 파일은 덮어쓰지 않았다. 별도 Luna read-only source 리뷰도 backend 선택·bank/voice 수명·pre-effect 준비·clock·CPAL 종료 설명에서 구체적 blocker를 찾지 못했다. 실제 consumer/native·managed/GPU·clean package·CI/main 인수는 계속 대기다. feature별 compile 통과를 해당 조합의 strict Clippy 통과로 대체하지 않는다.

## 조사 한계

버전 고정 docs.rs API 세 페이지 조회는 실패했다. local registry의 Kira0.12.5 source를 읽어 Frame Arc sharing/loop/state/final clamp를 확인했다. 이후 local VCS metadata의 commit `74b3ef0fd8bd15f90e3b469208ee741f04374a62`로 upstream StaticSoundData raw 원문을 확보·대조했으며 다른 raw 페이지 일부는 실패했다. browser audio·loopback/audibility·다른 장치/플랫폼은 현재 미검증이다.
