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

audio read-only 리뷰도 마쳤다. pinned Kira0.12.5의 render-owned sound가 실제 Frame Arc를 잡고 있고 handle reap은 이를 증명하지 않는다. weak observer만으로 callback의 마지막 큰 free를 막을 수 없어 실제 leaf 공유 store·generation alias/voice·cleanup strong sentinel과 정확한 owner baseline이 필요하다. project-audio/service source map은 명세에 반영했으며 audio 새 API·multibank/실제 회수는 아직 구현하지 않았다. 리뷰 담당은 source 수정과 tests를 실행하지 않았다.
