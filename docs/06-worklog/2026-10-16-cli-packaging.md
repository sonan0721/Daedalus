# E08 CLI 로컬 패키징·태그 배포 검증

상태: 로컬 strict package·실제 CLI/GPU·native PR dry build 확인, tag publish 미실행 · 2026-10-16

## 목적과 범위

사용자 답변 A-037에 따라 `daedalus` CLI를 버전·소스·실행 결과를 추적할 수 있는 배포 산출물로 만든다. 지금은 로컬 빌드와 PR artifact를 검증하며, 후속 `v<CLI SemVer>` 태그 push에만 GitHub Release를 발행하도록 구성한다. 이 작업에서 태그나 공개 Release는 생성하지 않는다.

엔진 구현 PR#3은 필수 문서·Rust·managed native integration 성공 후 `ec5d9c15374ac222706ab1185b3b96c2a46790c1`로 main에 병합했다. 패키징은 별도 변경으로 리뷰한다. 프로젝트 소유 스크립트·테스트·CI·manifest 구성은 Luna xhigh가 작성하고 조정자는 계약·문서·코드 리뷰·실행 검증을 담당했다.

## 변경과 이유

- `scripts/package_cli.py`: native release build, 고유 build ID, archive·manifest·SHA-256, 압축 해제 후 실행을 한 경로로 묶는다. dirty 로컬 빌드는 식별해 보관하고 태그 후보에는 깨끗한 worktree를 요구한다.
- `scripts/collect_cli_notices.py`: CLI에서 도달 가능한 Cargo graph의 의존성 선언·checksum·license/notice 자료를 수집한다. target·feature 통합의 보수적 범위를 명시한다.
- `third_party/license-overrides/manifest.json`과 자료: crate archive에 빠진 자료를 exact version·Cargo.lock checksum·upstream commit·파일 hash로 고정한다. 파일을 찾지 못한 네 package의 Cargo SPDX 선언과 공식 reference text는 upstream 원문과 구분한다.
- `.github/workflows/release.yml`: macOS arm64·Linux x64 PR dry build와 후속 tag publish를 분리한다. 두 native build가 모두 성공해야 공개 배포 단계로 진행한다.
- `README.md`, `TESTING.md`, [배포 계약](../03-architecture/cli-build-and-release.md): 사용자가 실행할 명령·archive 포함 범위·미검증 기능을 기록한다.
- [GitHub 운영](../04-plan/github-workflow.md): 실제 적용한 필수 체크 3개와 PR#3 병합 근거를 갱신한다.

## 의존성 자료의 확인

초기 graph에서 macOS는 외부 package 187개, Linux는 210개다. 이는 `cargo metadata --locked`의 target-filtered CLI 도달 집합이며 binary symbol 수준 SBOM이 아니다. Cargo의 workspace feature 통합 때문에 실제 실행 파일보다 넓은 집합일 수 있다.

보완 registry는 21개 exact package record다. 그중 `block 0.1.6`, `dispatch 0.2.0`, `malloc_buf 0.0.6`, `hexf-parse 0.2.1`은 `declared_spdx_reference`로 표시한다. 나머지는 고정된 upstream 자료를 보존하며 짧은 Apache/MIT 안내문에는 별도로 표시한 공식 reference text를 보충한다. 조정자는 pinned SPDX MIT, objc2 root LICENSE, gpu-alloc MIT, block Cargo.toml의 원격 bytes를 별도 조회해 registry의 SHA-256과 일치함을 확인했다. 수집 결과를 법적 적합성 인증으로 표현하지 않는다.

## 실행 근거

이 Mac에서 아래 native build가 성공했고 실제 실행 파일의 `version --json`, `inspect --entities 2`를 확인했다.

```sh
cargo build --release -p daedalus-cli --features capture --locked --target-dir target --target aarch64-apple-darwin
target/aarch64-apple-darwin/release/daedalus version --json
target/aarch64-apple-darwin/release/daedalus inspect --entities 2
```

출력은 CLI `0.1.0`, `capture: true`, CPU entity count 2다. 실제 창 명령은 아직 제공하지 않아 `window: false`다.

조정자가 Python 전체 discovery를 실행해 32건 통과를 확인했다. 문서 검사기 8건, notice 수집 14건, package 생성·검증 10건이다. strict 수집은 macOS 187개 중 미해결 0·선언 참조 4, Linux 210개 중 미해결 0·선언 참조 1로 통과했다. 실제 crate VCS 정보의 top-level `path_in_vcs`와 `git.sha1` 검증을 빠뜨린 초기 구현은 리뷰에서 수정하고 경로·commit 불일치 회귀 테스트를 추가했다.

