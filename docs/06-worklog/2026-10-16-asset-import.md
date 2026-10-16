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

후속 source 리뷰에서 image key의 width/height 누락과 ceil-halving mip chain을 발견해 각각 다른 크기 collision 방지와 pinned wgpu27의 floor/max1 extent로 수정하도록 했다. 실제 근거는 로컬 registry의 `wgpu-types-27.0.1/src/lib.rs` Extent3d::mip_level_size(6115행)이며 W3C 전체 spec fetch는 크기 한도로 실패했다. 단순 2×2 crop 대신 홀수 edge의 source 면적을 반영하고 불필요한 base/output clone도 줄인다. 이 시점에 결과 시험이 통과한 것은 아니다.

engine-owned transient parser/codec/cache buffer 한도는 payload 128 MiB + framing 여유64 KiB로 보완했다. 초기 GLB-copy 중심32 MiB를 그대로 PNG cache 직렬화에 사용하면 허용하는64 MiB RGBA에서 실패하며, Vec→Arc의 두 buffer를 계수하면 header88 bytes 때문에 단일4096² PNG도 거부되는 점을 확인했기 때문이다. 88 bytes를 피하려고 새 streaming API나 unsafe builder를 만들지 않고 명시적인 작은 framing 여유를 둔다. source32/total64·decoded image64·retained cooked128 MiB는 유지한다. foreign parser 내부 allocator/process RSS 보장과 구분하며 preallocation·동시 peak 검증을 요구한다.

개발 분담도 조정했다. 세 포맷의 cache codec 수정이 끝난 뒤 glTF를 순차 작성하면 가장 큰 format 작업이 대기하므로, 기존 담당자가 glTF 미착수를 확인한 후 두 glTF 파일만 별도 Luna xhigh 소유로 이전했다. dispatcher·image/atlas/audio·공통 codec 소유는 원 담당자가 유지한다. 공통 image cook을 재사용하며 Cargo 소유자는 늘리지 않는다. 이 변경의 시간 절감률은 측정하지 않았다.

CLI 담당자 보고: Python stub 자식의 정상 응답/종료를 1초로 제한한 7개 사례에서 timeout이 발생했다. 정상 stub deadline만 공통 5초로 바꾸고 의도적인 timeout 검사는0.15/0.1초로 유지했다. 17개 Python smoke unit test가 연속3회(2.378~2.424초) 통과했다. production retry 변경이나 실제 엔진 smoke 결과가 아니며 root 독립 검증은 통합 후 진행한다.

root 독립 Python 회귀는 `python3 -m unittest discover -s scripts -p 'test_*.py'`로53건/7.049초/exit0을 확인했다. 로그는 ignored `dist/project-evidence/p03-assets-python.log`다. 실제 엔진 binary의 assets smoke와 구분한다. 후속 script 수정이 생기면 영향 검사를 다시 실행한다.

Rust1.94.0의 native target만 설치되어 있던 환경에 `rustup target add wasm32-unknown-unknown --toolchain 1.94.0`으로 std component를 준비했다. 설치 후 target 목록에 native/Wasm 두 개가 있는 것을 확인했다. Cargo 소유자는 유지했고 아직 content Wasm compile이나 browser 실행을 통과했다고 기록하지 않는다.

## 실제 입력 확보 결과

고정 Khronos Cube glTF/BIN/512×512 PNG, Kenney Tiny Dungeon palette PNG, Kenney Interface Sounds OGG 원본 및 외부 FFmpeg 6.0 변환 PCM16 WAV, Kenney Nature Kit GLB를 확보하고 source 구조와 hash를 확인했다. 원본은 ignored `dist/qa-assets/`에만 있다. 자동 smoke는 별도 offline 입력을 사용한다.

확인한 제약은 Nature Kit 선택 GLB의 `KHR_materials_unlit` 선언과 Interface Sounds의 실제 OGG 배포다. 전자는 미지원 진단 입력으로, 후자는 변환 출처를 보존한 WAV 입력으로 사용한다. 아직 엔진 importer를 실행하지 않았으므로 cook/재생/표시 성공은 기록하지 않는다.

## 다음 검사

native/byte parity, 실제 네 포맷 출력, aggregate/preallocation 한도, 외부/embedded PNG edit의 selective reuse, geometry/atlas/WAV 변경, corrupt/atomic/pin/cache ownership, 취소/이전 정상 catalog 보존, bounded CLI 진단과 기존 CPU18/view22 회귀를 확인한다. 새 dependency의 locked graph·notice·필요한 Wasm compile도 별도 확인한다. 실제 명령과 counter가 나오기 전에는 DEV03/DEV05 최적화 효과를 달성했다고 표시하지 않는다.
