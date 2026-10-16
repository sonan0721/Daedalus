# P03 native host·staged renderer 설계와 진행 경계

작성일: 2026-10-17 · 대상 source `30d910be0d5b1a64940b93165ba73c927b1da3ee` · A-039/A-041, DEV01/03/04/08

## 목표와 이번 진행의 분류

전체 목표는 D1 실제 AI-driven 엔진→D2 별도 3D 게임→D3 별도 2D 게임이다. 모두 미완료다. 현재 P03.3b.2/PR16의 로컬 native 출력 인수가 보류됐고 b.3 코드에는 착수하지 않았다. 직전 goal turn은 test-only native 진단·source 대조·commit/CI라는 **진행**, 이번 turn은 재사용 host/renderer 계약을 구체화하고 기존 Rust type에 대한 설계 probe를 실행한 **진행**이다. 실패가 유지된다는 이유로 인수 성공이나 전체 완료로 바꾸지 않는다.

## 새 문서와 재사용할 경계

- [Native project host](../03-architecture/native-project-host.md)를 만들었다. CLI 안에 있는 실행·입력·actor·snapshot/packet 조합을 공개 engine library에서 재사용하기 위한 계약이다. CLI는 transport/report adapter로 남기고 runtime→renderer 역의존은 만들지 않는다. source lease/opaque pin·initial/candidate identity·실제 present ACK·큰 CPU backing의 off-UI 회수를 명시한다. 새 crate/facade는 아직 설계안이다.
- [Staged renderer](../03-architecture/staged-render-asset-generation.md)를 source 기반으로 좁혔다. 가짜 publication packet 대신 resource-only 입력을 제안하고 기존 validation과 stage가 같은 작은 resource projection/planner를 사용하도록 한다. stage chunk와 old-ready draw를 한 submit으로 조합해 별도 최소3 submission slots를 요구하지 않는다. incomplete 업로드와 GPU fence 완료를 구분하고, 취소된 incomplete 객체는 마지막 serial 뒤 폐기한다.

이 선택은 반복 연결 코드·후보마다 전체 packet 생성·별도 upload/cache planner·불필요한 제출 장부를 줄이려는 설계다. 아직 새 host/stage 코드나 iteration 시간 개선의 실측이 아니다. 4MiB/생성8회 제안과 frame reserve3,145,872bytes는 각각 미측정 기본안과 source layout 산술이다. CPU frame capacity·실제 GPU 이동/종료 검증은 최종 동결 전 남아 있다.

read-only 리뷰는 initial generation에 old-ready packet이 없다는 scheduler gap을 지적했다. 구체안에 clear/loading pass+initial chunk의 한 submit과 game identity 없는 bootstrap present를 추가했다. 조정자는 현재 packet-window가 initial packet을 요구함을 source로 확인했다. WindowApp의 no-packet legacy Scene 분기를 이미 구현된 packet bootstrap으로 읽은 리뷰 설명은 채택하지 않았다. 초기 CPU load 이전의 전체 loading UI는 여전히 별도 미완료다.

추가 source 감사에서는 geometry_bytes가 GPU stride 기준이고 미참조 mesh 준비도 포함해 legacy CPU copy estimator가 아님을 확인했다. 기존 metadata estimator와 content-owned imported backing은 재사용하고, legacy copy·frame·scratch를 별도 lane으로 제안했다. frame8MiB 초기 제안은 retained validation map까지 검증한 수치가 아니므로 채택하지 않았다. 최종 size_of/capacity 검증 전의 검토 기본안은 frame16MiB/aggregate48MiB·unique3개, 한 CPU owner scratch16MiB다. runtime/renderer GPU bookkeeping을 CPU 전체 예산으로 재사용하지 않는다. 새로운 frame 검증을 worker로 옮길 opaque input은 현재 코드에 없고 API/retained 비용 리뷰가 남아 있다.

## Rust 설계 probe: 기존 type과 Arc 수명

Luna xhigh가 ignored `dist/project-evidence` 안에서만 작은 probe를 작성했다. repository production/test source·manifest·dependency는 변경하지 않았다. 공유 cache에서 `cargo build -p daedalus-renderer --features window --locked --message-format=json`을 한 번 수행했고, 두 번째 probe는 같은 rlib를 직접 rustc --test로 재사용해 Cargo rebuild를 하지 않았다. Rust는1.94.0 (`4a4ef493e 2026-03-02`)이다.

