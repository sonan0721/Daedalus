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

반복된 evidence wrapper 실수를 줄이려고 [개발 파이프라인](../04-plan/development-iteration-pipeline.md)의 작은 공통 실행기를 먼저 계약화하고 Luna xhigh가 `scripts/run_logged.py`·`scripts/test_run_logged.py`만 구현했다. root 리뷰에서 newline 없는 child 출력/footer 경계·JSON 보고 실패의 exit0·실제 launch 직후 상태 기록을 수정했다. focused 실제5개가0.074초에 통과했고 raw `p03-run-logged-unit-luna-attempt01.log` SHA-256은 `df6e8513686a0a2bbcfc63f3675147135c47e51830b176da181b58d47fc411df`다. 새 실행기를 쓰기 전 기존 exclusive wrapper로 그 첫 unit 결과를 보존했다.

이후 새 실행기로 실제 docs 명령을 실행해 child/runner0·launched=true·0.1866초와 completion footer를 확인했다. raw `p03-run-logged-docs-luna-attempt01.log` SHA-256은 `7a3330b99fdd92d88a4e1bd6751627cae6c1ea3655ff6687a986b04b39256132`이며 root도 bytes/hash를 독립 대조했다. child17·SIGTERM·기존 binary bytes 불변/child 미실행·launch 오류·주입 hash 실패의 의미는 focused tests가 검사했다. 이 도구는 retry/build/lease를 자동화하지 않으며 과거 잃은 증거를 되살리거나 엔진 기능·GPU 동작을 인수하지 않는다.

## DEV06 실행기·설계 문서 PR18 인수

