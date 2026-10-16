# P03.2c imported PCM·공통 세션·실제 오디오 출력 계약

상태: source/API 리뷰 반영·구현 계약 확정·인수 미통과 · 2026-10-16 · P03.2c/DEV01/DEV03/DEV05/DEV08

## 목적과 선행 조건

P03.2a/b는 각각 PR11/PR12의 로컬·실제 GPU/native·clean package·CI/main 동일 tree까지 인수했다. 이번 계약은 같은 PreparedProject의 imported WAV를 실제 Kira mixer와 native 출력에서 소비하게 한다. 별도 WAV player나 다른 권한 actor를 만들지 않는다. 전체 P03.2·D1/D2/D3는 계속 미완료다.

첫 범위는 immutable mono/stereo static clip bank, 명시적 play/stop/volume/pan/full-clip loop, 같은 SessionControlService의 조회·재시도·종료, 장치 없는 실제 offline PCM 및 opt-in native 출력이다. source load 자체는 소리를 재생하지 않는다. spatial emitter/listener, streaming/OGG, mixer bus/effect, 음악 동기화, 액션/애니메이션 cue, source reload, C# 일반 audio SDK와 browser playback은 후속 제품 기능이다. 이번 subset을 최종 audio 범위로 고정하지 않는다.

## 확인한 source와 설계 선택

AudioAsset은 sample rate/channels/frame count와 Arc-backed finite f32 PCM을 가진다. importer는 mono/stereo, 8~384 kHz, integer8/16/24/32·float32 WAV를 지원하고 finite amplitude>1도 보존한다. 현재 PcmClip은 Vec<f32>를 받은 뒤 Kira Frame Vec→Arc<[Frame]>로 변환한다. `samples().to_vec()`를 거치면 불필요한 전체 sample 복사가 추가된다. voice의 StaticSoundData는 그 Frame Arc를 공유한다.

AudioService는 이미 Kira의 native/offline backend, 유한 voice arena, play/stop/set_volume/set_pan/reap을 구현한다. stop은 요청이며 실제 mixer가 Stopped를 관찰하고 reaping한 것과 다르다. 기본128 voices/128 internal frames, hard4096 voices/8192 internal frames, clip당10M frames다. 기존 offline helper는 호출자가 명시적으로 진행하며 현재 Engine Step과 연결되지 않는다.

아래 API·예산·wire 확장은 위 사실을 다이달로스에 적용하는 설계 선택이다. 공개 설정/보고 struct literal과 기존 start/play/stop/count signature를 보존한다. 새 runtime enum variant는 `cfg(feature="audio")`로 제한한다. 기본 feature의 enum shape는 보존하되 opt-in audio는 실험적0.1 API 확장이므로 audio-enabled downstream의 exhaustive match까지 source-compatible하다고 주장하지 않는다.

## 모듈·feature와 startup

| owner | 책임 |
| --- | --- |
| daedalus-content | 기존 WAV decoder·immutable AudioAsset·LeafCookKey/catalog. 새 decoder나 audio device 의존을 추가하지 않음 |
| daedalus-audio | borrowed PCM→Kira Frame 변환, 실제 mixer/backend·opaque voice 수명. content/runtime에 역의존하지 않음 |
| daedalus-runtime/project_audio | PreparedProject→bounded bank, backend/voice-ID sidecar, typed audio operation/result. renderer에 의존하지 않음 |
| 공통 SessionControlService | 같은 project actor·queue·ticket/credit·capability·command replay로 sidecar를 조작 |
| CLI protocol2 | 같은 typed API의 adapter. headless serve와 native view가 transport/DTO/ticket book을 공유 |

audio crate는 `default=[]`, `native=["kira/cpal"]`로 장치 계층을 opt-in한다. 기존 `probe_default_output_device`는 native가 없으면 명시적 backend unavailable을 반환하며 mock device를 성공으로 반환하지 않는다. native 전용 alias는 `#[cfg(feature="native")] NativeAudioService = AudioService<CpalBackend>`이며 constructor도 이 구체 backend에 둔다. Kira의 feature-disabled DefaultBackend가 MockBackend로 바뀌는 것을 native 성공으로 오인하지 않는다. 기존 native test의 feature 조건을 명시한다.

