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

이후 수정·최종 suite·CLI 실제 consumer 결과는 실행 후 기록한다. 위 실패를 해결 전 통과 기록으로 바꾸거나 P03.2c 인수로 올리지 않는다.

## 조사 한계

버전 고정 docs.rs API 세 페이지 조회는 실패했다. local registry의 Kira0.12.5 source를 읽어 Frame Arc sharing/loop/state/final clamp를 확인했다. 이후 local VCS metadata의 commit `74b3ef0fd8bd15f90e3b469208ee741f04374a62`로 upstream StaticSoundData raw 원문을 확보·대조했으며 다른 raw 페이지 일부는 실패했다. browser audio·loopback/audibility·다른 장치/플랫폼은 현재 미검증이다.
