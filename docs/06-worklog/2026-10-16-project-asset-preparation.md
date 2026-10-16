# P03.3a 비동기 에셋 후보 준비 작업 기록

상태: memory summary source 작업 중·worker/control API 리뷰 중 · 2026-10-16 · P03.3/D1 미통과

## 이 기록을 만든 이유

[실행 계약](../03-architecture/project-asset-preparation.md)은 AI가 같은 살아 있는 세션에서 에셋 후보를 준비·조회·취소할 때의 worker·cache·payload·예산·종료 책임을 고정한다. 이 기록은 source 감사·API 동결·Luna 구현과 실제 검사를 구분한다. CPU-prepared만으로 화면·audio의 active generation이 바뀌었다고 보고하지 않는다.

## 확인한 선행 gate

[PR13](https://github.com/sonan0721/Daedalus/pull/13)의 다섯 CI, 두 native packaged asset+offline audio37건, Linux linkage와 main `0b98805986f17598ccd2fca7dfaf5109f2f2cf7c`의 검증한 head 동일 tree를 확인했다. [audio 기록](2026-10-16-imported-audio-runtime.md)에 실제 PCM·native·same-window·clean archive identity와 실패를 보존했다. P03.2만 인수했으며 전체 P03/D1/D2/D3는 열려 있다.

새 branch는 `feat/project-asset-preparation`, 첫 계약 commit은 `2ffa4ed`다. 큰 source/control API는 리뷰 중이며 독립적인 content memory summary 하위 계약만 먼저 동결했다.

## source 감사와 구현 선택

Luna의 read-only 감사와 조정자 리뷰는 TaskPool의 단일 completion receiver를 다른 asset consumer와 공유하면 기존 terrain 결과를 가로챌 수 있음을 확인했다. project worker/cache owner 하나를 별도로 두는 안을 선택했으며 새 Engine/권한 world를 만들지는 않는다. FileCookCache의 lock은 catalog CachePin으로도 유지되므로 초기 load와 이후 준비가 같은 cache owner를 사용해야 한다.

현재 CookedBytes는 imported payload와 catalog/binding metadata 추정의 장부다. parsed manifest/scene JSON·legacy geometry·caller의 명시적 clone·foreign parser 내부 heap·전체 RSS는 다 세지 않는다. 후보 하나와 active 하나의256MiB 제안은 이 논리적 두-catalog 장부 범위다. decoded base 이미지와 cooked는 겹치므로 더해서 독립 총량처럼 쓰지 않는다. 현재 scratch 사용량만 있고 high-water가 없어 실제 계측을 추가해야 한다.

첫 content 작업은 public opaque `AssetImportMemorySummary`, private report field/public `memory()` getter, 실제 CookedBytes/DecodedImageBytes/scratch peak·captured source identity 합계를 추가한다. 기존 public AssetImportMetrics literal·source/cook identity·cache format은 유지한다. source bytes·PCM/image를 복사하거나 별도 estimator를 다시 만들지 않는다.

## Luna xhigh 파일 소유권과 검증 순서

- content 담당: `crates/daedalus-content/src/assets/mod.rs`, `types.rs`, `tests.rs`와 report construction에 필요한 `project.rs`. memory summary만 구현하며 runtime/CLI/manifest/lock은 건드리지 않는다.
- control/CLI 담당: 우선 read-only native factory/options·공통 operation/result/wire API를 제안한다. 기존 public config literal과 old start 경로를 보존하는 구체 signature를 조정자가 MD에 동결하기 전에는 구현하지 않는다.
- 조정자: MD·통합/source 리뷰·Cargo/target owner 인계·실제 검증·Git/gh를 담당한다. 현재 content는 source-only로 시작하고 반환 후 focused content test/strict Clippy를 한 담당에게 인계한다.

준비 worker는 큰 private candidate 하나를 worker/result 전체에서 보유하고 다음 준비 전 worker에서 이전 ready를 해제한다. 정상 ticket의 ACK는 candidate payload 회수 근거가 아니다. 이번 하위 단계의 제어 결과에는 요약만 싣고 activation/export lease는 후속 계약으로 남긴다.

## 실행 근거 — 아직 없음

이 단계의 새로운 코드 compile/test·live Prepare/Cancel·memory 수치·clean package·CI를 아직 통과시키지 않았다. cold/warm accounted 값, captured source byte 합계, lower-limit 사전 거부와 overlapping/released scratch peak를 구현 뒤 확인한다. 이전 단계의 CPU281·Python56·Metal9·smoke46을 새 memory/worker 기능의 통과 근거로 사용하지 않는다.

worker의 syscall/parser 취소는 협력적이고 captured files는 디스크의 동일 순간 snapshot이 아니다. 실제 source/input hash·candidate epoch·stale/cancel/failure와 마지막 정상 active generation을 별도로 관찰해야 한다. 일반 source watch·GPU/audio 교체·최초 loading UI·전체 P03.3 인수는 아직 구현하지 않았다.