runtime은 optional `audio`와 그 위의 `native-audio` feature를 둔다. CLI도 `audio`/`native-audio`를 연결하고 기존 window/capture feature는 그대로 둔다. no-feature CLI·서버·기존 service start는 audio-disabled이며 Frame bank 변환·장치 초기화가 없다. offline은 CPAL 없이 실행해야 한다. 이 분리나 Wasm compile을 browser audio 지원으로 보고하지 않는다. native backend는 이번 단계에서 desktop만 인수한다.

기존 SessionControlConfig/SessionCapabilities/SessionShutdownReport에 필수 field를 추가하지 않는다. additive `SessionControlService::start_shared_with_audio(Arc<PreparedProject>, SessionControlConfig, SessionAudioOptions)`를 제공한다. 기존 start/start_shared는 같은 내부 startup을 audio-disabled로 사용한다. SessionAudioOptions는 private field와 `offline(sample_rate)`, `native()` 생성 및 voice/bank-limit builder를 제공한다. 새 값은 startup 때 검증한다. 선택한 backend의 실패는 typed startup failure이며 다른 backend로 조용히 바꾸지 않는다.

bank는 immutable loaded generation을 기준으로 startup 때 한 번 준비한다. 실제 AudioManager/backend는 control actor 내부에서 생성·사용·종료하며 foreign device handle에 임의 Send를 붙이지 않는다. startup/device 초기화의 실제 비용과 실패를 기록한다. 실행 중 async 준비/세대 교체와 최초 로딩 UI 통합은 P03.3이며 이번 startup 경로만으로 무조건1ms 이내 초기화를 보장하지 않는다.

## 공유 clip bank와 메모리 admission

ProjectAudioBank는 PreparedProject catalog의 Audio entry를 borrowed 상태로 열거한다. 서로 다른 StableId라도 같은 LeafCookKey이면 Kira Frame backing 하나를 가리킨다. aliases는 고정 index로 unique clip table을 조회한다. load/play마다 PCM을 다시 변환하거나 hash하지 않는다. bank 준비 자체는 재생하지 않는다. legacy PreparedProject에 catalog가 없으면 clip0개의 명시적 empty bank로 시작한다. 조회도 loaded clips0을 보고하고 임의 clip을 만들어내지 않는다.

| 자원 | 첫 기본값/상한 | 근거·포함 범위 |
| --- | --- | --- |
| alias/unique clip 수 | 각각256 이하 | 기존 import max_assets와 동일. alias는 별도 PCM 복사를 만들지 않음 |
| Kira Frame resident payload | 256 MiB | 기존 cooked128 MiB가 모두 mono4-byte PCM이면 stereo Frame8-byte 변환은 최대256 MiB. source catalog와 별도 |
| conversion scratch | 128 MiB | Vec<Frame>→Arc<[Frame]> 최종화 시 추가 backing peak를 별도 예약. clip10M frames의 Frame80M bytes 포함 |
| runtime bank/voice-index metadata | 4 MiB | alias ID/table, unique clip wrapper/Arc header, runtime active voice index/requested controls·owner 여유. low-level voice arena와 Kira mixer/command buffer는 AudioConfig로 별도 제한하며 payload/driver/RSS를 이 값으로 포함·계측했다고 주장하지 않음 |
| voices/mixer block | AudioConfig 기존 계약 | 기본128/128, hard4096/8192. 낮춘 값은 허용하되 상한 초과/0 거부 |
| actor offline render | 호출당1..4096 frames | 기존 standalone helper의1,048,576 상한과 구분. 유한 PCM scratch32 KiB와 요약, 큰 JSON PCM 배열 없음 |

새 AudioBankLimits는 private field/default와 한도를 낮추는 builder를 제공한다. 최종 table/ID clone/Frame 변환 전에 unique key/count/frame bytes·owned metadata를 checked arithmetic으로 사전 계산한다. 합산 Frame payload와 변환 peak를 구분하고 alias를 중복 charge하지 않는다. allocator/driver 실제 RSS 측정으로 표현하지 않는다. budget error는 resource·required bytes·limit를 구분하며 부분 bank를 active로 게시하지 않는다.