[PR18](https://github.com/sonan0721/Daedalus/pull/18)은 렌더러 WIP를 제외한 Python 실행기/테스트·설계 문서만 선택 commit했다. exact head `226b698489850c662814123c55e7c2b0a3d5d2b8`, CI checkout `341b2a88e8b0d0299232b5dab6eaf8c6159db491`, squash main `daa161d405e80e1b53ab1167cb8d9fc506efd02a`의 tree는 모두 `c65ada3b6a7adde117d9a4ad86b40a70225871b5`다. 다섯 check 성공 뒤 ready/squash했고 Publish skipped·tag/공개 Release 없음이다.

docs run38992499013/job117277735782는 실제 Python70개·문서 검사를 통과했다. Rust run38992499081/job117277736241는 기존 committed 엔진 CPU337/failed0/ignored12와 locked check/fmt/strict Clippy를 통과했다. managed run38992498967/job117277735586도 성공했다. release run38992499016의 Linux117277736002/Mac117277736037는 각각 실제 packaged CPU62개를1.038902초/4.416979초에 통과했다. 새 렌더러 source가 remote checkout에 포함됐다는 뜻은 아니다.

두 CI staged binary의 SHA-256은 Linux `b7864d6e4207c93354b8bfbcae971e5a9a9d05e4312deda4bfaa5bbc3f85a1b8`, Mac `fbe7d138d7129b14df8ee2a6cf534f086175b9ef509477ce3e329968d1e2eff5`로 PR17과 같다. runtime/renderer/packaging backend·device helper 변경이 없고 Mac binary는 위 PR17의 실제 downloaded artifact native72 검사와 같은 bytes다. 따라서 이 workflow-only PR을 위해 native 검사를 반복하지 않았다. archive/manifest는 새 source metadata를 가지므로 archive 전체가 같다고 보고하지 않으며 새 hardware run도 주장하지 않는다.

새 실행기로 보존한 `dev06-ci-docs-root-attempt01.log`/`dev06-ci-rust-root-attempt01.log`/`dev06-ci-release-root-attempt01.log`의 SHA-256은 순서대로 `13cbff7f463723c919d9eb81c20bb94cb46edede85515ad8f22969cb297f76ae`/`fc205a791d8a965aeac53ad933455dfe1ccfeaf01f8bc89f5fea984d07016bd2`/`25d569f9c5c019366393e1f551deb434ba653dbc520850fdc66c2ffa086cd7bf`다. merge 결과 metadata JSON `dev06-ci-pr-root-attempt01.json`은 `590c4024829de1db4273f8f9b816d2b2615de6451356a4f512198d3be529be12`다. 실제 head/tree를 확인한 후 HEAD만 같은 main tree로 정렬하고 renderer WIP를 보존했다. 실행기 인수와 b.3.2 CPU/device·host·P03·D1/D2/D3의 미완료를 구분한다.

main daa161d의 docs38992751425·managed38992751388·Rust38992751397·release38992751439도 성공했다. main release의 Mac117278492880/Linux117278493098 성공·Publish117278827099 skipped를 확인했다. `dev06-main-release-root-attempt01.json` SHA-256은 `c1242fb72113739ca6fe6a174aebdf8b409784eeb9d125dfeab7ac8b2becb8c2`다. 이는 main workflow 상태 확인이며 추가 hardware 실행이 아니다.

## canonical slot 연결의 renderer type-check

CPU prepared slot-map 계약을 GPU에 연결한 뒤 libcheck attempt04는 `&Arc<HashMap>`의 for-loop iteration 오류1개로 exit101이었다. raw `p03-render-device-libcheck-luna-attempt04.log` SHA-256은 `de13282a7d8becaa02309dc2a2838705c3be02a854b6a2d0c45d1075fdda1294`다. 명시적 iter 호출로 수정한 attempt05는 warning 없이 exit0,1.86초에 끝났다. raw `p03-render-device-libcheck-luna-attempt05.log` SHA-256은 `a6e1dc9c23a6175a20afe062e4466c0d589641b1bf8d67971e418430b2507aa1`이다. 이 두 명령부터 검증된 공통 실행기를 사용했다. source scoped fmt/diff도 통과했으나 아직 CPU/GPU 테스트 인수는 아니다.

별도 read-only 감사는 기본 quota4MiB를 낮춘 scratch1MiB에 맞춰 줄이지 않는 경우와, raw4byte/default texture의 padded256byte가 staging cap128에 영구 ResidencyBusy가 되는 경우를 확인했다. [device 계약](../03-architecture/render-device-generation.md)에 effective unit·frame headroom·개별 영구 초과와 transient Busy의 분리를 먼저 명시했다. 현재 slot source의 compiler/CPU feedback을 받은 뒤 bounded follow-up으로 수정하며 cap/deadline을 늘리지 않는다. 이 기록 시점에서 기능 결함은 열린 상태다.

## 첫 renderer 테스트 compile/CPU feedback

test no-run attempt01은 nested module의 상대 path 오류로 실패했다. 부모 선언을 inline include로 바꾼 attempt02는 실제 테스트 컴파일까지 도달했지만 CPU 테스트 GeometryKey import4곳·부모 테스트 StagingSubmission import3곳·Ready 유지 pure fixture의 erased Arc/새 slot field2곳으로 실패했다. 소유자별 최소 수정 뒤 attempt03은 test binary compile을 통과했다. raw `p03-render-device-test-build-luna-attempt01/02/03.log` SHA-256은 순서대로 `0d8b93e00a4ac582e60367fcbcdfd8e6482d96837885c05c56ea269490e66327`/`694f8b6f7e4c48a297c439dc53f29cbcaac7b50900b41b1fb8d80b44e5821ee8`/`f83e30df0fe81fdc2db2aadb913305dd1021a6f5cee72e2a5b483b7f2b0df20b`다. 중간 path 수정의 standalone rustfmt 실패는 runner/Cargo 시작 전이므로 별도 Cargo attempt02 실행으로 세지 않는다.

첫 실제 all-features renderer CPU 실행은64pass/3failed/14ignored였다. sprite fixture의 필수 camera_2d 누락, source Vec capacity를 destination clone의 actual copy charge와 혼동한 assertion, 새 metadata layout이 반영되지 않은 template delta expectation을 source/계약과 대조해 수정 중이다. raw `p03-render-device-cpu-allfeatures-luna-attempt01.log` SHA-256은 `089a8645301cd0ff4f7f893b1f0fa0b52cf124810101d64d0dfb8fd1ca208222`다. assertion을 느슨하게 하거나 production을 fixture에 맞춰 바꾸지 않고 각각의 전제·계산 근거를 검토한다. CPU 전체·실제 GPU·낮춘 cap 수정·host 인수는 이 시점에 아직 미통과다.

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

## renderer CPU checkpoint와 낮춘 cap 후속

위 세 실패는 CPU fixture/계산 전제를 수정했다. sprite fixture에 필수 독립 camera_2d를 추가하고, 두 번째 model의 metadata delta160byte를 template/table144+정렬된 ID string16으로 확인했다. legacy copy 시험은 source Vec capacity에 대한 보수적 사전 예약과 destination clone의 실제 retained capacity 장부를 구분한다. production 한도·timeout·오류 판정을 fixture에 맞춰 바꾸지 않았다.

| 명령 범위 | 실제 결과 | raw SHA-256 |
| --- | --- | --- |
| renderer lib all-features/locked attempt02 | 67pass/0failed/14ignored | `3ca80576c3a83f8d0192d4e558442d00e90dc8de0394b854c8709ccc856861b1` |
| renderer lib default/locked attempt01 | 66pass/0failed/14ignored | `ec811bb4c755fee03c026ce4335ebfc96680844011e73aafb5c090c2ac611139` |
| renderer lib no-default-features/locked attempt01 | 51pass/0failed/13ignored | `b0bd2add76257e6420f59a7d330e38a301513a3e1713165d518f39366815faef` |

raw는 `p03-render-device-cpu-{allfeatures,default,nodefault}-luna-attemptNN.log`다. root는 마지막 no-default의 completion footer와 최종 hash도 대조했다. build/runner 경과는 각각15.95/16.45/63.42초이며 테스트 자체 시간과 구분한다. 다른 feature 설정의 반복 실행 수를 고유 테스트 수로 합산하지 않는다. ignore된 실제 GPU 테스트는 이 CPU 실행에서 실행하지 않았다. A의 현재 실제-adapter 시나리오는 여섯 개로 늘었으며 GPU 실행 전 source checkpoint다.

CPU checkpoint 뒤 Cargo lease를 반환하고 C에는 effective whole-record/row·frame/overlay headroom·permanent/Busy 분리의 source-only 수정, A에는 scratch1MiB와 staging1MiB를 서로 독립해 검증하는 실제 GPU 회귀 source를 배정했다. 낮춘 staging 시험은 scratch를 기본값으로 유지해 두 원인을 숨기지 않는다. padding 최소 단위 거부는 실제 frame 최소 예약에 가려지면 pure helper 시험으로 분리한다. cap/deadline/queue 증가나 runtime/window/CLI 변경은 허용하지 않는다. 소유자 동결 후 한 lease로 다시 CPU·strict·실제 GPU 검증한다. 이 시점에 GPU·새 renderer source의 workspace/package/CI·host·전체 b.3/P03/D1/D2/D3는 미완료다.

사용자가 조정자의 실행에서도 일부 소리가 들렸다고 한 보고는 A-042에 기록했다. 정확한 실행/시점·loopback 계측·과거 실패 원인은 확정하지 않는다. 같은 사용자 보고를 두 번째 확인이나 별도 새 오디오 재생 시험으로 세지 않는다.

## 낮춘 cap 구현과 첫 실제 GPU feedback

Luna C는 공통 frame-write byte 계산을 normal submit/clear/stage 계획에 재사용하고, raw quota·실제 vertex Vec capacity scratch·padded staging·frame/overlay/in-flight headroom으로 whole record/row를 나눈다. global 단위 초과는 효과 전 영구 오류다. 가변 dirty frame 때문에 여유가 없으면 frame-only submit으로 stage cursor를 유지한다. 매번 필요한 overlay 최소 비용과 양립할 수 없는 남은 단위는 create 전에 오류로 알리며, 기본 예산에서는 O(1) bound로 통과한다. 좁은 cap에서만 이미 완료된 단위를 제외한 remaining scan을 사용한다. 이 설명은 source와 아래 집중 검증의 범위이며 host 연결 인수가 아니다.

Luna A는 기존 여섯 실제-adapter 시나리오에 lowered scratch, lowered staging+overlay, 65pixel/padded512byte 행의 cap400 사전 거부, dirty old frame의 일시 지연/동일 frame 재시작을 더해 열 개를 작성했다. generated project·synthetic snapshot과 실제 GPU를 사용하며 runtime authority/window 통합 시험은 아니다.

root의 all-features CPU attempt01은 compile 오류2개로 exit101이었다. clear-only 통계에서 refactor 뒤 사라진 overlay 변수와 sibling-private field를 읽은 테스트였다. 공통 frame 계산 결과 재사용·기존 stage 상태 조회 method로 각각 수정했다. raw `p03-render-lowered-cpu-allfeatures-root-attempt01.log` SHA-256은 `da9ee9828168cca24a951dd6d6ade18d0fb8d4d46ff923f295eb63004fec0f3c`다. attempt02는71pass/0failed/18ignored·runner6.704초에 통과했고 raw SHA-256은 `f4def3c239d7b39b8e02f7349861f063664bfd9489a973a7ca6504fbfa590128`이다.

첫 실제 GPU 열 개는 Apple M3 Pro/Metal/IntegratedGpu에서7pass/3failed·test8.32초였다. raw `p03-render-staged-gpu-root-attempt01.log` SHA-256은 `c86061e86da7787f8b177f0331ed61054393d91ccee43e273240c56cfe70703a`다. 낮춘 scratch/staging, padded 최소 행 거부, 큰 geometry/mip·취소/drain·old pose/token·cold pipeline 시나리오는 통과했다. 실패 세 건은 다음과 같이 분리해 후속 수정한다.

- victim이 없는 후보를 status poll만 했는데 `victims_applied=true` 때문에 Admitted에서 Uploading으로 바뀌었다. 실제 create/upload 시작 여부를 기존 progress로 판단하도록 production phase 보고를 수정한다.
- quota 거부 전후 snapshot의 in-flight2→1은 정상 completion poll/회수였다. 실제 이전 제출을 drain한 뒤 baseline을 잡아 preflight의 새 resource 효과만 비교한다. ledger 수치를 제거해 차이를 숨기지 않는다.
- dirty frame의 전체 upload write counter는 instance+globals 두 쓰기도 센다. candidate stage counter는 그대로였지만 전체 쓰기 수를 불변으로 기대한 테스트가 잘못됐다. 두 쓰기/256byte와 texture 불변을 정확히 확인하도록 수정한다.

이 시점에 실패한 세 건의 수정·재검증과 전체 GPU/strict/workspace/package 인수는 대기다. 반복 실행 수를 고유 테스트 수로 합산하지 않는다. legacy slot-planner의 per-entry slack은 논리적 보수 추정이고 Vec/HashMap/HashSet allocator/RSS의 수학적 상한으로 보고하지 않는다. 전체 b.3/P03/D1/D2/D3는 미완료다.

수정 뒤 fully-qualified/exact 검사로 ledger 포화 case는 실제1개/1.65초, dirty frame deferral case는 실제1개/0.86초가 통과했다. raw `p03-render-staged-ledger-gpu-root-attempt01.log`/`p03-render-staged-headroom-gpu-root-attempt01.log` SHA-256은 각각 `7a9f4214c9bf0950b8ab0dcc5deb68064e2dd794bfa47c5cb23674bc6cf892df`/`77811067ff816e70581790dcfa66f85a90551e9ec5d4fe68cb5be99ea7c0de74`다. quota case의 새 실행은 앞선1/2/3-create 거부/진행 비교를 지나 cache-hit metadata에서 실패했다. raw `p03-render-staged-quota-gpu-root-attempt01.log` SHA-256은 `e1754a0833827f3b110060c9ea3b34fda65dd2344a8d5e0c51d1f6051f797b50`다.

cache-hit의 실제 자원 readiness 검사는 기존 initialization serial로 올바르게 진행됐지만 status는 이번 stage의 새 upload만 보고해 `initialization_serial=None`이었다. [device 계약](../03-architecture/render-device-generation.md)에 Ready 요약은 필요한 upload 초기화 의존성의 최댓값이며 cache-hit도 상속하고 빈 의존성만 None이라는 의미를 명시했다. 기존 readiness traversal에서 completeness와 작은 serial 요약을 함께 얻도록 Luna C에 배정했다. last-use/draw serial을 초기화로 간주하거나 실패 assertion을 제거하지 않는다. 이 추가 수정의 실행 검증은 아직 대기다.

## renderer 로컬 최종 회귀

cache-hit metadata 수정 뒤 quota exact case는 실제1개/1.48초에 통과했다. raw `p03-render-staged-quota-gpu-root-attempt02.log` SHA-256은 `c0ce98c1c41052f6ba7c69a658da03710d6e841e0f7bbcdc0063a2da8d384a4d`다. 이어 전체 renderer 실제 GPU는 library18+capture integration1=19pass/0failed였다. 첫 full raw `p03-render-all-gpu-root-attempt01.log` SHA-256은 `0a79b12cc28cdecf836ef640da36a9b4b5ba4ee23a94ce744e86a8aaeb58315b`다. subset/재실행 수를19개에 더하지 않는다.

다음 strict Clippy attempt01은15개 진단으로 실패했다. CPU의 명시적인 준비/장부 함수와 public render boundary의 인자 수, 새 private frame helper의 인자 수, 불필요 closure/borrow, 테스트 initializer 표현이었다. Luna D/C가 소유 파일에서 동작을 유지하는 정리만 했다. 필요한 boundary의 인자 수 lint는 이유를 적어 해당 함수에만 제한했고 private frame helper의 dimensions는 tuple로 묶었다. crate/module 전체 경고를 끄지 않았다. raw SHA-256은 `b5e029e9c3bf373d3037a872594d9aa6929745b2749c9422e0684ab3eb0688f0`다.

| 최종 검사 | 실제 결과 | raw SHA-256 |
| --- | --- | --- |
| workspace all-feature CPU attempt02 | 359pass/0failed/23ignored, runner17.739초. renderer72 포함 | `6c3c577e8344eb14963409860a4093eda965bb18e1f5fe7d65181ced5d6993c2` |
| renderer 실제 GPU attempt02 | M3 Pro/Metal, library18+integration1=19pass, runner16.180초 | `80fb6f00baa10d16901113a45c698f666875c7391174a8f92835327a9d2aeed2` |
| workspace all-target/all-feature strict Clippy attempt02 | success, runner5.071초 | `a7609ba117c525086c605082f9accb4bb677cfbaa6140a9bd42df4b6d1507269` |
| cargo fmt all/check attempt02 | success | `296706f7339ab61518bd00435240661a8ec10234325b3ed8e3dc9dd453cf636b` |
| included GPU test standalone rustfmt/check | success | `4541e1e908b5760190a858a2f682ad1d4d182ebc491988e55e81e1ddca9ccf0e` |
| Python discovery attempt01 | 70pass/0failed,11.347초 | `9e8d677dfc432d89a14abbe35f3529b413e12fe2c295b8423e06dd66ff36e16e` |

위 파일명은 각각 `p03-render-workspace-cpu-root-attempt02.log`, `p03-render-all-gpu-root-attempt02.log`, `p03-render-workspace-clippy-root-attempt02.log`, `p03-render-final-fmt-root-attempt02.log`, `p03-render-final-gpu-test-fmt-root-attempt01.log`, `p03-render-final-python-root-attempt01.log`다. 마지막 전체 검사 이후 renderer.rs의 한 predicate 줄바꿈만 rustfmt로 고쳤고 fmt를 다시 통과했다. 실패 fmt attempt01도 보존했으며 SHA-256은 `e468fc8ce1df0a327b0b8918a0769dd3e13633d2bf30adc02c451a04f3e160a8`다. 이 whitespace 변경으로 동작 시험을 반복하지 않았다.

style 정리 전 같은 최종 로직의 renderer default는71pass/18ignored, no-default는56pass/17ignored였다. raw `p03-render-final-cpu-default-root-attempt01.log`/`p03-render-final-cpu-nodefault-root-attempt01.log` SHA-256은 `45020fb5a0c958b45ff1654be859f5557de59005de7532051299fd800d44b030`/`f4f1ff7e5e09647c28088670492b9f7350a0d3d0ae5d104924759da230ad09b2`다. style 정리 전 workspace 첫 실행도359pass/23ignored였고 raw `p03-render-workspace-cpu-root-attempt01.log` SHA-256은 `93e996d98b5559fd22deb6d6ffc3bd0fd4297ed64536aacefc8ff20a5d96c194`다. Mac 전용 audio diagnostic ignore가 있어 Linux CI의 ignore 수와 다를 수 있다.

독립적인 기존 managed 회귀는 pinned home SDK10.0.402로 확인했다. bridge build, warning/error0의 Release solution build, 실제 Rust-backed self-test, native round-trip/module lifecycle 두 integration scenario가 통과했다. 순서대로 `p03-render-managed-{bridge-build,build,selftest,integration}-root-attempt01.log`의 SHA-256은 `868681d4f0bbad1b883d85a372c2c8d32bceb59b297a3b7c02e5dae28142d099`/`b1182587d1e1dc6239a83020d51a72445266792559e1978531660db3859a58bf`/`c958af7b5356db019fb0e871336978350000be3bbf694fa027be7522fdf6a7bb`/`6c403eb8673a0281cd18245d17a5fb54d4aaf0ef61c7207790e7e7ed20bb9e67`다. 새 renderer가 C# SDK나 host에 연결됐다는 뜻은 아니다.

Luna가 작성한 source와 root 리뷰·로컬 검증은 이 범위까지다. 다음은 clean PR의 다섯 check·두 package와 실제 artifact의 회귀/동일성·main tree 인수다. 해당 인수 전 b.3.2a/b 체크는 열어두며, host/실제 세션·창/SDK 연결과 전체 b.3/P03/D1/D2/D3도 미완료다.

## PR19 renderer library 경계 인수

[PR19](https://github.com/sonan0721/Daedalus/pull/19)의 exact head `e3056f0fe1692ea2f3ade250f102b2025fba8dbe`, CI checkout `956893762e812425edeff34eb1a76ffeb67d6e16`, squash main `c507835c9920cab4b075904e80f2d7f7980e8cdf`의 tree는 모두 `a6588117dc712296e1bad9b8698f5ff4d897fc44`다. 다섯 성공과 아래 extracted artifact 회귀 뒤 ready/squash했다. 보호 contexts3개·strict=true·squash-only 설정도 재조회했다. Publish skipped·tag/공개 Release 없음이다.

docs run39004284192/job117313445986는 실제 Python70·문서 검사, Rust39004284148/job117313446006은 CPU359/0failed/22ignored·locked check/fmt/strict, managed39004284146/job117313446226은 실제 native bridge/managed integration을 통과했다. release39004284190의 Linux117313446297/Mac117313446501는 각각 packaged CPU62/1.062378초·62/4.612823초를 통과했다. Linux audio linkage도 성공했다. staged binary SHA-256은 Linux `907f889d66112be7bffab900e272485fb40017ffb4685bdea3a3cbebb0172784`, Mac `73961a47103d00bb8ba6ea4414451c17d4c1965bbdc86aa625380502dd5fcdb0`다.

실제 Mac artifact12090148045를 내려받았다. archive `daedalus-0.1.0-aarch64-apple-darwin-20261017T045435Z-506aa951a12a4036bfee7e29c0913927.tar.gz` SHA-256은 `76f24f5bd69fe8ea2417a3ad9b6031c63c223b9cac3e2227e2148849d7c5a5f5`다. checksum·embedded manifest/file hash·dirty=false·exact CI checkout과 extracted CPU/offline audio를 기존 archive verifier로 확인했다. 추출 binary는 위 Mac SHA와 같다. local package rebuild나 별도 smoke backend는 추가하지 않았다.

그 binary에 기존 helper의 assets/view/models/sprites/audio/native-audio/preparation/activation 조합을 한 번 실행해 실제 M3 Pro/Metal의72/72·21.72137초를 확인했다. native finite voice도 실제 Ended였다. loopback/이 시도에 대한 새 사용자 가청 확인은 아니며 과거 audio 실패 원인은 여전히 미확정이다. 새 stage API는 별도 library GPU19로 검증했으며, 이 기존 CLI 회귀가 host activation을 연결했다는 뜻은 아니다.

| 보존한 raw | SHA-256 |
| --- | --- |
| `p03-render-ci-rust-root-attempt01.log` | `b3bc748dfc57ac1f9ebe985e623f76344c55dc945a90e805fafa5347226df281` |
| `p03-render-ci-release-root-attempt01.log` | `914e71bfbf6fd8ea9647805bf8e739c6a0a1f4ed838b73d0240a0cc68ab32a29` |
| `p03-render-ci-docs-root-attempt01.log` | `be934e5f1283604b011a7e68ac85425a3c0e6140ba974b7b8b8fa128f987de01` |
| `p03-render-ci-download-root-attempt01.log` | `03c2240e80e88358f38c99b362ac267a52c80334514ad78fe279ba0088d6174a` |
| `p03-render-ci-archive-verify-root-attempt01.log` | `7d8b17a882c811fe07b0948f275f86f1a60364186594275c41b832e130d3b404` |
| `p03-render-ci-macos-native-root-attempt01.log` | `c82696af29bebb58c48efed38cf1b9495a7f91e9b8f2ee9fdb931be57fb1f24c` |
| `p03-render-ci-merge-root-attempt01.log` | `95b2254e568ee8e79609352a8661439deb4e6c61e97daa91599596e0450feced` |

main c507835의 docs39004551395·Rust39004551392·managed39004551438·release39004551409도 성공했다. main package Mac117314235028/Linux117314235276 성공·Publish117314461500 skipped를 확인했다. status raw `p03-render-main-checks-root-attempt01.log` SHA-256은 `e7a5356b0af5a63b3ce34f332ae363383ce8a3cc01240f865ddef936be891268`다. 추가 hardware 실행으로 세지 않는다.

main release raw `p03-render-main-release-root-attempt01.log` SHA-256은 `d77d13c4f0f34404ab17793fb287c7f96bf8437395203f421126029cd899103e`다. 두 main staged binary도 위 PR19의 Mac/Linux hash와 각각 같고 packaged CPU62가 통과했다. 같은 binary bytes에 장치 검사를 다시 반복하지 않았으며 새 archive 전체가 동일하다는 뜻은 아니다.

이 근거로 b.3.2/2a/2b library 체크만 인수했다. b.3.3의 실제 host·window bootstrap/부분 초기화 회수·SDK·같은 authority의 실제 present ACK와 전체 b.3/P03/D1/D2/D3는 계속 미완료다. 다음 branch는 `feat/native-project-host`이며 exact API/오류 소유권을 먼저 문서화한다.

## 다음 host의 auto-trait 설계 probe

remote CI를 기다리는 동안 Luna D가 `/tmp`의 작은 Rust source만 작성해 기존 artifact에 rustc metadata probe를 실행했다. production/test/manifest/lock/dependency 변경이나 추가 Cargo build, runtime/GPU/audio 초기화는 없다. service Send+Sync·private Arc Send, PacketCpuOwner Send, CpuFrameHandle/PacketStageInput Send+Sync, PacketRenderer/Renderer Send가 해당 feature 조합에서 compile됐다. root도 probe source와 raw hash를 대조했다. 실제 host의 안전한 종료·device 마지막 drop·frame 선점은 여전히 별도 검증이다.

source `/tmp/daedalus-host-traits-luna-attempt01.rs` SHA-256은 `757bba9247137ed946b31ac6552d285dea7176fcce25f9f7529d51f70dce6902`, raw `p03-host-traits-luna-attempt01.log`는 `670854115b876c9dd389e2f9530928149f5a211821e8f2d8fafa055c51cf824d`다. renderer default+test-fixtures+window artifact `libdaedalus_renderer-5638dc27dcc90085.rlib` SHA-256은 `84611d768570ed177e3403a951e7485e84ff97c2036505590682eb2423ef1aa3`, runtime audio+native-audio artifact `libdaedalus_runtime-bda12bd485c4cc76.rlib`는 `06d9d7b83b9bca96a797963076f98bd1e33eebe1624f3c11259f259f669a7c4f`다. 새 host crate 자체의 compile 결과로 보고하지 않는다.

## 실제 host 연결 전의 owner 계약

새 [native renderer owner 수명](../03-architecture/native-renderer-owner-lifecycle.md)은 초기화 중 실패를 완성 PacketRenderer/NoDevice 둘로 뭉개지 않기 위해 만들었다. source 감사와 Luna read-only 리뷰를 거쳐 DeviceRequest/RendererPartial/RendererReady/PacketPartial/PacketReady를 분리하고, 고정 base/lazy handle·stage paired create·app 내부 catch·항상 owner를 반환하는 managed 결과를 계약으로 정했다. 실제 구현·주입 오류/panic·장치 종료 시험은 아직 수행하지 않았다.

첫 bounded 코드 task는 renderer owner와 stage partial pair이며 창/host 코드는 후속이다. 기존 CPU/GPU planner·legacy 생성자를 공통화해 코드 복제를 피하고 stage마다 새 thread/새 recovery framework를 만들지 않는다. 체크리스트 3.3a/b/c/d는 모두 열린 상태다. 같은 사용자 가청 답변을 새 테스트나 추가 확인으로 중복 계산하지 않는다.

[Managed packet 창](../03-architecture/managed-packet-window.md)은 첫 실제 frame 없이 GPU loading을 표시하고 stage 준비/actual-present event를 분리하기 위해 추가했다. root는 Luna의 read-only 제안을 검토해 payload capacity1·원래 payload 반환, 별도 cancel-through atomic watermark, host가 발급하는 checked local key, noncoalescing stage event와 generation 내 latest-only present를 정했다. Busy 입력은 canonical CPU owner에서 다시 투영하며 import/prepare를 반복하지 않는다. window와 owner 파일을 분리하고 공통 export 소유자를 C로 고정해 동시 편집/compile 반복을 줄인다. 이 계약은 실제 창 인수 결과가 아니다.

계약 MD 검사 raw는 각각 `p03-owner-contract-docs-root-attempt01.log` (`f7fd468050944985eb1c6095b5d9f7c21acf94cf67f35c15d7315902663727f8`), `p03-managed-window-contract-docs-root-attempt01.log` (`452474e1b1efce79f31b803b4394578c29af0329cd39b6c0f20a1f2bbc9bf417`), `p03-host-start-contract-docs-root-attempt01.log` (`48734e350678601b2a390f21201cf94444aa52c22d6f557cf828ed94062ea80f`)로 모두 exit0이다. 이 세 실행은 문서/링크 검사이며 새 source의 compile/기능 통과 근거가 아니다.

코드 소유권은 C(renderer/packet/staging/owner·공통 export), A(window/하위 module), D(새 host source/외부 SDK integration tests)로 나눴다. D는 C/A의 focused compile 동안 manifest/Cargo.lock을 바꾸지 않는다. root가 공개 계약·리뷰·단일 검증 lease를 조정한다. 새 host의 기본 feature는 장치 없이, window는 opt-in이며 serde/별도 transport framework·새 registry dependency는 추가하지 않는 안을 채택했다. 실제 구현/테스트 결과는 뒤에 별도로 기록한다.

source 리뷰로 계약의 두 과도한 표현도 고쳤다. WindowGpuOwner가 감싼 PacketRenderer에는 drain까지 필요한 source/frame pin이 실제로 남고, 일반 cache/stage retirement의 GPU handle drop은 기존 render 호출 thread에서 발생할 수 있다. 이번 작업은 초기화·fatal pair panic·창 종료 반환을 보존하는 범위이며 모든 정상 GPU 해제를 worker로 옮겼다는 주장이 아니다. 또 managed present에 실제 view-projection 행렬을 추가해 host가 camera 수학을 복제하지 않게 했고, CPU owner가 기존 authority frame mailbox를 직접 읽어 큰 snapshot을 coordinator에서 교체/해제하는 새 경로를 줄이는 방향으로 계약을 구체화했다. 실제 성능 개선율과 성공 여부는 미측정이다.

후속 WIP 리뷰는 shared FIFO8/blocking result send가 stop/device drain을 막을 수 있는 초안을 거부하고 lane별 capacity1·관측까지 보유하는 결과 slot으로 고쳤다. 실제 saturation/stop 검증은 후속이다. 또 GPU owner 반환 try-lock 경합을 facade field/mem::forget으로 넘기는 초안 대신 단일 publisher의 one-shot 설치 slot을 계약에 명시했다. code가 이 계약을 만족하고 실제 종료 검사를 통과하기 전 host 인수는 열어둔다. renderer 오류 주입은 동일한 필드 대입마다 hook을 복제하기보다 서로 다른 소유권 전이·전체 고정 handle 보유를 검사하도록 표를 명시적으로 수정했으며 미실행 경계는 구분한다.

S90은 pinned Rust1.94.0 source를 확인해 get 비대기와 set의 경쟁 초기화 시 대기 가능성을 분리했다. host의 private 단일 publisher 전제를 코드로 유지해야 하며 표준 type의 일반 wait-free 보장으로 바꾸지 않는다. 현재 온라인 rustdoc은1.99.1이고 versioned rustdoc 접근 실패도 출처 등록부에 남겼다.

초안의 CPU worker 종료 조건에는 owner poll 이후 UI publication이 일어나면 returned flag만 보고 수신 전에 종료할 수 있는 경합도 있었다. 실제 take/drain 관측과 publication을 분리하도록 계약·Luna 작업을 수정했다. 아직 재현/수정 시험 통과로 기록하지 않는다. 같은 검토에서 facade Drop 뒤 오류 slot의 미관측 ACK가 cleanup을 영구 유지하지 않는지도 검사 항목으로 추가했다.

## Owner/managed window 구현 중간 검증

아래는 host/CLI 통합 전 WIP의 focused 결과다. 최종 source/전체 회귀 인수를 대신하지 않는다. Luna C가 단일 Cargo/GPU lease로 실행했고 root가 raw footer/hash를 대조했다. 실제 adapter는 Apple M3 Pro/Metal/IntegratedGpu다.

| raw log | 실제 결과 | SHA-256 |
| --- | --- | --- |
| `p03-owner-gpu-owner-luna-attempt03.log` | request error·awaited request panic(모두 handle 없음), post-device/base partial, packet buffer/group partial, RendererReady validation error, 지정 worker의 ready owner drop의 7개 통과 | `85b629f226ca8590c66c28ec2f2c8110be4561d1610a1df83f1794ad7bf6af99` |
| `p03-owner-gpu-lazy-luna-attempt02.log` | lazy layout/shader interruption 2개 통과 | `230b8d376b571f6ce17cbe863b533a9957cd79922346630d22815b1722b1ab17` |
| `p03-owner-gpu-staged-all-luna-attempt02.log` | 기존 staged GPU와 새 paired-create 중단을 포함해 11개 통과 | `da5381eff1060aabba2042bd0208cafa5bd93b86a3f029247fd55c03591515b1` |
| `p03-owner-clippy-luna-attempt04.log` | renderer no-default + test-fixtures strict Clippy exit0 | `50c1c57715dc9d31cbe32175c44ab2236f214940506fc3d8eff3bfaf32de6741` |

오류 주입의 panic 출력은 catch와 owner 보존을 검사하는 예상 입력이며 전체 명령은 exit0이다. paired-create의 geometry vertex→index·texture→view·default texture→view 세 하위 경우는 한 테스트 안에 있고 위11개에 포함한다. 같은 owner subset·재실행을 합산해 unique 통과 수를 늘리지 않는다. 실제 surface 생성/표시·window 반환·host ACK/종료는 이 시험의 범위 밖이다.

all-feature no-run 첫 시도는 A 소유 window의 import/borrow/privacy/derive 오류로 실패했다(`p03-owner-allfeatures-luna-attempt01.log`, `b6f0f09afd8cb930965a67a9995962342647b0f18af5f87557f22144bc0e32e7`). 수정 후 두 번째도 test fixture closure의 mutable borrow와 unused mut으로 실패했다(`p03-owner-allfeatures-luna-attempt02.log`, `1b849d5dd6fb5d41b82196d27313e6db04ecd90c96b54c9869b32b54a5cc879b`). A가 두 번째 수정을 마치고 파일을 재동결했다. 후속 성공은 별도 raw로 남긴다. 실패 기록을 덮어쓰지 않았다.

one-shot disposition 계약의 문서 검사 `p03-host-disposition-contract-docs-root-attempt01.log`는 exit0·SHA-256 `d9beddccf1e755199accd39ccce1f77cbaa1cba3aefef1c0586774d87a591224`다. 이는 코드 기능 통과가 아니다.

첫 all-feature CPU 실행은 83통과/1실패였다(`p03-owner-allfeatures-cpu-luna-attempt01.log`, `d74928310fa81b1fe0d612433f411fe5e5d353d595d3f6283fbe454ef6a14a15`). root가 source를 대조한 결과 content의 `sha256:<64hex>` 71바이트 digest를 새 managed identity가 진단 문자열처럼64바이트로 자르는 production 오류였다. 테스트의 identity 비교를 약하게 하지 않고 stage admission/present/summary와 host의 동일한 truncation 경로를 모두 바로잡도록 계약과 Luna 작업을 수정했다. 기존 packet validator의 형식 검증을 재사용한다. 수정 후 성공은 별도로 기록한다.

### Renderer 구현 commit의 최종 focused 근거

실제 digest를 그대로 보존하고 기존 packet validator를 crate 내부에서 공통 사용하도록 수정했다. 새 managed public API를 추가하면서 기존 packet/legacy API·기본값은 유지했다. owner in-place 초기화와 stage paired-create 보존은 같은 renderer helper를 사용한다. 아래는 renderer source 동결 뒤의 근거이며 host/CLI 코드는 아직 포함하지 않는다.

| raw log | 결과 | SHA-256 |
| --- | --- | --- |
| `p03-owner-allfeatures-cpu-luna-attempt03.log` | CPU85통과/0실패·장치28ignored. 실제 CPU owner fixture의 포화/연결해제 원본 payload 반환·bootstrap camera·취소 중 active pose·invalid startup·71바이트 digest 보존 포함 | `d21731d6933da7430172c5c03f9bf600ad53c9618ca11162e767c9dc49f03bb5` |
| `p03-owner-allfeatures-clippy-luna-attempt03.log` | renderer all-target/all-feature strict Clippy exit0 | `0b061833708ae6481de85a274c765f19a85eb17f58b67aa394e76252684b8b3f` |
| `p03-owner-renderer-fmt-luna-attempt01.log` | renderer package fmt check exit0 | `5af79323d6d020951ab4e2d01e8a197d8c5013eebe165e44f1d1041372bcd61c` |
| `p03-host-renderer-full-gpu-root-attempt01.log` | root가 전체 ignored renderer 명령을 한 번 실행. 실제 Metal library28+PNG capture1=29통과/0실패 | `7209ad719355ac6bf283115b13b377e07ce397ba7bcdd00a3840fee71044c2e8` |

마지막 명령은 `cargo test -p daedalus-renderer --all-features --locked -- --ignored --nocapture --test-threads=1`이다. 이전 owner7/lazy2/staged11 subset과 기존 model/sprite/alpha/legacy capture를 모두 포함하므로 이29개에 subset 수를 더하지 않는다. 실제 adapter는 Apple M3 Pro/Metal/IntegratedGpu이며 root가 출력된 metal-roughness PNG의 세 구와 바닥을 시각 확인했다. 이미지 확인은 상용 비주얼 품질 인수가 아니다.

앞선 strict Clippy01은 dead code·unused variable·조건문 정리를 요구했고02는 정리 뒤 unused import를 발견했다. raw `p03-owner-allfeatures-clippy-luna-attempt01.log` (`1e0b251b7eff255b7697134ea973ec906703bf0d81b65a4c94e027b5dbbd45c3`)와02를 보존했으며03이 최종 성공이다. 별도 focused digest 재실행은 위85개에 포함한다. renderer snapshot의 CPU/Metal 통과와 실제 새 managed 창/host 연결 통과를 혼합하지 않는다.

C가 renderer lease를 반환한 뒤 root가 전체 Metal 회귀를 실행했다. 종료 후 D에게 새 host manifest/workspace member/Cargo.lock의 단독 편집·focused Cargo lease를 넘겼다. 기존 target cache를 재사용하고 새 registry dependency·추가 cold workspace·새 smoke framework를 만들지 않는다. 공개 Rust SDK start/attach/Step/Realtime/stop 및 실제 worker 포화/반환/Drop 시험은 source 작성 뒤 실행 대기 중이다. 전체 workspace·managed·CLI 실제 창/교체/ACK·CI/packaged 검증과 3.3a/b/c/d 최종 인수는 아직 남았다.

검증한 renderer source는 `cf86707`로 commit/push했다. host source/manifest는 이 commit의 인수 범위에 포함하지 않았다. A의 후속 read-only CLI 감사는 host Closed event가 Empty/Busy와 합쳐져 CPU frame 요청이 계속되는 초안을 발견했고 D가 수정 중이다. report에는 last actual-present만 있고 legacy accepted_packet/final shutdown 보고를 채울 근거가 부족한 것도 확인했다.

그래서 [CLI activation 연결 계약](../03-architecture/native-project-view-activation.md)을 새로 만들고 explicit activation branch·동일 host·redundant preparation 허용·activation+audio 효과 전 거부·report v2를 동결했다. 기존 runtime publication token을 queue ingress 관측에 재사용하고 기존 service의 SessionShutdownReport를 빌려 종료를 보고한다. queued 값을 UI accepted/present로 꾸미지 않는다. 별도의 identity type/큰 final packet/새 smoke framework를 만들지 않는 선택이다. 문서 검사 `p03-host-cli-adapter-contract-docs-root-attempt01.log`는 exit0·SHA-256 `d590abdf4b7d7d3d56e09d39359528323fbab37fb92bd8e74f2691d6eed72102`다. A는 CLI source만, D는 host와 manifests/lock/Cargo만 소유하도록 후속 구현을 나눴다.

추가 source 감사는 CPU Busy memoization이 취소/currentness보다 먼저 return해 held alias가 계속 남으면 superseded lease를 해제하지 않는 초안을 발견했다. 같은 장부 상태의 반복 prepare를 줄이는 최적화에도 매 poll의 취소 검사는 유지하도록 계약을 구체화했다. D에게 실제 held-alias Busy 취소/동일 lease 재개·canonical stage_input 재투영 검사와 수정을 맡겼으며 성공으로 선기록하지 않는다.

report v2의 오류 경로는 compact identity/dimensions만 확보한 경우 실제 view/행렬을 null·actual_view_available=false로 명시하기로 했다. 성공 outcome의 actual view와 혼합하거나 source camera로 복원하지 않는다. 후속 host 리뷰는 창 종료의 최신 present 관측을 이전 pending ACK Applied가 덮어쓸 수 있는 순서도 발견해 same-session present_sequence 단조성을 계약·검사에 추가했다. 아직 실제 통합 통과는 아니다.

bootstrap 리뷰에서는 후보 stage의 gate가 initial DeviceReady뿐이어서 첫 실제 initial frame보다 빠른 candidate BeginStage가 renderer의 active+1 검사에서 실패할 수 있는 초안도 발견했다. 후보 CPU 준비는 병렬로 허용하되 window의 실제 active 관측 뒤 StageInput을 보내고 first frame을 우선하도록 계약·Luna 작업에 추가했다. CLI control pump의 무조건 join도 finished 관측/유한 polling 뒤 join으로 바꾸도록 요청했다. source 리뷰에서 찾은 수정 사항이며 실행 증거는 후속이다.

## 공개 host API의 집중 검사와 다음 통합 경계

Luna D는 같은 service의 Manual/Realtime clock·외부 SDK에서 이름 붙일 수 있는 public accessor type·실제 worker의 Busy/취소/회수와 Drop을 구현했다. 아래는 root가 원본 footer/hash와 실행된 test 이름을 대조한 focused 결과다. 기본과 all-feature의 중복 사례를 합산하지 않는다.

| raw log | 결과 | SHA-256 |
| --- | --- | --- |
| `p03-host-test-allfeatures-luna-attempt12.log` | unit10+외부 public API3=13통과 | `80fd5ce7b0c1c49ef5588d94fad406c83ab892596eaed0c1c2e8b2e6c2d44bf4` |
| `p03-host-test-no-default-luna-attempt07.log` | unit7+외부 public API3=10통과. 당시 window-off dead-code 경고는 남음 | `19a2205bfff8148b708d29255d2c326693b38130b0b6385dbacac5d52fa4605c` |
| `p03-host-clippy-allfeatures-luna-attempt04.log` | all-target/all-feature strict Clippy exit0 | `3503d8e6e446da4e95b7a1b0ec882b94923a302cb0362724a7d14e3f8cd70674` |
| `p03-host-test-default-luna-attempt01.log` | C의 manifest 연결 뒤 같은 기본10통과 | `2428ae77cd3d47bb130cb109ac569d27e6d05a19465d12c2ec8b5e4f693beec7` |
| `p03-host-example-check-luna-attempt01.log` | public SDK `native_host_smoke` example compile exit0. 창 실행은 아님 | `d4e86f9827d14ef7c13852e7ede04762a4124b22599c532d8556b7459d12f2c7` |
| `p03-host-clippy-default-luna-attempt01.log` | exit101. window-off 미사용 field/helper와 조건문 진단 보존, 수정 후 재검사 대기 | `0c0c70330d42abdd8f25c75dd9b79817ec77a2e626ae18c2bd1751810a15d6bd` |

실제 worker 시험은 prepared result의 관측 ACK를 보류해도 반환 owner를 먼저 drain하고, ACK 후 종료하는 순서를 확인했다. 여기의 owner는 NoDevice variant이며 실제 device drain은 앞선 renderer GPU 시험과 구분한다. owner poll 직후 publication 경합·facade Drop 뒤 failure slot·실제 CPU prepare 실패 뒤 worker 종료도 포함한다. held-alias Busy 상태에서는 취소 시 같은 lease를 해제하고, alias 회수 뒤에는 같은 candidate로 재개하는 각각의 시험이 통과했다. bootstrap stage gate와 오래된 ACK의 last-present 덮어쓰기 방지는 작은 상태 정책 시험이며 실제 창에서 빠른 candidate와 첫 frame을 경쟁시킨 근거는 아직 아니다.

앞선 host 실패 시도도 보존한다. fixture 파일명 `project.json`을 실제 규격 `daedalus.project.json`으로 고쳤고, CPU prepare 실패의 실제 host 진단 code에 기대를 맞췄다. Busy fixture는 Stage 전에 lease가 있다고 가정한 오류를 실제 Prepare→Stage→lease로 수정했다. no-default attempt06의 초기 lease 일회 조회는 실제 transient Busy를 허용하지 않아 실패했으며 bounded retry로 고친07이 위 성공이다. 실패 fixture를 production 한도 완화로 통과시키지 않았다.

C가 Cargo/manifests/lock 단독 lease를 이어받아 CLI optional host 연결과 공개 SDK example을 작성한다. D의 default lint 정리는 owned source에만 제한하고 C가 동결 뒤 재검사한다. `cargo tree`에서 no-default host에도 renderer를 통한 wgpu dependency가 남음을 확인했다. 기본 API의 비장치 약속은 runtime device/window/audio 초기화 없음이며 dependency graph에서 wgpu 제거가 아니다.

AI가 final report 전에 실제 표시를 관측하도록 기존 protocol2 attachment에 직접 `view_status` read를 추가하는 계약도 동결했다. 별도 actor/ticket/상태 복제 없이 같은 host control의 compact status를 읽는다. ingress queue 관측과 실제 present, 완전한 성공 view와 compact 오류 metadata를 분리한다. `--view --asset-activation` 조합은 기존 smoke fixture/JSONL을 재사용해 같은 세션의 source 변경→실제 새 present→old charge 회수를 검증할 예정이다. 아직 실행 결과가 아니며 별도 batch PNG를 실제 창의 변경 픽셀 증거로 쓰지 않는다.

실제 desktop 픽셀 검사를 위해 local computer-use skill(`/Users/danny/.agents/skills/computer-use/SKILL.md`)을 읽었다. 두 환경 override가 없어 선택한 `orca skills get computer-use` 실행은 exit127, `zsh:1: command not found: orca`로 실패했다. 지침의 “If the selected executable cannot run, report its exact error and stop”에 따라 이 경로를 중단했다. tool 응답에 오류가 남아 있으며 별도 raw 실행 로그를 만들었다고 하지 않는다. 다른 executable로 우회하지 않았고 native 창 픽셀 검증은 열린 상태다. SDK 실제 창 실행·CLI 연결·전체 workspace/managed·CI/package와 P03/D1/D2/D3는 계속 미완료다.

문서 검사 첫 시도 `p03-host-public-api-docs-root-attempt01.log`는 이 local skill 절대 경로를 repository link로 쓴 탓에 실패했다(SHA-256 `99db214e5759221db02f7a2400d7f3754d7b826b88d5aa0da4a83fc781f77b0a`). 재현 환경 설명의 코드 경로로 바꾸고 후속 검사를 별도 실행한다.

수정 뒤 `p03-host-public-api-docs-root-attempt02.log`는 exit0·SHA-256 `135632fd8e4ade9df7ebcebcc8c32895de7c966e841dd57a022d29393805bcba`다. 문서 검사는 source compile/제품 인수 근거와 구분한다.

실제 SDK 실행 전 root source 리뷰는 두 추가 오류를 발견했다. window 성공 후 poisoned host status를 session1의 가짜 Failed status로 바꾸던 경로는 owner 반환 뒤 typed status_poisoned 오류로 고치도록 요청했다. 또 managed 창은 같은 packet을 매 redraw마다 새 present sequence로 보고하지만 host가 첫 ACK 뒤 pending publication을 지워 후속 정상 재표시를 unknown으로 처리할 수 있었다. 마지막 신뢰 token/key 하나를 작은 metadata로 보존하고 정확한 identity를 검증하는 계약을 추가했다. 큰 frame/source pin·새 publication·재전송으로 우회하지 않는다. 실제 SDK 실행은 수정/집중 검사 뒤로 미루고 Manual publication을500ms 더 관측하도록 검증 계획을 바꿨다. 이 발견은 source 근거이며 아직 실행 실패/수정 성공으로 보고하지 않는다.

A의 CLI/protocol/Python source가 동결된 뒤 D의 host 수정 동안 C의 비어 있는 검증 lease를 잠시 넘겨받아 root가 Python 전체 discovery를 실행했다. `p03-host-python-root-attempt01.log`는72통과/0실패·unittest11.948초/runner12.118초·SHA-256 `d0d892e53a8dec4b91065f58b48f8b9debbdc504578629837d3b343ae9d979b4`다. 실제 CLI/GPU가 아닌 stub process·schema·기존 package/docs 검사를 포함한다. Cargo/hardware와 겹치지 않았고 종료 후 C에게 lease를 반환했다. 이 실행을 native source 교체 통과로 세지 않는다.

같은 대기 구간에 C가 의존 없는 기존 bridge/managed 회귀를 실행했고 root가 원본을 대조했다. locked bridge build, pinned .NET10.0.402 Release build(경고0/오류0), 실제 Rust-backed self-test, native round-trip/module lifecycle 두 integration scenario가 모두 통과했다. 순서대로 `p03-host-managed-{bridge,release,selftest,integration}-luna-attempt01.log`의 SHA-256은 `a99a9156df136850d9d7fe6899965e5649370108c659a6f277c44fa8975feca6`/`586056b53e62fd953ecbe48d39613bcc9ed8c2b350d74b7d668ece6233f93850`/`c3aa500c113c9f3b28631633b19f4c470bfb2c95236c69880a0b96aedd64c9b8`/`9cf2f020c293a928130e8c2e0f716925477246ef50f6ac64f1b0381a1f63ab4f`다. managed/bridge/runtime source를 바꾸지 않았으며 새 native host의 C# attach 인수는 아니다. 관련 source 변경이 없으면 이 회귀를 다시 실행하지 않는다.

추가 host 리뷰는 close 요청을 실제 ViewClosed로 선보고하던 경로와 initial GPU Ready가 ViewRunning을 덮어쓰는 상태 보고 오류도 수정 대상으로 찾았다. 요청 intent와 실제 channel Closed/run_view 반환을 분리하고, 첫 실제 표시 뒤 실행 상태를 보존하도록 Luna에 배정했다. 또 ResultMailbox는 delivered flag를 slot 잠금 전에만 확인해, 그 사이 consumer가 기존 payload를 take한 뒤 producer가 두 번째 payload를 넣을 수 있었다. 같은 nonwaiting slot guard 안에서 delivered를 재확인해 관측 ACK 전 용량1 예약을 지키도록 요청했다. can_publish/fully-acknowledged도 같은 stale precheck를 점검한다. 이 항목들은 source 감사 결과이며 후속 결정적/통합 검사 전 성공으로 기록하지 않는다.

CLI adapter의 오류 변환도 추가 검토했다. window outcome Ok·actor StopRequested만으로 exit0을 반환하면 host coordinator/CPU failure를 report에 기록하고도 성공할 수 있었다. 최종 report 보존 뒤 실제 host code/message로 비영 종료하도록 계약을 명시하고 A의 해당 파일만 잠시 재개방했다. C가 CLI를 컴파일하기 전에 다시 동결한다. read-only live-capture 감사는 현재 managed window의 실제 metadata/SurfaceTexture를 재사용하되 새 readback·image credit·artifact 수명이 필요함을 확인했다. 기존 capture 문서를 현재 host 소유권에 맞췄고 live_capture=false/픽셀 미검증 상태는 유지한다.


## 공개 native host의 집중 인수와 실제 창 실행

Luna source를 동결한 뒤 root가 raw completion/footer와 SHA-256을 대조했다. 기본 feature의 host는4 unit+3 외부 public API=7통과, window 포함은14 unit+3 public API=17통과다. feature별 수를 합산하지 않는다. GPU owner 반환·후보 Busy 시나리오는 window 전용으로 제한했으므로 앞선 기본10통과 기록과 시험 설정이 다르다. 양쪽 all-target strict Clippy도 통과했다. 마지막 두 identity 비교 helper의 인수 개수 lint에는 field를 명시적으로 검증하는 이유를 붙인 함수 범위 허용만 적용했으며 비교 로직은 유지했다.

| 집중 검사 | raw log (`dist/project-evidence/`) | SHA-256 |
| --- | --- | --- |
| host 기본7통과 | `p03-host-test-default-luna-attempt02.log` | `29f1cc360795419bf1288a4957d76704e935ca52b41eb44f66a0f906ffbdd139` |
| host window17통과 | `p03-host-test-window-luna-attempt03.log` | `1bceaeeb4a9c5ae868a27c85cda4511d290f375db86b2edb25e1157e31758d3b` |
| host 기본 strict | `p03-host-clippy-default-luna-attempt04.log` | `204c89b4e8348944af0c4d69809f858b9d9c3302bb508f8bbedaba7431185338` |
| host window strict | `p03-host-clippy-window-luna-attempt03.log` | `3adc3f8f782e6f71677243daba34f0e24d7da7e334af9628dddd1161937963ae` |
| CLI 기본35통과 | `p03-host-cli-default-test-luna-attempt01.log` | `d441725fa08a5b4562f2f0537f186174c69ce0b52df179a58bcf2ffec2264368` |
| CLI 기본 strict | `p03-host-cli-default-clippy-luna-attempt01.log` | `a3300dac6c99808e9eb76358eaca0a314dfae2a0e3c278a02b71e8af009399b7` |

실패 시도도 보존했다. 기본 strict02는 필수 초기 collect_bindings까지 cfg로 숨긴 오류·조건부 필드 접근·unused import,03은 수명 유지 lease와 window 전용 owner signal의 dead-code를 드러냈다. raw `p03-host-clippy-default-luna-attempt02.log`/`03.log`의 hash는 `c19b38d82bc27388b4d11caad7a418321ab05541fe6e660eb9135c0511e9553f`/`ec099ff76bba5e334c130d983e38cfcddd9beaa8bc577209e9096cf3be8c1fe2`다. window test02는 ManagedPresentedView/Summary 타입 연결과 private test helper 범위 때문에 compile에 실패했다(`67dc2355b4ef3326e2b3ccf25f6e2adb9825a9992e5d9b55d299fc3bde96e080`). window strict02의 두 인수 개수 진단 raw hash는 `4e07db361b4440f8a37f0a555bf05a2a0e4cf26679a4aeab6d253d51b27e435d`다. 해당 source 수정 후 위 성공 실행을 얻었으며 실패를 runtime 실패로 바꾸어 해석하지 않는다.

공개 SDK `native_host_smoke --observe-ms 500`의 첫 실행은 링크 중 `ld: write() failed, errno=28 (No space left on device)`로 종료했다. 실행 당시 Luna가 확인한 여유 공간은145MiB였으며 example 자체는 실행되지 않았다. raw `p03-host-native-sdk-luna-attempt01.log` hash는 `024b4e9a8ff809ec3fdb50f568ac8c698ca7f90d9fcf943287c05428705d0ca6`다. root가 이후12GiB 여유를 확인했고 별도 삭제 없이 새로운 시도만 허가했다. Luna는 compiler 경쟁이 없고 공간이 유지되는 것을 확인한 뒤 단 한 번 새 raw로 실행했다.

`p03-host-native-sdk-luna-attempt02.log`는 exit0·11.301초·SHA-256 `e11383af6cc0be086d9aadb89b2cda6ebb02709eb54e2e526725fab01258d27c`다. 기존 외부 model-runtime-project의 실제 session1/tick0/publication1/authority snapshot1/generation1, source digest `sha256:e6fa43a687b25ce65829dd0e958dd39b11f60137aa47e52d5ef3314cab694d52`, present sequence60과1280×720을 관측했다. 창을 닫은 뒤 같은 Manual authority의 Step이 tick0→1로 진행했고 닫힌 consumer의 Stage는 `asset_activation_unavailable`로 거부했다. 종료 보고는 stopped/StopRequested/final tick1이었다. 실제 창·반복 표시 metadata·동일 authority 생존·종료의 근거다. 실제 창 이미지의 픽셀 정확성·144fps·실행 중 새 세대 교체를 증명하지 않는다. 추가 오디오 실행은 하지 않았다.

후속 CLI all-feature test01은 WireResult에 Debug를 요구한 expect_err와 불필요한 mut 진단으로 compile에 실패했다. raw `p03-host-cli-allfeatures-test-luna-attempt01.log` hash는 `e269711801ff5fab60cc0097652bcdd58731d4379ed15c0123cb777a1ecd18e5`다. Luna가 assertion을 match로 유지하고 mut만 제거해 해당 파일을 다시 동결했으며, 집중 재검증을 이어간다. CLI 실제 source 교체·live capture·전체 workspace/CI/두 package 인수와 P03/D1/D2/D3는 계속 미완료다.


CLI 수정 후 all-feature50/50통과(`p03-host-cli-allfeatures-test-luna-attempt02.log`, SHA-256 `65627f3bc29aa722350c286eb832fc02b95190dc1122eda24b87488458e8d72f`)를 확인했다. 이어진 strict01은 default 생성 뒤 field 재할당 lint로 실패했고 raw hash는 `8eb9e329f384d9ab4f8bc77cf384208cae75adce5b22e096928dc7edec840aad`다. Luna가 같은 값의 struct initializer로 정리한 뒤 all-target/all-feature strict02와 build01이 모두 통과했다. 두 raw `p03-host-cli-allfeatures-{clippy-luna-attempt02,build-luna-attempt01}.log`의 hash는 `ff1b1e23795a540479846e375311960f71b98148f9bd1c6ef3620d4b85853fa6`/`4c53cf7bb5ec4bdef661186ca4665e27d0e0a6f7803bd68520d24b95cf972892`다. 실제 debug CLI hash는 `ac6fcdea7adc9ccc96d3d7b5244fedf7821a432d9e8f63012d4db7e8c330ebcf`이며 새 실제 CLI 교체 smoke/전체 workspace/CI/package 인수는 아직 실행 전이다.

공통 host·manifest·lock·공개 SDK example과 그 집중 근거를 `de92a53`에 별도 commit해 작업 branch에 push했다. CLI adapter/control/Python smoke source는 통합 검증 전 working tree에 남겼다. 엔진 main 인수·전체 P03/D1/D2/D3 완료로 보고하지 않는다. 이 상태 기록은 현재 하위 gate와 제품 목표를 구분해 사용자에게 진행 상황을 설명하기 위한 것이다.
