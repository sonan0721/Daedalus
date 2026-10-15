# 기술·범위·운영 위험 등록부

상태: R01. 위험을 제거했다는 기록이 아니라 추적 시작 · 2026-10-15

| ID | 위험과 발생 조건 | 영향 | 대응·검증 | 닫는 목표 |
| --- | --- | --- | --- | --- |
| RSK-01 | 1인+AI에서 2D·3D·모든 플랫폼·모든 고급 기능을 동시에 구현 | 통합·리뷰·유지보수 과부하 | 장기 범위 유지 + 모듈별 명세·작은 goal·게이트·동시 WIP 제한 | R02·R08 |
| RSK-02 | `latest` 라이브러리의 Bevy 버전·feature 불일치 | 중복 의존·renderer 결합·업그레이드 비용 | 고정 BOM·manifest·독립 부품 경계·대안 비교 | R03 |
| RSK-03 | queue·snapshot·worker 결과를 무한 축적 | 메모리 증가·반응성 붕괴 | byte/count credit·admission·revision 폐기·retention 시험 | R04 |
| RSK-04 | 전체 ECS 복제 또는 잘못된 2slot 덮어쓰기 | 복사 비용·데이터 경쟁·잘못된 화면 | 최소 extract·불변 게시·독자 수명 모델 | R04 |
| RSK-05 | ECS·Rayon·Jolt가 각자 full-core pool 사용 | CPU 경쟁·tick/UI 지연 | 프로세스 실행 예산·job adapter·우선순위·trace | R03·R04 |
| RSK-06 | Web 공유 메모리·GPU 소유·host 조건 미고려 | 웹 멀티스레드·export 실패 | browser/worker·배포 계약·최소 타깃 실험 | R05 |
| RSK-07 | C# PC CoreCLR과 후속 web/mobile 실행·패키징 차이를 늦게 발견 | SDK·모드·배포 경로 재설계 | PC 동적 모드와 공통 게임 의미를 분리·native host 배포 검증·플랫폼 conformance | R03·R05 |
| RSK-08 | managed GC/JIT·무한 script·긴 native host 함수 | tick 과부하·취소 불가 | 등록된 실행·협조 취소·batch·allocation 관측·신뢰 경계·오류 시험 | R03·R04 |
| RSK-09 | mesh·light·collider·save가 서로 다른 revision | 틈·빛 잔상·통과·저장 불일치 | 작업 입력 revision·경계·apply 검증·안전 정책 | R06 |
| RSK-10 | 유체 병렬 destination 경쟁·경계 중복 | 질량 생성/소멸·순서별 결과 변화 | reference solver·보존·capacity·결정적 transfer | R06 |
| RSK-11 | UI를 Taffy만으로 끝낸다고 가정 | 텍스트·IME·focus·접근성 누락 | UI 계층별 명세·실제 입력·Web 어댑터 | R07 |
| RSK-12 | script·신규 asset·GPU readback이 frame에 동기 대기 | 로딩 중 UI 끊김 | staging·예산·capture ring·실패 시 이전 상태 | R04·R07 |
| RSK-13 | 30명 모두 집중 파괴·신규 청크 요청 | authority·I/O·전송 병목 | AOI·우선순위·부하 분포·대역폭·30명 harness | R06 |
| RSK-14 | 저장 ack를 내구성 완료로 잘못 표시 | crash 후 유저 변경 손실 | journal/checkpoint·ack 의미·장애 주입·RPO/RTO | R06 |
| RSK-15 | 고급 효과 추가만으로 참조 게임 품질 달성 가정 | 스타일·움직임·아트 파이프라인 부족 | 참조별 기능·아트·엔진 책임·화질/행동 카드 | R07 |
| RSK-16 | 기준 Mac 18GB에서 고해상도·청크·GPU 과다 점유 | 통합 메모리 압박·swap·성능 급락 | 통합 메모리 예산·residency·high-water·해상도 프로파일 | R02·R07 |
| RSK-17 | 144fps 목표와 실제 display cadence 혼동 | 잘못된 성능·표시 주장 | CPU/GPU 완료·present·고주사율 화면을 분리 | R02 |
| RSK-18 | 공개 에셋·Spine·SDK 조건을 엔진 라이선스로 덮음 | 재배포·기여·포팅 제한 | provenance·별도 조건·비공개 SDK 경계 | R03·R05·R07 |
| RSK-19 | AI 출력의 그럴듯한 API·성능·문서 오류 | 빌드 실패·허위 지원·설계 부채 | 1차 자료·리뷰·실제 명령·구현/계획 구분 | 전 단계 |
| RSK-20 | 불신 PR CI에 과한 권한·임의 실행 | 저장소·credential 노출 | 최소 권한·Action SHA 고정·PR용 검사·비밀 분리 | R01·릴리스 |
| RSK-21 | ALC를 sandbox·강제 종료 수단으로 오해하거나 참조 회수 실패 | 모드 권한 오해·반복 reload 누수·hung system | 신뢰한 in-process 플러그인 계약·협조 취소·세대/참조 추적·격리 별도 설계 | R03·R04 |
| RSK-22 | Rust/C++ ABI·allocator·함수 포인터 수명 혼합 | crash·메모리 오류·타깃 호환 실패 | Rust 빌드형 등록·native hot-reload 제외·타깃 C ABI·어댑터 검증 | R03·R04 |
| RSK-23 | 오토파일럿의 debug 조작·입력 수락·오래된 화면을 성공으로 간주 | 실제 플레이·서버 회귀 누락 | 정상 입력 경로·server ack·적용 tick/snapshot/frame 연결·취소 시험 | R04·R06 |

## 변경과 대응

심각도·발생 가능성의 숫자를 근거 없이 정밀하게 매기지 않는다. 각 위험은 관측 징후·영향 문서·실험·결정자를 붙여 갱신한다. 검증으로 줄어든 범위만 닫고 남은 플랫폼·작업량까지 해결된 것으로 확대하지 않는다.

사용자가 품질을 우선한다고 했으므로 목표를 임의로 낮춰 위험을 숨기지 않는다. 필요한 경우 기능별 구현 순서·중간 증거·장기 범위를 구분하고, 의미가 달라지는 변경은 답변·ADR로 남긴다.