PcmClip에 `from_interleaved_slice(sample_rate, channels, &[f32])`를 추가하고 기존 owned 입력 API도 공통 validation/conversion을 사용한다. mono는 두 채널로 복제하고 stereo 순서는 보존한다. source의 finite amplitude를 임의 정규화하지 않는다. importer는 모든 finite float PCM을 계속 보존하되 재생용 PcmClip은 sample의 절댓값이64 이하인 입력만 허용한다. 공개 `MAX_PLAYBACK_SAMPLE_MAGNITUDE=64.0`은 이번 static mixer의 방어적 admission 정책이며 WAV 형식 자체의 한계가 아니다. 초과/nonfinite 값은 allocation·재생 전에 typed input 오류로 거부한다. 일반적인1 초과 headroom은 보존하고 자동 normalization은 하지 않는다. 기존 owned constructor도 같은 검증을 하므로 extreme finite 입력의 수락 범위가 좁아지는0.1 동작 변경을 명시한다. finite f32만 허용하면 안전하다는 가정은 폐기한다. Kira 최종 clamp는 NaN을 제거하지 않으며 mix/interpolation 전에 범위를 제한해야 한다. 실제 ±64·4096 voice 경계의 finite mixer 출력과 초과 입력 거부를 검사한다. 공통 `play_with_looping(clip, volume, pan, looping)`를 추가하고 기존 play는 looping=false adapter다. loop는 전체 clip이며 custom region/seek를 구현했다고 표시하지 않는다.

bank는 actor 실행 동안 Frame backing을 소유한다. callback에서 decode/I/O/큰 변환 allocation을 하지 않고 실행 중 voice 종료가 bank의 마지막 PCM owner를 해제하지 않게 한다. actor 종료 때 backend/mixer owner를 먼저 drop한 뒤 bank를 해제한다. 단, pinned Kira의 desktop CPAL backend Drop은 stop atomic만 설정하고 detached stream-manager thread가 이후 정리하므로 actor join을 callback/장치 thread의 동기 join이나 즉시 PCM 해제로 보고하지 않는다. Kira가 보유한 Frame Arc는 eventual cleanup까지 남을 수 있다. 종료 순간 마지막 PCM 해제가 어느 thread에서 일어나는지와 teardown 최대 지연은 이번 backend로 보장하지 않는다. ticket/replay/query 결과에는 PCM/bank owner를 노출하지 않는다.

low-level AudioConfig validator·기존 limit·`PCM_FRAME_BYTES`를 공개해 runtime이 검증과 Frame layout을 다시 정의하지 않게 한다.

추가 source 리뷰에서 기존 reap이 호출마다 stopped-index Vec를 새로 할당하고 offline process 뒤에도 allocation 오류를 낼 수 있음을 확인했다. 공통 voice arena는 free-slot 저장 공간을 사전 확보한 뒤 stopped slot을 직접 정리해 반복 scratch를 없앤다. offline 출력 Vec와 reap의 필요한 용량은 mixer clock을 진행하기 전에 확보한다. PCM을 이미 진행한 뒤 발생한 allocation 실패를 clock 미진행으로 보고하지 않는다. bank 준비·native 출력에 같은 arena를 사용하고 별도 reaper를 만들지 않는다.

## 공통 operation·voice ID와 결과

runtime이 feature-gated `SessionOperation::Audio(SessionAudioOperation)`와 `SessionRequestResult::Audio(SessionAudioResult)`를 제공한다. 새 공개 audio 값/보고는 가능한 private field+accessor로 확장하며 기존 struct literal은 보존한다. bank/operation/result는 GPU/DCC/JSONL과 무관한 typed Rust 값이다.

| SessionAudioOperation | capability·의미 |
| --- | --- |
| Status | inspect. backend, bank conversion/byte counters, tracked voice 수, offline progressed frames |
| Clips {offset,limit} | inspect. StableId/key/rate/channels/frame count의 bounded page |
| Voice {voice} | inspect. current-session voice의 Playing/StopRequested/Ended 상태 |
| Voices {offset,limit} | inspect. 현재 tracked voice의 bounded page. history 전체를 누적하지 않음 |
| Command {command_id: Option<CommandId>, mutation} | mutate. 아래 실제 side effect와 compact terminal 결과 |

