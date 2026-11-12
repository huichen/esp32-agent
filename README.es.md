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
| Un shell y un lenguaje de script | `shell` con 55 comandos con nombres de NuttX, y un intérprete para bucles sobre llamadas a herramientas. [Shell](docs/shell.md) · [Script](docs/scripting.md) |
| Recuerda lo que se le dijo | Órdenes permanentes, `/AGENTS.md`, un índice `/MEMORY.md` y habilidades, ensamblados en el prompt en cada turno. [Lo que el modelo ve en cada turno](docs/context.md) |
| Sin nube en el camino | HTTPS desde la placa, un certificado que genera ella misma, un PIN para tres invocadores. [Autenticación](docs/security.md) |
| Comprobable sin hardware | El simulador ejecuta el mismo `agent-core` contra una placa virtual. [Simulador](docs/simulator.md) |

## Grabar y conectar

Escribe la imagen fusionada de una [release](https://github.com/huichen/esp32-agent/releases) en el desplazamiento 0 (o
compílala, ver [docs/flashing.md](docs/flashing.md)):

```bash
espflash write-bin --chip esp32s3 --flash-size 16mb 0x0 esp32-agent-vX.Y.Z.bin
```

Sin credenciales guardadas, la placa levanta un punto de acceso llamado `esp32-agent-setup`.
Únete desde el móvil, abre **http://192.168.4.1/** y escribe el nombre y la contraseña del
WiFi. Se guardan en NVS, nunca en la imagen, y son lo que la placa necesita para llegar a algo
fuera de la red local: el modelo, un espejo de paquetes, un `/agent.json` en la mesa de al lado.

## Uso

Sirve para dos cosas.

**Una persona con un móvil.** Abrir `http://<board>/` (o lo mismo en https en 443): hablar con
el modelo, ver los dispositivos encontrados, apagar una herramienta, leer el registro, poner el
PIN.

**Un agente al que se le entrega `http://<board>/agent.json`.** Ese documento es la interfaz
como datos: cada ruta con método, cuerpo y autenticación, la superficie MCP, los puntos de
streaming, la tabla de ajustes con tipos y valores por defecto, y los límites. Un agente que
pueda pedir una URL puede usar esta placa sin leer una línea de este README: para eso existe el
endpoint.

## Dale un modelo

Dos formas, los mismos tres campos.

**En la consola.** Abre la página, panel Agent, escribe endpoint, modelo y clave, guarda. La
clave es de solo escritura: va a NVS y se relee como `__set__`.

**Por un agente.** `POST /api/agent/config` con los mismos campos:

```bash
curl -sS -X POST http://<board>/api/agent/config -H 'Content-Type: application/json' \
  -d '{"base_url":"https://your-provider/v1/chat/completions","model":"…","api_key":"…"}'
```

Tres protocolos (`chat`, `responses`, `anthropic`), un nivel de razonamiento, una ventana de
contexto y un tope de salida están en el mismo panel y la misma ruta. Sin ninguna clave, apunta
`base_url` a `http://127.0.0.1/selftest/chat/completions` y el bucle funciona igual, así que una
placa se prueba antes de tener cuenta. La lista de campos está en [Model](docs/model.md).

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