attempt01은3/3 통과했지만 요청된 `ProjectRenderResources: Send` assertion이 빠졌다. 조정자가 source를 보고 지적했고 원본을 보존한 새 attempt02에 assertion을 추가했다. attempt02도 실제3/3·0failed·0ignored·0filtered다.

1. 기존 PacketRenderer/PacketViewSubmission/ProjectRenderResources의 Send, RenderPacket의 Send+Sync compile 확인. device는 생성하지 않는다.
2. auto-trait-only `Arc<dyn Send + Sync>` coercion/clone·opaque Debug wrapper 확인.
3. unsized Arc slice에서 strong alias 또는 Weak가 남으면 get_mut가 실패하고, 모두 해제한 뒤 cleanup worker가 실제 마지막 backing을 drop하는 것을 thread ID로 확인.

| 로컬 근거 | SHA-256 |
| --- | --- |
| `dist/project-evidence/p03-host-design-probe-luna-attempt02.rs` | `50de704c61d607282df4bb3d47b302935dab6b149829e3e60451e58b37c33fcf` |
| `dist/project-evidence/p03-host-design-probe-luna-attempt02-test` | `7cf58347a4740bc754e5803bd501c6f791758693f2442ea08faf76b32288fec5` |
| `dist/project-evidence/p03-host-design-probe-luna-attempt02-rustc.log` (empty) | `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` |
| `dist/project-evidence/p03-host-design-probe-luna-attempt02-test.log` | `2be8130f729eb52747b6f2b9d4a026d4ab30b17a1ca97c60de766e4da09c1573` |
| linked `target/debug/libdaedalus_renderer.rlib` | `cf867e5e5d5e0884281eee37c6ddc6ae11f198be47b19bcd24548e4ab6b68432` |

조정자가 source·실제 test 수·full hashes를 다시 대조했다. 이 실험은 production GPU 자원을 다른 thread에서 drain/drop하거나 새 registry가 모든 실제 alias를 추적한다는 증명이 아니다. Window/Surface를 worker로 옮기는 근거도 아니다. unsized Arc의 언어/library 수명 규칙과 기존 type bound까지만 확인했다.

## 직전 source의 원격 CI 확인

