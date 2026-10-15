# 3D 복셀·일반 오픈월드 스트리밍

상태: 엔진 서브시스템 설계 제안. 게임·샘플 프로젝트 없음 · 2026-10-15

## 데이터 모델

수평 확장을 표현하는 정수 chunk 좌표 + chunk 내부 cell 좌표를 권한 데이터로 삼는 안을 비교한다. 렌더·물리에는 카메라 근방의 local origin을 사용한다. 좌표 타입·음수 나눗셈·변환 범위·origin shift 시점을 명세한다. “무한”은 좌표상 확장이며 resident 메모리는 유한하다.

고정 chunk grid + palette/dense data를 기준 후보로 삼고 octree는 희소성·조회·수정·압축·LOD 비용과 비교한다. 처음부터 전체 cell을 ECS entity 하나씩으로 표현하지 않는다. ECS는 chunk의 수명·상태·참조 관리에 활용하는 안이다.

## 청크 수명

`Requested → Loading/Generating → DataReady → DerivedJobs → RenderReady / CollisionReady → Active → EvictPending → Persisted/Released`

렌더 준비와 충돌 준비는 병렬 상태이며 하나의 선형 ready로 숨기지 않는다. 영속 변경이 있으면 저장 완료 조건을 충족하기 전에 데이터만 버리지 않는다. generation·content hash·dirty revision·neighbor revision을 별도로 추적한다.

## 작업과 버전 계약

| 단계 | 입력·출력 | 검증·예산 |
| --- | --- | --- |
| load/generate | seed·generator version·chunk key·저장 변경 → 불변 cell data | I/O·CPU·bytes·동시 요청·취소 |
| mesh | cell revision + 필요한 neighbor 경계 → vertex/index/material groups | 재질·빛·AO·neighbor revision 일치 |
| light | 광원·차폐·이웃 경계 → light field | 추가·제거·경계 전달·수렴 예산 |
| collision | 권한 geometry revision → shape/query data | 플레이 영역 마감·안전 정책 |
| upload | 검증된 mesh → GPU handle generation | frame bytes/time·VRAM credit |
| save | dirty revision → journal/checkpoint | write ack·hash·새 수정과 경쟁 |

worker는 live chunk를 공유 가변 수정하지 않고 고정 입력을 받는다. 결과는 `chunk lifetime generation + data revision + 경계 revision 집합 + job generation`이 맞을 때만 적용한다. 이웃이 바뀌거나 chunk가 unload·재사용되면 과거 결과를 버린다.

## 메시 생성

culled face와 greedy meshing을 기준 후보로 비교한다. greedy는 합칠 수 있는 인접 면을 묶는 방법이며 모든 형태에서 최적 면 수를 보장하는 구현으로 가정하지 않는다. [원저자 설명](https://0fps.net/2012/06/30/meshing-in-a-minecraft-game/)

면 결합 key에는 face 방향·material·투명 정책·빛·AO·UV 의미를 포함한다. PBR 재질이 다르거나 corner 값이 다른 면을 무조건 합치지 않는다. 단색·빈 공간·checkerboard·얇은 지형·많은 재질·투명 인접·청크 경계를 시험한다. LOD·부드러운 표면·경사 블록은 사용자 요구와 별도 알고리즘 비교 후 정한다.

## 스트리밍과 과부하

수요는 camera·플레이어 충돌 범위·서버 관심 영역으로 계산한다. 근접 안전 영역·가시성·방향 선행 로딩·오래 기다린 작업을 반영하고, 이동으로 불필요해진 요청은 취소·낮은 우선순위로 변경한다. 재계산 폭주를 막기 위해 chunk별 요청을 통합한다.

CPU data·mesh 대기·GPU residency·in-flight 작업·저장 대기를 각각 bytes와 개수로 제한한다. 빠른 이동·teleport는 유한 큐와 플레이 영역의 안전 정책으로 처리하며, 렌더가 새 지형 완성을 기다리게 하지 않는다. 이전 시각 표현·placeholder의 허용 범위와 플레이어 진입 제한은 명세해야 한다.

## 편집·조명·충돌

설치·파괴는 권한 command로 적용하고 관련 경계 neighbor를 무효화한다. 광원 제거와 차폐 변경에는 기존 빛을 지우고 다시 전파하는 경로가 필요하다. 무제한 flood fill을 한 tick에 수행하지 않는다. revision 있는 frontier를 예산 내에서 처리한다.

mesh·light·collider의 지연은 따로 측정한다. 권한 terrain data로 즉시 block query를 제공하거나 안전 proxy·진입 제한을 두는 후보를 비교한다. 눈에 보이는 블록과 통과 가능한 공간의 불일치를 무시하지 않는다.

## 일반 3D 오픈월드와의 공통점·차이

공간 수요·asset residency·취소·예산·local origin·저장·관심 영역은 공통 서비스로 제공한다. 일반 메시·scene instance·skeletal actor는 복셀 cell 모델을 강요하지 않는다. 일반 월드 partition·HLOD/LOD·navigation·animation 스트리밍은 후속 상세 명세로 분리한다.

## 검증과 미결

검증에는 음수 좌표, 청크 이음새, 같은 seed, generator upgrade, 저장 중 편집, 오래된 mesh, 급회전·teleport, 반복 파괴, collider 지연, 30명 분산/집중 수요를 포함한다. chunk 크기·시야 거리·수정률·예산·충돌 마감은 미결이며 수치 없이 완료로 처리하지 않는다.
