# 출발 제안의 기술 주장 검증

상태: R01 조사. 대체 문구와 설계 방향은 제안 · 2026-10-15

## 핵심 판단

원안은 반응성·AI 반복·2D/3D 월드라는 방향은 유효하지만, 비동기·스레드 분리를 곧 무제한 처리량이나 고정 fps 보장으로 연결한다. 다이달로스에서는 **주요 경로가 장기 작업을 기다리지 않도록 설계하고, 제한된 자원 안에서 응답 지연을 측정**하는 요구로 바꾼다.

| 주장 | 판정 | 대체 명세 |
| --- | --- | --- |
| 상용 엔진과 같은 구조면 144Hz 유지 | 근거 부족 | 기준 기기·씬·해상도에서 CPU/GPU/presentation 지연을 각각 검증 |
| 메인 스레드를 단 1ms도 멈추지 않음 | 무조건 보장 불가 | 기다림·긴 임계구역·무제한 배치 제거, p99/p99.9 지연·초과 횟수 측정 |
| Bevy의 렌더 분리는 완전 무동기화 | 부정확 | 공식 경로에도 sync/extract 경계 존재. 다이달로스의 독립 게시 모델은 별도 설계 |
| 전체 ECS를 이중 버퍼링하면 해결 | 설계 누락 | 렌더가 필요한 데이터만 추출. 슬롯 소유권·독자 수명·회수·복사 비용 명세 |
| Crossbeam 큐는 무조건 락·대기 없음 | 부정확 | bounded `send`는 찰 수 있음. 중요 경로에 try API·실패 정책·메모리 한도 적용 |
| 모든 코어를 쓰면 최적 | 불충분 | 이벤트·렌더·오디오·시뮬레이션·배경 작업의 경쟁과 oversubscription 제어 |
| QuickJS 하나에서 TS 로직을 자유롭게 병렬 실행 | 잘못된 전제 | VM 소유자·메시지 경계 정의. 여러 VM이나 실험적 기능의 의미를 구분 |
| oxc 5ms이면 reload도 5ms | 범위 혼동 | 감시→의존 그래프→변환→검증→VM 평가→migration→안전한 적용을 나누어 측정 |
| TS 변환이 곧 타입 검증 | 부정확 | 변환과 의미적 타입 검사는 별도 도구·진단 경로로 설계 |
| 모니터 없는 Docker면 자동 GPU 렌더 | 불충분 | adapter·드라이버·컨테이너 장치 접근 필요. 창 없는 texture 렌더 경로 사용 |
| 프레임 CPU 복사·인코딩은 즉시 | 잘못된 성능 가정 | GPU 완료·비동기 map·복사·인코딩·출력 예산을 각각 계측 |
| Taffy로 UI 엔진 완성 | 범위 누락 | 레이아웃 외 텍스트·입력·IME·포커스·클리핑·렌더러 필요 |
| Hanabi를 독립 wgpu 파티클로 바로 연결 | 결합 비용 누락 | Bevy renderer 의존성을 검토하고 자체 파티클 경로와 비교 |
| Lightyear를 마지막에 붙이면 네트워크 완성 | 아키텍처 위험 | 초기부터 authority·ID·명령·관심 영역·저장·타깃 전송 계약 설계 |
| Jolt가 2D 타일 유체까지 해결 | 역할 혼동 | 3D rigid body와 2D 충돌·셀 유체를 별도 시스템으로 설계 |
| 10만 엔티티 유닛 테스트가 레이스·프레임 드랍 0건 증명 | 증명 범위 과장 | 소유권 리뷰·모델 검사·스트레스·실제 부하 성능 시험을 조합 |
| 무한 월드는 메모리도 계속 확장 가능 | 자원 정책 누락 | 좌표상 확장, 유한 resident set·캐시·저장·예산·정리 정책 |
| 데스크톱 스레드 구조를 웹에 그대로 적용 | 플랫폼 제약 누락 | Workers·공유 메모리·배포 헤더·GPU 객체 소유권·fallback 명세 |

## 확인한 사실의 근거

Bevy는 프레임 중첩과 sync/extract 단계를 문서화하며 해당 pipelined plugin을 non-WebAssembly 대상으로 표시한다. [공식 API](https://docs.rs/bevy/latest/bevy/render/pipelined_rendering/struct.PipelinedRenderingPlugin.html)

Crossbeam은 blocking과 non-blocking 연산을 구분한다. rquickjs는 런타임의 락과 실험적 parallel 기능을 설명한다. oxc Transformer는 TS type stripping을 설명한다. [Crossbeam](https://docs.rs/crossbeam-channel/latest/crossbeam_channel/), [rquickjs](https://docs.rs/rquickjs/latest/rquickjs/), [oxc](https://oxc.rs/docs/guide/usage/transformer)

Hanabi와 Lightyear는 Bevy 생태계용이며 Taffy는 레이아웃 라이브러리다. 이들 소개를 엔진 전체 구현으로 해석하지 않는다. [Hanabi](https://github.com/djeedai/bevy_hanabi), [Lightyear](https://github.com/cBournhonesque/lightyear), [Taffy](https://github.com/DioxusLabs/taffy)

## 논블로킹이라는 말의 사용

다이달로스의 제품 요구는 로딩·생성·저장 같은 장기 작업이 사용자 입력·UI·표시를 동기적으로 막지 않는다는 뜻이다. 자료구조의 lock-free·wait-free 같은 진행 보장은 개별 알고리즘 단위로 증거가 있을 때만 사용한다. 이벤트 대기, idle worker 주차, GPU present throttling까지 금지하는 뜻은 아니다.