mutation은 Play {clip,volume,pan,looping}, Stop {voice}, SetVolume {voice,volume}, SetPan {voice,pan}, RenderOffline {frames}다. volume은 finite[0,1], pan은 finite[-1,1]이며 clip ID는 실제 audio bank에서 kind/존재를 검증한다. 범위·예산·voice capacity·ID overflow·backend mode를 효과 발생 전에 검사한다. native에서 RenderOffline은 typed unsupported이며 background에서 임의 native clock을 진행시키지 않는다.

SessionAudioVoiceId는 opaque engine-session + 단조 증가/non-reused u64 sequence다. session/sequence accessor와 명시적 parts 생성 경로를 제공하고 consumer가 current session·issued range를 검사한다. wire는 두 decimal string으로 표현한다. process-local SessionId이므로 영속 저장·다른 프로세스 재접속 토큰으로 쓰지 않는다. sequence0/미발급/다른 session은 오류다. voice ID는 성공한 Play마다 한 번만 발급한다.

active map은 voice cap 이내이고 internal VoiceHandle을 소유한다. low-level AudioService에 immutable `contains_voice(handle)`를 추가해 기존 reap 뒤 어떤 handle이 실제로 남았는지 조회한다. 기존 count/reap/opaque handle API는 보존한다. actor는 처리 loop 및 audio 조회/명령 전에 reap/reconcile하며 제거된 정상 발급 ID는 Ended로 조회한다. 완료 history map을 무한 보관하지 않는다. 이는 reason/time을 보관한 playback history가 아니다.

terminal Play는 Kira가 voice를 수락했다는 뜻이며 실제 speaker audibility나 자연 종료를 뜻하지 않는다. terminal Stop은 요청이며 StopRequested/Ended를 구분한다. voice 조회의 volume/pan은 requested controls로 표시하며 callback에서 이미 적용된 DSP 값으로 표현하지 않는다. 새 Stop/SetVolume/SetPan이 이미 Ended voice를 가리키면 typed voice-ended 오류다. 같은 command ID의 Stop 재시도는 아래 replay로 원래 결과를 반환한다. voice cap은 pending stop/unreaped sound도 고려한다. Ended는 실제 Stopped/reaped 관찰로 map에서 제거됐음을 뜻하며 요청 직후 임의로 성공 종료시키지 않는다.

queries는 기존 max_page_entities/1 MiB ticket·wire 예산을 따른다. invalid input은 typed audio error이며 큰 error 문자열을 무제한 반환하지 않는다. Audio command 결과는 actual command ID·observed engine tick·compact outcome을 담는다. world의 future apply_tick/expected_revision을 audio에 거짓으로 붙이지 않는다. 같은 actor에서 처리하지만 audio clock과 engine tick은 별도다.

audio의 즉시 command/replay는 Queued→Terminal로 완료하고 world 전용 AuthorityAdmission은 None으로 둔다. world outcome의 replay는 기존 world admission을 보존한다. WaitStage::AuthorityAccepted는 기존 구현대로 terminal 도달도 충족하므로 가짜 WorldCommandSubmission이나 별도 authority field가 필요 없다. 이 대기를 audio terminal로 충족하는 회귀 검사를 둔다.

## 재시도와 기존 replay의 재사용

protocol request_id는 ticket 보유 중의 correlation일 뿐이다. ack/forget/detach 뒤 중복 Play를 막는 근거로 사용하지 않는다. caller가 명시한 CommandId는 기존 service의 session-wide lower-half namespace/high-water/FIFO replay를 **world와 audio가 함께** 사용한다. caller는 모든 command kind/attachment에 걸친 단조 증가 issuer를 사용해야 한다. 독립 무작위 ID나 attachment-local 증가값이 session 전체 high-water와 호환된다고 주장하지 않는다.

기존 replay entry를 world 또는 compact audio terminal outcome을 담도록 일반화한다. capacity는 기존 default1024/hard16384이고 audio용 무한 cache·다른 actor/WorldCommand를 만들지 않는다. retained ID 재시도는 원래 terminal 결과를 반환하고 Kira/오프라인 mixer를 다시 호출하지 않는다. evicted ID가 high-water 이하이면 기존 ReplayWindowExpired를 반환한다. queue/permission/구문 admission 거부는 ID를 소비하지 않는다. actor에서 새 audio command ID를 수락하면 execute의 typed 실패도 compact terminal outcome으로 보관하고 high-water를 진행한다. 효과 전에 replay/map 저장 공간을 확보하고 효과 뒤 결과를 cache한 다음 ticket에 게시한다. backend error 문자열은 유한 길이로 제한한다.

