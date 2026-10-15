# Native live session 통합과 작업 파이프라인

상태: Mac 실제 창·전체 회귀·local package·첫 PR CI 성공 · 2026-10-16 · E01/E02/E03/E08 · 사용자 A-038/A-039

## 이 기록을 만든 이유

앞선 checkpoint의 CPU 제어와 offscreen capture를 실제 창·ECS·background terrain이 연결된 한 세션으로 확장한다. 구조를 작성한 것과 실제 Mac 입력으로 동작을 확인한 것을 구분하고, 이번에 함께 요청한 개발 파이프라인 최적화의 적용 범위도 남긴다. 엔진 검증 fixture이며 sample game은 추가하지 않는다.

구현 전에 [native live session 계약](../03-architecture/native-live-session.md)을 만들고 체크리스트에 통과 조건을 등록했다. 코드·시험·CI는 Luna xhigh가 작성하고 조정자가 Markdown·리뷰·독립 실행·Git 통합을 담당했다.

## 리뷰에서 고친 수명과 과부하 경계

- 같은 entity의 tick 전 연속 이동을 대기열에 모아 실제 revision에 따라 순서대로 적용한다.
- 수락한 명령의 결과 credit을 제한하고, 종료 때 아직 읽지 않은 결과를 유한한 shutdown report로 회수한다.
- terrain job의 세대·source revision·lifetime·coordinate를 확인하고 취소된 결과를 새 지형으로 채택하지 않는다.
- live state는 변경 또는 fixed tick에 맞춰 게시해 polling만으로 같은 geometry를 반복 구성하지 않는다.
- renderer는 scene 크기 제한을 먼저 확인하고, CLI는 presentation source 단절 시 오류 종료 옵션을 사용한다.

## 개발·검증 파이프라인

공개 API부터 공유한 뒤 runtime/window/CLI 파일 소유권을 나누고 Cargo.lock 편집자를 한 명으로 고정했다. 공유 target cache로 focused 검사부터 실행한 뒤 통합 회귀로 확대한다. 이전 컴파일이 끝날 때마다 동일한 전체 검사를 반복하는 대신 변경·실패가 발생한 범위에 맞춰 재검사한다.

CI는 window 경로를 포함하는 all-features 검사와 공식 SHA에 고정한 Cargo cache를 적용한다. OS·architecture·toolchain·lock·작업 단계별로 분리하고, cache hit도 실제 테스트와 archive smoke를 생략하지 않는다. 첫 CI 실행의 cache 저장은 준비 단계이며 속도 향상 수치를 증명하지 않는다.

## 현재 실행 근거

- 문서 검사와 Python 36건 성공: 문서 8, notice 14, packaging 14.
- .NET SDK 10.0.402 Release build: 경고 0·오류 0. 실제 Rust-backed host self-test와 managed/native 2개 scenario 성공.
- macOS arm64 window 의존성 notice strict 수집 성공: resolved dependency 187개. 경로는 ignored `dist/live-evidence/notices/`이며 원본 프로젝트에 외부 에셋을 추가하지 않았다.
- renderer CPU 14건, runtime unit/integration 22건, CLI 12건과 각 담당자의 focused Clippy 성공.
- 조정자의 workspace all-features check/test/Clippy·fmt 성공: CPU 102건 성공, 기본 비활성 hardware 2건. renderer의 추가 test-fixture 검사도 이 숫자에 포함된다.
- 별도 요청한 실제 Metal GPU regression 성공: 513×289 padded-row readback·PNG decode 검증. 출력 `dist/live-evidence/gpu-regression.png`.

## 로컬 배포 단위와 PR

source `8679c5d4df58cf3c8d25893f8b828802f0f23d2f`의 clean tree에서 기본 `python3 scripts/package_cli.py`가 release `window` package를 생성했다. compiler build log는 기존 공유 cache를 사용한 8.34초였으며 전체 packaging 시간이나 CI 개선 배율로 해석하지 않는다. 출력은 ignored `dist/local/daedalus-0.1.0-aarch64-apple-darwin-8679c5d4df58-clean-20261015T233153Z-837ae14a9f6b4d8c996cd36ad003a70d/`다.

별도 `--verify-archive`도 성공했고 manifest는 `dirty: false`, `cargo_features: ["window"]`, target `aarch64-apple-darwin`을 기록했다. binary SHA-256은 `b8eecf82951a25a8446090525f289b9d70cdc551182611dda598dc294ef9630c`다. package 실행 파일의 version JSON은 window/capture를 true로 보고했다.

동일 package 실행 파일의 `view --seconds 3`도 exit 0·270 frames·자동 time limit 종료·terrain generation 1 완료·pending result 0·last error 없음으로 성공했다. `dist/live-evidence/packaged-view.json`을 남겼다. CPU archive smoke와 실제 GPU/window smoke를 별도로 수행했다.

