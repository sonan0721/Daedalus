# P03.3b/c 에셋 세대 전환 설계·작업 기록

상태: 선행 P03.3a 인수 후 상세 계약 리뷰 시작·activation source 구현 미착수 · 2026-10-16

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

## P03.3b.1 공통 비교기부터 구현

소비자마다 중복 비교를 만들지 않도록 [호환성 상세 계약](../03-architecture/asset-replacement-compatibility.md)을 먼저 동결했다. manifest/asset metadata·entity/component 관계와 authored data는 보존하고, source payload/URI/hash·scene revision/배열 순서의 허용 범위를 명시했다. 큰 JSON을 복사하지 않는4MiB logical reference-index 상한과 deterministic bounded 오류를 둔다. 아직 구현 통과로 체크하지 않는다.

Luna project 담당에게 content 새 module·lib re-export·필요한 기존 project test 최소 변경만 배정했다. 단독 Cargo/shared-target lease와 고유 `p03-compatibility-*-luna-attemptNN.log`를 사용해 focused→content 전체→strict Clippy→content Wasm/fmt를 검사한다. 다른 Luna 두 명은 additive consuming native owner/retirement, 실제 audio Frame backing 회수 seam을 각각 read-only로 조사한다. renderer/runtime/audio의 dependent source는 API를 동결하기 전 수정하지 않는다.
