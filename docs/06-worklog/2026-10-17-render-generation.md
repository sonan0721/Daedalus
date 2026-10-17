# P03.3b.3 Render generation 구현 기록

작성일: 2026-10-17 · 전체 D1/D2/D3·b.3 미완료

## 선행 b.2 인수

[PR16](https://github.com/sonan0721/Daedalus/pull/16)은 exact head `523757a7ff92e9fc36a8a83a0bba661dec47a44c`의 다섯 성공 뒤 squash main `360ceb6cf13fb3682e2c9cf97ced06a3e37c7b9a`로 병합했다. 실제 CI checkout `8d5bbf1255ac362e61b8b43996e1828762a2fcaa`·head·main의 tree는 `57d87c2ab924e5efd2690313df7330e0eab1f0db`로 같다.

로컬 workspace326/0/ignored13·strict runtime Clippy, 원격 Rust326/0/ignored12·실제 두 회귀/strict Clippy·managed/docs·두 package CPU62가 통과했다. 최종 CI의 macOS6.744616초/Linux1.034006초, binary hashes는 각각 `71b6940b12a7a5fac2e803f0654facc8e401c59b633941c0a68d2c12cc20b8eb` / `d336d4dbe7f3165f1659bc9dfdb3fedbd057eedb20d3d20b5e6f0d8d59fd260f`다. Linux hash를 이전 artifact와 같다고 가정하지 않는다. Publish skipped·tag/Release 없음이다.

실제 장치 combined72는 c9f5dee clean extracted binary SHA-256 `b6ab0afc72d6c82a2eeab8116a9b877f9fcadd24e5fe381e7db5f7cc4f098596`에서 통과했다. 최종523757a clean archive를 별도로 만들어 archive/manifest/CPU smoke를 검증했고 tar 안의 binary·notices·inventory hashes가 그 장치 검증 artifact와 모두 같았다. test-only/MD 외 production/helper 변경이 없으므로 device suite를 중복 실행하지 않았다. 최종 archive hash는 `e75927c373f3ce3caab32bd235acfd38c6c8bf6508f1edb221d54bf6ddeb824c`, build ID는 `20261017T000251Z-94b20ce25a91459381f36d4dab9ab4a9`다. source manifest는 exact523757a/dirty=false다.

raw `p03-replay-terminal-ci-rust-root-attempt01.log` SHA-256은 `7279b267c324edd4a20125613a3b0d2b6165f10a617b796850647a4b4c2f7d0c`, 최종 PR metadata JSON은 `beb2b4377a4eff037342c35f84b5a960022c0003a073fd8155ab54a78f8cec97`이다. raw는 ignored dist/project-evidence에 보존한다. 이전 native 실패·회복의 정확한 범위는 [선행 기록](2026-10-17-native-host-design.md)에 남긴다. 원인 미확정인 환경 회복을 test-only 수정 효과로 부르지 않는다.

병합 후 exact main360ceb6의 docs38984810552·managed38984810514·Rust38984810521·release38984810596도 성공했다. release의 macOS117253678189/Linux117253678395는 성공, Publish117254035947는 skipped다. 이 추가 확인은 실제 main workflow/job 결과이며 새 native 장치 검사를 수행했다는 뜻은 아니다. package jobs 원본 JSON `p03-b2-main-ci-root-attempt01.json`의 SHA-256은 `ffff19b47d395cbd1d04207cbceb27dbc1bd6458e6114dc8c4cec021ca5f3186`이다.

## 새 문서와 첫 작업

[ADR-0007](../05-decisions/0007-reusable-native-host-and-generation-consumer.md)은 CLI 밖의 host·의존·thread/회수 선택을 기록한다. [generation consumer](../03-architecture/project-generation-consumer.md)는 첫 runtime handshake API·Queued/Applied·작은 publication token·ACK 대기 pin을 동결한다. 이 두 문서는 구현 증거가 아니다.

첫 코드 소유자는 Luna xhigh 한 명이며 runtime source만 변경한다. renderer/host/CLI는 이 public API가 local 검사·리뷰를 통과한 뒤 연결한다. 이후 renderer의 immutable requests/CPU validation/실제 leaf registry·bounded device staging, native host·window disposition/CLI adapter를 순서대로 통합한다. 같은 window/device/session의 실제 source edit→ready→Commit→현재 pose/pixel/present→old backing 회수까지 확인한 뒤에만 b.3를 인수한다.

현재 이 기록 시점에는 b.3 코드/새 API가 없다. 워터폴 선행 인수가 끝났다는 뜻이며 엔진 제품이나 두 게임이 완성된 것은 아니다.

## CPU renderer 계약 추가

[render-cpu-generation](../03-architecture/render-cpu-generation.md)은 독립 leaf alias·prepared/frame/scratch 장부와 첫 committed frame 예약을 코드 전에 고정한다. source 감사에서 두 sidecar map의 중복 capacity와 maximum instance count/metadata budget의 독립성을 확인했다. 실제 type size/capacity 경계와 회수 시험은 구현 후 검증한다. first CPU API의 prepare는 동기 owner 작업이며 취소 checkpoint를 frame 선점으로 부르지 않는다. 실제 host의 frame 우선/pose 지연 검증은 b.3 미통과 조건으로 남긴다.

[render-device-generation](../03-architecture/render-device-generation.md)은 resource-only input/같은 planner·4MiB/8 actual-create의 combined submit·partial completeness와 upload/use serial을 동결한다. source 감사에서 기존 commit_resources의 전체 생성/upload와 saturating submission serial을 staged transaction으로 그대로 재사용할 수 없음을 확인했다. 실제 GPU 준비/취소/픽셀/drain은 후속 테스트 대상이며 문서/코드 작성으로 통과 처리하지 않는다.
