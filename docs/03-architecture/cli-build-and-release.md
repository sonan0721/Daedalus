# Daedalus CLI 로컬 빌드·태그 릴리스

상태: 사용자 확정 배포 방향 + 구현 계약 · 2026-10-15

## 사용자 요구

엔진의 주 진입점인 `daedalus` CLI를 실제 배포 산출물로 관리한다. 지금은 로컬에서 빌드·버전·실행 파일을 관리하고, 후속 릴리스 태그 push가 GitHub Release 빌드와 업로드를 유발하게 한다. 이 작업에서는 태그를 생성하거나 공개 릴리스를 발행하지 않는다. CI build artifact와 공개 Release asset도 구분한다.

## 빌드 산출물

초기 배포 단위는 타깃별 native CLI 실행 파일, 원본 엔진 라이선스, 사용 안내, JSON build manifest, archive·SHA-256이다. 최초 검증 범위는 이 Mac의 `aarch64-apple-darwin`이다. GitHub 빌드는 macOS-arm64·Linux-x64부터 실제 빌드/실행 상태를 검증하고 확대한다. Windows·web·mobile 산출물 이름만 만들어 지원으로 표시하지 않는다.

기본 CPU 제어 경로와 capture/window feature를 manifest에 기록한다. 릴리스 CLI의 capture feature는 실제 renderer를 포함해야 하고 GPU 부재의 오류 계약을 유지한다. 실행 파일 하나를 배포했다고 C# SDK·CoreCLR runtime·외부 에셋·게임 export가 모두 포함됐다고 하지 않는다. 초기 archive의 실제 포함 범위를 사용 안내에 명시한다.

## 버전과 로컬 관리

`daedalus --version`은 실제 crate SemVer를 표시한다. build manifest에는 schema version, CLI version, exact Git commit, dirty 여부, target triple, Cargo profile, feature 목록, Rust toolchain, binary SHA-256, build 식별자를 남긴다. 조회하지 못한 값은 임의로 채우지 않는다. dirty 빌드는 로컬에서 허용하되 공개 릴리스 후보와 구분한다.

로컬 packaging 명령은 `--locked`로 native release build를 수행하고 격리된 ignored 디렉터리 아래 산출물을 보관한다. 여러 빌드를 식별할 수 있게 이름·manifest를 연결하고 기존 산출물을 조용히 덮어쓰지 않는다. 프로젝트 source·로그·토큰·개인 경로·임의 사용자 파일을 archive에 포함하지 않는다. 빌드물은 Git에 커밋하지 않는다.

archive에서 꺼낸 실행 파일로 `--version`과 CPU inspect smoke를 실행한다. macOS 실행 권한도 보존한다. SHA-256은 무결성 비교이며 code signing/notarization을 대신하지 않는다. 초기 unsigned 배포와 서명된 사용자 배포의 품질 게이트를 구분한다. SDK/framework-dependent host의 부속 packaging은 E04 결과와 연결해 후속 확장한다.

## 태그와 GitHub Release

초기 tag convention은 `v<CLI SemVer>`다. 정상 예시는 `v0.1.0`이며 prerelease 예시는 `v0.1.0-alpha.1`이다. tag의 version은 실제 CLI manifest version과 일치해야 한다. 0.x 버전의 API는 아직 안정화되지 않았으며 버전 문자열만으로 전체 엔진 품질 완료를 뜻하지 않는다.

태그 workflow는 source commit이 main 이력에 있는지와 version 일치를 검사한 뒤 `--locked` build·CPU smoke·packaging을 수행한다. 모든 대상의 성공 뒤 publish job 하나가 기존 tag를 검증해 archive·manifest·checksum을 업로드한다. prerelease tag는 prerelease로 표시한다. 실패한 플랫폼을 조용히 빼고 완전한 릴리스를 발행하지 않는다. 기존 Release asset을 자동 덮어쓰지 않는다.

PR 또는 수동 build 검증은 읽기 권한으로만 수행하며 공개 Release를 만들지 않는다. `contents: write`는 tag publish job에만 부여한다. Action은 공식 저장소의 실제 tag→commit을 확인해 SHA로 고정한다. 외부 PR 코드를 publish 권한으로 실행하는 `pull_request_target` 경로를 두지 않는다.

## 검증과 제한

1. 로컬 package 생성·manifest/실행 파일 version 일치·SHA-256 재계산·깨끗한 디렉터리에서 archive 추출/실행.
2. malformed tag·version 불일치·기존 출력·실패한 build·잘못된 target를 명시적 오류로 처리.
3. PR dry build에서 macOS/Linux archive를 생성하고 추출한 CLI를 실행. 이것은 tag 발행 성공의 증거가 아니다.
4. 최초 실제 태그 발행 시에만 Release asset의 플랫폼·checksum·다운로드 후 실행까지 기록한다. 현재 공개 Release는 만들지 않는다.

동일 소스를 동일 명령으로 다시 빌드하는 절차와 byte-for-byte reproducible build 증명은 다르다. 처음부터 후자를 달성했다고 표시하지 않는다. 자동 업데이트·패키지 매니저·Homebrew tap·코드 서명은 이 초기 작업의 완료 주장에 포함하지 않는다.

근거: [GitHub push 이벤트](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#push), [hosted runner 목록](https://docs.github.com/en/actions/reference/runners/github-hosted-runners), [gh release create](https://cli.github.com/manual/gh_release_create), [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html). 조회일2026-10-15. workflow와 실제 실행 로그가 최종 플랫폼 근거다.
