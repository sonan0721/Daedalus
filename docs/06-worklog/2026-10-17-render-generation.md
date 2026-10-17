# P03.3b.3 Render generation 구현 기록

작성일: 2026-10-17 · 전체 D1/D2/D3·b.3 미완료

## 선행 b.2 인수

[PR16](https://github.com/sonan0721/Daedalus/pull/16)은 exact head `523757a7ff92e9fc36a8a83a0bba661dec47a44c`의 다섯 성공 뒤 squash main `360ceb6cf13fb3682e2c9cf97ced06a3e37c7b9a`로 병합했다. 실제 CI checkout `8d5bbf1255ac362e61b8b43996e1828762a2fcaa`·head·main의 tree는 `57d87c2ab924e5efd2690313df7330e0eab1f0db`로 같다.

로컬 workspace326/0/ignored13·strict runtime Clippy, 원격 Rust326/0/ignored12·실제 두 회귀/strict Clippy·managed/docs·두 package CPU62가 통과했다. 최종 CI의 macOS6.744616초/Linux1.034006초, binary hashes는 각각 `71b6940b12a7a5fac2e803f0654facc8e401c59b633941c0a68d2c12cc20b8eb` / `d336d4dbe7f3165f1659bc9dfdb3fedbd057eedb20d3d20b5e6f0d8d59fd260f`다. Linux hash를 이전 artifact와 같다고 가정하지 않는다. Publish skipped·tag/Release 없음이다.

실제 장치 combined72는 c9f5dee clean extracted binary SHA-256 `b6ab0afc72d6c82a2eeab8116a9b877f9fcadd24e5fe381e7db5f7cc4f098596`에서 통과했다. 최종523757a clean archive를 별도로 만들어 archive/manifest/CPU smoke를 검증했고 tar 안의 binary·notices·inventory hashes가 그 장치 검증 artifact와 모두 같았다. test-only/MD 외 production/helper 변경이 없으므로 device suite를 중복 실행하지 않았다. 최종 archive hash는 `e75927c373f3ce3caab32bd235acfd38c6c8bf6508f1edb221d54bf6ddeb824c`, build ID는 `20261017T000251Z-94b20ce25a91459381f36d4dab9ab4a9`다. source manifest는 exact523757a/dirty=false다.

raw `p03-replay-terminal-ci-rust-root-attempt01.log` SHA-256은 `7279b267c324edd4a20125613a3b0d2b6165f10a617b796850647a4b4c2f7d0c`, 최종 PR metadata JSON은 `beb2b4377a4eff037342c35f84b5a960022c0003a073fd8155ab54a78f8cec97`이다. raw는 ignored dist/project-evidence에 보존한다. 이전 native 실패·회복의 정확한 범위는 [선행 기록](2026-10-17-native-host-design.md)에 남긴다. 원인 미확정인 환경 회복을 test-only 수정 효과로 부르지 않는다.

병합 후 exact main360ceb6의 docs38984810552·managed38984810514·Rust38984810521·release38984810596도 성공했다. release의 macOS117253678189/Linux117253678395는 성공, Publish117254035947는 skipped다. 이 추가 확인은 실제 main workflow/job 결과이며 새 native 장치 검사를 수행했다는 뜻은 아니다. package jobs 원본 JSON `p03-b2-main-ci-root-attempt01.json`의 SHA-256은 `ffff19b47d395cbd1d04207cbceb27dbc1bd6458e6114dc8c4cec021ca5f3186`이다.

## 새 문서와 첫 작업

[ADR-0007](../05-decisions/0007-reusable-native-host-and-generation-consumer.md)은 CLI 밖의 host·의존·thread/회수 선택을 기록한다. [generation consumer](../03-architecture/project-generation-consumer.md)는 첫 runtime handshake API·Queued/Applied·작은 publication token·ACK 대기 pin을 동결한다. 이 두 문서는 구현 증거가 아니다.

첫 코드 소유자는 Luna xhigh 한 명이며 runtime source만 변경한다. renderer/host/CLI는 이 public API가 local 검사·리뷰를 통과한 뒤 연결한다. 이후 renderer의 immutable requests/CPU validation/실제 leaf registry·bounded device staging, native host·window disposition/CLI adapter를 순서대로 통합한다. 같은 window/device/session의 실제 source edit→ready→Commit→현재 pose/pixel/present→old backing 회수까지 확인한 뒤에만 b.3를 인수한다.

최초 계약 기록 시점에는 b.3 코드/새 API가 없었다. 이후 아래 source 작업을 시작했다. 워터폴 선행 인수가 끝났다는 뜻이며 엔진 제품이나 두 게임이 완성된 것은 아니다.

## CPU renderer 계약 추가

[render-cpu-generation](../03-architecture/render-cpu-generation.md)은 독립 leaf alias·prepared/frame/scratch 장부와 첫 committed frame 예약을 코드 전에 고정한다. source 감사에서 두 sidecar map의 중복 capacity와 maximum instance count/metadata budget의 독립성을 확인했다. 실제 type size/capacity 경계와 회수 시험은 구현 후 검증한다. first CPU API의 prepare는 동기 owner 작업이며 취소 checkpoint를 frame 선점으로 부르지 않는다. 실제 host의 frame 우선/pose 지연 검증은 b.3 미통과 조건으로 남긴다.

[render-device-generation](../03-architecture/render-device-generation.md)은 resource-only input/같은 planner·4MiB/8 actual-create의 combined submit·partial completeness와 upload/use serial을 동결한다. source 감사에서 기존 commit_resources의 전체 생성/upload와 saturating submission serial을 staged transaction으로 그대로 재사용할 수 없음을 확인했다. 실제 GPU 준비/취소/픽셀/drain은 후속 테스트 대상이며 문서/코드 작성으로 통과 처리하지 않는다.

## runtime source 작성과 초기 compile

Luna xhigh가 runtime consumer module과 기존 actor/worker를 연결했다. 코드 작성 중 조정자 리뷰에서 consumer close의 mutex 경합·cancel 뒤 늦은 CPU Ready의 currentness 복구·worker Stage 수락 뒤 consumer Busy·worker Staged와 consumer ReadyApplied의 시간차·mailbox 게시 뒤 ACK watermark 기록 지연을 확인했다. 독립 atomic currentness/close/stop과 worker cleanup backstop, 효과 전 stage admission guard·report apply guard·publication guard로 대응했다. 이 문단은 source 수정 방향이며 충돌 테스트 통과를 뜻하지 않는다.

초기 `cargo check -p daedalus-runtime --all-features --locked` attempt01/02는 import/type/borrow 통합 오류로 실패했고 원본을 보존했다. attempt03은2.13초에 compile을 통과했지만 unused helper 경고가 남았으므로 strict Clippy/테스트 인수는 아직 아니다. raw SHA-256은 순서대로 `bde5bc6db2566eef6975bf6258ac015d2023f5a11ab333b8642c0f16311fa48b`, `3be246fb38a8e0b75251dc2f37f3536a7eb319da59292943858be5eb63e315ec`, `ffd3ec6e9c804020cf3a43d43ac6e1679df5deb1b7ddefb8ddd1f8409f247472`이다. 파일은 ignored `dist/project-evidence/p03-consumer-runtime-check-luna-attemptNN.log`다.

renderer CPU/device는 서로 API를 먼저 확인하고 disjoint source-only 작업을 진행한다. Cargo/CPU lease는 runtime 담당 한 명에게만 있다. 먼저 runtime-only 검증 source를 선택 commit해 clean PR CI에서 기존 renderer와 전체 회귀를 검사할 계획이다. renderer WIP를 runtime 인수 결과에 섞지 않고 이후 해당 source의 CPU/GPU 근거를 별도로 얻는다. host/window/CLI 연결 및 전체 b.3는 계속 미완료다.

첫 집중 검사 세 건은 각각 실제1개가 실행돼 통과했다. `stage_report_worker_mark_and_consumer_ready_are_atomic_under_contention`, `consumer_lock_busy_defers_frame_mailbox_publication_until_identity_can_be_recorded`, `consumer_close_rejects_new_stage_and_commit_without_changing_worker_state`다. 실제 actor/worker와 held consumer mutex를 사용하며 Ready/ACK 보고는 trusted protocol 테스트 입력이다. GPU upload/실제 표시 검증은 아니다. 뒤따른 integration binary의0 matching tests를 추가 실행 수로 세지 않았다.

raw `p03-consumer-runtime-focused-luna-attempt01/02/03.log`의 SHA-256은 순서대로 `f2af80b51fce6e75d65d2a0e2061ab1fc0b796fecb15f41f8a844141395bef14`, `e440c5a290cc40f5a8bffe06e5bea00cf6c11875f87b407d53d84e9a879ab68f`, `e6149d1859c2842117be694ab6fd5c4610172af6bb7e803f6d7d9eca449ebab8`이다. 나머지 source lease/ACK/회수·전체 runtime·strict 검사와 PR 인수는 이 시점에 대기다.

## 실제 publication·두 세대 교체 집중 검사

`consumer_presents_gate_each_commit_and_keep_only_real_source_pins_charged`는 실제1개가0.12초에 통과했다. 초기 lease take-once, 다른 service의 identity 거부, 실제 SessionFrame token과 initial/새 세대 present ACK에 따른 Commit 제한, generation2·3의 교체와 동일 handle/pose/revision/tick 유지, 같은 publication의 새 view, 중복·충돌·오래된 보고, 외부 source pin 유지/해제 후 장부 회수를 검사한다. renderer나 실제 화면이 ACK를 만들었다는 증거는 아니며 runtime protocol 검증이다. raw `p03-consumer-ack-luna-attempt-ruvFTD`의 SHA-256은 `29e639445787efc481e885b51a3f698ec9d19b2d72fe22841067a1584f080258`이다.

이 검사 앞의 실패도 보존했다. `…-Lf0C0K`는 test import/lease 비교 assertion의 compile 오류, `…-JjHu51`은 fixture command ID의 역순으로 생긴 ReplayWindowExpired, `…-El3HvV`는 fixture가 초기·commit 전·generation2의 Frame Arc를 동시에 보유해 기본3개 retention 한도를 채운 상태에서 네 번째 게시를 기다린 실패다. 필요한 작은 identity를 복사한 뒤 지난 Frame Arc를 해제하도록 fixture를 고쳤다. production replay·retention 한도·timeout을 늘리지 않았다. SHA-256은 순서대로 `1ba618f5ef13ea78adced2beb1269acd7fbcbdb3d17a02660b3c79ef66c44211`, `251ef42e1f0b6bb95f43282201127b7e0ddf466eb9dac945b2a67ac554783915`, `28a2f9635cb632d1f737180651df917be2043280d959333d168c0d6cd1fab0c0`이다.

`preparation_supersession_invalidates_taken_candidate_lease_under_contention`도 실제1개가 통과했다. 앞선 fixture는 stale lease를 계속 보유한 채 다음 준비 완료를 기다려 source pin 회수를 막았다. lease를 해제하고 복사한 identity로 늦은 Ready의 Stale 거부를 검증하도록 수정했다. 성공 raw `p03-consumer-runtime-focused-supersession-drop-luna-XXXXXX.log`의 SHA-256은 `1077dfadd386d06a160b1ef4c5c27434821503fe05ce8f651f5f77e9d503938b`이다. 파일명에 남은 literal XXXXXX는 다시 실행해 교체하지 않았다.

집중 검사 attempt07의 빈 파일·08/09의 exit-only 파일은 exclusive log 생성 충돌로 생긴 기록이며 테스트 실행/성공 근거로 세지 않는다. 이후 한 번 연 exclusive file descriptor로 명령 출력과 종료 결과를 함께 보존하도록 실행 절차를 조정했다. retained Port를 남긴 종료 검사의 첫 실행은 candidate를 의도적으로 untaken으로 두면서 Staged를 기다리는 fixture 오류로1실패/109filtered였고 raw redirect가 없어 원본을 잃었다. consumer 모드에서는 Ready 적용 전 Staging인 계약을 따라 WaitingDevice/initial Available 상태를 기다리도록 고친 후 별도 검사한다. 이 잃은 출력을 복원하거나 통과로 추정하지 않는다.

수정 뒤 `activation_stop_clears_untaken_initial_and_candidate_leases_while_port_is_retained`와 `consumer_worker_panic_clears_untaken_initial_lease_before_shutdown_returns`는 각각 실제1개, 합계2개가 통과했다. 첫 검사는 consumer mutex 경합과 retained Port를 남긴 상태에서 untaken initial/candidate cleanup을 확인한다. 둘째는 실제 worker panic 후 untaken initial과 cache owner 해제를 확인한다. raw `p03-consumer-untaken-panic-luna-attempt-K8aD5V`의 SHA-256은 `a72ff5a0e5d9ece3c6981855adccc9ad217a133f5bc6bf3c2d4990c17e74cef5`다. 전체 runtime/strict 검사는 이어 진행한다.

추가 coverage 전의 runtime default는99unit+2integration, all-features는111unit+2integration이 통과했다. strict Clippy 첫 실행은 nested-if와 test의 불필요 borrow 진단으로 실패했고 최소 수정 뒤3.45초에 통과했다. raw default `…default-luna-attempt-5hNugD`/allfeatures `…allfeatures-luna-attempt-J2vvNp`/실패 Clippy `…clippy-luna-attempt-jql7Zt`/성공 Clippy `…clippy-luna-attempt-m0JDu7`의 SHA-256은 순서대로 `a77b44420a8de6a4b646b91717bf622032195c03df99ca4d1056cdf045566e21`, `73c27c8379bd5993ab6e784ef87536a7260b99e572d7233570d683ebf0e24660`, `a627c6863e9d8ed7eaf40d8f839ea64c679651da3acb9c828d7e6d8a0ae356a2`, `2a078ee1b7c7a298af5c728487647db6c02f17016dd2fac00913c6df3b0b9cb8`이다. 이 수치는 아래 새 테스트 추가 전 source의 결과다.

계약 coverage 리뷰 뒤 실제 Failed(CpuPrepare/DevicePrepare)의 Queued→Applied/중복/충돌/Commit 거부, 유일한 normal result credit을 보유한 채 Ready/실제 frame ACK/예약 Step 진행, finite view·failure code/message 경계 검사를 보강했다. `cargo test -p daedalus-runtime --all-features --locked consumer_`는 실제9개가 통과했다. raw `p03-consumer-reports-credits-luna-attempt-6733I6`의 SHA-256은 `cfd2a8e7cf36d929f8d3fcf593bc589ec9aac91237d5b3ea358cb436f427ba08`이다. 최종 source의 전체 runtime/strict/docs는 이어 재검증한다. subset9와 앞선 전체 실행 수를 합쳐 고유 테스트 수로 보고하지 않는다.

## renderer source 사전 리뷰

CPU owner의 source는 실제 leaf registry·Weak를 포함한 독립 alias·첫 frame 예산·취소 정리·낮춘 한도에서 active/candidate 전환을 기준으로 리뷰 중이다. GPU source에서는 candidate 준비 중 전달된 최신 old-generation frame 대신 기존 ready packet만 그리는 경로와 Ready 뒤 plan 해제 시 다음 poll이 Uploading으로 되돌리는 경로를 확인했다. Luna가 기존 validation을 재사용하는 managed submission/정확한 CPU frame pin과 Ready terminal 유지로 수정한다. 이 사전 리뷰는 compile/GPU 회귀 전의 결함 발견이며 동작 인수나 측정된 개선으로 세지 않는다.

CPU 표 장부의 기존 `size_of<K>+size_of<V>`는 bucket마다 발생하는 실제 tuple padding을 누락할 수 있었다. GeometryKey/MeshHandle 같은 실제 pair의 `size_of<(K,V)>`를 쓰고 HashSet은 key layout을 쓰도록 같은 estimator의 prepared/scratch/frame 경로를 수정했다. Weak가 남은 실제 mesh·부분 준비 후 취소·old frame을 보유한 active/candidate 전환 시험 source도 추가했다. 아직 이 renderer source의 Cargo 검증은 하지 않았다. GPU source에는 whole-buffer 대신 whole-record chunk, 후보별 최소 create quota, 추가 stage bookkeeping 예약과 managed slot 갱신의 계약 누락이 있어 수정 중이다.

별도 read-only host 감사에서는 device 생성 후 dimensions/configure/consuming PacketRenderer 변환 실패와 Renderer 내부 post-request_device unwind를 확인했다. 기존 함수가 owner를 반환하지 않으므로 NoDeviceCreated라고 보고할 수 없다. [host 계약](../03-architecture/native-project-host.md)과 ADR-0007을 수정해 initialized/partial owner 반환·app 생존 범위의 unwind 처리·surface/window 선행 해제를 후속 구현의 선행 조건으로 명시했다. 부분 초기화 holder/API·실제 실패/panic 시험은 아직 열려 있으며 현재 renderer staging 작업에 무단으로 구현 범위를 합치지 않는다.

첫 managed frame 사전 리뷰에서는 CPU validation이 있다는 이유로 active slot map의 재사용을 강제한 결함도 확인했다. 초기 map은 비어 있으므로 비어 있지 않은 첫 frame을 거부하고, 같은 authored entity라도 expanded model draw 변경을 막을 수 있었다. CPU에서 canonical dense draw slot map을 한 번 준비·장부/registry에 등록하고 candidate first draw에서 사용한 뒤 성공 submit에만 active Arc를 교체하는 계약을 먼저 추가했다. 이 수정의 source·compile·실제 initial/topology GPU 시험은 후속이며 source 발견을 통과로 세지 않는다.

첫 renderer `cargo check -p daedalus-renderer --lib --all-features --locked`의 보존된 attempt02는 exit101이다. CPU backing Result 전파와 camera closure의 오류 type 추론이 남았고 unused import/mut 경고도 있었다. raw `p03-render-device-libcheck-luna-attempt02.log`의 SHA-256은 `6d37f55a17a3918755061f8c311860d7ca37d9da37242053acb7d40362d46046`이다. attempt01 파일은0byte(`e3b0c442…b855`)여서 진단 원본 근거로 쓸 수 없다. 작업자도 다른 원본을 보유하지 않았고 retained evidence로 원인을 확인할 수 없어 출력 유실/원인 불명으로 기록했다. 파일을 덮어쓰거나 과거 진단을 복원하지 않는다.

소유자별 최소 compile 수정 뒤 attempt03은 exit0이다. raw `p03-render-device-libcheck-luna-attempt03.log`의 SHA-256은 `fedcd7631152376d160bddb6b751e0f1a5f308b5483d264011611c88d591a3e9`이며 output/종료 footer가 보존됐다. in-progress API의 dead-code/unused 경고7개가 남아 strict 검사 통과는 아니다. 알려진 slot-map 기능 결함은 이 초기 type-check와 별도 수정 대상이고 아직 renderer CPU/GPU 테스트는 실행하지 않았다. Cargo lease를 반환한 뒤 canonical slot map의 CPU 준비와 GPU 소비를 다시 disjoint source-only 작업으로 배정했다.

## runtime source 최종 동결·로컬 검증

마지막 fixture 리뷰에서 nonwaiting port의 정상 호출도 actor와 경합하면 합법적인 Busy를 반환할 수 있음을 확인했다. 테스트의 정상 성공/오류 판정만 기존 방식의 유한 Busy 재시도로 묶고, mutex를 의도적으로 잡고 Busy를 검사하는 assertion은 그대로 유지했다. production·deadline·임의 sleep을 바꾸지 않았다. 이 수정 뒤 runtime source 네 파일을 동결했다.

| 최종 명령/범위 | 실제 결과 | raw SHA-256 |
| --- | --- | --- |
| runtime default `cargo test -p daedalus-runtime --locked` | unit102+integration1+1=104pass, failed0/ignored0/doc0 | `c42cb8e28d0cce8179c0c48520862c9f8c399bbbd63b8f47c1c278488b33ccfb` |
| runtime `--all-features --locked` | unit114+integration1+1=116pass, failed0/ignored0/doc0 | `734e972b0c6438637cf339f2c391cdba21908ae9043adc923808d1c96440b0c3` |
| runtime all-target/all-feature Clippy `-D warnings` | success,1.98초 | `084555292084cdd9f24f50c3235778954d92fa547f8937bea04628e05cd583e8` |

위 raw는 `p03-consumer-busy-fixture-runtime-{default,allfeatures,clippy}-luna-attempt01.log`다. 마지막 scoped runtime rustfmt/diff check도0으로 끝났고 그 뒤 source 변경은 없다. focused `consumer_`는 실제9개가 통과했으나 wrapper가 zsh readonly 변수 `status`에 종료 코드를 대입해 footer 기록에 실패했다. 해당 raw `p03-consumer-busy-fixture-luna-attempt01.log`의 SHA-256은 `9af500cfa2e1cb6cc63d6c2dc8e46a3c90bda24062be9c8f59eb54f814c80a20`이며 Cargo 성공 summary와 wrapper 결함을 구분한다. 최종 전체 두 실행/Clippy에는 exit_code0 footer가 있다.

문서 checker·문서 unit8도 통과했다. Luna raw `p03-consumer-docs-check-luna-attempt-HM0rI6`/`p03-consumer-docs-unittest-luna-attempt-oukBLT`의 SHA-256은 각각 `e9202009d391bd258dcf3f484424f260c9a0c3d079b1448d12bb8e2d0048696c`/`60bb849356ffbc091f601e17fc6ae0b2eb5e26d734d433da020c6e775efbf97f`다. source/로컬 통과와 clean PR의 workspace/managed/두 package 인수는 구분하며 후자는 아직 대기다. renderer WIP·실제 GPU/present·host/SDK/CLI 연결과 전체 b.3/P03/D1은 미완료다.

## PR17의 runtime 하위 경계 인수

[PR17](https://github.com/sonan0721/Daedalus/pull/17)은 exact head `7987d6810f96b15a73849e044b9dd24b9c1532fe`의 다섯 성공과 아래 실제 Mac package 회귀 뒤 ready/squash했다. main `3bc9274163c1a8374c6da7e20e72ea89b3192a09`, head, CI checkout `cbfce6bd738522217e297c757f54f3cf23fbc875`는 모두 tree `e142d43b4db4e10a5365eba837c99f277efe9938`다. 보호3개와 추가 두 package, Publish skipped를 확인했고 tag/공개 Release는 만들지 않았다.

Rust run38990034865/job117270161752는 workspace337pass/0fail/ignored12, strict Clippy/locked check/fmt를 통과했다. docs38990034844/job117270161652와 실제 managed38990034837/job117270161596도 성공했다. release38990034894의 Mac117270162266/Linux117270162107은 각각 packaged CPU62/4.392261초와62/1.051191초로 성공했고 Linux ALSA linkage도 확인했다. binary SHA-256은 Mac `fbe7d138d7129b14df8ee2a6cf534f086175b9ef509477ce3e329968d1e2eff5`, Linux `b7864d6e4207c93354b8bfbcae971e5a9a9d05e4312deda4bfaa5bbc3f85a1b8`다.

실제 CI Mac artifact12085135474를 내려받아 checksum/manifest를 검증했다. archive `daedalus-0.1.0-aarch64-apple-darwin-20261017T011520Z-def80b7d5c0d4e65a4d96f9bd881d5bf.tar.gz`의 SHA-256은 `2c74ee84dde4658a959d82ae333d005e4235794cc704add32543b04d46b1e301`이다. manifest는 exact CI checkout/dirty=false였고 실제 tar 추출 binary도 위 Mac SHA와 같다. local renderer WIP를 포함한 별도 Cargo/package rebuild를 하지 않았다.

그 추출 binary에 기존 helper의 assets/view/models/sprites/audio/native-audio/preparation/activation 조합을 실행해 실제 Apple M3 Pro/Metal에서72/72·19.84801초·stderr0을 확인했다. native finite voice는0.521707초 기다린 뒤 Ended였다. 기존 화면/오디오 경로의 회귀이며 새 render consumer의 실제 GPU 세대 교체가 연결됐다는 뜻은 아니다. raw `p03-consumer-ci-macos-native-root-attempt01.json`의 SHA-256은 `10756a4e6778a40e3993ba998c6c28b077c6c675bbee7c5c12f17d2d02dfa96f`다.

PR metadata raw `p03-consumer-ci-pr-root-attempt01.json`은 SHA-256 `8dda520170c0f9188bf97d698d3777680d0daeb6b7e6a86ddaf3c066229d0cec`, Rust/Mac/Linux job raw는 각각 `d6e9662bed32c69eb8f4fdabce1b65abafcc7a7e9a0b21d2179964ecc9aecadb`, `759717b041ef74ecd736d0a0240c1cbb456a350c7c9b410235e6d9ba79f6ab7b`, `655409756d95e56dacfb06ad20ce1c03cf11042c82714c8a57a4643924182247`이다. Rust의 첫 log 요청은 run 진행 중이라 사용할 수 없었고 빈 attempt01을 보존했다. 실제 완료 로그는 attempt02이며 이를 테스트 실패/재실행으로 세지 않는다.

후속 renderer source 리뷰는 미참조 legacy mesh를 GPU stage 집합에 포함하는 중복과 첫 candidate draw의 expected bind group×draw 중첩 탐색을 확인했다. 실제 참조 binding에서만 stage 집합을 만들고, CPU 검증이 발급한 generation별 private token을 GPU admission/active에 공유하는 작은 계약을 추가했다. 같은 token/identity/limits를 O(1)로 대조하며 actual GPU completeness/serial 검증은 그대로 필요하다. token의 registry/실제 charge/alias 시험도 구현 범위다. 아직 renderer Cargo/실제 GPU 인수는 하지 않았다. 이 runtime slice 인수 후에도 전체 b.3·P03·D1/D2/D3는 계속 미완료다.

병합 뒤 main3bc9274의 docs38990388159·managed38990388158·Rust38990388223·release38990388349도 모두 성공했다. main release의 Linux117271273655/Mac117271273740는 success, Publish117271547194는 skipped다. raw `p03-consumer-main-release-root-attempt01.json`의 SHA-256은 `b328eaad6a07987f6aed406b336bda7b5f6c31d56a87da10f3f0ba1191e14fb9`다. 새 main artifact의 장치 재검사를 수행한 것으로 부르지 않는다.

renderer GPU scenario test source는 Luna A가 새 `packet_renderer/staged_gpu_tests.rs`만 소유하고, Luna C는 production device/staging·parent module 선언·기존 pure tests, Luna D는 CPU/packet/lib를 소유하도록 나눴다. A의 네 실제 GPU 시나리오는 큰 geometry record chunk/odd-width mip, cold/warm quota·inflight1/2, old pose/token/첫 draw, partial cancel/drain/pin이다. 각 source가 동결되기 전에는 Cargo/GPU를 실행하지 않고 한 lease로 집중 검증한다. 이 분담은 작성 중인 계획이며 테스트 통과 수가 아니다.