수정 후 실제 `python3 scripts/package_cli.py`는 고유 build ID `20261015T151633Z-a65a23860e5840dc9a5ad90f7fcd2409`로 package를 생성했다. manifest는 source commit `ec5d9c15374ac222706ab1185b3b96c2a46790c1`, `dirty: true`, target `aarch64-apple-darwin`을 기록한다. 변경 사항을 커밋하기 전의 검증이므로 clean release 후보로 표시하지 않는다. archive SHA-256은 `8a5920306073510f4ede7cdd18ded2ca9d78f9bd8389761baf1c69be7964140d`다. 산출물은 ignored `dist/local/` 아래에 보관하고 Git에 추가하지 않았다.

조정자가 이 archive를 별도로 `--verify-archive`에 전달해 checksum·embedded hashes·압축 해제 후 `--version`/CPU `inspect` 성공을 확인했다. 같은 manifest의 실행 파일로 320×180 실제 capture를 실행해 Apple M3 Pro/Metal에서 RGBA 230,400 bytes의 PNG를 얻고 이미지를 확인했다. 작은 재질 fixture이며 일반 scene importer·완성된 게임 화면의 증거가 아니다.

[PR#4](https://github.com/sonan0721/Daedalus/pull/4)의 첫 source checkpoint `c35a7ab4e302e13d96355ce7993610ccc2970ecf`에서 [macOS·Linux package](https://github.com/sonan0721/Daedalus/actions/runs/38759136059), [Rust](https://github.com/sonan0721/Daedalus/actions/runs/38759136026), [managed](https://github.com/sonan0721/Daedalus/actions/runs/38759136320), [문서](https://github.com/sonan0721/Daedalus/actions/runs/38759135543)가 모두 성공했다. 공개 publish job은 PR 조건에 따라 skipped였다. Linux native CPU smoke는 Ubuntu 24.04 runner에서 실행했고 조정자는 artifact를 내려받아 `--metadata-only`로 checksum·embedded hashes·실제 PR merge commit 일치를 별도 확인했다. Mac에서 Linux binary를 실행했다고 기록하지 않는다.

최종 archive 리뷰에서 Rust panic source 경로에 빌드 Mac의 절대 경로가 포함된 것을 발견해 배포용 compiler remapping과 환경 변수 회귀 검사를 보완했다. 이 수정 후 Python 전체 33건(문서 8·수집 14·package 11)이 통과했고 build-only 환경의 encoded flags 우선순위·공백 포함 경로·프로세스 전역 환경 불변을 확인했다. Cargo config rustflags보다 우선하는 환경 변수를 사용한다는 제한도 manifest와 계약에 기록했다.

보완 package의 build ID는 `20261015T152422Z-73e56846111242a190fe2e403f3c6b7f`, archive SHA-256은 `14d33258093f5f7665a2e4759d0d5cfa24ffafb3d3f6c2f2b5693af442905959`다. source `c35a7ab4e302e13d96355ce7993610ccc2970ecf`, `dirty: true`인 후속 검증 산출물이다. 조정자의 별도 archive 검사·압축 해제 후 실행도 통과했다. 실제 binary의 `strings`에는 확인 대상 build-home 절대 경로가 없고 의존성 위치는 `/daedalus/cargo/...`로 표시됐다. 이 검사는 모든 arbitrary 문자열의 제거 인증이 아니다.

빌드 중 HEAD 변경·수정 사항의 발생도 검사하도록 build 전후 Git 확인을 추가했다. HEAD 변경 시 output 생성 전에 중단하고, 로컬 dirty는 전후 결과를 합치며 tag build는 양쪽 clean을 요구한다. 이 세 회귀 경우를 추가한 뒤 조정자의 전체 Python 검사는 36건(문서 8·수집 14·package 14) 통과했다.

최종 source의 CI 상태와 병합 기록은 [PR#4 checks](https://github.com/sonan0721/Daedalus/pull/4/checks)에서 추적한다. PR에서 tag publish가 실행된 것으로 해석하지 않는다. 후속 clean 로컬 빌드는 매번 생성되는 manifest로 source·hash를 확인하며, 문서 수정 때마다 예전 build ID를 최신 산출물이라고 덮어쓰지 않는다.

## 남은 경계

초기 CLI archive에는 C# SDK·CoreCLR runtime·게임 에셋·게임 export가 포함되지 않는다. 코드 서명·notarization·Windows package·자동 업데이트·동일 bytes 재현 빌드는 별도 작업이다. PR artifact 성공은 실제 tag 발행·Release 다운로드 검증의 증거가 아니며, 태그 publish 경로는 현재 실행하지 않는다. 전체 엔진의 웹·30명 서버·고급 렌더링·UI·144fps 품질 게이트는 계속 열려 있다.
