# esp32-agent

**Español** · [English](README.md) · [简体中文](README.zh.md) · [Français](README.fr.md) · [Русский](README.ru.md) · [العربية](README.ar.md)

Firmware para un ESP32 que ejecuta su propio bucle de agente y sirve un endpoint MCP, de modo
que un modelo puede gobernar la placa directamente y cualquier agente externo puede usar las
mismas herramientas por HTTP.

Se graba, se conecta a la WiFi y sondea los buses para los que tiene drivers. Cada dispositivo
que responde se convierte en herramientas MCP. El agente de la placa ve esas mismas
herramientas y responde con ellas; se puede declarar un sensor nuevo en JSON con la placa en
marcha.

> El bucle de agente sigue a [FutureOS](https://github.com/futuregene/future-os): transcripción,
> diario de herramientas, eventos de flujo, adaptadores de protocolo, índice de memoria.

| Qué | En detalle |
|---|---|
| Un modelo se ejecuta en la placa | El bucle, las herramientas, la memoria y la transcripción son firmware; el modelo es cualquier endpoint compatible con OpenAI o de Anthropic. [Cómo darle un modelo](docs/model.md) |
| El conjunto de herramientas es un documento | `manifest_apply` valida un manifiesto JSON, lo compila en herramientas y está activo en el mismo turno. [Manifiesto](docs/manifest.md) |
| Encuentra su propio hardware | I2C, 1-Wire y los pines declarados se sondean al arrancar; solo lo que responde obtiene herramientas. [Capacidades](docs/capabilities.md) |
| Un conjunto de herramientas, dos invocadores | El endpoint MCP y el agente de la placa ven las mismas herramientas, desde una sola descripción. [API HTTP y MCP](docs/http-api.md) |
| Una consola sobre HTTPS | Hablar con el modelo y configurar dispositivos, herramientas, programación, registros, WiFi y el modelo desde un teléfono; la placa emite su propio certificado. [Consola web](docs/web-console.md) |
| Actúa sin nadie delante | Prompts temporizados y reglas de sensores que lanzan un prompt cuando una lectura cruza un umbral. [Trabajo programado](docs/scheduling.md) |
| Un shell y un lenguaje de script | `shell` con 26 comandos con nombres de NuttX, y un intérprete para bucles sobre llamadas a herramientas. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Recuerda lo que se le dijo | Órdenes permanentes, `/AGENTS.md`, un índice `/MEMORY.md` y habilidades, ensamblados en el prompt en cada turno. [Lo que el modelo ve en cada turno](docs/context.md) |
| Sin nube en el camino | HTTPS desde la placa, un certificado que genera ella misma, un PIN para tres invocadores. [Autenticación](docs/security.md) |
| Comprobable sin hardware | El simulador ejecuta el mismo `agent-core` contra una placa virtual. [Simulador](docs/simulator.md) |

## Grabación y conexión

```bash
cargo install espup espflash ldproxy
espup install
powershell scripts/build-firmware.ps1 -Flash -Port COM3
```

**¿Sin toolchain?** Cada release etiquetada incluye una imagen fusionada construida desde ese
tag: `espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin`.
Los scripts de `scripts/` son PowerShell; en macOS o Linux los mismos pasos son
`. $HOME/export-esp.sh` y luego `cargo build --release --target xtensa-esp32s3-espidf` en
`firmware/` ([detalles](docs/flashing.md)).

Sin credenciales guardadas, la placa levanta su propio punto de acceso. Se puede conectar a él
desde un teléfono y abrir `http://192.168.4.1/`, o enviar `provision <ssid> <password>` por USB.
Las credenciales van a NVS, nunca dentro de la imagen.

```bash
curl -sS http://<board>/agent.json | jq '.endpoints | length'
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' | jq '.result.tools | length'
curl -k -sS https://<board>/api/tls | jq -r .fingerprint
```

El puerto 443 sirve las mismas rutas sobre TLS con un certificado generado en el primer
arranque; los navegadores avisan, así que conviene comparar la huella. El puerto 80 sigue
abierto, así que un certificado malo cuesta una URL y no la placa. `GET /agent.json` es toda la
interfaz como datos: 40 rutas con método, cuerpo, retorno y autenticación, más cada ajuste
escribible con su tipo y su valor por defecto.

## Dale un modelo

El modelo es un ajuste, no una compilación:

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
curl -sS -X POST http://<board>/api/agent/message -H 'Content-Type: application/json' \
  -d '{"text":"escribe HELLO en el OLED y pon el LED en azul"}'
```

Tres protocolos (`chat`, `responses`, `anthropic`), un nivel de razonamiento, una ventana de
contexto y un tope de salida, en el panel Agent de la consola o con el mismo `POST`. Sin clave,
apunta `base_url` a `http://127.0.0.1/selftest/chat/completions` y el bucle funciona igual.
[Model](docs/model.md) tiene la lista de campos y los tres protocolos en paralelo.

## Uso

| Desde | Qué obtienes |
|---|---|
| `http://<board>/` en el móvil | hablar con el modelo, ver dispositivos y herramientas, cambiarlo todo |
| `POST /mcp` desde un cliente MCP | las mismas herramientas que usa el agente de la placa |
| `POST /api/script`, `POST /api/sched` | ejecutar un script ahora, o programarlo |
| `GET /agent.json` | toda la interfaz como datos: rutas, ajustes, límites |

```bash
curl -sS -X POST http://<board>/mcp -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"shell","arguments":{"command":"ls /"}}}'
```

El agente de la placa también puede añadirse hardware (`manifest_apply`), memoria
(`write /memory/…`) y shell: las mismas llamadas, el mismo PIN.

## Notas de seguridad

- **Las credenciales se quedan en la placa.** El WiFi va a NVS por `provision`, nunca a la
  imagen; `api_key` es de solo escritura y se relee como `__set__`.
- **Pon un PIN antes de que la placa salga de tu mesa.** `auth.portal_pin` protege todas las
  escrituras y las lecturas que exponen el transcript, el prompt, la configuración y los registros.
- **Compara el certificado.** El de la placa es autofirmado; su huella está en `GET /api/tls`.
  El puerto 80 sigue abierto por defecto: un certificado malo cuesta una URL, no la placa; `mcp.plain_http: false` lo cierra, y entonces el cable USB es la única vuelta atrás.
- **`shell`, `script` y las herramientas del manifiesto mueven pines.** Ese es el objetivo de la
  placa; el PIN separa al agente haciendo su trabajo de un transeúnte haciéndolo.

[Security](docs/security.md) dice qué no se defiende a propósito.

## Estructura del repositorio

`crates/` compila y se prueba en un host; `firmware/` es el backend de ESP-IDF. `agent-core` es
la placa detrás de un único trait `Hardware` sin código específico de chip, así que `cargo test`
lo ejecuta en un portátil; `agent-sim` implementa ese trait sobre un fichero de registros.
Fusionarlos metería el simulador en cada imagen y las dependencias del firmware en cada prueba.
`firmware/` queda fuera del workspace porque necesita `cargo +esp`, `-Zbuild-std` y un ESP-IDF
descargado.

```
crates/agent-core/    hardware.rs (the trait) · drivers/ (one module per chip) · boards.rs ·
                      manifest.rs · agent.rs + llm.rs · sched.rs · tools/ ·
                      http.rs · app.rs · sitemap.rs + settings.rs · virtual_board.rs
crates/agent-sim/     the virtual board behind an HTTP port
firmware/             main.rs (boot order) · runner.rs (agent thread, TLS) ·
                      esp_hw.rs (GPIO/I2C/PWM/ADC/RMT/I2S) · console.rs · wifi.rs
```

## Estado

| Parte | Estado |
|---|---|
| `agent-core` | 427 pruebas unitarias y 14 de integración pasan; `scripts/smoke.ps1` pasa de extremo a extremo |
| Simulador | la consola, MCP y el bucle de agente funcionan de extremo a extremo |
| Firmware | grabado en una ESP32-S3-DevKitC-1 N16R8 (IDF v5.5.2): 42 herramientas, OLED 128×32, micrófono y amplificador I²S, WS2812, consola TLS, turnos del modelo por HTTPS |
| No implementado | OTA, tarjeta SD, DHT11/22, cámara. Ver `ROADMAP.md` |

Números del banco: un turno con cuatro herramientas en 13.7 s; consola por TLS en 0.55 s;
escaneo I²C estable en 8 ejecuciones; SRAM interna 5–6 kB libres tras una página TLS, la
restricción más ajustada aquí.

## Documentación

[Arquitectura](docs/architecture.md) · [Manifiesto](docs/manifest.md) · [Capacidades](docs/capabilities.md) ·
[API HTTP y MCP](docs/http-api.md) · [Añadir un dispositivo](docs/adding-a-device.md) · [Grabación](docs/flashing.md) ·
[Solución de problemas](docs/troubleshooting.md) · [Bucle de agente](docs/agent-loop.md) · [Flujo](docs/agent-stream.md) ·
[Skills](docs/agent-skills.md) · [Memoria](docs/agent-memory.md) · [Scripting](docs/scripting.md) ·
[Model](docs/model.md) · [Web console](docs/web-console.md) · [Scheduling](docs/scheduling.md) · [Shell](docs/shell.md) · [Context layers](docs/context.md) · [Security](docs/security.md) · [Simulator](docs/simulator.md) · [Performance](docs/performance.md)

## Contribuir

Un periférico nuevo empieza como fragmento de manifiesto en `examples/manifests/`; un driver en
Rust solo se necesita para temporización, calibración o estado. `CATALOG` es donde un driver se
registra. Las convenciones están en [AGENTS.md](AGENTS.md) y [CONTRIBUTING.md](CONTRIBUTING.md).

## Licencia

MIT. Ver [LICENSE](LICENSE).
