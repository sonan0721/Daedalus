# ADR-0006: 공통 session과 owned request 결과

상태: 구현 계약 채택·구현/제품 검증 진행 중 · 2026-10-16

## 배경

P01.1에서 외부 data-only project의 실제 ECS 실행을 연결했다. 그러나 fixture live window·JSONL·managed bridge가 서로 다른 host/control 경로를 가진다. AI-driven 제작의 같은 session 입력·관찰·표시를 제공하려면 하나의 authority와 공통 결과 수명이 필요하다. 사용자의 A-041은 반복되는 연결/검증 작업의 공통화를 요구한다. 전체 D1/D2/D3는 [ADR-0005](0005-engine-product-and-game-validation.md)에 따라 계속 미통과다.

## 결정

- native `SessionControlService`가 한 ProjectSession/Engine을 actor thread 안에 소유한다. Client는 typed 요청과 불변 frame을 사용하며 renderer/MCP/JSONL이 Engine을 하나씩 더 만들지 않는다.
- 공유 global result receiver 대신 non-Clone RequestTicket이 자기 결과를 읽는다. admission·authority acceptance·terminal outcome을 구분한다. Manual과 Realtime의 clock 의미도 명시한다.
- pending/result/frame/replay 예산을 제한하고, ticket ack/drop·외부 progress Arc 보유까지 실제 수명을 추적한다. timeout은 이미 접수된 world 명령을 되돌린다는 뜻이 아니다.
- Manual에서는 hard total 안의 Step 전용 slot/byte reserve를 둔다. 미적용 mutation이 모든 credit을 점유해 그것을 적용할 step마저 막는 deadlock을 방지한다. stop은 독립 신호다.
- 새 service의 completed replay ledger는 유한하다. caller lower-half의 새 CommandId는 session 전역 단조 증가를 요구하고, ledger 밖의 high-water 이하 ID는 expired/out-of-order로 거부한다. auto upper-half ID는 host 전역 allocator를 사용한다. 기존 bare JSONL/bridge/world를 이번 단계에서 변경하지 않는다.
- 작은 credit/latest-frame/stop primitive를 기존 LiveSession에서도 재사용하되 terrain/demo 의미는 fixture adapter에 남긴다. 미완료 migration을 공통화 완료로 표시하지 않는다.

실제 API·상한·오류·회귀는 [상세 service 계약](../03-architecture/session-control-service.md)에 둔다. 최초 actor backend는 native이며 브라우저 Worker·공유 메모리 backend는 별도 구현/실행 검증이 필요하다. 원격 인증·durable exactly-once·재접속 resume를 이 계약만으로 지원한다고 표시하지 않는다.

## 대안과 비용

한 global result queue를 여러 client가 소비하면 다른 client 결과를 가져갈 수 있어 채택하지 않는다. 무제한 결과 보존은 오래된 요청 retry를 단순하게 하지만 memory와 종료 수명을 제한하지 못한다. 유한 ledger만 쓰고 예전 ID를 fresh로 취급하면 재시도 의미가 모호하므로 high-water 오류를 선택했다.

결과 lease/ack·Step reserve·cached replay의 테스트 비용이 생긴다. caller ID를 직접 발급하는 여러 attachment는 순서를 공유해야 하므로 일반 사용은 auto ID를 기본으로 한다. 후속 wire adapter는 새 caller 정책과 expired 오류를 명시해야 한다. 이 alpha service 계약을 기존 bridge ABI의 호환 규칙으로 소급 적용하지 않는다.

새 service의 JSONL endpoint는 protocol 2로 구분한다. 기존 protocol 1의 field shape를 재사용하더라도 strict caller 정책을 몰래 소급 적용하지 않는다. 기존 bare/project serve의 실제 migration·compatibility 검증은 별도 작업이며 P02.1에서 transport를 바꾼 결과가 아니다.

## 통과 근거

이 ADR 작성 시 P02.1은 Luna xhigh가 구현 중이다. public 계약을 먼저 공유했으며 독립 감사에서 frame retention·replay 보존의 누락을 보완했다. 실제 manual/realtime·복수 client·포화/ack/drop/timeout·dedup/eviction·shutdown·legacy 회귀를 통과해야 하위 구현을 체크한다. 아직 실행하지 않은 검사를 성공으로 기록하지 않는다. visible project/MCP/live capture와 전체 P02/D1 인수는 후속이다.
