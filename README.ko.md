# esp32-agent

[English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · [Español](README.es.md) · [Русский](README.ru.md) · [العربية](README.ar.md) · [日本語](README.ja.md) · **한국어**

자체 에이전트 루프를 실행하고 MCP 엔드포인트를 제공하는 ESP32용 펌웨어입니다. 하나의 모델이
보드를 직접 제어할 수 있으며, 외부의 어떤 에이전트든 HTTP를 통해 같은 도구를 사용할 수 있습니다.

플래시하고 WiFi에 연결하면 드라이버가 있는 버스를 탐색합니다. 응답하는 각 장치가 MCP 도구가
됩니다. 보드의 에이전트도 같은 도구를 보고 이를 사용해 응답합니다. 보드가 실행 중인 상태에서도
JSON으로 새 센서를 선언할 수 있습니다.

> 에이전트 루프는 [FutureOS](https://github.com/futuregene/future-os)의 방식을 따릅니다.
> 대화 기록, 도구 저널, 스트림 이벤트, 프로토콜 어댑터, 메모리 인덱스가 포함됩니다.

| 기능 | 상세 |
|---|---|
| 보드의 모델 | 루프, 도구, 메모리, 대화 기록은 펌웨어에 있으며, 모델은 OpenAI 호환 또는 Anthropic 엔드포인트라면 무엇이든 사용할 수 있습니다. [모델](docs/model.md) |
| 매니페스트 기반 도구 | `manifest` (`command: apply`)가 JSON 매니페스트를 검증하고 도구로 컴파일하여 같은 턴 안에 활성화합니다. [매니페스트](docs/manifest.md) |
| 자동 장치 탐색 | 부팅할 때 I2C, 1-Wire, 선언된 핀을 탐색하며, 응답하는 대상에만 도구가 생깁니다. [기능 목록](docs/capabilities.md) |
| 도구 집합 하나, 호출자 둘 | MCP 엔드포인트와 보드의 에이전트는 하나의 설명에서 나온 같은 도구를 봅니다. [HTTP 및 MCP API](docs/http-api.md) |
| HTTPS 콘솔 | 휴대전화에서 모델과 대화하고 장치, 도구, 일정, 로그, WiFi, 모델을 설정할 수 있습니다. 보드가 자체 인증서를 발급합니다. [웹 콘솔](docs/web-console.md) |
| 무인 운영 | 시간에 맞춰 실행하는 프롬프트와, 측정값이 임계값 경계를 넘을 때 프롬프트를 실행하는 센서 규칙이 있습니다. [스케줄링](docs/scheduling.md) |
| 셸과 스크립트 | NuttX의 명명 방식을 따른 명령 57개를 갖춘 `shell`과, 도구 호출을 반복하는 인터프리터가 있습니다. [셸](docs/shell.md) · [스크립트](docs/scripting.md) |
| 장기 메모리 | 상시 지침, `/AGENTS.md`, `/MEMORY.md` 인덱스, 스킬을 매 턴 프롬프트에 조합합니다. [컨텍스트 계층](docs/context.md) |
| 내장 스킬 | 펌웨어에 포함된 절차는 표준 `---` 헤더로 작성된 `create-skill` 하나이며, 이 절차가 나머지를 가르칩니다. 스킬이 무엇이고 어떻게 작성하는지 설명합니다. 첫 부팅 때 `/skills`에 설치되며, 편집한 내용은 그대로 유지됩니다. [스킬](docs/agent-skills.md) |
| 보드 자체가 서버 | 보드가 HTTPS를 제공하고 인증서를 직접 생성하며, 호출자 셋이 PIN 하나를 공유합니다. [보안](docs/security.md) |
| 하드웨어 없이 테스트 | 시뮬레이터가 가상 보드에서 동일한 `agent-core`를 실행합니다. [시뮬레이터](docs/simulator.md) |

## 플래시 및 연결

```bash
espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin
```

보드에는 WiFi 자격 증명이 없으므로 `esp32-agent-setup`이라는 이름의 액세스 포인트를 시작합니다.
여기에 연결하고 `http://192.168.4.1/`을 열어 네트워크 이름과 비밀번호를 입력한 뒤 저장합니다.
자격 증명은 NVS에 저장되고 보드는 다시 연결합니다. 그때까지는 LAN 밖의 어떤 곳에도 접근할 수
없습니다. 모델 엔드포인트에도, 패키지 미러에도 연결되지 않습니다.

## 사용 방법

**브라우저에서**: `http://<board>/`(또는 `https://<board>/`)가 콘솔입니다. 모델과 대화하고,
탐색으로 발견한 장치를 나열하고, 도구를 켜거나 끄고, 로그를 읽고, PIN을 설정할 수 있습니다.

**에이전트에서**: `GET http://<board>/agent.json`은 인터페이스를 JSON으로 반환합니다. 각 경로의
메서드, 본문, 인증, MCP 인터페이스, 형식과 기본값을 포함한 설정 표, 각종 제한이 담겨 있습니다.
URL을 가져올 수 있는 에이전트라면 다른 것은 필요하지 않습니다.

**새 보드는 이미 배우는 방법을 압니다.** `/skills/create-skill`은 초기 설치되는 유일한 절차입니다. 스킬이 무엇인지, 반드시 갖춰야 할 `---` 헤더는 어떤 것인지, 다음 세션이 찾을 수 있는 스킬을 어떻게 작성하는지 설명합니다. 그 밖의 모든 것, 즉 작업대의 핀 배치나 보드가 고장 난 것처럼 보일 때의 대처 방법 등은 에이전트가 스킬로 작성한 뒤 보관합니다. 이는 일반적인 작업 공간 파일이며, 첫 부팅 때 해당 경로에 파일이 없으면 설치됩니다. 공장 초기화를 하면 복원되며, `skills.seed_defaults=false`는 이를 막습니다. 갓 플래시한 보드에 처음 건넬 유용한 말은 "자신의 핀 배치를 기록해 둬"일 수 있습니다.

## 모델 설정

`http://<board>/`를 열고 **Agent**를 선택한 뒤 엔드포인트, 모델 id, 키를 입력하고 저장합니다.
키는 쓰기 전용입니다. NVS에 저장되고, 읽으면 `__set__`으로 반환됩니다. 같은 패널에서
`protocol`(`chat`, `responses`, `anthropic` 중 하나), `reasoning`, `context_tokens`,
`max_output_tokens`도 설정할 수 있습니다.

**에이전트에게 맡겨도 됩니다.** 에이전트가 `agent.json`을 가져오면 이 설정들의 형식과 기본값까지
알게 되므로, 프롬프트 하나면 충분합니다.

> 보드의 엔드포인트를 `https://your-provider/v1/chat/completions`, 모델을 `…`, 키를 `sk-…`,
> 추론을 `low`, 컨텍스트를 `64000`, 출력을 `4096`으로 설정해 줘

에이전트는 패널과 동일한 호출인 `POST /api/agent/config`로 설정을 적용한 뒤 다시 읽어 확인합니다.
사용자 쪽에서는 명령줄이 필요하지 않으며, 에이전트가 접근할 수 있는 모든 보드에 같은 프롬프트를
사용할 수 있습니다. 키가 전혀 없어도 `base_url`을
`http://127.0.0.1/selftest/chat/completions`로 설정하면 보드 자체를 상대로 루프를 실행합니다.

## 안전 참고 사항

- **자격 증명은 보드에 남습니다.** WiFi 자격 증명은 `provision`을 통해 NVS에 저장되며, 이미지에
  들어가지 않습니다. `api_key`는 쓰기 전용이고 읽으면 `__set__`으로 반환됩니다.
- **보드가 책상을 떠나기 전에 PIN을 설정하세요.** `auth.portal_pin`은 모든 쓰기 작업과, 대화 기록,
  프롬프트, 설정, 로그를 노출하는 읽기 작업을 보호합니다.
- **인증서를 대조하세요.** 보드의 TLS 인증서는 자체 서명이며, 지문은 `GET /api/tls`에서 확인할 수
  있습니다. 포트 80은 기본적으로 열려 있으므로 인증서가 잘못되어도 잃는 것은 URL 하나이지 보드에
  대한 접근 자체가 아닙니다. `mcp.plain_http: false`로 포트를 닫으면, 다시 접근할 수 있는 수단은
  USB 케이블이 됩니다.
- **`shell`, `script`, 매니페스트 도구는 핀 상태를 바꿉니다.** 그것이 바로 이 보드의 목적입니다.
  PIN은 에이전트가 제 일을 하는 것과 지나가던 사람이 같은 일을 하는 것을 구분합니다.

[보안](docs/security.md) 문서에 의도적으로 보호하지 않는 대상이 명시되어 있습니다.

## 상태

| 부분 | 상태 |
|---|---|
| `agent-core` | 단위 테스트 794개와 통합 테스트 5개 통과. `scripts/smoke.ps1`도 엔드투엔드로 통과 |
| 시뮬레이터 | 콘솔, MCP, 에이전트 루프가 엔드투엔드로 동작 |
| 펌웨어 | ESP32-S3-DevKitC-1 N16R8(IDF v5.5.2)에 플래시됨: 도구 42개, OLED 128×32, I²S 마이크와 앰프, WS2812, TLS 콘솔, HTTPS를 통한 모델 턴 |
| 미구현 | OTA, SD 카드, DHT11/22, 카메라. `ROADMAP.md` 참고 |

작업대에서의 측정값: 도구 네 개를 사용하는 턴은 13.7 s, TLS를 통한 콘솔은 0.55 s입니다.
I²C 스캔은 8회 실행에서 안정적입니다. TLS 페이지를 읽은 뒤 내부 SRAM의 여유 공간은
5–6 kB로, 여기서 가장 빠듯한 제약입니다.

## 문서

[아키텍처](docs/architecture.md) · [매니페스트](docs/manifest.md) · [기능 목록](docs/capabilities.md) ·
[HTTP 및 MCP](docs/http-api.md) · [장치 추가](docs/adding-a-device.md) · [플래시](docs/flashing.md) ·
[문제 해결](docs/troubleshooting.md) · [에이전트 루프](docs/agent-loop.md) · [스트림](docs/agent-stream.md) ·
[스킬](docs/agent-skills.md) · [메모리](docs/agent-memory.md) · [스크립트](docs/scripting.md) ·
[모델](docs/model.md) · [웹 콘솔](docs/web-console.md) · [스케줄링](docs/scheduling.md) · [셸](docs/shell.md) · [컨텍스트 계층](docs/context.md) · [보안](docs/security.md) · [시뮬레이터](docs/simulator.md) · [오디오](docs/audio.md)

## 기여

새 주변 장치는 `examples/manifests/` 아래의 매니페스트 조각으로 시작합니다. 타이밍, 보정 또는
상태가 필요한 경우에만 Rust 드라이버가 필요합니다. 드라이버는 `CATALOG`에서 자신을 등록합니다.
규칙은 [AGENTS.md](AGENTS.md)와 [CONTRIBUTING.md](CONTRIBUTING.md)에 있습니다.

## 라이선스

MIT. [LICENSE](LICENSE)를 참고하세요.
