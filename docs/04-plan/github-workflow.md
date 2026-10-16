# GitHub 운영과 현재 검증 절차

상태: 2026-10-16 실제 저장소 설정·엔진 통합 PR 병합 확인. 공개 릴리스 미발행.

## 공개 저장소

[sonan0721/Daedalus](https://github.com/sonan0721/Daedalus)는 공개 엔진 저장소다. 원본 엔진·도구·문서는 MIT OR Apache-2.0이며 두 라이선스 원문을 포함한다. GitHub의 자동 라이선스 요약이 한 라이선스만 표시하더라도 선택 조건은 루트 README와 라이선스 파일로 명시한다.

## main에 적용한 규칙

- PR 경로로 변경하고 필수 `docs`, `rust`, `managed native integration` 체크를 모두 통과해야 한다.
- 최신 base 반영을 요구하는 strict status check를 적용한다.
- 관리자도 보호 규칙을 적용받는다.
- 강제 push·branch 삭제를 금지하고 선형 이력을 요구한다.
- PR 대화 해결을 요구하며 squash merge를 사용한다.
- 사용자 1인 유지보수 상황에 맞춰 필수 승인 수는 0이다. 자기 PR도 CI 통과 후 병합할 수 있다. AI 리뷰를 독립 인간 승인으로 계산하지 않는다.
- 병합한 원격 작업 branch는 삭제하도록 설정했다.

이는 문서에만 적은 제안이 아니라 GitHub API로 적용·조회한 설정이다. 관리자는 필요 시 정책을 수정할 수 있으며 변경 이유를 남긴다.

## 현재 실행 가능한 체크

```sh
python3 scripts/check_docs.py
python3 -m unittest discover -s scripts -p 'test_*.py'
git diff --check
```

문서 검사기 자체의 테스트는 8건이다. 전체 Python discovery는 packaging 검사 등 추가된 테스트도 실행하므로 총수는 해당 실행 로그를 따른다. 문서 CI가 통과했다고 Rust 엔진·GPU·웹·30명·144fps가 검증된 것은 아니다. [최초 CI 실행](https://github.com/sonan0721/Daedalus/actions/runs/38741106943)은 성공했다.

[엔진 구현 PR#3](https://github.com/sonan0721/Daedalus/pull/3)은 `docs`, `rust`, `managed native integration` 성공 뒤 squash 병합했다. 병합 commit은 `ec5d9c15374ac222706ab1185b3b96c2a46790c1`이다. 병합 전 마지막 검증은 [문서](https://github.com/sonan0721/Daedalus/actions/runs/38756044859), [Rust](https://github.com/sonan0721/Daedalus/actions/runs/38756051150), [실제 native bridge와 .NET 연결](https://github.com/sonan0721/Daedalus/actions/runs/38756045045)에서 확인할 수 있다. 실제 엔진·GPU·managed 검증 명령은 루트 [TESTING](../../TESTING.md)에 유지한다. 필수 보호 체크 3개는 workflow 추가와 별도로 GitHub API에 적용하고 조회했다.

CLI 로컬 package와 후속 tag 기반 공개 배포는 [배포 계약](../03-architecture/cli-build-and-release.md)과 `release` workflow로 구성한다. [PR#4](https://github.com/sonan0721/Daedalus/pull/4)의 첫 macOS arm64·Linux x64 native dry build가 성공했고 [E08 기록](../06-worklog/2026-10-16-cli-packaging.md)에 source checkpoint와 후속 보완을 남겼다. PR artifact는 공개 Release가 아니며 tag publish는 아직 실행하지 않았다. 현재 태그와 GitHub Release는 생성하지 않는다.

CI는 read-only contents 권한으로 실행하고 checkout Action을 확인한 commit SHA에 고정했다. 공개 PR이 권한 있는 `pull_request_target` 경로에서 코드를 실행하는 구성을 사용하지 않는다. [GitHub Actions 공식 보안 문서](https://docs.github.com/en/actions/reference/security/secure-use)

## 기여·PR의 실제 흐름

1. 큰 변경은 요구·goal·ADR·이슈로 문제와 방향을 기록한다.
2. 작업 branch에서 작은 변경과 의미 있는 검증을 수행한다.
3. PR에 목적·실제 동작·근거·한계·실행한 검사를 적는다.
4. 조정자가 Luna xhigh의 프로젝트 소유 코드를 리뷰하고 CI를 확인한다.
5. 필수 체크·대화를 충족한 뒤 squash 병합한다. 구현·검증 결과는 작업 기록에 남긴다.

2026-10-16 PR10 종료 때도 API의 protected contexts는 위3개, strict는true였다. 엔진 인수 절차는 여기에 두 native package job 성공을 추가해 총5개를 확인한다. P03.1부터는 두 package 안의 `Verify packaged CLI asset cook smoke` step 성공도 확인한다. 저장소의 allow_merge_commit/allow_rebase_merge는false, allow_squash_merge는true이므로 `gh pr merge`에는 `--squash`를 사용한다. 조회한 최종 head를 `--match-head-commit`으로 고정하고, 병합 후 main file tree와 검증한 head의 동일성을 확인한다. 이는 기존 repo 정책의 실행 방법이며 새 승인 절차가 아니다.

P03.2c에서는 같은 packaged step에 offline audio를 포함했다. P03.3a의 다음 변경은 이 step을 `Verify packaged CLI asset, audio, and preparation smoke`로 이름 붙이고 `--assets --audio --asset-preparation`을 같은 staged binary에 한 번 실행하는 것이다. 장치·다운로드·추가 job 없이 새 native worker/공통 제어도 실제 배포 바이너리에서 검사한다. source/로컬 통과와 두 CI의 실제 새 step 통과를 구분하며, 기존 보호 check3개·제품 gate5개·tag publish 조건은 유지한다.

PR14에서 두 새 combined50 step·Linux linkage·다섯 CI와 main 동일 tree를 인수했다. 이후 cache 감사는 PR에만 저장된 package cache가 다음 PR에 재사용되지 않는 범위 문제를 확인했다. [DEV06/07 개선 계약](development-iteration-pipeline.md)에 따라 같은 workflow의 main push build를 추가할 계획이다. publish job의 tag-only 조건과 read-only build 권한은 유지한다. 현재 main의 manual package run은 cache 초기 공급이며 공개 Release가 아니다.

초기 main manual run은 두 package/staged50 성공·Publish skipped, 실제 main-scoped cache 두 개 생성을 확인했다. Luna가 main push trigger 두 줄을 구현했고 YAML/기존 package14를 통과했지만 자동 trigger·다음 PR cache hit의 실제 인수는 아직 남았다. 새 job이나 별도 packaging backend는 추가하지 않는다.

기여 문서의 구조는 Bevy·Godot의 공식 기여 입구를 참고하되 대규모 팀 규칙을 그대로 복제하지 않았다. [Bevy](https://github.com/bevyengine/bevy/blob/main/CONTRIBUTING.md), [Godot](https://raw.githubusercontent.com/godotengine/godot/master/CONTRIBUTING.md)

## 개발·CI 파이프라인 최적화

사용자는 2026-10-16 native live 작업 중 프로세스·파이프라인도 함께 최적화하도록 요청했다. runtime/window/CLI의 공개 API를 먼저 공유하고 파일 소유권과 Cargo.lock 편집자를 분리한다. 구현 중에는 package별 CPU 검사를 사용하고, 통합이 안정된 뒤 workspace·managed 회귀와 실제 Mac UI 검증을 수행한다. 동일 target cache를 재사용하고 lockfile 갱신 대기를 피하려고 별도 cold workspace를 추가하지 않는다.

native window feature가 실제 CLI 경로에 추가되므로 Rust CI의 check/test/clippy도 all-features로 검사한다. 공식 `actions/cache`의 실제 v6.1.0 commit `55cc8345863c7cc4c66a329aec7e433d2d1c52a9`과 node24 action metadata를 확인했다. registry/git source와 target 출력만 OS·architecture·작업 단계·toolchain/manifest/lock 기준으로 분리해 캐시하고 credentials/config는 포함하지 않는다. 캐시가 적중해도 `--locked`·CPU 테스트·압축 해제 후 실행을 생략하지 않는다. 시간 절약 효과는 실제 CI 실행 이후에만 수치로 기록한다. [공식 cache Action](https://github.com/actions/cache/tree/55cc8345863c7cc4c66a329aec7e433d2d1c52a9)

[Native live PR #5](https://github.com/sonan0721/Daedalus/pull/5)의 첫 검증에서 required 3개와 두 native package가 모두 통과했다. package cache가 실제 저장된 것도 확인했다. PR artifact의 exact commit은 GitHub가 checkout한 synthetic merge SHA이며 head SHA와 구분한다. 실제 Mac 입력·archive 다운로드 검증은 [작업 기록](../06-worklog/2026-10-16-native-live-session.md)에 기록한다.

## 보안·지원

private vulnerability reporting을 활성화하고 API의 `enabled: true`를 확인했다. 민감한 제보는 [SECURITY](../../SECURITY.md)의 비공개 경로를 사용한다. 일반 질문·버그·설계 제안은 이슈 양식을 사용한다. 릴리스가 없으므로 지원 중인 엔진 버전이나 응답 SLA를 발명하지 않는다.