[PR #5](https://github.com/sonan0721/Daedalus/pull/5)의 첫 source checkpoint `8679c5d`에서 [docs](https://github.com/sonan0721/Daedalus/actions/runs/38824114972), [all-features Rust](https://github.com/sonan0721/Daedalus/actions/runs/38824114970), [managed/native](https://github.com/sonan0721/Daedalus/actions/runs/38824115009), [macOS/Linux package](https://github.com/sonan0721/Daedalus/actions/runs/38824114945)가 모두 성공했다. publish job은 PR이므로 skipped이며 태그·공개 Release는 생성하지 않았다.

두 PR archive를 실제 다운로드해 재검증했다. GitHub PR checkout은 head 자체가 아니라 base와 합친 synthetic merge commit `3fdc327c923961cc6b3cc0c1322c6a2426c8022c`다. GitHub API의 해당 merge SHA와 양쪽 manifest의 exact commit을 대조했다. Mac은 추출한 CPU executable smoke까지, Linux는 이 Mac에서 실행하지 않고 metadata·hash·archive 검증까지 수행했다. Linux executable smoke는 Linux CI에서 성공했다. package cache는 macOS 약 204MB·Linux 약 279MB 저장을 GitHub API로 확인했으며 cache 적중을 이유로 검사를 생략하지 않는다.

마지막 보완은 runtime 시험의 시간 의존성을 줄였다. OS가 thread를 늦게 실행해도 고정 tick 2/3을 가정하지 않고 initial frame 이후·두 입력의 적용 순서·정확한 revision과 누적 위치를 검사한다. transient mailbox contention도 bounded retry로 처리한다. production 동작은 바꾸지 않았고 조정자가 전체 CPU 102건·Clippy·fmt를 다시 통과시켰다. 최종 PR head의 필수 체크도 병합 전에 확인한다.

## 실제 Mac window 검증

M3 Pro·12 logical CPU·18GB Mac에서 debug 실행 파일을 사용했다. GPU는 Apple M3 Pro/Metal이며, window 시작은 1024×640 logical points, resize 후 실제 render size는 1100×700이었다. 모든 screenshot은 정확한 engine PID/window만 캡처했다. Cua Driver의 key 입력은 background 경로로 적용됐고, mouse drag는 background 경로가 unavailable을 반환한 뒤 새 snapshot에 근거해 같은 창의 foreground 경로로만 재시도했다.

```sh
target/debug/daedalus view --seconds 180 --work-iterations 2000 \
  --width 1024 --height 640 \
  --report dist/live-evidence/interactive.json \
  --capture-output dist/live-evidence/final-scene.png
```

초기 generation 1의 실제 mesh 78 quads를 표시한 뒤 R·화살표·W·연속 R·resize·mouse drag·R·Escape를 수행했다. 로딩 중 이전 지형을 유지하면서 blue entity 위치와 camera가 바뀌는 것을 이미지로 확인했다. 마지막은 generation 5가 loading인 상태에서 Escape로 종료했다.

| 관측 | 실제 결과 |
| --- | --- |
| 권한 entity | `[8,14,8]` → `[9,14,8]`; movement accepted/applied 1, rejected 0 |
| 표시 | 15,718 presents, 이 중 loading 8,542 |
| 로딩 입력 | key events 8, loading resize events 6; 전체 observer drop 0 |
| camera·resize | camera change 18, resize event 6; mouse orbit 변화 이미지 확인 |
| terrain | started 5, completed 3, cancelled 2, stale 1, failed 0 |
| R 명령 | accepted 4; ready 2, superseded 1, stopped 1 |
| 종료 | Escape·process exit 0; tick 8,280, join 3,417µs, active jobs 0 |
| 결과 회수 | shutdown에서 미수집 1건 회수; bridge 최종 pending 0; 마지막 stopped outcome 포함 |

cancelled와 stale은 상호 배타적인 통계가 아니므로 합산해 job 수로 해석하지 않는다. 초기 자동 terrain 생성도 사용자 R command와 구분한다. report의 CPU budget은 ECS workers 8·background 1·simulation 1·window 1·bridge 1, 추정 합계 12다. 드라이버·OS 등의 모든 thread 수 제한을 뜻하지 않는다.

마지막 512 present interval 표본은 p50 8.331ms·p95 8.789ms·p99 9.120ms·max 9.549ms였다. 최근 표본이며 전체 실행의 worst frame이나 GPU 완료 시간을 측정한 값이 아니다. 144fps 달성 증거로 사용하지 않는다.

로컬 근거는 ignored `dist/live-evidence/`의 `interactive.json`, `window-ready.png`, `window-regenerating.png`, `window-moved.png`, `window-resized.png`, `window-dragged.png`, `window-before-close.png`다. `final-scene.png`는 종료 뒤 별도 offscreen render이며 loading overlay나 실제 window screenshot과 구분한다. QA 창과 task 전용 driver session은 종료했다.

## 남아 있는 엔진 범위

한 청크의 제한된 generate/mesh 반복은 무한 지형 streaming·VRAM residency 검증이 아니다. 현재 scene 전달·렌더 업로드는 작은 fixture를 위한 경로이며 정식 render extraction/resource cache가 필요하다. loading overlay와 title은 전체 UI·text/IME·접근성 시스템을 대신하지 않는다. 30명 네트워크·웹·일반 scene import·C# 전체 SDK·물리/오디오 ECS 통합·상용 visual 품질·144fps 인증은 후속 체크포인트다.
