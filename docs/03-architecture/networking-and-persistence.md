# 약 30명 전용 서버·복제·월드 저장

상태: 초기 필수 아키텍처 초안. 프로토콜·전송·서버 실측 없음 · 2026-10-15

## 권한 모델

server-authoritative 모델을 기본 검토안으로 삼는다. 클라이언트는 입력·편집 요청을 보내고 서버가 권한·범위·revision을 검증해 상태를 확정한다. local·offline 실행이 필요하면 같은 명령·권한 계약을 프로세스 안에서 사용할 수 있게 한다.

30명은 사용자 목표다. 분산된 30명과 같은 장소에서 파괴·유체·물리를 일으키는 30명은 다른 부하다. tick·활성 entity/cell/chunk·편집률·입력 빈도·RTT·손실·대역폭·서버 기기를 명시해야 수용력을 판정할 수 있다.

## 공통 프로토콜 데이터

| 데이터 | 계약 |
| --- | --- |
| Session | protocol·engine·world schema·content/generator version 협상 |
| Input | client ID·sequence·대상 tick·중복/지연/손실 정책 |
| World edit | command ID·주체·권한·좌표·기대 revision·검증 결과 |
| State update | server tick·entity generation·baseline ID·delta/full 의미 |
| Chunk/region | content revision·경계·hash·분할 전송·취소·우선순위 |
| Resync | 부족 baseline·불일치 revision·재접속 범위·복구 상태 |
| Diagnostics | bytes·age·resend·drop·server correction·원인 |

로컬 ECS ID를 네트워크 개체 ID로 직접 노출하지 않는다. 중복 요청으로 블록·아이템·자원이 두 번 생성되지 않도록 command dedup과 결과를 둔다. 네트워크가 정확히 한 번 전달한다고 가정하지 않는다.

## 관심 영역과 대역폭

관심 영역은 플레이어 수요·거리·가시성·게임플레이 관련성·진입 예정 영역을 고려한다. entity snapshot·타일 변경·새 청크·effect event·채팅의 우선순위를 나눈다. 같은 청크를 여러 사람이 필요로 하면 준비 작업은 공유하고 전송 상태는 연결별로 관리한다.

`서버 송신량 ≈ 연결별(관심 entity 변화 + 월드 delta + 신규 청크 + 입력/제어 overhead)`로 계산하고, 압축 전후 bytes·갱신률·baseline 유지 비용을 포함한다. 단순 `entity 수 × 30`만으로 대역폭을 예측하지 않는다.

포화 시 시각 갱신·새 영역·장식 전송을 조절할 수 있으나, 중요한 편집 결과를 조용히 버리지 않는다. bootstrap stream이 입력·권한 ack·keepalive를 굶기지 않게 한다. 신규 접속·teleport·30명 집중 파괴를 별도 시험한다.

## 예측·보간·결정성

로컬 캐릭터 입력 예측·서버 보정과 원격 entity 보간을 나누어 설계한다. 모든 유체·복셀·물리를 클라이언트가 매 tick rollback해야 한다는 전제는 두지 않는다. 예측할 상태 범위·저장 비용·부작용 event의 취소·재적용을 명세한다.

고정 tick·동일 seed만으로 CPU·OS·병렬 순서가 다른 부동소수점 물리의 bitwise 결정성을 보장하지 않는다. 서버 권한 정합성, 동일 플랫폼 재현, cross-platform 결정성을 별도 계약으로 다룬다.

## 전송 선택과 독립성

Lightyear는 Bevy용 서버 권한 네트워킹 후보이며 결합 비용을 검토한다. 독립 transport와 엔진 소유 replication을 구성하는 안과 비교한다. native UDP·browser WebTransport/WebSocket 등 후보의 연결·reliability·congestion·배포 조건을 명세한다. [Lightyear 공식 저장소](https://github.com/cBournhonesque/lightyear)

브라우저가 native UDP에 직접 접속한다고 가정하지 않는다. 필요하다면 server의 별도 transport endpoint·gateway가 배포 산출물에 어떻게 포함되는지 설계한다. HTTP 정적 호스팅과 멀티플레이 전용 서버는 다른 역할이다.

## 저장과 장애 복구

영속 ID·schema version·generator/content version을 저장한다. 절차 생성 seed와 플레이어 변경을 분리할 수 있지만 generator upgrade가 과거 월드를 바꾸지 않도록 정책을 정한다. full snapshot과 journal의 비용·복구 시간을 비교한다.

`명령 수락 → 권한 적용 → journal → checkpoint → 복제 ack`의 순서는 내구성 계약에 따라 결정한다. 메모리 반영 완료와 디스크 내구성 완료를 같은 ack로 숨기지 않는다. 저장 중 새 수정은 revision으로 분리하고, 파일 교체·checksum·부분 기록·rollback을 시험한다.

웹과 native는 저장 의미와 schema를 같게 유지하되 backend가 다를 수 있다. 플랫폼 byte layout·포인터를 직렬화 형식으로 쓰지 않는다. 월드 export/import·migration 도구와 버전 호환 표를 후속 명세에 둔다.

## 검증

손실·중복·역순·지연·재접속·서버 crash·손상 journal·편집 충돌·오래된 baseline·불일치 content·동시 저장을 시험한다. 최소 두 클라이언트의 정합성 시험과 30명 부하 시험을 구분한다. screenshot만으로 권한 상태·저장 일치를 판정하지 않는다.