[PR16](https://github.com/sonan0721/Daedalus/pull/16)은 draft다. head `30d910be0d5b1a64940b93165ba73c927b1da3ee`와 synthetic merge `9c5e716b2e6e7fccf7a770639df88f4f17984d4f`의 tree는 `5763529e5a0f94578b57c41c46805177622a784b`로 같다. docs·managed native integration·rust·macOS package·Linux package 다섯 check는 성공했다. Publish는 skipped이고 tag/Release/merge는 하지 않았다.

Rust run38933842814/job117084243182는326passed/0failed/12ignored이며 실제 Stage 결과 손실 회귀도 통과했다. release run38933842523의 macOS job117084242829와 Linux job117084242385는 각각 packaged CPU62/62다. macOS6.695085초·binary SHA-256 `71b6940b12a7a5fac2e803f0654facc8e401c59b633941c0a68d2c12cc20b8eb`, Linux1.005131초·`8abc8112392ff48fe7a147c492444b86c3f0d34e8a9161086588d957550b7350`다. 이는 장치 없는 CPU smoke이며 native 출력 성공을 뜻하지 않는다. test-only/MD 수정으로 두 packaged binary hash는23479b5 checkpoint와 같았다.

위 check들은 이 기록을 추가하기 전 head의 결과다. 이후 docs commit의 check로 전용하지 않는다. raw는 `dist/project-evidence/p03-generation-pr16-nativeaudio-*root-attempt01.*`에 보존한다. branch/main/synthetic commit을 서로 같은 SHA라고 부르지 않는다.

## native 출력의 현재 실패와 안전한 작업 범위

[전날 진단 기록](2026-10-16-project-asset-activation.md)의 Kira position0·직접 CPAL callback0·같은 physical HAL callback0과 macOS afplay 실패가 해결되지 않았다. 이번 turn 시작에도 read-only 실행 상태를 다시 확인했다. `/usr/bin/afplay -v 0 /System/Library/Sounds/Tink.aiff`를 신호 없이 끝까지 관찰했으며 약15.24초 뒤 AudioQueueStart(-66681)·exit1이었다. raw는 `dist/project-evidence/p03-generation-nativeaudio-afplay-revalidation-root-attempt01.log`, SHA-256 `dccacd474df1bb85ee427d625688d2583e229b6c507f11e0837d8ca3dee211e9`다. 이전 파일을 덮어쓰거나 engine timeout을 늘리지 않았다.

현재 공통 출력 경로의 실패라는 관측만 유지하며 OS/driver/daemon의 근본 원인은 미확정이다. 다른 앱의 실제 소리 상태에 대한 사용자 질문은 답변 대기 중이다. 답변이나 환경 변화가 없는 동안 동일 helper/native suite를 반복해서 성공을 기다리지 않는다. CoreAudio 재시작·전역 설정 변경·다른 장치 선택은 하지 않았다.

안전하게 진행할 host/renderer source 조사·설계는 계속했지만 이 작업은 b.2 실패를 면제하지 않는다. 실제 native 인수와 최종 API/CPU 수치 동결 이후에만 b.3 구현을 배정한다. P03·D1·D2·D3는 열린 상태다.

## A-042 이후 오디오 조사 재개

사용자는 다른 앱에서 정상적으로 소리가 나며 조정자의 실행음도 일부 들렸다고 답했다. 정확한 시도/시점은 미확정이다. 직전 goal turn은 실제 원격 CI job을 확인·기다리고 최종 다섯 성공/동일 tree를 PR에 반영한 진행이다. 새 답변으로 조사를 재개하며 이전 blocked 판정 이후의 감사는 새로 센다. 다른 앱 정상/가청 보고를 failed helper의 Ended 통과로 취급하지 않는다.

재검증 전 같은 console user danny/UID501·launchctl Aqua/UID501을 read-only 확인했다. 별도 다른 UID/비GUI session이라고 단정할 근거는 없다. macOS26.6.2/build25G83은 그대로이고 agent가 OS 설정을 변경하지 않았다. 보존 extracted 수정본 CLI SHA-256 `b6ab0afc72d6c82a2eeab8116a9b877f9fcadd24e5fe381e7db5f7cc4f098596`를 먼저 확인했다. 같은 helper native-only 한 번의 새 raw는 기존 attempt와 다른 prefix에 exclusive로 보존한다.

실제 unchanged helper의 `--audio-native` 실행은 **19/19·0.660931초·stderr0**으로 통과했다. 그 안의 finite voice는6polls·0.490285초 뒤 backend-reported Ended였고 session1/voice1을 대조했다. speaker loopback을 측정한 것은 아니다. raw `dist/project-evidence/p03-native-recovery-user-report-root-attempt01.json` SHA-256은 `d73f88fea6ec3ac017a70a08a668a5da0dce1616d88a0edfc3416662e5cd762e`, empty stderr hash는 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`다. binary/backend/10초 deadline/장치/production을 바꾸지 않았다. 과거 실패와 원인 미확정은 보존한다.

source676f93f의 원격 CI도 최종 다섯 성공을 확인했다. Rust run38937185868/job117095527277의326passed/0failed/12ignored·실제 Stage 회귀/Clippy, 두 package의 CPU62/62·macOS4.708728초/Linux0.992508초, managed와 docs 성공, Publish skipped다. actual CI checkout `a70556d52b90b1a869391b0971bb130dd436d481`과676f93f의 tree는 `430e359e609ba206fd20987f516df39aef57ebaf`로 같았다. raw `p03-host-design-pr16-rust-root-attempt01.log` SHA-256은 `a3fcfa21c4042ec943c4ece7a759c8982a71896db6153fed41689d37dff2ab57`이다. main은 아직 변경하지 않았다.

다음은 이 문서 묶음을 commit한 clean source로 새 local archive를 한 번 만들고 실제 extracted binary에 기존 combined72 helper를 실행하는 것이다. 이미 회복한 보존 binary에 전체72를 먼저 반복하지 않는다. 새 head의 다섯 CI·archive identity·실제 장치 통합·source review/main 동일 tree까지 확인한 뒤에만 b.2를 인수한다. 원인 미확정인 transient 출력 실패를 엔진 수정으로 해결했다고 보고하지 않는다.
