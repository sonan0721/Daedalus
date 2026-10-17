# ADR-0007: 재사용 native host와 generation consumer 경계

상태: 구현 계약 채택·b.3 구현 전 · 2026-10-17

## 배경과 결정

A-041의 반복 연결 최적화와 D1의 공개 엔진 SDK 요구에 따라 CLI 안의 실행 조합을 별도 native host library로 재사용한다. CLI/MCP는 transport adapter이며 두 번째 Engine/actor/device를 만들지 않는다. Rust core/C# SDK·플랫폼·144fps 품질 요구를 변경하는 결정은 아니다.

- runtime은 source/authority를, renderer는 device/cache/실제 표시를 소유한다. 둘을 native host가 조합하며 runtime→renderer 또는 renderer→runtime 의존을 추가하지 않는다.
- startup에 render consumer를 선언하고 runtime-neutral lease·표준 erased Arc pin·유한 Queued/Applied 보고로 Stage→Commit→present를 연결한다. 기존 headless의 CPU Staged 의미는 보존한다.
- host는 값싼 typed coordinator와 장수하는 CPU frame/prepare/reclaimer owner 하나를 둔다. 무거운 prepare 때문에 입력/control relay가 멈추지 않게 한다. 기존 TaskPool의 shared completion receiver를 다른 poller가 가져가지 않는다.
- UI는 Window/Surface와 PacketRenderer를 소유한다. view 종료 뒤 Window/Surface는 같은 thread에서 해제하고 PacketRenderer만 기존 CPU owner에 반환해 nonwaiting completion drain/최종 drop을 맡긴다. per-close janitor thread를 만들지 않는다.
- resource-only 준비와 기존 planner/cache 재사용·bounded chunk+기존 화면의 한 submit을 채택한다. initial GPU 준비는 clear/loading pass와 조합하고 가짜 game packet/ACK를 만들지 않는다.
- CPU 실제 backing·GPU upload completeness·submission completion·source charge를 구분한다. marker/wrapper refcount만으로 회수를 보고하지 않는다. 숫자와 renderer public API의 최종 동결은 별도 상세 계약에 따른다.

정확한 runtime API는 [consumer 계약](../03-architecture/project-generation-consumer.md), host/renderer 경계는 [host](../03-architecture/native-project-host.md)와 [staged renderer](../03-architecture/staged-render-asset-generation.md)에 둔다. 이 ADR은 구현/성능/제품 인수 증거가 아니다.

## 대안과 비용

CLI에서만 연결하면 각 게임/SDK가 실행과 shutdown을 다시 구현해야 하므로 채택하지 않는다. runtime이 renderer를 직접 소유하면 headless/서버·다른 frontend 경계가 흐려진다. 전역 queue/무제한 retired list·세대별 device 생성도 기존 비동기 소유권과 cache 재사용을 훼손한다.

대신 source lease·ACK·canonical leaf registry·window 반환의 구현/테스트 비용이 생긴다. initial CPU load는 첫 slice에서 동기이며 CPU 로딩 이전부터 UI를 표시하는 D1 bootstrap은 여전히 필수 후속이다. arbitrary trusted native clone/RSS·driver 호출의 hard latency 상한을 보장하지 않는다. C#·web/다른 platform adapter의 실제 실행 검증도 별도다.