동일 CommandId는 하나의 semantic command다. 기존 SetTransform처럼 이미 알려진 ID에 다른 payload를 보내도 새 payload를 실행하지 않고 처음 결과를 따른다. cross-kind reuse도 처음 typed outcome을 반환하며 호출자는 command ID를 새 작업에 재사용하지 않는다. pending world ID와 중복이면 기존 pending ticket 결과에 연결한다. 새 payload fingerprint 정책을 audio에만 따로 추가해 기존 transform semantics와 어긋나게 하지 않는다.

command_id=None이면 기존 server-generated namespace를 사용한다. 이것을 응답 유실/attachment 교체 이후의 caller replay 보장으로 광고하지 않는다. AI/네트워크 adapter가 그 보장을 원하면 명시적 caller ID를 유지해야 한다. generated high-half ID는 기존 caller 제출 제한을 유지한다.

RenderOffline도 mixer clock을 진행하는 mutation이므로 동일 replay 규칙을 적용한다. 반환하는 OfflineAudioSummary는 실제 mixed PCM의 frames/rate, 좌·우 채널별 peak/RMS, 전체 peak/RMS/nonzero count와 누적 audio frame clock이다. 채널 통계로 실제 pan 결과를 공개 경로에서도 검사한다. 통계는 f64 누적 등 overflow/finite를 고려해 계산하고 nonfinite 출력은 명시적 오류다. 긴 PCM 배열을 ticket/JSON에 복사하지 않는다. Engine Manual Step은 audio clock을 암묵적으로 진행시키지 않는다.

기존 정상/Step credit·client 보유 result 수명·forget/ACK·Close 우선권을 그대로 적용한다. audio 요청은 normal lane이며 별도의 무한 lane이나 Step reserve를 쓰지 않는다. accepted Play는 ticket을 잊어도 취소되지 않는다. actor panic 중 효과 후 원래 결과가 확정되지 않으면 기존 OutcomeUnknown 의미를 사용한다. 종료는 queued not-applied와 이미 효과가 발생한 작업을 구분하고 공통 join을 사용한다.

## CLI·protocol2·패키징

`project serve --protocol 2 --clock manual --audio offline`를 renderer/GPU 없이 제공한다. 기존 `project serve`의 protocol1 기본값·동작은 보존하고 protocol1에 audio 옵션을 조용히 무시하지 않는다. `project view --stdio-control --audio native`도 같은 options와 typed service를 사용한다. native 선택에는 CLI native-audio feature가 필요하며 offline은 audio feature만 필요하다.

기존 project_view_control의 transport/DTO/ticket book을 renderer-neutral 공통 module로 이동·재사용한다. Close는 renderer-neutral intent/비차단 callback으로 owner에 전달하고 native owner만 기존 bounded PacketWindowMessage::Close로 변환한다. headless owner는 service stop/join을 수행한다. transport worker는 계속 raw bytes만 소유한다. native EOF는 attachment만 분리하고 창을 유지하며, headless serve EOF/detach는 actor를 종료한다. unread/blocked stdout·열린 stdin에서도 유한 owner 종료를 검사한다. Close receipt는 기존처럼 best effort이며 실제 shutdown/exit와 구분한다.

protocol2에는 audio_status/audio_clips/audio_voice/audio_voices/audio_command를 추가한다. audio_command의 mutation과 decimal command/voice ID는 typed 공통 연산으로 변환한다. 지원 operation 목록은 실제 feature/capability를 반영하고 기존 key·integer string encoding·64 KiB input/1 MiB output·7 normal+1 Step ticket 상한을 유지한다. audio-disabled/missing-feature 상태는 명시적 실패다. 새 wrapper가 entity/transform 결과의 serializer를 복제하지 않게 한다.

기존 smoke helper에 CPU opt-in `--audio`와 hardware opt-in `--audio-native`를 추가한다. 기존 `--assets/--view/--models/--sprites` 의미와 default CPU18은 보존한다. offline 생성 WAV·project·bounded JSONL/ticket·cleanup helper를 재사용한다. native는 actual CPAL stream을 선택하고 finite imported voice의 eventual completion을 관찰한다. probe-only 성공으로 playback을 대체하지 않는다.

