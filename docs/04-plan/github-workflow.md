# GitHub 운영과 현재 검증 절차

상태: 2026-10-15 실제 저장소 설정 확인. 엔진 테스트·릴리스는 아직 없음.

## 공개 저장소

[sonan0721/Daedalus](https://github.com/sonan0721/Daedalus)는 공개 엔진 저장소다. 원본 엔진·도구·문서는 MIT OR Apache-2.0이며 두 라이선스 원문을 포함한다. GitHub의 자동 라이선스 요약이 한 라이선스만 표시하더라도 선택 조건은 루트 README와 라이선스 파일로 명시한다.

## main에 적용한 규칙

- PR 경로로 변경하고 필수 `docs` 체크를 통과해야 한다.
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

현재 문서 검사와 검사기 테스트 8건을 실행한다. 문서 CI가 통과했다고 Rust 엔진·GPU·웹·30명·144fps가 검증된 것은 아니다. [최초 CI 실행](https://github.com/sonan0721/Daedalus/actions/runs/38741106943)은 성공했다.

CI는 read-only contents 권한으로 실행하고 checkout Action을 확인한 commit SHA에 고정했다. 공개 PR이 권한 있는 `pull_request_target` 경로에서 코드를 실행하는 구성을 사용하지 않는다. [GitHub Actions 공식 보안 문서](https://docs.github.com/en/actions/reference/security/secure-use)

## 기여·PR의 실제 흐름

1. 큰 변경은 요구·goal·ADR·이슈로 문제와 방향을 기록한다.
2. 작업 branch에서 작은 변경과 의미 있는 검증을 수행한다.
3. PR에 목적·실제 동작·근거·한계·실행한 검사를 적는다.
4. 조정자가 Luna xhigh의 프로젝트 소유 코드를 리뷰하고 CI를 확인한다.
5. 필수 체크·대화를 충족한 뒤 squash 병합한다. 구현·검증 결과는 작업 기록에 남긴다.

기여 문서의 구조는 Bevy·Godot의 공식 기여 입구를 참고하되 대규모 팀 규칙을 그대로 복제하지 않았다. [Bevy](https://github.com/bevyengine/bevy/blob/main/CONTRIBUTING.md), [Godot](https://raw.githubusercontent.com/godotengine/godot/master/CONTRIBUTING.md)

## 보안·지원

private vulnerability reporting을 활성화하고 API의 `enabled: true`를 확인했다. 민감한 제보는 [SECURITY](../../SECURITY.md)의 비공개 경로를 사용한다. 일반 질문·버그·설계 제안은 이슈 양식을 사용한다. 릴리스가 없으므로 지원 중인 엔진 버전이나 응답 SLA를 발명하지 않는다.
