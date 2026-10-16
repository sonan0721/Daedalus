# P03.3b/c 에셋 세대 전환 설계·작업 기록

상태: P03.3b.1 로컬 선행 검증·b.2 runtime/CLI/process 구현 중. 실제 generation 전환·b/c 통합 인수 전 · 2026-10-16

## 문서 목적

[activation 명세](../03-architecture/project-asset-activation.md)는 CPU 후보를 실제 render/audio 세대로 바꿀 때 state 보존·단계별 성공·합산 예산·old backing 회수 책임을 정한다. 이 기록은 read-only source 확인, 확정한 API와 아직 미결인 제안, Luna 코드, 실제 실패/검증을 구분하기 위해 만든다. 작성만으로 P03.3b/c를 인수하지 않는다.

## 선행 인수와 출발점

[PR14](https://github.com/sonan0721/Daedalus/pull/14)의 head `d766d0dc8abc1bd97b7f7d32ffdc38f86ebd9bff`, CI synthetic merge `ebc93efd8afefa40fec387f79f1602fc8c2f33c7`, 실제 main `275ea360fb53dde3713aca84910781a4c895a8e4`의 tree `8bac52ca92d2df237246a59d554a50a3e318717e`가 같았다. 다섯 CI/두 packaged50·local Rust298/Python61/Metal9/managed·debug/clean package CPU50/native60을 확인해 P03.3a만 인수했다. [준비 작업 기록](2026-10-16-project-asset-preparation.md)이 근거다.

초기 설계/cache branch `feat/project-asset-activation`은 PR15로 인수했다. 후속 구현 branch는 `feat/project-asset-generation`이며 기준 main은 `c2283cb686ff7eb5589d6e38f66d5e357993531e`다. engine repository에 게임을 추가하지 않으며 D1→별도 D2→별도 D3의 전체 목표는 진행 중이다.

## 확인한 구현 gap

ProjectSession 교체는 entity/Engine을 다시 만들어 live state를 잃는다. actor/coordinator는 source와 generation1을 고정한다. renderer의 CPU 준비는 one-shot이고 실제 GPU admission은 redraw에 결합돼 있다. audio는 단일 bank/voice alias index를 쓰며 callback이 Frame Arc를 유지할 수 있다. 이들은 실제 source 확인이고 새 activation API가 있다는 뜻이 아니다.

그래서 payload를 status에 공개하지 않고 generation별 private lease·consumer staging·actor commit·actual present·retirement로 연결하는 방향을 채택했다. semantic topology 비교·public API·cleanup owner·aggregate 수치·post-commit 실패·old Frame 회수 observation은 아직 리뷰 전이며 source를 배정하지 않았다. root는 명세/리뷰·통합을 맡고 코드/테스트/CI는 Luna xhigh가 작성한다.

## 병행하는 개발 파이프라인 개선

PR14 실제 로그의 두 package cache miss와 main package-cache 부재를 확인해 [DEV06/07 범위 개선](../04-plan/development-iteration-pipeline.md)을 별도로 먼저 명세했다. 기존 main의 read-only [manual run38907971641](https://github.com/sonan0721/Daedalus/actions/runs/38907971641)을 초기 공급으로 실행 중이다. 새 기능 test를 생략하거나 package workflow를 복제하는 변경이 아니다.

Luna에게는 `.github/workflows/release.yml` 한 파일의 main push trigger 추가만 배정했다. 기존 tag-only publish·권한·action pins·cache paths/keys·package/smoke를 유지한다. YAML syntax/기존 package unit과 실제 main cache 생성·후속 PR restore는 각각 실행 후 기록한다. 현재 통과를 미리 표시하지 않는다.

이후 실제 두 main package/staged50·Publish skipped와 main cache 생성을 확인했다. workflow 두 줄·YAML syntax·기존 package14도 통과했다. [DEV06/07 기록](../04-plan/development-iteration-pipeline.md)에 source/API/log 한계를 남겼다. 자동 main trigger와 후속 PR restore 검증은 아직 인수하지 않았다. activation runtime source에는 변경이 없다.

## 다음 순서

명세의 동결 목록을 source/API 리뷰로 마친 뒤 public signature·소유 파일·한 Cargo owner를 공유한다. 그 전에는 runtime/renderer/audio dependent source를 동시에 작성하지 않는다. 구현 이후 focused CPU→통합/managed→실제 GPU/PCM/native→clean package/CI/main 순서로 인수한다.

## 첫 worker/CPU lease 리뷰

Luna의 read-only 리뷰는 source상 세 가지 선행 gap을 확인했다. runtime worker는 renderer에 의존하지 않아 renderer CPU prepare를 직접 맡길 수 없고, renderer 쪽 staging owner가 필요하다. service/port/CLI의 초기 project Arc가 남아 actor 교체만으로 첫 세대를 회수하지 못한다. worker의 active cooked bytes도 현재 startup 값으로 고정돼 commit/retire의 generation-wide 장부 갱신이 필요하다.

move-only opaque project lease·single handoff slot·off-actor retirement owner를 후보로 제안받았다. candidate의 폐기 요청과 실제 backing release/charge 반환을 구분하고 retirement slot 포화의 pre-commit 처리를 동결해야 한다. 제안한 source getter·crate-private take API는 cross-crate visibility/Clone 노출 문제를 검토한 뒤 확정한다. prepared CPU128MiB/16MiB·audio aggregate256MiB/4MiB도 수치 제안이며 실행 검증/채택 완료가 아니다. actor/consumer handshake와 audio backing 리뷰는 진행 중이다.

추가 renderer source 리뷰는 asset-only commit이 같은 Engine snapshot을 사용할 때의 순서 충돌을 확인했다. 현재 extract는 snapshot.sequence를 packet sequence로 복사하고 renderer transition은 다음 packet의 sequence가 엄격히 증가해야 한다. tick/transform을 유지한 채 asset generation만 바꾸려면 authority snapshot identity와 별도 presentation 순번을 명시하거나 동등한 엄격한 ordering 규칙을 설계해야 한다. generation 검사만 풀어 같은 snapshot을 다시 보내는 수정은 허용하지 않는다.

PR15의 실제 cache restore·compile 구간·기존 packaged50·다섯 CI/main 동일 tree까지 확인했다. [개발 반복 기록](../04-plan/development-iteration-pipeline.md)에 exact identity와 raw log를 둔다. 새 main push 실행은 진행 중이며 activation code가 추가된 것으로 계산하지 않는다.

후속 자동 main push run38909669190도 두 실제 package/staged50·cache restore·Publish skipped를 확인했다. 이 좁은 개발 pipeline 개선은 인수했으며 새 activation source의 근거와 분리한다.

## P03.3b.1 공통 비교기부터 구현

소비자마다 중복 비교를 만들지 않도록 [호환성 상세 계약](../03-architecture/asset-replacement-compatibility.md)을 먼저 동결했다. manifest/asset metadata·entity/component 관계와 authored data는 보존하고, source payload/URI/hash·scene revision/배열 순서의 허용 범위를 명시했다. 큰 JSON을 복사하지 않는4MiB logical reference-index 상한과 deterministic bounded 오류를 둔다. 아직 구현 통과로 체크하지 않는다.

Luna project 담당에게 content 새 module·lib re-export·필요한 기존 project test 최소 변경만 배정했다. 단독 Cargo/shared-target lease와 고유 `p03-compatibility-*-luna-attemptNN.log`를 사용해 focused→content 전체→strict Clippy→content Wasm/fmt를 검사한다. 다른 Luna 두 명은 additive consuming native owner/retirement, 실제 audio Frame backing 회수 seam을 각각 read-only로 조사한다. renderer/runtime/audio의 dependent source는 API를 동결하기 전 수정하지 않는다.

focused attempt01은8개 중6개 통과·2개 실패였다. 하나는 기존 loader가 parent/hierarchy를 거부해 valid PreparedProject fixture를 만들 수 없는 사례였고 허위 입력으로 비교기 지원을 증명하지 않는다. 다른 하나는 entity mismatch를 기대하면서 asset set도 다른 fixture여서 deterministic 우선순위상 AssetSet이 올바르게 먼저 반환된 test 구성 오류였다. 작업자는 같은 asset set의 valid 입력으로 수정 중이며 최초 raw를 보존했다. 아직 최종 통과가 아니다.

후속 focused10/content84·strict content Clippy·content Wasm/scoped fmt는 통과했다. 조정자 review는 동시에 살아 있는 asset/entity/component index 합산·entity마다 반환·checked overflow와 실제 legacy mesh 변경의 source identity를 보강하도록 요청했다. 마지막 review에서 immutability test가 실제 비교한 changed 대신 무관한 candidate의 불변성을 assert하는 빈틈을 발견해 해당 test만 수정/집중 재검증하도록 배정했다. production 비교기 오류로 보고하지 않으며 최종 source hash/로컬 선행 인수는 수정 뒤 기록한다.

수정 후 실제 입력 active/changed의 source digest/files 불변 assertion을 조정자가 확인했다. final focused attempt05는10/10·0.01초로 통과했고 scoped fmt attempt03/diff도 확인했다. module SHA-256 `2398f756a9ee09295d9aaf3310df8e6110d9f74b500c0f048792453882ab14ab`, lib re-export `62137e4082e610b73cf71f105a78ad9ba5568128ea37bdd1d87003e211097662`다. 전체 content84/5.76초·strict Clippy·Wasm은 test-only 마지막 assertion 수정 전 attempt02이며 production 함수는 변경하지 않았다. source review와 이 로컬 선행 근거로 b.1을 다음 runtime 구현의 입력으로 사용한다. PR/main·실제 activation·b/c 전체 인수는 계속 열려 있다.

raw는 `p03-compatibility-focused-luna-attempt01..05.log`, `...content-luna-attempt01..02.log`, `...clippy-luna-attempt01..02.log`, `...wasm-luna-attempt01..02.log`, `...fmt-luna-attempt01..03.log`다. 최초6/8을 포함한 실패를 보존했다. unsupported hierarchy/scene schema branch는 valid loader test로 증명하지 않았고 실제 같은 engine의 activation은 이 단계에서 실행하지 않았다.

## P03.3b.2 다음 소유권 계약

[headless CPU 세대 계약](../03-architecture/headless-asset-generation.md)을 작성했다. 공통 Stage/Commit/Cancel/Status·consuming owner·worker reclamation Arc·같은 snapshot의 새 frame sequence를 명시했다. renderer/audio를 지원한 것으로 우회하지 않고 CPU identity/수명을 먼저 검사하는 b의 내부 단계다. b.1 검증/source 리뷰와 다음 계약의 구현 가능성 확인을 마친 뒤 runtime 코드를 배정한다.

runtime read-only 재검토는 동일 worker/cache·기존 public API 보존과 새 headless factory가 구현 가능함을 확인했다. 같은 후보의 두 번째 다른 Stage는 effect-free Busy, 같은 CommandId는 공통 replay로 고정했다. operation/result/outcome/status/getter·오류 unit variant·inspect/mutate 권한·단조 frame/authority snapshot sequence·CLI decimal wire도 먼저 공유했다. CLI는 기존 네 capability 조합의 base7 반복을 bounded extension composer로 줄이는 좁은 개선을 함께 구현할 계획이다. source 구현/실제 process 통과는 아직 아니다.

b.1 source와 다음 public API/CLI 계약은 `ddcec71`로 branch에 commit/push했다. main에 새 activation이 들어간 것으로 보고하지 않는다. b.1 로컬 선행 검증 뒤 Luna runtime 담당에게 worker/actor/source swap/lib 전체를 함께 맡겨 private handoff를 서로 추측하며 병렬 구현하지 않도록 했다. 별도 CLI 담당은 고정 public API를 사용하는 CLI Rust/DTO/tests만, process 담당은 기존 Python helper/tests만 수정한다. 파일 소유권은 disjoint이고 runtime 담당 한 명만 shared-target Cargo를 사용한다. 나머지는 source-only이며 Python/process 검사는 다음 별도 lease 뒤 실행한다. 조정자는 MD·API/코드 review·통합/실제 artifact/Git을 맡는다.

병행 root renderer read-only 감사는 stage-only의 추가 선행 조건을 찾았다. 현재 resource planner는 같은 제출에서 active를 바꾸므로 geometry `no_active`/새 texture requested set으로 old completed 자원을 victim으로 삼을 수 있다. stage-only에서 active+staged pin/회수를 바꾸지 않으면 last-good을 보존하지 못한다. 전체 upload/draw가 한 submission인 현재 구조도 pump별 진행/예산으로 나누어야 한다. source는 `packet_renderer.rs::plan_resources/submit_packet_frame/commit_resources`, `window.rs::redraw`이며 renderer source를 변경하거나 장치 검사를 실행하지 않았다. 후속 render 상세 동결 때 사용할 gap으로 기록했다.

## b.2 source 분담과 첫 검증 도구 리뷰

공개 API를 먼저 고정한 뒤 CLI는 activation 전용 protocol을 새로 만들지 않고 기존 protocol2 stdio loop를 공유하도록 소스를 작성했다. composer는 기존 base7·audio5·preparation3·activation4 이름을 조합한다. runtime·실제 CLI 연결은 아직 검증 전이다. 구현 body를 기다리느라 adapter source까지 멈추지 않도록, 동결한 factory/getter/outcome을 기준으로 source-only 작업을 진행하고 실제 compile은 provider와 통합 후 수행한다.

runtime source 작업 중 Cargo를 잠깐 명시적으로 보류하고 Python 담당에게 한정 lease를 주었다. fake subprocess/transport/ACK/deadline 검사28개가6.894초에 통과했다. raw `p03-generation-python-luna-attempt01.log` SHA-256은 `66f4cb35716fe1dd91adaea80c386d8f61b314faacf954ef6d591c360e5149c8`다. 검사 뒤 Cargo를 runtime 담당에게 돌려줬다. 실제 engine/CLI 세대 전환은 실행하지 않았으며 stub의 고정 응답을 그 근거로 사용하지 않는다. source-only와 짧은 Python 검사를 겹친 실행 순서의 이전 대비 절감 시간은 측정하지 않았다.

root 후속 source 리뷰는 실제 multi-commit helper의 명령 ID 오류를 찾았다. 첫 Prepare930001→Stage940001→Commit940002 다음에 Prepare930002를 쓰므로 공통 caller high-water보다 작아진다. 실제 CLI 실패를 실행한 것은 아니며 소스와 기존 ledger 계약의 충돌이다. 기능별 번호대 대신 세션 하나의 증가 issuer를 initial transform부터 사용하도록 수정했다. 새 명령은930001~930018 순서이며 replay·stale-generation 오류 뒤 같은 ID 재시도만 의도적으로 재사용한다. owner 종료도 exit0만 보지 않고 실제 `asset_activation_owner=stopped` stderr를 요구한다. 수정 source SHA-256은 smoke `f82180635164046e58a4ee332fd089fbe028b4ecbf4036f9f714d492c8b27edd`, tests `87bd9e399ba29f4817785bac0fac8b83e0a7f3020fab193df8e5bdb582baaa78`다. diff check만 통과했고 이 수정 뒤 Python/실제 CLI는 다음 lease 대기다.

worker 구현에는 commit metadata→pending promotion과 다음 Prepare의 순서도 review 대상으로 공유했다. committed 후보를 stale ready처럼 지우지 않도록 promotion을 먼저 처리하고, handed-off/cancelled candidate의 실제 final backing release 전 quota를 돌려주지 않는다. 이 순서의 테스트 통과는 아직 기록하지 않는다.

CLI adapter source는 고정한 public API 기준으로 먼저 완성해 scoped rustfmt/diff를 통과했다. 기존 stdio pump·capability composer를 공유하고 실제 service를 쓰는 stage→commit→status/frame test를 추가했지만, runtime actor integration/Cargo 검증은 아직 대기다. root는 worker Stop에서 candidate reclamation Arc를 먼저 비우는 경로도 발견해 수정/지연 reader 테스트를 요청했다. 종료 요청이나 actor 종료만으로 worker final drop을 증명하지 않는다.

CLI 담당의 read-only 교차 검토는 helper wire/tags·ACK/retry·단일 issuer와 controller 사이 충돌을 찾지 않았다. 다만 staged/committed summary 자체의 source digest 일치 검사가 빠져 두 assertion을 보강했다. 최종 smoke source SHA-256은 `53c5a3c86d77551eaf764c14626ad62cb64f5e96e926957c53969e534d3b0256`이며 Python 재검증 전이다. renderer 후속 리뷰는 runtime-neutral lease의 신뢰 경계·stage-only completion ledger 재사용·현재 부재한 live presentation ACK를 위 명세에 반영했다. token/callback 새 API는 제안이며 source로 배정하지 않았다.

S86은 최신 웹 문서를 pinned API의 증거로 대신하지 않고 로컬 Cargo registry의 wgpu27.0.1 원본도 읽었다. `src/api/queue.rs` SHA-256은 `a7f918a08db2b084343a6973eae999e5b92ae862f1d0e4515d16fa78179ed993`, `device.rs`는 `02d3a6be80612a669ace9a27d9f90691547989e8b41c69ed903ff25484dcf8f0`다. callback의 thread/비용·write staging·Wait/WebGPU poll 차이를 확인했으며 렌더러 source나 dependency를 변경하지 않았다.

## b.2 첫 compile과 수정본 도구 검사

`cargo check -p daedalus-runtime --all-features --locked` attempt01은 cfg expression 배치·staged borrow를 replay-slot mutable 예약까지 유지한 문제로3개 error와4개 warning을 보고했다. cfg block과 borrow 수명·미사용 항목을 수정한 attempt02는2.27초에 통과했다. 이는 library compile이며 새 activation의 동작 테스트가 아니다. raw는 `p03-generation-focused-luna-attempt01..02.log`이고 attempt02 SHA-256은 `ef59d1f36c49037047f0abd7235e72e45095eaceff8fdfaf3c4ca4062d1a68dd`다.

검증 소유권을 또 넘기는 대신 같은 Luna Cargo 담당자가 완료된 명령 사이에 frozen Python suite를 순차 실행했다. 수정본28/28이7.172초에 통과했다. raw `p03-generation-python-luna-attempt02.log` SHA-256은 `f3eff65ccbc2b8c4cb65173dbd4541f31adf9fb463eaeae62e590cb13bec069b`이며 smoke/tests source는 위 최종 hash와 같았다. 이 suite는 transport/stub 회귀다. 실제 headless actor의 첫 Stage/Commit/replay/frame test와 실제 CLI 실행은 아직 대기다. 취소·stale·retained reader/한도·종료/panic을 포함한 b.2 인수로 확대 해석하지 않는다.

첫 실제 headless test는 all-feature focused attempt03의1/1·0.03초로 통과했다. scene authored revision37→38 준비를 worker gate로 지연하고 Stage의 효과 전 Busy 뒤 같은 caller ID 재시도·실제 Staged/Commit·entity handle/position9/revision1 보존·같은 tick/authority snapshot sequence의 새 generation2/frame·cross-kind world 요청의 activation receipt replay를 검사한다. 네 포맷 payload/실제 renderer/audio 전환 테스트는 아니다. raw `p03-generation-focused-luna-attempt03.log` SHA-256은 `4f413ef6f5496002d5aba73f8f9d7f0861de7b2f28796feebd19779cbc176db1`이다.

그 source의 runtime 전체 all-feature attempt04도 unit90/0.77초·integration2(0.00/0.02초)를 통과했다. raw `p03-generation-allfeatures-luna-attempt04.log` SHA-256은 `d043a8b9cba1459b33d8b98a5ce249a857fabd4b846c40eeda2c9c405038ef47`이다. test import warning1개가 남았고 strict Clippy/default/실제 CLI는 아직 실행 전이다. 후속 review는 취소를 가로지른 stale failure·state/summary 일치와 worker panic join 후 Stopped ledger가0이 되지 않는 경로를 발견했다. 해당 수정과 취소/retirement/panic stress 후 최종 검증이 남아 있으므로 이 중간90/2를 최종 b.2 인수로 쓰지 않는다.

실제 CLI adapter focused attempt01은 strict decimal/unknown field·service-backed stage/commit/frame2개가0.02초에 통과했다(compile11.90초). debug all-feature build는4.00초였고 binary SHA-256은 `84d9b50ae451863a9a6847170b57ff81c1abdf0a598df2f514f78459b7f7bfc0`이다. 이때 test-only unused-mut warning이 있었고 현재 사용을 확인한 뒤 해당 mut만 제거했다. tests source SHA-256은 `18ca8feb051d0ecfb73b5e4febe436143684a91752e30fd6e350f54a42ab39a1`이며 그 수정 뒤 집중 재검증은 대기다.

해당 binary의 실제 `smoke_project.py --asset-activation` attempt01은 initial ProjectInfo에 없는 asset_count를 helper가 요구해 실패했다. 실제 응답의 entity1·예상 source9·digest는 정상이고 ProjectInfo 계약에는 asset_count가 없다. engine field를 추가하지 않고 잘못된 helper assertion만 제거했다. 실제 ready-candidate summary의 기존 asset_count6 검사를 유지한다. 수정 smoke SHA-256은 `c9ceb2b6d0d74bc296eb2c440c1c18e1d47fb596d6ded7466172f28837c0658e`다. Python assertion-only 변경이라 같은 binary로 실제 process를 재검증하며 Cargo rebuild/도구28 전체를 이 수정만으로 반복하지 않는다.

raw는 `p03-generation-cli-{activation,build,smoke}-luna-attempt01.log`다. focused log SHA-256은 `e9bd4ee78038ca89a66c26b0c20f70665628386498914a3cf9000fe0186b89e1`, 실패 smoke는 `df56b6aae5d6431cca3b35386ec6b3e14b0388c535a4dde6e4a1a5722fc664cd`이며 최초 실패를 보존한다. 아직 네 포맷 연속 commit이 통과한 것으로 기록하지 않는다.

같은 binary와 수정 helper의 actual smoke attempt02는28/28(CPU18+activation10)·1.005974초에 통과했다. source PNG→model node→atlas→WAV를 같은 session1에서 순서대로 commit해 generation2/3/4/5와 frame3/4/5/6을 관찰했다. authority tick1·기존 handle/transform/revision·authority snapshot sequence를 보존하고 replay·stale expected-generation 같은 ID 재시도·실제 Staged 취소·entity-set incompatibility의 last-good·owner Stopped도 검사했다. 최종 active cooked21038 + 실패 candidate21056 = 논리적42094bytes, retired0이었다. 이 CPU 결과를 GPU 픽셀/실제 audio 전환으로 보고하지 않는다.

raw `p03-generation-cli-smoke-luna-attempt02.log` SHA-256은 `4b6c9ed0bc3022c1459ff1711b32dd805f6790639c47452282ef47a350fe4ad9`이며 조정자가 JSON·binary identity·증가 caller ID930001~930018과 실제 commit/status를 독립 확인했다. Python-only 수정 뒤 동일 binary를 사용해 불필요한 rebuild를 피한 실제 사례다. 시간 개선 배율은 측정하지 않았다. 취소 경계의 지연 결과·held reader/retirement 포화·panic ledger와 반복 shutdown의 최종 regression을 다음으로 남겨 b.2/전체 b/c를 아직 체크하지 않는다.

## 중간 source checkpoint

이후 focused test에 worker shared-state 잠금으로 Commit Busy를 유발했다. Busy 뒤 과거에 사용하지 않은 caller88009가 기존 high-water88012와 함께 ReplayWindowExpired로 거부되고 같은 Commit88013 재시도는 성공한다. 이는 거부된 ID의 재사용뿐 아니라 기존 watermark를 잃지 않는지도 검사한다. all-feature focused attempt07은1/1·0.02초, SHA-256 `33bef6e4311f1ce48b25b908de8fe89815fcad187db682d19232a96309e3a2b3`다. legacy preparation worker-panic test도 반복 WorkerPanicked와 join 뒤 byte/count0·cache reopen assertion을 보강해 attempt06의1/1·0.01초로 통과했다. 해당 raw SHA-256은 `fb6b7776a541b33fc1f997fe29fd3c8f68e206de52fa453a01c953eaba711931`이다. consuming activation owner의 panic/held reader 검증까지 대신하지 않는다.

default-feature focused attempt01은 gate release/wait helper가 audio feature에 잘못 묶여2개 compile error를 보고했다. test-only cfg로 바로잡은 attempt02는1/1·0.02초로 통과했다(compile3.56초). raw `p03-generation-default-luna-attempt01..02.log`를 보존했고 pass log SHA-256은 `57a7c563008f12cbba38f05992bdff4eb0155fd2a2238f5ee95f89328450c7cf`다. 전체 default suite·post-edit all-feature 전체·strict Clippy는 아직 대기다.

작업자는 runtime 다섯 파일의 scoped rustfmt/diff 뒤 source를 잠시 동결했다. 이 동작 가능한 일반 경로와 adapter/helper·검증 기록을 중간 feature commit으로 남기고 lifecycle/stress를 이어 간다. b.2 체크/PR/main 인수나 전체 목표 완료를 뜻하지 않는다. renderer/audio 세대 전환·SDK/MCP/제품 인수는 그대로 남는다.

중간 feature source는 `66f190c`로 commit/push했고 clean tree를 확인한 뒤 runtime 담당에게 edit/Cargo lease를 돌려줬다. 별도 renderer GPU 상세 조사 두 명은 read-only로 진행했다. [b.3 상세 초안](../03-architecture/staged-render-asset-generation.md)은 whole-record/texture-row upload 단위·old-ready/staged pin·source token과 별도 CPU 복사본·final window outcome을 유지한 join의 순환 대기 위험을 정리한다. 공통 planner/ledger를 확장하고 새 renderer를 만들지 않기 위한 문서다. API/수치와 b.2 predecessor 인수 전이라 source를 배정하지 않는다. S87 공식 자료 확인으로 auto-trait-only Arc에 공유 marker trait가 문법상 필수라는 리뷰 가정도 바로잡았다. pinned1.94 구현 검증은 후속이다.

## b.2 lifecycle 집중 검증 계속

실제 worker가 current 확인과 StageResult 계산을 마친 뒤 send 직전에 test-only gate로 멈추게 했다. Cancel을 가로지른 Ready가 다시 staged되지 않고, 새 Prepare를 가로지른 incompatible 오류가 최신 failure를 덮지 않으며 epoch2가 실제 CpuPrepared/Staged에 도달한다. attempt01 delayed2/2·0.03초, raw `p03-generation-focused-lifecycle-luna-attempt01.log` SHA-256 `6a8043bdc4281cd941af1d50b277b0b08833d54488d932b8e56744b7d19890b0`다. 고정 Staged 응답이나 가짜 worker로 대체하지 않았다.

Commit source/counter 효과 뒤 result cache 전에 actor panic을 넣은 실제 service test는 ticket의 해당 caller OutcomeUnknown·worker cleanup 완료·반복 ActorFailed를 확인했다. attempt02의1/1·0.01초, raw SHA-256 `679b6edc9e1e5f9033402b3a9044076df6318e06469e8013af488f35bf01ac52`다. applied 효과를 unapplied로 보고하거나 rollback하지 않는다.

낮춘 catalog ceiling을 initial cooked bytes의 정확한2배로 둔 test는 pending promotion 동안 Prepare Busy→promotion 뒤 held old source의 retired charge 때문에 Capacity→reader Arc release/worker 실제 회수 뒤 같은 Prepare ID의 epoch2 성공을 확인했다. attempt03은 이 test의 borrowed Result assertion 두 곳 타입 불일치로 compile 실패했다. assertion 형식을 수정한 attempt04는1/1·0.04초로 통과했고 raw SHA-256 `8a669236b893b90893cc5faf2399c6d902df289915d7ece0611ca7c58dfb6562`다. 모든 attempt raw를 보존하며 전체 post-edit/default/CI 통과로 확대하지 않는다.

activation consuming owner 자체의 worker-panic test는 attempt05에서 종료한 actor에 Status를 다시 제출하려다 Stopped로 실패했다. attempt06은 actor 종료가 반드시 worker-panic EngineError라고 가정해 실패했다. owner try_shutdown의 Stop 요청과 actor의 panic 감지 중 먼저 진행한 경로에 따라 StopRequested도 정상이다. attempt07은 통과했으나 반대 순서를 가정한 assertion이 남은 것을 root가 발견했다. 두 정당한 termination만 허용하고 EngineError의 원인 문자열을 확인하는 attempt09는1/1·0.05초로 통과했다. 외부 owner의 오류는 어느 순서든 반복 WorkerPanicked를 유지한다. 이들은 test 관찰 조건의 수정이며 임의 termination을 허용해 실패를 숨기지 않는다.

owner Drop을 import gate 동안 실행한 attempt08도1/1로 통과했다. Drop은 join하지 않고 반환했고 실제 worker가 gate/정리를 마치기 전 cache lock은 Busy, 이후 reopen은 성공했다. raw SHA-256은 `ae5fd7880cf68bb51336641936e1f11fb870c3c13f2c848b293868e24af2b0b8`다. 이어 runtime default84+integration2·all-feature96+integration2·strict all-target Clippy가 통과했지만 held frame/held source의 종료 검사를 추가 중이므로 최종 source 인수가 아니다. raw는 `p03-generation-runtime-{default,allfeatures,clippy}-luna-attemptNN.log`다.

audio read-only 리뷰도 마쳤다. pinned Kira0.12.5의 render-owned sound가 실제 Frame Arc를 잡고 있고 handle reap은 이를 증명하지 않는다. weak observer만으로 callback의 마지막 큰 free를 막을 수 없어 실제 leaf 공유 store·generation alias/voice·cleanup strong sentinel과 정확한 owner baseline이 필요하다. project-audio/service source map은 명세에 반영했으며 audio 새 API·multibank/실제 회수는 아직 구현하지 않았다. 리뷰 담당은 source 수정과 tests를 실행하지 않았다.
