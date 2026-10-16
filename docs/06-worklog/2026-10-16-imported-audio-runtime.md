# P03.2c imported audio 작업 기록

상태: source/API 리뷰 반영·구현 계약 확정·인수 미통과 · 2026-10-16

## 이 문서를 만든 이유

[상세 계약](../03-architecture/imported-audio-runtime.md)은 CPU WAV import 이후 실제 mixer/장치·공통 세션·재시도·유한 PCM 수명을 구현하기 전에 책임과 검증 경계를 고정한다. 이 작업 기록은 설계 제안·source 확인과 실제 소리/PCM/배포 증거를 구분한다. 기존 AudioService의 synthetic PCM이나 장치 probe만으로 imported consumer를 완료 처리하지 않기 위해 만들었다.

선행 P03.2b는 [PR12](https://github.com/sonan0721/Daedalus/pull/12)의 다섯 CI/두 packaged-assets step·main `dd2de2ed37a244aa77be342c62b8afafe958d697` 동일 tree를 확인했다. 현재 branch는 `feat/asset-audio-consumer`이며 P03.2/D1/D2/D3는 계속 열려 있다.

## source 감사와 선택

- imported AudioAsset의 borrowed Arc PCM→Kira Frame bank를 unique LeafCookKey당 한 번 변환한다. 별도 sample Vec 복사를 줄이되 Frame Vec→Arc finalization peak는 예산에 포함한다.
- project actor의 기존 capability/ticket/normal credit·caller high-water/FIFO replay를 재사용한다. protocol request ID는 ACK/detach 이후 side-effect replay key가 아니다. Play와 offline clock 진행의 중복 실행·eviction을 실제 시험해야 한다.
- 장치 없는 기본/오프라인과 명시적 native feature/startup을 나눈다. native 실패는 가짜/mock/offline 성공으로 바꾸지 않는다. same actor의 headless protocol2와 native protocol2가 같은 codec/운영 의미를 사용해야 한다.
- voice는 현재 engine session에서만 유효한 non-reused sequence다. stop 요청·실제 ended 관찰·reaping을 구분한다. 실행 중 bank를 유지하고 종료 때 backend owner를 먼저 drop하되 CPAL의 detached stream-manager 최종 종료/PCM 해제를 actor join으로 인증하지 않는다.
- Luna 세 담당의 read-only source/API 리뷰를 반영했다. native alias는 실제 CpalBackend에만 연결한다. 최종 clamp가 NaN을 제거하지 않아 playback sample ±64 admission을 결정했으며 importer의 원본 finite PCM은 보존한다. 기본 API shape/기존 public struct literal을 유지하고 audio enum 확장은 opt-in 실험적0.1 변경으로 기록한다.
- world 전용 authority 구조는 그대로 둔다. audio는 typed terminal로 완료하고 기존 AuthorityAccepted 대기가 terminal도 허용하는 동작을 재사용한다. 음향 actor·별도 replay cache·별도 JSONL protocol을 늘리지 않는다.
- 새 code는 아직 없다. 이 계약 commit 이후 파일 소유권·manifest/단일 Cargo 순서를 지정해 구현한다. 실제 테스트/시간/장치/패키지 결과는 실행 뒤 추가한다.

## 조사 한계

버전 고정 docs.rs API 세 페이지 조회는 실패했다. local registry의 Kira0.12.5 source를 읽어 Frame Arc sharing/loop/state/final clamp를 확인했다. 이후 local VCS metadata의 commit `74b3ef0fd8bd15f90e3b469208ee741f04374a62`로 upstream StaticSoundData raw 원문을 확보·대조했으며 다른 raw 페이지 일부는 실패했다. browser audio·loopback/audibility·다른 장치/플랫폼은 현재 미검증이다.