로컬/PR CLI package는 기존 backend에서 window+native-audio feature를 빌드하도록 확장하고 manifest/notices/inventory를 갱신한다. Linux build에 ALSA 개발 package를 명시하고 추출 CPU/audio-offline 실행의 runtime library 가용성도 확인한다. native device를 CI 기본값으로 강제하지 않는다. 공개 tag/Release는 여전히 만들지 않는다. browser export/runtime 배포나 모든 Linux 배포판 호환성을 이 package로 인증하지 않는다.

## 검증과 하위 gate

CPU 검사는 실제 imported integer/float mono/stereo WAV→bank→Kira offline PCM을 사용한다. aliases의 같은 clip pointer/1회 conversion, frame/rate/channel 변환, 예산 사전 거부/overflow, loop/volume/pan/stop/reap/voice capacity, wrong-session/future/ended ID, bounded page를 확인한다. 새 synthetic sine만으로 importer 소비를 통과시키지 않는다. importer fixtures와 원본 [Kenney 파생 WAV provenance](../02-research/asset-validation-provenance.md)를 재사용한다.

같은 actor/복수 client에서 Play·RenderOffline의 중복이 한 번만 실행되고 ACK/detach 후에도 retained caller ID replay가 유지되며 evicted ID가 재실행되지 않아야 한다. world/audio namespace 충돌·pending world duplicate·실패 replay·capability·포화/Step·중도 종료/panic/Unknown을 확인한다. 기존 transform replay 및 native EOF/blocked stdout/Close 회귀는 유지한다.

native 검증은 실제 장치 초기화, imported finite voice 수락, backend가 진행한 종료/reap, 정상 actor join을 유한 deadline으로 관찰한다. 장치 probe 정보는 probe 당시 default config로 표시하며 실제 열린 stream sample rate로 단정하지 않는다. 장치 정보와 clock/clip/source/binary identity를 기록한다. 현재 Kira API에는 loopback capture/실제 speaker 음압 계측이 없어 사람이 들은 소리를 자동 검증했다고 보고하지 않는다. no-device/stream 실패는 요청한 hardware 검사에서 explicit failure이며 offline으로 바꾸지 않는다. detached CPAL backend thread의 동기 종료도 이 actor join으로 인증하지 않는다.

source 고정→focused CPU→workspace/strict Clippy/Python→실제 imported native 출력·기존 GPU/native 회귀→clean local archive/extracted offline+native smoke→PR 다섯 CI/packaged asset·offline audio step→main 동일 tree를 확인한 뒤 P03.2c를 체크한다. c가 통과해도 P03.3/03.4와 전체 엔진 제품 gate는 남는다.

## 조사 한계와 분담

2026-10-16 local pinned Kira0.12.5의 StaticSoundData Arc sharing, StaticSoundSettings.loop_region, StaticSoundHandle.state, backend Renderer의 final clamp를 읽었다. local VCS metadata의 commit `74b3ef0fd8bd15f90e3b469208ee741f04374a62`와 [upstream StaticSoundData 원문](https://raw.githubusercontent.com/tesselode/kira/74b3ef0fd8bd15f90e3b469208ee741f04374a62/crates/kira/src/sound/static_sound/data.rs)도 대조했다. docs.rs의 개별 API 세 페이지와 다른 raw 페이지 일부는 접근 실패했으므로 성공 조회로 쓰지 않는다. upstream API 기능은 엔진 통합 통과 근거가 아니다. native backend Send·장치 생성/종료·정확한 Frame conversion peak는 실제 source/검사로 대조한다.

Luna xhigh audio 담당은 audio/service·native feature·low-level 검사, runtime-bank 담당은 새 project_audio bank/sidecar/types, control/CLI 담당은 기존 actor replay/operation·공통 protocol/CLI/smoke/packaging을 맡는다. 파일 소유권은 disjoint이고 public signature/manifest를 먼저 고정한다. audio 담당 한 명이 Cargo.lock/shared target을 먼저 소유해 의존 edge를 정리하고 low-level focused 검사를 수행한다. 나머지는 source-only로 진행한다. 통합 source 고정 뒤 control/CLI 담당에게 Cargo를 인계해 runtime/CLI focused를 하고 root가 broad/hardware/package를 검증한다. 새 source/API 모순은 code 전에 이 MD를 수정한다.
