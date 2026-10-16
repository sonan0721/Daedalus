# P03.1 공통 에셋 import·cook 작업 기록

상태: 구현 진행 중·인수 검사 전 · 2026-10-16

## 이번 목표와 선행 상태

P02 foundation은 PR9 main `66121d7676727b496d894d1c64a7fc6616a3551d`의 필수 CI·Mac/Linux package 검사까지 통과했다. 이를 선행 조건으로 [공통 에셋 계약](../03-architecture/asset-import-foundation.md)을 `96f6f49`에 기록하고 P03.1을 시작했다. P02 전체·P03 전체·D1/D2/D3 인수는 계속 미완료다.

이번 목표는 glTF/GLB static model, PNG, sprite atlas, WAV를 같은 bounded resolver/cache/catalog로 실제 CPU import하고 `project cook`에서 검증하는 것이다. 실제 renderer/audio 소비와 source watch/reload는 후속 P03.2/P03.3이며 이번 CLI 성공으로 대신하지 않는다.

## 문서와 분담을 먼저 고정한 이유

- `asset-import-foundation.md`: 네 포맷의 IR·URI·예산·cache·CLI·실패·invalidation을 코드 전에 합의했다. 컨테이너 전체 hash와 출력별 key를 분리해 불필요한 geometry cook을 줄이는 계약이다.
- [asset-validation-provenance.md](../02-research/asset-validation-provenance.md): 실제 공개 원본의 revision/license/hash와 외부 변환을 보존한다. 생성 fixture만 성공하거나 미지원 자료를 조용히 변형해 성공시키는 것을 피한다.
- 이 작업 기록: 실제 실행 전 계획과 나중의 독립 검증을 구분한다. 성공 횟수·속도·패키지 identity는 실행 후에만 추가한다.

모든 구현·test/script 코드는 사용자 지정 Luna xhigh에 맡긴다. content core/API와 Cargo.lock/shared target 소유자를 한 명으로 두고, exact types/context를 공유한 뒤 importer와 CLI가 disjoint 파일을 편집한다. root는 문서·원본 조사·리뷰·독립 QA·Git/PR를 맡는다. API 명칭만 합의한 것을 컴파일 가능한 handoff로 취급하지 않는다.

실제 core seam 파일이 만들어지고 담당자가 content library `cargo check`를 통과한 뒤 driver/CLI 병렬 구현을 열었다. 이것은 신규 포맷/CLI 통과가 아니다. 리뷰에서는 wrap bool/min-mag만 가진 sampler의 의미 손실, context-local PNG memoization만으로는 CLI 재실행 때 decode가 반복되는 점, atlas가 이미 읽은 image dependency를 누락할 수 있는 점을 보완하도록 했다. schema/default를 먼저 공유해 CLI와 importer가 다른 fixture 형식을 추측하지 않게 했다.

CI는 기존 두 native package job의 staged binary에 동일 offline `--assets` helper를 연결한다. package backend의 CPU18 기본 검증을 바꾸거나 새 Cargo build를 중복하지 않는 구현 계약이며 현재 아직 실행 전이다.

## 실제 입력 확보 결과

고정 Khronos Cube glTF/BIN/512×512 PNG, Kenney Tiny Dungeon palette PNG, Kenney Interface Sounds OGG 원본 및 외부 FFmpeg 6.0 변환 PCM16 WAV, Kenney Nature Kit GLB를 확보하고 source 구조와 hash를 확인했다. 원본은 ignored `dist/qa-assets/`에만 있다. 자동 smoke는 별도 offline 입력을 사용한다.

확인한 제약은 Nature Kit 선택 GLB의 `KHR_materials_unlit` 선언과 Interface Sounds의 실제 OGG 배포다. 전자는 미지원 진단 입력으로, 후자는 변환 출처를 보존한 WAV 입력으로 사용한다. 아직 엔진 importer를 실행하지 않았으므로 cook/재생/표시 성공은 기록하지 않는다.

## 다음 검사

native/byte parity, 실제 네 포맷 출력, aggregate/preallocation 한도, 외부/embedded PNG edit의 selective reuse, geometry/atlas/WAV 변경, corrupt/atomic/pin/cache ownership, 취소/이전 정상 catalog 보존, bounded CLI 진단과 기존 CPU18/view22 회귀를 확인한다. 새 dependency의 locked graph·notice·필요한 Wasm compile도 별도 확인한다. 실제 명령과 counter가 나오기 전에는 DEV03/DEV05 최적화 효과를 달성했다고 표시하지 않는다.
